# Areev as a Hermes Agent memory provider

[Hermes Agent](https://github.com/NousResearch/hermes-agent) has a pluggable
memory layer: one external `MemoryProvider` runs alongside its built-in files.
This directory is that provider, backed by a single Areev file.

Verified against **Hermes 0.16.0** — loaded through its real plugin loader,
subclassing its real ABC, driven through its real `MemoryManager`.

## What it adds

Hermes's built-in memory is two character-capped markdown files (`MEMORY.md`,
2,200 chars; `USER.md`, 1,375) plus an FTS5 index over session history. It is
deliberately small and it works. What it cannot do is keep what it evicts:
when a file approaches its cap the agent consolidates by *replacing* entries,
and the previous wording is gone from disk.

| | Hermes built-in | with this provider |
|---|---|---|
| an entry is rewritten | previous text gone | previous text is still a grain; the sequence stays queryable |
| per-turn recall | both files pasted in whole, every turn | one budgeted `ASSEMBLE` — profile + prior sessions, token-capped |
| current session's turns | in context already | deliberately excluded from injection, so budget goes to what the model *cannot* see |
| "what did I believe last month" | not answerable | `areev_memory` with `action: history` |
| hygiene | — | Areev Loop's deterministic analyzers at session end, advisory only |

Measured here, at 2,080 grains (40 profile notes + 2,000 prior turns), Apple M4
Max: **`prefetch()` p50 0.83 ms** (p95 3.9 ms), `sync_turn()` 0.47 ms. `prefetch()`
sits on Hermes's *synchronous* turn path (`agent/turn_context.py`), which is
why in-process matters — a network-backed provider pays its round trip there,
on every turn.

## Install

```bash
pip install areev                      # into the venv Hermes runs from
ln -s "$(pwd)/examples/hermes/areev" "$HERMES_HOME/plugins/areev"
hermes config set memory.provider areev
```

`$HERMES_HOME` defaults to `~/.hermes`. Confirm with `hermes memory setup`,
which will list `areev` and walk through the config below.

> **Requires an Areev newer than 1.0.4.** The provider uses `index_text=` on the
> constructor and `search()`, both added after that release. Until the next
> release, build the binding from this repo:
> `maturin build --release -m crates/areev-py/Cargo.toml` and `pip install` the
> resulting wheel.

## Configure

Optional; every key has a default. `hermes memory setup` writes
`$HERMES_HOME/areev.json`:

```json
{
  "db_path": "",
  "namespace": "hermes",
  "index_text": true,
  "embed_cmd": "",
  "budget_tokens": 800,
  "recent_turns": 8,
  "loop_on_session_end": true
}
```

`index_text` enables the BM25 leg and is on by default. It was off in the first
version of this provider, because the index was then Turso's experimental FTS
and made every write cost time proportional to the file's size — the one thing
a per-turn writer cannot absorb. Areev now uses its own inverted index
(`docs/facts/bm25-index.md`), so writes stay flat at ~0.4 ms at 4k grains *and*
free-text search works. Turn it off only for a write-heavy edge profile that
never searches by text.

`embed_cmd` adds the vector leg alongside, which catches paraphrase that
lexical matching misses:

```json
{ "embed_cmd": "python3 /path/to/embed.py" }
```

Same contract as the CLI's `--embed-cmd`: text on stdin, JSON array of floats on
stdout (see [`../llm/`](../llm/) for backend patterns). With neither leg, recall
still works through the structural legs (profile by subject, prior turns by
time); only `areev_memory action=search` is unavailable, and it says so rather
than returning an empty list.

## How it maps

| Hermes hook | what this does |
|---|---|
| `prefetch(query)` | one CAL `ASSEMBLE`: profile facts + prior-session turns, budgeted, rendered markdown |
| `sync_turn(user, assistant)` | one Event grain per side, tagged with the Hermes `session_id` |
| `on_memory_write(action, target, content)` | mirrors each `MEMORY.md`/`USER.md` edit as an audit Event **and** a profile Fact |
| `on_pre_compress(messages)` | persists what compression is about to discard |
| `on_session_end(messages)` | `loop_run()` — deterministic analyzers, no LLM, advisory only |
| `get_tool_schemas()` | one tool, `areev_memory`, with `search` / `recall` / `history` / `cal` |

Assembly logic is data, not code: define a saved query named `hermes_prefetch`
in the file and the provider runs that instead, so you can re-tune what gets
injected without touching this plugin or restarting the agent.

```bash
areev cal 'DEFINE QUERY "hermes_prefetch"($user, $query) DESCRIPTION "my prompt"
  AS { ASSEMBLE "session" FROM
         profile: (RECALL facts WHERE subject = $user)
       BUDGET 400 tokens FORMAT markdown }' --db ~/.hermes/areev/default.db
```

## Limits, stated plainly

- **`remove` is never mirrored.** The bridge that notifies providers fires only
  for `add` and `replace` (`action in {"add", "replace"}` in
  `agent/agent_runtime_helpers.py`). So an entry the agent deletes outright
  leaves no record *of the deletion* — though the content itself is still here
  from when it was added. The provider handles `remove` already, so this starts
  working if that guard widens.
- **No LLM distillation.** Turns are stored verbatim as Events; the profile
  comes from mirroring Hermes's own curated `USER.md`/`MEMORY.md` rather than
  from extracting facts out of conversation. That is a deliberate floor, not a
  placeholder: extraction without a verifier is how memory layers accumulate
  confident nonsense. Run `areev loop` for the governed version.
- **Skills are out of scope.** Hermes's procedural memory has its own
  `skills.write_approval` path and no provider hook; a memory provider cannot
  see or govern it.
- **One writer per file.** Concurrent gateway sessions share one Areev file
  behind a mutex, so their writes serialize. Fine for CLI and typical chat
  volume; if you run many concurrent users, give each a `db_path`.

## Verify it end to end

```bash
python3 examples/hermes/test_provider.py     # no Hermes needed
```

With Hermes installed, `hermes memory setup` → pick `areev` → start a session,
then:

```bash
areev recall --db ~/.hermes/areev/default.db --ns hermes/<user> --subject user:<user>
areev cal 'RECALL events WHERE role = "memory_write" RECENT 20' --db ~/.hermes/areev/default.db
```

The second is the audit trail of the agent editing its own memory.
