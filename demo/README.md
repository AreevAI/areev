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

## The walkthrough film — not in this repo

The README's Getting Started links a ~2m35s narrated film: what Areev is (the
four questions a self-improving agent has to answer, and the mechanism that
answers them), then one agent running for real — the invoice desk, from an
invoice landing in an unwatched mailbox, through the plan on the console
canvas and *"may I remember this?"*, to a person signing the lesson and the
next invoice being categorised from what was signed — then where to find that
agent in
[`examples/agents/invoice-to-accounting`](../examples/agents/invoice-to-accounting/).

**It is hosted on YouTube and its sources live outside this repo**, deliberately:
video files are tens of megabytes and would bloat every clone forever, and the
render toolchain (Remotion + an ElevenLabs narration track) is a content
pipeline, not part of the engine. The old `remotion/` launch-teaser project
that used to sit here was removed on 2026-08-26 for the same reason — and its
console views had drifted from Console v2 besides.

What lives where:

| | |
|---|---|
| The film, as published | YouTube (linked from the README) |
| Sources — beats, narration manifest, render config | the content repo's `content/video/remotion/` |
| Rendered cuts (landscape / Short / Reel) + poster | `~/Movies/areev-agent-film/`, untracked |
| The screenshots inside it | a real run on a live tenant — the same agent as the example above |

A console change means the film's captures go stale the same way `screens/`
does. Re-shoot, re-render, re-upload, and keep the YouTube URL — the README
points at the video id, so replacing the upload in place is what keeps the
link alive.
