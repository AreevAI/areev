# Areev Cookbook

Task-oriented recipes with copy-pasteable commands. Every command below is
verified against the `areev` CLI. Run `areev help` for the full usage summary.

Conventions used throughout:

- Every command needs `--db <file>` (the memory file). It is created on first
  write.
- `--ns <namespace>` partitions grains within a file; it defaults to `shared`.
  On **reads** it also takes a prefix scope: `--ns 'org.*'` selects `org` plus
  its `.`-descendants (`org.sales`, `org.sales.emea` — never `organization`).
  Writes and destruction always take one exact namespace.
- `-k <N>` caps result counts.

If you are running from source instead of an installed binary, replace `areev`
with `cargo run -p areev --`.

---

## 1. Add and recall a memory (CLI)

Store a fact (subject–relation–object), then read it back:

```bash
# Add a fact (confidence defaults to 0.9)
areev add --db john.db --ns caller \
  --subject john --relation prefers --object "window seat" --confidence 0.95

# Recall everything about a subject, newest-first (JSON lines)
areev recall --db john.db --ns caller --subject john

# Narrow to one relation, cap results
areev recall --db john.db --ns caller --subject john --relation prefers -k 5
```

`add` prints the new grain's content address (64-hex). Fetch any grain by hash:

```bash
areev get <hash> --db john.db
```

### Render model-ready context

Instead of raw JSON, render recall results into context for a model, under an
optional token budget:

```bash
areev recall --db john.db --ns caller --subject john --render sml
areev recall --db john.db --ns caller --subject john --render markdown --budget 300
```

`--render` accepts `sml`, `toon`, `markdown`, `plain`, or `json`. A one-line
summary (grain count, estimated tokens, whether it was truncated) is printed to
stderr.

### Hybrid text search

`search` runs hybrid recall (structural + BM25, fused with RRF):

```bash
areev search --db john.db --ns caller --query "seat preference" -k 10
```

---

## 2. Run a CAL query

CAL (Context Assembly Language) is Areev's query language. It has no bulk
destruction — `DELETE`/`DROP` are not tokens in the grammar; the only
destructive statement is `FORGET <hash>` (a single-grain tombstone, gated —
disable with `--no-destructive-ops`).

```bash
# Count matching facts
areev cal 'RECALL facts WHERE subject = "john" | COUNT' --db john.db --ns caller

# Recall with a filter
areev cal 'RECALL facts WHERE subject = "john" AND relation = "prefers"' \
  --db john.db --ns caller

# Scope a recall to a namespace subtree ("org" + every .-descendant), or an
# explicit set — the same "org.*" convention works on --ns and in MCP
areev cal 'RECALL facts WHERE namespace = "org.*" AND subject = "acme-hq"' --db org.db
areev cal 'RECALL facts WHERE namespace IN ("org.sales", "personal") AND subject = "john"' --db org.db

# Add through CAL (ADD requires a REASON/BECAUSE clause)
areev cal 'ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "session note"' \
  --db john.db --ns caller

# Assemble one prompt from several sources in a single statement
areev cal 'ASSEMBLE "prompt" FROM
  policies: (RECALL facts WHERE namespace = "org.policies" AND subject = "refunds"),
  profile:  (RECALL facts WHERE subject = "john")' --db john.db

# Ask whether a specific grain exists, or view a subject's history
areev cal 'EXISTS sha256:<64-hex>' --db john.db
areev cal 'HISTORY WHERE subject = "john" AND relation = "prefers"' --db john.db --ns caller
```

For an interactive shell (with `.stats`, `.log`, `.verify`, `.help`, `.quit`
dot-commands):

```bash
areev repl --db john.db --ns caller
```

See [`cal-reference.md`](cal-reference.md) for the full language.

---

## 3. Run the MCP server for Claude Code

Areev ships a built-in MCP server on stdio. Register it with Claude Code in one
line:

```bash
claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db --ns claude-code
```

This exposes 6 tools to the model: `areev_recall`, `areev_add`,
`areev_supersede`, `areev_forget`, `areev_remember`, and `areev_cal`.

Any MCP client can launch the same server directly:

```bash
areev serve --mcp --db ~/.areev/code.db --ns claude-code
```

### Auto-capture each Claude Code turn (optional)

Print a ready-made hook snippet for `~/.claude/settings.json` (it only *prints* —
it never edits your config):

```bash
areev hook claude-code --db ~/.areev/code.db --ns claude-code
```

The snippet wires the `Stop` hook to `areev capture-stop`, which reads Claude
Code's hook JSON on stdin and stores the last exchange as thread-indexed Event
grains.

See [`mcp-reference.md`](mcp-reference.md) for the tool schemas.

---

## 4. Use encryption at rest

Add `--passphrase-env <VAR>` to **any** command to encrypt the database at rest.
Areev derives an AES-256 key (Argon2id) from the passphrase held in the named
environment variable; the non-secret salt is kept in a `<db>.kdf` sidecar.

```bash
# Keep the passphrase in the environment, never on the command line
export AREEV_PASS='correct horse battery staple'

areev add    --db secret.db --passphrase-env AREEV_PASS \
  --ns caller --subject john --relation prefers --object tea
areev recall --db secret.db --passphrase-env AREEV_PASS --ns caller --subject john
```

Back up the `secret.db.kdf` sidecar alongside `secret.db` — without it the key
cannot be re-derived.

> Caveats: the `.blobs` sidecar (large binary payloads) is **not** encrypted, and
> encryption-at-rest uses the storage engine's AES-256-GCM, an experimental
> Turso feature. Treat it as defense-in-depth, not a substitute for
> full-disk encryption. Read [`../SECURITY.md`](../SECURITY.md) first.

---

## 5. Back up with a bundle, then restore

A **bundle** is a portable, incremental, git-shaped backup of the op-log.

```bash
# Write a full backup
areev bundle --db john.db --out john-backup.mgb

# Apply it to another file (fast-forward, idempotent)
areev import --db restored.db --bundle john-backup.mgb
```

For incremental backups, `bundle` prints the cursor for the next run — pass it
back as `--since`:

```bash
areev bundle --db john.db --out inc-01.mgb                 # prints: next --since <N>
areev bundle --db john.db --out inc-02.mgb --since <N>     # only new ops
```

Inspect the change feed at any time:

```bash
areev log   --db john.db --limit 20
areev verify --db john.db      # integrity + full content-address recheck
areev stats  --db john.db
```

---

## 6. Stream / sync between two files

`stream` continuously ships op-log segments (with generations, Litestream-shaped)
to a directory — a local path, an NFS mount, or an object-store mount. `follow`
subscribes and applies new segments; `restore` rebuilds from scratch, including
point-in-time restore.

