# Areev — demo video script & storyboard

A shot-by-shot script for a ~3-minute launch demo. Optimized for a terminal
screen-cast (asciinema/`vhs`) with optional voice-over. Keep it real: every
command below actually runs and was verified against `areev` 1.0.0. Record at a
calm pace; viewers should be able to read each result.

**Goal of the video:** in ~180 seconds, show that Areev is (1) an *embedded*
memory engine — a file you own, microsecond recall, (2) *memory that can't
silently rot* — immutable, content-addressed, supersession + provenance, (3)
*safe for agents that learn* — the loop where rot compounds, made auditable and
reversible, (4) *model-native* — one line to give an agent memory.

---

## Cold open (0:00–0:12)

- **Screen:** black terminal, Areev wordmark fades in.
- **VO / caption:** "Your AI agent's memory shouldn't be a database server on the
  other side of a network call. It should be a file you own — and one that can't
  quietly rot."
- **Cut to:** an empty prompt.

## Beat 1 — Add & recall (0:12–0:38)

- **Type:**
  ```bash
  areev add --db john.db --ns caller --subject john --relation prefers --object "window seat"
  areev recall --db john.db --ns caller --subject john
  ```
- **Show:** the recall returns the grain instantly. Highlight there was **no
  server, no embedding API call** — it's in-process, microsecond-class.
- **Caption:** "Store a memory. Recall it. No sidecar, no network hop."

## Beat 2 — It doesn't rot (0:38–1:08)

- **Type** — re-learn the same value idempotently, then edit it:
  ```bash
  H=$(areev add --db john.db --ns caller --subject john --relation prefers --object "window seat" --idempotent)
  areev add --db john.db --ns caller --subject john --relation prefers --object "window seat" --idempotent   # → same hash, "(unchanged)"
  areev cal "SUPERSEDE sha256:$H SET object = \"aisle seat\" BECAUSE \"changed mind\"" --db john.db --ns caller
  areev recall  --db john.db --ns caller --subject john                              # → aisle seat (current only)
  areev history --db john.db --ns caller --subject john --relation prefers           # window seat kept, not lost
  ```
- **Show:** the second add prints the **same hash** and says *(unchanged)* — one
  grain, not two. The edit supersedes; recall returns the **current** value
  (aisle), and `history` shows the old value is *kept, not overwritten*.
- **Caption:** "Re-learning the same value is a no-op — no duplicate bloat.
  Edits supersede; the old version stays in history. Nothing silently lost,
  nothing silently duplicated."
- **VO (optional):** "And it's not a promise — it's measured. `honesty_metrics`
  reproduces it deterministically, no LLM in the loop."

## Beat 3 — A query language with no bulk delete (1:08–1:28)

- **Type:**
  ```bash
  areev repl --db john.db --ns caller
  # in the shell:
  RECALL facts WHERE subject = "john" | COUNT
  DESCRIBE CAPABILITIES
  ```
- **VO / caption:** "Query with CAL. And notice what you *can't* do — `DELETE`
  and `DROP` aren't even tokens in the grammar. The one destructive statement is
  a gated, single-grain `FORGET <hash>` — no query can wipe a namespace."

## Beat 4 — Safe for agents that learn (1:28–2:05)

- **Setup (off-camera or shown):** an agent logs an experience, then distills a
  lesson **linked to that experience**.
  ```bash
  OBS=$(areev remember --db agent.db --ns agent --observer executor \
    --content "session 41: flaky test fixed by isolating the tempdir" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["event"])')

  areev cal "ADD fact SET subject = \"fix_flaky\" SET relation = \"lesson\" \
    SET object = \"Isolate the shared tempdir per test.\" SET derived_from = \"$OBS\" REASON \"distilled\"" \
    --db agent.db --ns agent
  ```
- **Type** — trace a lesson back to the experience that taught it:
  ```bash
  areev provenance "$OBS" --db agent.db --ns agent      # every lesson distilled from that session
  ```
- **VO / caption:** "In a learning loop, rot *compounds* — an agent that
  re-learns duplicates and keeps stale lessons gets worse, not better. Every
  lesson links to the experience that taught it, so you can trace it — and undo a
  bad session precisely. A novelty check supersedes a paraphrase instead of
  duplicating it. Memory safe enough to let your agent learn on it."

## Beat 5 — One line of memory for an agent (2:05–2:30)

- **Type:**
  ```bash
  areev hook claude-code --db ~/.areev/code.db --ns claude-code   # prints the settings.json snippet
  claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db --ns claude-code
  ```
- **Cut to:** a Claude Code (or any MCP client) session recalling something from a
  *previous* session — automatically, because the printed `UserPromptSubmit` hook
  injects matching memory before each prompt.
- **Caption:** "One line gives any MCP client persistent memory — recall injected
  automatically, each turn captured. Works with Claude Code today; it's just MCP."

## Beat 6 — Yours, private, portable (2:30–2:52)

- **Type:**
  ```bash
  export AREEV_KEY="correct horse battery staple"
  areev add --db secret.db --ns caller --subject john --relation ssn --object "***" --passphrase-env AREEV_KEY
  areev stream --db john.db --to ./backup/     # git-style op-log shipping
  areev verify --db john.db                    # full content-address recheck
  ```
- **Show:** encryption-at-rest turning on (AES-256-GCM warning), a backup stream,
  and an integrity verify pass.
- **Caption:** "Encrypted at rest. Backed up like git. Verifiable to the byte.
  One memory = one file you can move, encrypt, or crypto-erase."

## Close (2:52–3:05)

- **Screen:** the benchmark headline — "structural recall ~28µs · LoCoMo
  retrieval 81.6% hit@20 (54.2% end-to-end with an untuned reader) · dedup,
  staleness & provenance measured, no LLM" — then the repo URL.
- **VO / caption:** "Areev 1.0. The embedded memory engine for AI agents —
  memory that doesn't rot. Open source. github.com/AreevAI/areev"

---

## Production notes

- **Tooling:** [`vhs`](https://github.com/charmbracelet/vhs) for a scripted,
  reproducible cast, or [`asciinema`](https://asciinema.org) for a live capture +
  [`agg`](https://github.com/asciinema/agg) to export a GIF for the README.
- **Pre-seed** `john.db` and `agent.db` with a little history off-camera so
  `history` and `provenance` look lived-in, or run the beats in order so they
  build naturally. Beat 4's `$OBS` capture needs the two commands run in the same
  shell session (the event hash flows from `remember` into the lesson).
- **Novelty check (Beat 4 VO):** `areev novelty` needs an embedder
  (`--embed-cmd`); mention it in narration rather than wiring one on-camera, or
  pre-install one so `areev novelty --text "give each test its own tempdir"
  --subject fix_flaky --relation lesson` returns a live similarity.
- **Font:** a legible mono at large size; dark theme; hide the shell prompt noise.
- **Length:** keep under ~3 minutes; the first 15 seconds must land the "a file
  you own that can't rot" hook.
- When a new demo is recorded, render it from `demo/remotion/` and add it back to
  the README (a poster thumbnail linking to the hosted video) — see `demo/README.md`.
  The committed `demo/screens/*.png` console shots stay regardless.
