# Areev launch demo

The launch teaser (a ~50s, 1080p/30fps video) is built from the
[Remotion](https://remotion.dev) source in `remotion/` and rendered on demand
(`out/areev-demo.mp4`) — the rendered mp4 is **not committed** here yet; see
**Re-render** below. The committed `screens/` PNGs (below) are what the README uses.

Flow (mostly animated — one terminal): cold open → **memory rots** (one grain
duplicates into a messy pile as a `×247` counter races up) → **can't rot** (the
pile collapses to one grain, then supersedes — old card slides to history, new
slides in) → **see it run** (the single terminal: idempotent + supersede +
history) → **inspect it** (the web console's *graph view*) → **safe to learn**
(the provenance chain) → **gated by design** (no bulk delete) → **model-native**
(the one-line MCP command) → close card (stats count up). Every command is real and the outputs are
the actual ones the `areev` binary produces. Colours, the logo, and the console
window are lifted from the shipped web console (`crates/areev-server/src/console.html`,
tokens in `remotion/src/theme.ts`) so the video matches the product.

`screens/` holds the three console designs exported straight from the **Paper
design file "Areev"** (the design source of truth, light theme): `memories.png`,
`graph.png`, and `query.png` (CAL query + the live grain inspector). These are
used as the README thumbnails.

> Note: the console/graph views *inside the video* are recreated in Remotion
> from the shipped `console.html` (dark theme) so they animate; the `screens/`
> PNGs are the real Paper exports.

## Re-render

```bash
cd remotion
npm install
npm run render      # → out/areev-demo.mp4
npm run studio      # interactive editor at http://localhost:3000
npm run still -- --frame=45   # a single frame
```

Requires Node 18+ and ffmpeg. First render downloads a headless Chrome shell.

## Edit

Scenes live in `remotion/src/scenes/`; the fake terminal is
`remotion/src/components/Terminal.tsx`; scene order, durations, and the
cross-fades are in `remotion/src/AreevDemo.tsx`. Change a caption or command and
re-render.