```bash
# Producer: keep shipping changes to a shared directory
areev stream --db john.db --to ./sync-dir --interval-ms 500

# Consumer: subscribe and apply new segments as they appear
areev follow --db replica.db --from ./sync-dir --interval-ms 1000

# One-shot variants for scripts/cron
areev stream --db john.db     --to ./sync-dir --once
areev follow --db replica.db  --from ./sync-dir --once

# Rebuild a fresh file from a stream dir, optionally to a point in time
areev restore --db new.db --from ./sync-dir
areev restore --db new.db --from ./sync-dir --until-hlc <HLC>
```

Because grains are content-addressed and imports are idempotent, concurrent edits
that arrive out of order become **branches (heads)** with a deterministic
provisional head rather than lost writes.

---

## 7. Use the Python bindings

Install the published package:

```bash
pip install areev
```

Or build from a local checkout with [maturin](https://github.com/PyO3/maturin):

```bash
pip install maturin
maturin develop -m crates/areev-py/Cargo.toml    # into the active virtualenv
```

Then:

```python
import areev, json

m = areev.Areev("john.db", ns="caller")

# Add facts (returns the 64-hex content address)
h = m.add_fact("john", "prefers", "window seat", confidence=0.95)

# Structural recall and CAL both return JSON strings
print(m.recall("john"))
print(m.cal('RECALL facts WHERE subject = "john" | COUNT'))

# Current head for a (subject, relation); evolve it with supersede
head = m.latest("john", "prefers")
m.supersede(h, "fact", json.dumps({
    "subject": "john", "relation": "prefers", "object": "aisle seat"
}))

# Full history, portable backup, integrity check
print(m.history("john", "prefers"))
m.bundle("john-backup.mgb", 0)
print(m.verify())

# Anthropic memory-tool backend, scalars in / JSON string out
print(m.memory_tool(json.dumps({"command": "view", "path": "/memories"})))
```

The bindings follow **scalars in, JSON strings out**; errors raise
`ValueError`. Encryption at rest is available from both bindings, not just the
CLI — pass a `passphrase` to the constructor
(`areev.Areev("john.db", ns="caller", passphrase=os.environ["AREEV_PASS"])` in
Python, `new Areev("john.db", "caller", pass)` in Node). It derives an
AES-256 key with Argon2id exactly as the CLI's `--passphrase-env` does; the key
is host-supplied and never stored in the file, and the `.blobs` CAS sidecar is
**not** covered (see `open_warnings()`).

---

## 8. Open the web console

`ui` serves a local, browser-based console (memories, graph, and query tabs;
light + dark themes; grain inspector):

```bash
areev ui --db john.db
# → areev console → http://127.0.0.1:7437
```

The console binds loopback (`127.0.0.1:7437`) with **no authentication** by
design. It refuses to bind a non-loopback address unless you pass
`--allow-remote` — and even then serves an unauthenticated, writable console over
plaintext HTTP, so only do that behind a TLS-terminating reverse proxy with its
own auth:

```bash
# Choose a different loopback port
areev ui --db john.db --addr 127.0.0.1:8080

# Override the loopback guard (NOT recommended — front it with a TLS proxy + auth)
areev ui --db john.db --addr 0.0.0.0:8080 --allow-remote
```

### Serving the console behind a reverse proxy

`--allow-remote` alone gets you a console you can *see* but not *use*: the
Origin check still rejects every POST, and CAL runs by POST. That check is
CSRF protection, not a formality — browsers cache HTTP Basic credentials and
re-attach them to cross-site requests, so Origin is what tells the console's
own page apart from an attacker's page riding a viewer's cached login. Name
your public origin instead of stripping the header at the proxy:

```bash
export AREEV_TOKEN=$(openssl rand -hex 32)   # entropy IS the control here
areev ui --db john.db \
  --token-env AREEV_TOKEN \
  --allow-origin https://console.example.com \
  --read-only
```

`--allow-origin` takes a comma-separated list and matches **exactly** —
scheme + host[:port], no wildcards, no subdomains — so naming
`https://console.example.com` never also admits
`https://evil-console.example.com`.

`--read-only` is worth adding to any console that is only for looking. On the
Postgres backend it is what makes a least-privilege database role possible at
all: without it, opening a memory runs schema bootstrap and index maintenance
on every open, so the connecting role must **own** the schema — see
[`deployment-profile.md`](deployment-profile.md) for the `GRANT` recipe.

Rate limiting belongs at the proxy, not in `areev`: the console serves one
connection at a time, and it only ever sees the proxy's IP. `areev` logs each
failed auth as `areev: console auth FAILED from <ip> (<n> consecutive)` so you
have something to write a fail2ban-style rule against.

See [`../SECURITY.md`](../SECURITY.md) for the trust model and the operator
hardening checklist, and [`security-model.md`](security-model.md) for the
console's auth surface in detail.

---

## 9. Ingest raw conversation, then distill facts

`remember` stores raw content as an **Event** grain (it prints the hash) and
attaches the facts distilled from it. You supply those facts, or you let a model
extract them.

Every surface that captures raw text writes the same grain through the same
path — `areev remember`, the Python and Node bindings, the MCP `areev_remember`
tool, and the `capture-stop` hook. Pass `--session-id` and `--role` to record a
turn as part of its conversation (`RECALL events WHERE session_id = "..."`).

**Host-supplied** — you already ran your own extractor, so no model is called:

```bash
areev remember --db john.db --ns caller \
  --content "I always want a window seat, and I'm vegetarian." \
  --observer voice-agent \
  --facts '[{"subject":"john","relation":"prefers","object":"window seat","confidence":0.9},
            {"subject":"john","relation":"diet","object":"vegetarian","confidence":0.95}]'
```

**Model-extracted** — `--model provider:name` uses a built-in backend (key read
from the environment: `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`, or `ollama:` for
a local model, never on the command line). `--llm-cmd 'CMD'` is the
zero-dependency escape hatch: your command gets the request JSON on stdin and
prints the response.

```bash
areev remember --db john.db --ns caller \
  --content "I always want a window seat, and I'm vegetarian." \
  --observer voice-agent \
  --model openai:gpt-4o-mini
```

```json
{"event":"a1b2…","facts":["c3d4…","e5f6…"],"model":"gpt-4o-mini",
 "verification_status":"unverified","proposed":2,"dropped":0}
```

Extraction is the point where a model can write its own hallucinations into
memory, so the write is shaped to stay honest about that:

- **The raw text lands first.** The Event is written *before* the model is
  called. A failed or garbage extraction costs you the facts, never the source —
  the hash is still printed, and you can retry the extraction against it.
- **Extracted facts are marked, not trusted.** Each one carries `derived_from`
  (the event), `source_type=derived`, `extractor_model` (which model wrote
  it), and `verification_status="unverified"`. That last one is CAL-filterable,
  so the extraction queue is reviewable:

  ```bash
  areev cal 'RECALL facts WHERE verification_status = "unverified"' --db john.db --ns caller
  ```

- **Grounding is opt-in.** `--ground-model` / `--ground-cmd` runs a *separate*
  call that checks each proposed fact against the source text — proposer ≠
  scorer, the same rule the Areev Loop verifier follows. Facts the grounder does not
  support are dropped; survivors are stamped `"verified"`.
- **Nothing is dropped silently.** `proposed` vs `dropped` in the output account
  for everything the confidence floor (`--min-confidence`) and the grounder
  removed.

Use `--dry-run` to see what a model would extract without writing anything —
useful for iterating on `--extract-hint`, which steers extraction toward your
domain ("only travel preferences; ignore scheduling chatter").

The same knobs exist on the bindings (`model=`, `llm_cmd=`, `ground_cmd=`,
`extract_hint=`, `min_confidence=` in Python; the camelCase equivalents in
Node). MCP's `areev_remember` deliberately has none of this — there the client
*is* a model, so it stores the exchange as an Event and distills with
`areev_add`.

The Anthropic memory-tool backend maps a `/memories/...` file space onto
supersession chains of Fact grains (full wiring guide:
[`memory-tool.md`](memory-tool.md)):

```bash
areev memtool '{"command":"view","path":"/memories"}' --db john.db --ns caller
areev memtool '{"command":"create","path":"/memories/notes.md","file_text":"prefers window seat"}' \
  --db john.db --ns caller
```

## 10. Build an agent that learns (and can unlearn) — by hand

> This section builds the loop **manually** — you drive reflection and the
> writes. For the same loop **governed** — deterministic analyzers that find
> duplicates, contradictions, recurring tool failures, and stale lessons, each
> as a reviewable, undoable, audited recommendation — see
> [§12 Self-improve with Areev Loop](#12-self-improve-with-loop-governed-deterministic)
> and [docs/loop.md](loop.md). Areev Loop sits on exactly the substrate below.

A self-improvement loop is: **act → log experience → reflect → distill lessons
→ recall them next time**. Areev is the substrate for that loop, not the loop
itself: reflection (deriving lessons from experience) is a model call your host
makes, like all LLM work. What the store guarantees is that learning cannot rot
the memory it feeds on *silently* — revised lessons replace instead of
co-ranking (supersession), every lesson links to the experience that taught it
(`derived_from` + `REASON`), replayed or re-synced writes are idempotent
(content addressing), and a bad learning episode can be rolled back
(point-in-time restore). One honest limit: a **paraphrased re-learning is new
bytes and therefore a new grain** — content addressing alone cannot know it's a
duplicate. For that, `areev novelty` gives an *advise-mode* check (below): the
harness looks up the nearest existing lesson before writing and supersedes it
instead of adding a paraphrase. In a learning loop these properties are not
hygiene: rot compounds, because the agent keeps learning from its own mistakes.

Log experience as it happens — `remember` stores each entry as an Event grain
and prints its hash (keep it: the lesson below links back to it). Without
`--model` no model is called, so this stays a pure write — log everything:

```bash
areev remember --db agent.db --ns agent --observer executor \
  --content "Task: fix flaky test. Attempt 1: reran without isolation - FAILED."
areev remember --db agent.db --ns agent --observer executor \
  --content "Task: fix flaky test. Attempt 2: isolated the shared tempdir per test - PASSED."
```

Read experience back for reflection with `areev log --db agent.db` (op-log,
newest ops last) and `areev get <hash>` per grain. Write-cost note: the
microsecond write path assumes the text index is off or deferred — a live FTS
index costs ~140ms/write (`RESULTS.md` finding #1). For high-volume experience
logging, open with `--index-text false` and `areev reindex` before
recall-heavy phases, or keep raw experience and lessons in separate files.

After an episode, reflect (your model call) over the recent experience and
store each distilled lesson as a fact keyed to the skill it belongs to.
`derived_from` links the lesson to the observation that taught it —
structural provenance, not just a comment — and `REASON` records why:

```bash
areev cal 'ADD fact SET subject = "fix_flaky_tests" SET relation = "lesson"
  SET object = "Flaky tests sharing a tempdir need per-test isolation; rerunning alone never fixes them."
  SET confidence = 0.7 SET derived_from = "<observation-hash>"
  REASON "distilled from session flaky-01"' --db agent.db --ns agent
```

Track proficiency as its own supersession chain — `ADD` once, then `SUPERSEDE`
the tip after each success (both print the hash you supersede next time):

```bash
areev cal 'ADD fact SET subject = "fix_flaky_tests" SET relation = "proficiency"
  SET object = "0.30" REASON "first successful fix"' --db agent.db --ns agent

areev cal 'SUPERSEDE sha256:<tip-hash> SET object = "0.55"
  BECAUSE "second successful fix, different repo"' --db agent.db --ns agent
```

Recall surfaces only the current value — no stale value co-ranks with a
revised one. (That guarantee holds *within* a supersession chain: two
independently `ADD`ed facts on the same subject both surface as current, so
revise, don't re-add.) The full learning curve — every level and the reason it
changed — is one query; per-version wall-clock rides the op-log (`areev log`),
since supersession carries the original `created_at` forward:

```bash
areev cal 'HISTORY WHERE subject = "fix_flaky_tests" AND relation = "proficiency"' \
  --db agent.db --ns agent
```

At act time, pull the lessons back into the model's context:

```bash
areev search --db agent.db --ns agent --query "flaky test" -k 5
areev recall --db agent.db --ns agent --subject fix_flaky_tests --render sml --budget 300
```

**Unlearning** is what makes the loop safe to run unattended. A single bad
lesson is superseded (revised) or forgotten (tombstoned) by hash.

For an **episode-scoped** unlearn — undo everything the agent distilled from one
bad session without losing the good writes around it — link each lesson to its
source experience with `SET derived_from = "<observation-hash>"` (shown above),
then walk it back with `areev provenance`:

```bash
# Which lessons came from this observation/session? (reverse provenance)
areev provenance <observation-hash> --db agent.db --ns agent
# Revise or tombstone each returned hash — e.g. forget them all:
areev provenance <observation-hash> --db agent.db --ns agent \
  | python3 -c 'import json,sys; [print(json.loads(l)["hash"]) for l in sys.stdin]' \
  | while read h; do areev cal "FORGET sha256:$h" --db agent.db --ns agent; done
```

`areev provenance` is precise (only grains derived from that source) and keeps
the surrounding good writes intact — the credit-assignment tool for a learning
loop. When you instead need to roll the *whole file* back to a point in time,
checkpoint before risky learning and rewind (this also discards good writes in
the window, and produces a new file you swap in):

```bash
areev stream --db agent.db --to ./checkpoints --once   # checkpoint: ship the op-log
# ... a bad reflection episode writes junk lessons ...
areev log    --db agent.db                             # note the HLC of the last good op
areev stream --db agent.db --to ./checkpoints --once   # ship the rest, then rewind:
areev restore --db rewound.db --from ./checkpoints --until-hlc <HLC>
```

For a typed capability record there is also the OMS **Skill** grain:

```bash
areev cal 'ADD skill SET name = "fix_flaky_tests" SET description = "Diagnose and fix flaky tests"
  SET when_to_use = "test passes alone but fails in suite" SET confidence = 0.3
  REASON "first successful fix"' --db agent.db --ns agent
```

A Skill's `confidence` **is** its proficiency (OMS aliases them), and it
carries definition fields like `instructions` and `when_to_use`. Evolve it with
`SUPERSEDE sha256:<hash> SET confidence = 0.55 BECAUSE "..."` — unchanged
fields carry forward — and fetch any version with `areev get <hash>`. Skill
grains are hash-addressed records today; keep the *queryable* index in facts,
as above.

### Closing the loop automatically (Claude Code)

The steps above are the mechanics; two hooks make the loop run without you
thinking about it. `areev hook claude-code` prints a `settings.json` snippet
that wires both directions:

```bash
areev hook claude-code --db ~/.areev/code.db --ns claude-code   # prints, never writes
```

- **`UserPromptSubmit` → `areev recall-hook`** reads each prompt, hybrid-searches
  memory, and prints matching lessons to stdout — which Claude Code injects as
  context. Retrieval stops depending on the model *choosing* to call a tool.
- **`Stop` → `areev capture-stop`** stores the turn's last exchange as Events,
  including tool calls and their outcomes (a failing `tool_result` is captured
  and flagged), which is the raw signal reflection distills from.

`recall-hook` reads the hook JSON on stdin (`{"prompt": "..."}`), so it also
works from any tool that can run a command per prompt; it stays silent when
there is no prompt or no match, so it never adds noise.

### The reflection harness (your model call)

The *reflect* step — turning captured experience into lessons — is a model
call. Areev will make it for you if you point it at a model (`areev remember
--model` for extraction, §9; `areev loop run --model` for governed
reflection), but nothing runs one by default, and the harness below keeps the
call entirely in your hands. The shape of a nightly (or on-`SessionEnd`)
harness:

```bash
# 1. Pull recent experience (now that the experience log is recallable):
areev cal 'RECALL events RECENT 100' --db agent.db --ns agent --render plain > episode.txt

# 2. Distill lessons with YOUR model (any stdin→stdout command), e.g.:
#    claude -p "Read this session. Emit each durable lesson as one line:
#               subject | relation | object | derived_from=<observation-hash>"
cat episode.txt | claude -p "$(cat reflect-prompt.txt)" > lessons.tsv

# 3. Write each lesson back. --idempotent collapses an exact repeat; for
#    paraphrases, ask `areev novelty` for the nearest existing lesson first and
#    supersede it past a similarity threshold instead of adding a near-dup:
while IFS='|' read -r s r o df; do
  near=$(areev novelty --text "$o" --subject "$s" --relation "$r" \
           --db agent.db --ns agent --embed-cmd 'my-embedder' -k 1)
  sim=$(printf '%s' "$near" | python3 -c 'import json,sys; l=sys.stdin.read().strip(); print(json.loads(l)["similarity"] if l else 0)')
  if awk "BEGIN{exit !($sim > 0.9)}"; then
    hash=$(printf '%s' "$near" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["hash"])')
    areev cal "SUPERSEDE sha256:$hash SET object = \"$o\" BECAUSE \"refined lesson\"" --db agent.db --ns agent
  else
    areev add --db agent.db --ns agent --subject "$s" --relation "$r" --object "$o" --idempotent
  fi
done < lessons.tsv
```

`areev novelty` is *advise-only* — it never drops or writes; the harness decides
supersede-vs-add (the exact failure mode of stores that silently deduped and
lost updates). `--idempotent` handles exact repeats, `SET derived_from` on
lessons lets `areev provenance` walk them back. The engine gives you the safe
substrate; the harness is the one piece you own.

---

## 11. Migrate from mem0, Zep, Letta, LangMem, or Basic Memory

Dump your existing memories to a file (per-source one-liners in
[`migrate.md`](migrate.md)), then:

```bash
areev migrate --from mem0 --file export.json --history history.json --db mine.db
areev migrate --from basic-memory --file ~/basic-memory --db mine.db   # notes → /memories/*
areev migrate --from jsonl --file memories.jsonl --db mine.db          # pgvector/Chroma/homegrown
```

mem0 history becomes real supersession chains with original timestamps;
re-running an import skips what's already there. Check the result:

```bash
areev stats  --db mine.db
areev search --query "anything you remember" --db mine.db
areev memtool '{"command":"view","path":"/memories"}' --db mine.db
```

To embed while importing (vector recall), add
`--embed-cmd 'python3 my_embedder.py'` — the command reads text on stdin and
prints a JSON array of floats.

---

## 12. Self-improve with Areev Loop (governed, deterministic)

[§10](#10-build-an-agent-that-learns-and-can-unlearn--by-hand) builds the loop
by hand. **Areev Loop** governs it: it turns your agent's history into
recommendations — evidence-cited, reviewable, undoable, measured — with **no
model calls**. The 60-second proof needs no agent:

```python
import areev, json
db = areev.Areev("proof.db", actor="user:me")
for _ in range(5): db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2): db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)
db.add_fact("acme", "deploy_target", "us-east-1", 0.9)
db.add_fact("acme", "deploy_target", "eu-west-1", 0.9)     # a contradiction

db.loop_run()                                            # deterministic; bare = never gated
pending = json.loads(db.recommendations('{"status":"pending"}'))
for r in pending: print(r["severity"], r["summary"])

# model judgment — apply one with a reason, dismiss one with a reason
db.apply_recommendation(pending[0]["hash"], because="retries belong in the client")
db.dismiss_recommendation(pending[1]["hash"], "kept the newer region intentionally")
```

From the CLI, the same loop against a seeded demo backend:

```bash
areev init --db demo.db --template demo --ns caller     # plants dupes, a contradiction, a stale grain
areev loop run  --db demo.db --ns caller
areev loop list --db demo.db --ns caller              # the queue
areev loop approve <hash> --db demo.db --ns caller --because "confirmed"
areev loop apply   <hash> --db demo.db --ns caller --because "consolidating"
areev ui --db demo.db --token-env AREEV_TOKEN            # the Areev Loop tab (token-less = read-only)
```

Ops without a daemon — a run is a cheap idempotent command hosts trigger
however they already do (hook, cron, CI):

```bash
areev loop run  --db agent.db --min-new 20 --min-new-errors 3 --if-stale 6h --quiet   # cheap no-op off a watermark
areev loop list --db agent.db --fail-on high --format json                            # CI gate: exit 2 on match
```

Import history that predates Areev, auto-apply structural curation via a
policy file, and the multi-agent supervisor pattern each have a runnable
example under [`../examples/`](../examples/). Auto-apply is off unless a host
`loop-policy.json` grants it, and even then it is limited to non-destructive
structural curation with no model- or tool-derived free text. Full guide:
[docs/loop.md](loop.md).

---

## 13. Answer a data-subject request (access, portability, erasure)

The access request and the erasure run **one selector**, so what you disclose
is exactly what you delete. Three commands, one audit trail:

```bash
# 1. Art. 15 access + Art. 20 portability — nothing is modified.
areev subject-report "pat" --db memory.db --ns caller \
     --out pat.jsonl --bundle pat.mgb

# 2. Art. 17 erasure — history, partition keys (pat#visit1), dictionary
#    entries, and sole-referenced attachments, replicating as tombstones.
areev forget-subject "pat" --db memory.db --ns caller --yes \
     --because "Art. 17 request #42"

# 3. Art. 5(2)/30 evidence — who erased what, when, and why.
areev audit export --db memory.db --out evidence.jsonl
```

Add `--text-mentions` to both the report and the erasure to reach grains
whose *indexed text* mentions the identity (needs the text index on and fully
built; it errors rather than answering partially). The `.mgb` is a portable
bundle any OMS store can import — that is the portability deliverable, not a
screenshot.

The audit record names a **fingerprint** of the identity, not the identity:

```bash
areev audit export --db memory.db | head -1
# {"trail":"destruction","verb":"erase","target":"subject:68d753f055b1a15b ns:caller",
#  "subject_ref":"sha256-64/hex","because":"Art. 17 request #42","grains_erased":2,...}

# Verify a record refers to "pat" by recomputing the digest:
python3 -c 'import hashlib; print(hashlib.sha256(b"pat").hexdigest()[:16])'
# 68d753f055b1a15b
```

That is deliberate: an audit grain is immutable, replicates, and lands in
archives, so writing the identifier there would undo the erasure it records.
The fingerprint is verifiable from a candidate identity but not enumerable.

To make the erasure reach your archives too, run streaming with a retention
window — each checkpoint snapshots the already-erased store, and generations
older than the window are dropped whole:

```bash
areev stream --db memory.db --to /var/lib/areev/archive --checkpoint --retain 30d
```

Full obligation map, deployment requirements, and honest limits:
[`gdpr.md`](gdpr.md).

---

## 14. Capture a reproducible run and export a governed corpus

Bind the host configuration and recall telemetry to one run id, then export
only the transcript selected by read-only CAL:

```bash
areev run-manifest --db agent.db --run-id eval-42 \
  --config '{"model":{"base":"model:v1"},"policy":{"version":"p3"},"seed":7}'

# Use --run-id eval-42 on recall/search/CAL invocations whose full telemetry
# rows should join to this trajectory.
areev corpus --db agent.db --ns caller \
  --select 'RECALL events WHERE session_id = "session-42"' \
  --out train.jsonl --recipient trainer:model-v2
```

The JSONL row uses OpenAI chat messages and a top-level `tools` list. Areev
also emits step quality/loss weights, observation elisions, and the binding to
source hashes, model/policy versions, trace, and data-subject fingerprints.
The export receipt is an immutable grain in `agent:harness` (and therefore
requires `write ON agent:harness` in addition to the selector's read grant); its
`mg:corpus_source` links make every input reverse-traversable and let later
subject or retention erasure identify stale corpus files. Those files must be
retired or re-derived—the receipt is not an unlearning claim about model
weights.

### …then tune a small model on it (the tuning seam)

Hand that corpus to **your** trainer — Areev never trains — and govern the
resulting adapter through the same gates a memory edit passes:

```bash
# The gate first: the evalset hash is the Rule E1 pin.
areev eval create --db agent.db --name support-gate --cases cases.json

# Export + train in one command (or reuse an earlier export with
# --corpus train.jsonl --manifest <hash>). The trainer gets the job spec on
# stdin and AREEV_CORPUS_PATH/AREEV_CORPUS_MANIFEST/AREEV_EVALSET in env,
# and prints an adapter reference on stdout:
#   {"adapter": {"uri", "sha256"}, "base_model", "serves_as",
#    "quantization"?, "serving_runtime"?, "base_build"?, "metrics"?}
areev tune --db agent.db --cmd 'my-trainer --base qwen3-4b' \
  --select 'RECALL events WHERE session_id = "session-42"' \
  --out train.jsonl --evalset <PIN>

# Propose → grade → promote → (regret → roll back):
areev loop run --db agent.db                 # adapter_intake files the rec
areev eval run --db agent.db --evalset <PIN> \
  --model openai-compat:<serves_as>           # vLLM/SGLang; ollama:<name> for GGUF
areev loop approve <rec> --db agent.db --because "corpus + lineage reviewed"
areev loop apply   <rec> --db agent.db --because "gated and green" \
  --gating-run <eval-run-id>
```

The apply writes `(model:<serves_as>, mg:adapter_promotion)` — the host
serves what a live promotion names and stops when it is retracted. Because
the adapter grain's `derived_from` is the corpus manifest, a later
`forget-subject` names the stale adapters right beside the stale corpora.
Serving is any OpenAI-compatible endpoint: set `OPENAI_BASE_URL` (ending in
`/v1`) and a non-empty `OPENAI_API_KEY`, or pass `--base-url`/`--key-env`.

---

## 15. Ship assembly logic in the file (saved queries + templates)

Prompt-assembly logic can live in the memory file as a named, versioned saved
query instead of in your agent's code — so operators re-tune what gets
recalled without redeploying anything. Define it once:

```bash
areev cal --db agent.db 'DEFINE QUERY "session_prompt"($user, $session)
  DESCRIPTION "standard session bootstrap"
AS {
  ASSEMBLE "session" FROM
    profile: (RECALL facts  WHERE subject = $user),
    recent:  (RECALL events WHERE session_id = $session RECENT 10)
  BUDGET 1200 tokens FORMAT sml
}'
```

Any surface can now run it — CLI, MCP (`areev_cal`), the console's SAVED
list, or the bindings:

```bash
areev cal --db agent.db 'RUN "session_prompt"($user = "john", $session = "call-42")'
areev cal --db agent.db 'DESCRIBE QUERIES'   # what this file provides
```

The agent-side pattern (from `examples/hermes/`): at startup, `DESCRIBE
QUERIES` and prefer a deployment-provided query over composing your own —
redefining `session_prompt` in the file changes the agent's context on the
next call, no restart. Saved-query bodies are read-only by construction
(no writes, no destruction, no recursion), so handing one to an agent
never widens its authority.

Custom render templates work the same way — defined in the file, applied
with `FORMAT TEMPLATE <name>`, budget-aware out of the box (`ELEMENT`
degrades to `ELEMENT_SUMMARY` as a token budget squeezes, and
`ELEMENT_OMIT` accounts for what was dropped):

```bash
areev cal --db agent.db 'DEFINE TEMPLATE brief DESCRIPTION "one line each"
  ELEMENT { - {{grain.subject}} {{grain.content}} }
  ELEMENT_SUMMARY { ~ {{grain.subject}} }'
areev cal --db agent.db 'RECALL facts WHERE subject = "john" FORMAT TEMPLATE brief'
```

Both registries are part of the file: they survive a copy, ride `areev
bundle` / `stream` to every replica (usage timestamps stay local), and
`DEFINE`/`DROP` sit behind the `admin` verb so only governing principals can
change them.

---

## 16. Pseudonymize what leaves for the model (anonymization)

Declare a per-namespace policy once and every model-facing read — recall,
search, CAL, MCP, the graph reads — returns typed placeholders
(`[PERSON_1]`, `[PHONE_1]`) instead of identities. An `egress` policy also
covers `areev run`'s abstract nodes, which never were a read: the prompt is
pseudonymized and the model's tool-call arguments are rehydrated before the
tool runs, so the model sees a placeholder and the tool still gets the real
value ([`run.md`](run.md)). The placeholder→value
mapping stays in your process; the model's reply is rehydrated by exact
token replacement. This is pseudonymization, not anonymity — the honest
scope is the egress channel (provider logs, prompt retention).

Try the detector chain with no memory file at all:

```bash
areev anonymize scan --text "reach me at j.doe@acme.io, pin number is 1462"
# → detections: email + pin (Tier-0: regex + checksum + keyword cues)
```

Declare a policy; it travels with the file and replicates to synced copies:

```bash
areev add --db support.db --ns caller \
  --subject caller:john --relation prefers --object "call me at +1 415 555 0142"

areev anonymize set --db support.db --ns caller --policy '{"mode": "egress"}'

areev recall --db support.db --ns caller --subject caller:john
# → subject "[PERSON_1]", object "call me at [PHONE_1]"
# Bare "john" in prose is caught too: the store knows its interned subjects.

areev anonymize list --db support.db        # what is declared
areev anonymize clear --db support.db --ns caller   # back to raw reads
```

The round trip in an app (Python; Node is the same surface, camelCased):

```python
m = areev.Areev("support.db", ns="caller")
m.set_anon_policy("caller", json.dumps({"mode": "egress", "scope": "session"}))

hits = json.loads(m.recall("caller:john"))          # already pseudonymized
reply = llm(prompt_with(hits))                       # model sees tokens only
mapping = json.loads(m.anon_mappings())[0]["mapping"]
final = json.loads(m.rehydrate_text(reply, json.dumps(mapping)))["text"]
```

### Gate the policy in CI

An egress control you cannot test is close to one you cannot use, and a
fixture file is the first artifact an auditor asks for. Write down what the
policy **must** redact and what it **must not**, and fail the build on either
direction:

```json
{
  "policy": {
    "mode": "egress",
    "default_action": "allow",
    "categories": { "sg_nric": "redact", "mrn": "redact", "email": "redact" }
  },
  "must_redact": [
    "NRIC S1234567D on file",
    "MRN 00456123 admitted",
    "contact jane@example.com"
  ],
  "must_not_redact": [
    "invoice total 4471820 aed",
    "S1234567A is not a valid NRIC",
    "the ward saw 12345678 visitors"
  ]
}
```

```bash
areev anonymize test --fixtures policy-fixtures.json
# → 6 fixtures: 6 passed, 0 failed (0 missed, 0 false positive)
# exits non-zero on any miss or false positive
```

The `must_not_redact` half is the load-bearing one: a policy that redacts
everything passes `must_redact` trivially. Note what the negatives above pin —
`S1234567A` has a valid NRIC *shape* but a wrong check digit, and the bare
digit runs are quantities, not identifiers. The national-ID detectors are
checksum-gated (Singapore NRIC/FIN weighted mod-11, UAE Emirates ID Luhn +
`784` prefix), and MRN is cue-gated on a nearby `MRN` / `medical record
number`, because matching bare digit runs would redact every quantity in a
clinical note.

### Redact on context, not just on category

A name alone may be fine in a prompt; a name **beside a condition** is health
data. That is a property of the pair, so no per-category action can express it:

```json
{
  "mode": "egress",
  "default_action": "allow",
  "categories": { "person": "allow", "condition": "allow", "phi": "pseudonym" },
  "term_sets": { "condition": ["type 2 diabetes", "hypertension"] },
  "co_occurrence": [
    { "when": "person", "near": "condition", "within_chars": 120, "as_category": "phi" }
  ]
}
```

"Jane Doe called about the invoice" keeps the name; "Jane Doe was diagnosed
with type 2 diabetes" comes back as `[PHI_1]` — and the placeholder says *why*
it was escalated rather than reading like an ordinary `[PERSON_1]`.

`scope: "session"` keeps tokens stable across calls in one process. MCP and
CAL payloads carry an `anonymized` report with mapping **ids** only — the
mapping itself never rides a payload; the host process rehydrates.

The free-text APIs (`scan_text`/`anonymize_text`) propagate the same
interned-subject table — a name written as a `subject` under the handle's
namespace is caught in prose you scan/anonymize directly, not only in
grain reads:

```python
m = areev.Areev("support.db", ns="caller")
m.add_fact("Kenneth Shea", "role", "sell-side banker", ns="caller")
m.set_anon_policy("caller", json.dumps({"mode": "egress"}))

out = json.loads(m.anonymize_text("Kenneth Shea sent the Project Falcon NDA.", None, None))
out["text"]   # "[PERSON_1] sent the Project Falcon NDA."
```

For identities you hold but never interned as a grain subject — an email's
From header, a CRM row, a project codename — pass them straight in the
policy's `known` list, each with its own category:

```python
policy = json.dumps({"known": [{"value": "Project Falcon", "category": "custom"}]})
out = json.loads(m.anonymize_text("...Project Falcon...", policy, None))
```

Cross-process reconstruction needs the sealed vault (encrypted memory):

```bash
export MEMPASS="correct horse battery staple"
areev anonymize set --db support.db --passphrase-env MEMPASS --ns caller \
  --policy '{"mode": "egress", "scope": "session", "vault": true}'
areev recall --db support.db --passphrase-env MEMPASS --ns caller --subject caller:john
# …later, any process — admin-gated, audited by fingerprint:
areev anonymize reveal --db support.db --passphrase-env MEMPASS \
  --ns caller --token "[PERSON_1]"
```

Store less instead: `{"mode": "ingress"}` (encrypted memory required)
transforms **before** the hash commits — `remember`'s extractor LLM never
sees the raw text, the stored blob holds value-derived tokens, and
`forget-subject`/`subject-report` by the real name still work (the pseudonym
is recomputed from the identity).

Roll out and harden:

```bash
# Measure first: counts per category, no transform (visible in /api/config)
areev anonymize set --db support.db --ns caller --policy '{"mode": "audit"}'

# Per-category actions + a custom dictionary
areev anonymize set --db support.db --ns caller --policy '{
  "mode": "egress",
  "categories": {"credit_card": "redact", "secret": "redact",
                 "phone": "mask", "date": "generalize:month"},
  "custom_terms": ["Project Nightingale"],
  "because": "prompts leave the boundary via provider X"
}'

# Host floor (any verb): force egress on without a declared policy
areev recall --db support.db --ns caller --subject caller:john --anonymize-egress

# Beyond Tier-0: a local NER command and/or a grounded LLM detector
areev recall --db support.db --ns caller --subject caller:john \
  --anonymize-cmd './presidio-bridge.py' --anonymize-llm-cmd 'ollama run llama3'
```

A policy that demands `"detectors": ["tier0","ner"]` with no backend
installed fails the read closed — never a silent downgrade. In the console,
**Connect → Settings** has the declare/clear form and any grain's developer
panel has **Model view** (what the model would see).

**Backends.** Everything above except the value-derived features works on
both backends (conformance-pinned): on **Postgres** there is no page cipher,
so ingress, `memory` scope, and the vault refuse loudly at `set` — use
`egress` with `context`/`session` scope there, and note session tokens are
per-process (each writer holds its own mapping). Cross-process-stable
tokens and the vault are file-backend + encryption features by design.

---

## 17. Attach a file to a memory (CAS blobs)

Grains stay small; media lives in the per-memory content-addressed store and is
referenced by `cas://` URI. The address IS the content, so storing the same
bytes twice stores them once.

```bash
# Store bytes, get the address back (idempotent).
URI=$(areev blob put invoice-001.pdf --db acct.db)
echo "$URI"          # cas://sha256:a0864a70...

# …or from a pipe.
pdftotext invoice-001.pdf - | areev blob put --stdin --db acct.db

# Read them back, hash-verified.
areev blob get "$URI" --db acct.db > roundtrip.pdf
```

Reference the blob from a grain's `content_refs` so erasure and GC can see it:
a blob referenced by no live grain is reclaimed by `gc_blobs`, and a
sole-referenced attachment is reclaimed by `forget-subject` along with the
grain that named it.

**`blob get` deliberately does not open the memory.** The embedded backend
takes an *exclusive* file lock, so while `areev run` holds a memory every other
verb is refused — including a read. A tool subprocess launched by that run
would otherwise be unable to fetch the very attachment it was started to
process:

```bash
# inside a --tool-cmd subprocess, while the run holds the writer:
areev blob get "$attachment_uri" --db acct.db > /tmp/att.pdf   # works
areev recall ... --db acct.db                                   # STO-E001, locked
```

This is safe rather than a loophole: blobs are immutable, live beside the file
rather than in it, and carry their checksum as their address, which the read
re-verifies. For *grains* the doors are different: declare a
`--context-query` on the trigger so the evaluator assembles a saved query's
result into the run input before the run starts, or run the PostgreSQL
backend, where reads never block and a tool may open the memory mid-run
(see [run.md](run.md#backend-divergence-reading-the-memory-mid-run-85)). An **encrypted** memory is the exception — decrypting the sidecar
needs the derived key, so `blob get` opens it and therefore needs
`--passphrase-env` and an unheld file.

Both bindings carry the pair, bytes in and bytes out (not the usual JSON-string
return — base64 through JSON would inflate every payload by a third):

```python
uri = m.put_blob(open("invoice-001.pdf", "rb").read())
data = m.get_blob(uri)          # bytes
```

```js
const uri = await m.putBlob(await fs.readFile('invoice-001.pdf'))
const data = await m.getBlob(uri)   // Buffer
```

There is no MCP tool for blobs, deliberately: MCP results are JSON over stdio,
so a binary payload would have to be base64'd into the response and would blow
the context window it landed in. Tools that need bytes should take the `cas://`
URI from a grain's `content_refs` and shell out to `areev blob get`.

A **sandboxed** capability tool has its own door (#106): declare
`{"blob": {"read": true}}` and call `areev::blob_get`, which reads by content
address through the run's broker and journals a `blob_read`. That is the path
for anything processing untrusted attachment bytes, since it needs no
subprocess and no filesystem access — see [run.md](run.md).

---

## 18. Start a workflow when something happens (triggers)

A trigger is a standing rule that starts a workflow. The cadence lives in the
memory rather than in someone's crontab, so a synced file can say what it was
supposed to be doing — and **there is still no daemon**: `areev trigger run` is
a one-shot command that asks the memory what is due.

Poll a mailbox and start a workflow per message:

```bash
areev trigger add --db ap.db --ns accounting \
  --type polling --observer outlook \
  --scope 'mailbox:accounts@acme.com' --interval 120 \
  --workflow <WF_HASH> --dedup-key /message_id \
  --because "poll the AP mailbox for invoices"

areev trigger run --db ap.db --ns accounting --dry-run    # safe first command
areev trigger run --db ap.db --ns accounting \
  --connector-cmd ./outlook.sh --tool-cmd ./tools.sh
```

`--dedup-key` mints the run id from the item's identity, so a redelivered
webhook, an overlapping cursor, and two nodes racing all produce **one run and
one recorded skip**. The first poll seeds the cursor and fires nothing, so you
do not process the mailbox's history on day one.

A firing gets **the same runner `run start` builds**, so a plan that runs by
hand runs on a heartbeat — the executor pin, the sandbox and the model all
reach it, and each also reads its `$AREEV_RUN_*` variable, because a heartbeat
is a cron line rather than something you type:

```bash
# The cron line: no flags, all host config from the environment.
AREEV_RUN_TOOL_CMD=./tools.sh \
AREEV_RUN_ALLOW_EXECUTOR=1671652297b93a6a… \
AREEV_RUN_SANDBOX_CMD=/usr/local/bin/areev-sandbox \
AREEV_RUN_MODEL=claude-sonnet-5 \
  areev trigger run --db ap.db --ns accounting --max-usd 0.25 --ask-ttl 3600
```

Budgets matter more here than anywhere else: a standing rule fires unattended,
so an unbounded run has nobody watching it and an ask with no TTL parks
forever. Give the trigger a `--name` too — it is what `trigger list` and
`trigger status` print, and unlike the workflow hash it survives re-declaring
the plan (a re-`add` of identical plan JSON mints a **new** grain, because the
content address covers `created_at`; see [triggers.md](triggers.md)).

Fire on the memory's own contents instead of an external source:

```bash
areev trigger add --db crm.db --ns sales \
  --type memory --workflow <WF_HASH> \
  --where 'grain_type = "fact" AND relation = "signed_nda"' \
  --because "kick off onboarding when an NDA is recorded"
```

Wait for two things to arrive before starting anything — an invoice and its
purchase order, correlated by thread and windowed:

```bash
areev trigger add --db ap.db --ns accounting \
  --type composite --workflow <WF_HASH> \
  --members 'invoice=<HASH_A>,purchase_order=<HASH_B>' \
  --where 'invoice = true AND purchase_order = true' \
  --correlate /thread_id --window 10m \
  --because "match an invoice to its purchase order before posting"
```

A gate naming a member the declaration does not carry is refused when you
declare it (`TRG-E008`), not when it first comes due — a trigger that can never
fire has no symptom except silence.

Webhooks arrive through your own listener; Areev never opens a port:

```bash
areev trigger deliver --db crm.db --ns sales --id <TRIGGER> < payload.json
```

Then put evaluation on whatever heartbeat you already run. The rendered
interval is the GCD of your declared intervals, floored at 60s — deliberately
coarser than your shortest trigger, because the memory owns the real cadence:

```bash
areev trigger render --db ap.db --ns accounting --target cron   # or launchd,
                                                                # systemd,
                                                                # k8s-cronjob
areev trigger status --db ap.db --ns accounting   # what fired, what is due
```

`trigger status` is per-host: evaluation state deliberately does not replicate,
so a dev memory restored from prod cannot inherit prod's cursor and silently
skip real work.

In containers the heartbeat is an image command — `docker run areev heartbeat
--ns accounting` loops the same one-shot evaluation on `$AREEV_HEARTBEAT_SECS`
ticks; compose files and the multi-agent fleet pattern are in
[`docker.md`](docker.md).

If your host already embeds Areev, skip the binary — every subcommand above is a
binding method, so the process holding the memory can fire its own rules:

```python
m = areev.Areev("ap.db", ns="accounting")
report = json.loads(m.trigger_run(connector_cmd="./gmail.sh", tool_cmd="./tools.sh"))
```

Full reference: [`triggers.md`](triggers.md).

---

## 19. Minting credentials from a vault

An agent that calls a real API needs a real secret, and the usual arrangement —
export it and hope — has two problems. It is static, so a cloud token that
expires hourly breaks an overnight heartbeat; and it is ambient, so every tool
subprocess can read it out of its own environment and skip the broker entirely.

Areev's answer is in two halves, and it matters that they are separate:

| Half | Question | Mechanism |
|---|---|---|
| The broker | where may a secret be **sent**? | `--allow-host`, `--tool-egress`, the tool's `capabilities` |
| The source | where does a secret **come from**? | `--credential NAME=…` |

A vault only improves the second. It does not remove the broker: a perfectly
minted 5-minute token handed to a compromised tool is still exfiltrated in
under five minutes. Run both.

### The source, in one flag

```bash
# 1. static, as before — right for an API key that does not rotate
--credential zoho=ZOHO_TOKEN

# 2. any command that prints a token on stdout — the general form
--credential 'sheets=cmd:gcloud auth print-access-token'

# 3. Vault / OpenBao natively, so a container needs no vault binary
--credential 'sheets=vault:secret/data/google#access_token' \
--resolver-env VAULT_ADDR,VAULT_TOKEN
```

Form 2 is the one to reach for first: it needs no vendor client in Areev and it
covers every provider below. Values are cached for `--credential-ttl` seconds
(default 300) and minted again after, so a revocation upstream takes effect
without restarting anything.

### Per platform

Each row is a `cmd:` resolver. The auth column is what makes it work with **no
secret on disk** — that is the property worth optimizing for, and it is what
`--resolver-env` then has to carry.

| Platform | Store | How the resolver authenticates | Resolver |
|---|---|---|---|
| AWS | Secrets Manager | EC2 instance role / ECS task role | `cmd:aws secretsmanager get-secret-value --secret-id sheets --query SecretString --output text` |
| GCP | Secret Manager | attached service account / Workload Identity | `cmd:gcloud secrets versions access latest --secret=sheets` |
| Azure | Key Vault | Managed Identity | `cmd:az keyvault secret show --vault-name kv --name sheets --query value -o tsv` |
| Self-hosted | Vault or OpenBao | AppRole, KMS auto-unseal | `vault:secret/data/sheets#token` or `cmd:vault kv get -field=token secret/sheets` |
| Any | short-lived cloud token | ambient credentials | `cmd:gcloud auth print-access-token` |

On the three managed services the resolver needs only its platform's ambient
identity, so `--resolver-env` often names nothing at all — the CLI finds its
config under `$HOME`, which resolvers already get. Self-hosted Vault is the
case that needs `--resolver-env VAULT_ADDR,VAULT_TOKEN`.

### Self-hosted, with Docker

OpenBao is the MPL-licensed fork of Vault and the one to default to for a new
self-hosted deployment. Beside the compose file in [`docker.md`](docker.md):

```yaml
  openbao:
    image: openbao/openbao:latest
    command: ["server", "-config=/bao/config.hcl"]
    volumes:
      - bao-data:/bao/data
      - ./docker/bao.hcl:/bao/config.hcl:ro

  heartbeat:
    image: areev:latest
    command:
      ["heartbeat", "--db", "/data/agent.db", "--ns", "agent",
       "--credential", "gmail=vault:secret/data/google#access_token",
       "--resolver-env", "VAULT_ADDR,VAULT_TOKEN"]
    environment:
      VAULT_ADDR: http://openbao:8200
      VAULT_TOKEN_FILE: /run/secrets/bao_token   # read it into VAULT_TOKEN at entry
```

Use `raft` integrated storage (the recommended backend) and auto-unseal against
a cloud KMS; on the PostgreSQL deployment profile a vault can share the
Postgres *server* in its own database, so you add no new storage system.

**Never store secrets in the Areev memory itself.** Grains are immutable,
content-addressed and they *replicate* — `areev stream`/`follow` would carry
every secret to every replica, and a supersession cannot actually remove the
old value. A declaration names a credential; it never carries one.

### Two things the flags do that are easy to miss

- **`--resolver-env` is a carve-out, not a pass-through.** The variables it
  names are withheld from every *other* subprocess and re-admitted only for
  resolvers. That is the point: a `VAULT_TOKEN` left ambient is readable by
  every `--tool-cmd` you run, and unlike the credential it fetches, it can
  fetch *all* of them.
- **A failing resolver refuses the call.** It never sends the request
  unauthenticated, and the error names which credential failed without
  repeating what the resolver printed. Check for it in the audit trail the way
  you check for any other refusal:

```bash
areev cal 'RECALL observations WHERE namespace = "agent:harness"' --db agent.db
# → observation_kind "egress_refusal", reason "credential 'sheets' could not be resolved"
```

Full rules: [`run.md`](run.md#where-a-credential-comes-from) and
[`security-model.md`](security-model.md).

---

## See also

- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — how Areev is built
- [`cal-reference.md`](cal-reference.md) — the CAL query language
- [`mcp-reference.md`](mcp-reference.md) — the MCP tools
- [`triggers.md`](triggers.md) — the eight trigger kinds, in full
- [`run.md`](run.md) — the governed workflow runtime
- [`docker.md`](docker.md) — the container image: compose, heartbeat, cloud deploys
- [`gdpr.md`](gdpr.md) — GDPR obligations → capabilities (for a DPIA)
- [`../FAQ.md`](../FAQ.md) — concepts and comparisons
- [`../SECURITY.md`](../SECURITY.md) — trust model and hardening
