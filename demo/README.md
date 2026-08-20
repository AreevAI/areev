# Areev demo assets

## `screens/` — the README screenshots

Real screenshots of the shipped web console (`crates/areev-server/src/console.html`)
served by `areev ui` over the committed demo memory, [`data/demo.db`](../data/demo.db).
Nothing is redrawn or mocked: a screenshot that drifts from the product is worse
than no screenshot, so these are regenerated from the binary.

Five pages, each shot light and dark so the README can hand GitHub a
`<picture>` and let the reader's theme choose:

| | |
|---|---|
| `graph-{light,dark}.png` | Memory → Graph, focused on one person at two hops, with the rewind scrubber |
| `workflow-{light,dark}.png` | The eight-step invoice plan, its conditional edges and its human-approval node |
| `runs-{light,dark}.png` | Eight governed runs — six completed, one failed, one waiting on a person |
| `suggestions-{light,dark}.png` | The loop's review queue: a contradiction and a fork, each undoable |
| `analytics-{light,dark}.png` | The grain-type census, namespace breakdown, and active recall legs |

### Re-shoot them

```bash
scripts/build_demo.sh                                   # rebuild data/demo.db first, if it changed
areev ui --db data/demo.db --ns accounting --addr 127.0.0.1:7461 &
npm i playwright                                        # node_modules is gitignored
node scripts/shoot_console.mjs http://127.0.0.1:7461 demo/screens
```

Do this whenever the console changes — the README quotes the UI, and a stale
screenshot is a stale claim.

## `remotion/` — the launch teaser

A ~50s, 1080p/30fps video built from [Remotion](https://remotion.dev) source in
`remotion/` and rendered on demand (`out/areev-demo.mp4`); the rendered mp4 is
**not committed**.

Flow (mostly animated — one terminal): cold open → **memory rots** (one grain
duplicates into a messy pile as a `×247` counter races up) → **can't rot** (the
pile collapses to one grain, then supersedes — old card slides to history, new
slides in) → **see it run** (the single terminal: idempotent + supersede +
history) → **inspect it** (the web console's *graph view*) → **safe to learn**
(the provenance chain) → **gated by design** (no bulk delete) → **model-native**
(the one-line MCP command) → close card (stats count up). Every command is real
and the outputs are the actual ones the `areev` binary produces.

> **Known drift**: the console views inside the video are recreated in Remotion
> from an older `console.html`, so they do not match Console v2 (the redesign the
> `screens/` shots are taken from). Re-render before using it anywhere.

### Re-render

```bash
cd remotion
npm install
npm run render      # → out/areev-demo.mp4
npm run studio      # interactive editor at http://localhost:3000
npm run still -- --frame=45   # a single frame
```

Requires Node 18+ and ffmpeg. First render downloads a headless Chrome shell.

### Edit

Scenes live in `remotion/src/scenes/`; the fake terminal is
`remotion/src/components/Terminal.tsx`; scene order, durations, and the
cross-fades are in `remotion/src/AreevDemo.tsx`. Change a caption or command and
re-render.
