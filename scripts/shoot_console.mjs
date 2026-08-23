/* Screenshot the shipped web console against the demo memory.
 *
 * These are the README's screenshots, and they are real: the console is the
 * one in `crates/areev-server/src/console.html`, served by `areev ui` over
 * `demo.db`. Nothing is mocked or redrawn, which is the point — a screenshot
 * that drifts from the product is worse than none.
 *
 *   areev ui --db ~/Documents/areev/demo.db --ns accounting --addr 127.0.0.1:7461
 *   node scripts/shoot_console.mjs [BASE_URL] [OUT_DIR]
 *
 * Each shot is taken twice, light and dark, so the README can hand GitHub a
 * <picture> and let the reader's theme pick.
 */
import { chromium } from 'playwright';
import { mkdir } from 'node:fs/promises';

const BASE = process.argv[2] || 'http://127.0.0.1:7461';
const OUT = process.argv[3] || 'demo/screens';

/* One entry per shot: the hash route, and an optional settle step run after
 * the page renders (graph layouts need to converge before they photograph
 * well). */
const SHOTS = [
  {
    name: 'graph',
    hash: '#graph',
    settle: async (page) => {
      await page.waitForSelector('#gcanvas', { timeout: 15000 });
      await page.waitForTimeout(4500);          // force-directed layout settles
      // The graph draws entities, not the values hanging off them, so the
      // whole file fits in one frame. Widen the focus to two hops and fit —
      // which is how the view is actually used, not a screenshot-only mode.
      // Layout is seeded from the node names, so this frames the same way
      // every time.
      await page.click('#gdepth button[data-d="2"]').catch(() => {});
      await page.waitForTimeout(1500);
      await page.click('#gfit').catch(() => {});
      await page.waitForTimeout(1800);
    },
  },
  {
    name: 'workflow',
    hash: '#workflows',
    // A plan is wide and short. On a tall viewport `Fit` has to shrink it to
    // fill the height, so this one gets a shorter window instead.
    viewport: { width: 1440, height: 620 },
    settle: async (page) => {
      // Open the biggest plan in the file — the invoice one, with the branch
      // and the human gate. Resolved at run time because a plan's hash is its
      // content, so it changes whenever the seeder does.
      const hash = await page.evaluate(async () => {
        const r = await fetch('/api/browse?limit=500').then((x) => x.json());
        const all = (r.grains || r.items || []).filter((g) => g.type === 'workflow');
        all.sort((a, b) => (b.fields?.nodes?.length || 0) - (a.fields?.nodes?.length || 0));
        return all[0]?.hash || '';
      });
      if (hash) {
        await page.evaluate((h) => { location.hash = '#workflows/' + h; }, hash);
        await page.waitForTimeout(3000);
      }
      await page.click('#wffit').catch(() => {});
      await page.waitForTimeout(900);
      await page.waitForTimeout(1200);
    },
  },
  {
    name: 'runs',
    hash: '#runs',
    settle: async (page) => { await page.waitForTimeout(2000); },
  },
  {
    name: 'suggestions',
    hash: '#suggestions',
    settle: async (page) => { await page.waitForTimeout(2000); },
  },
  {
    name: 'analytics',
    hash: '#analytics',
    settle: async (page) => { await page.waitForTimeout(2500); },
  },
];

const browser = await chromium.launch();
await mkdir(OUT, { recursive: true });

for (const theme of ['light', 'dark']) {
  const ctx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 1.5,   // ~2160px wide: crisp on retina, sane in a git repo
    colorScheme: theme,
  });
  const page = await ctx.newPage();
  for (const shot of SHOTS) {
    await page.setViewportSize(shot.viewport || { width: 1440, height: 900 });
    const url = `${BASE}/?theme=${theme}${shot.hash}`;
    await page.goto(url, { waitUntil: 'networkidle' });
    await page.waitForTimeout(1200);
    if (shot.settle) await shot.settle(page);
    const file = `${OUT}/${shot.name}-${theme}.png`;
    await page.screenshot({ path: file });
    console.log('wrote', file);
  }
  await ctx.close();
}

await browser.close();
