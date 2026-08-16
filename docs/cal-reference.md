# CAL Reference

**CAL** — the **Context Assembly Language** — is Areev's query language and its
primary API surface. It reads memory (`RECALL`, `ASSEMBLE`, `EXISTS`,
`HISTORY`), writes memory append-only (`ADD`, `SUPERSEDE`), introspects the
store (`DESCRIBE`, `EXPLAIN`), and renders results into model-ready context —
all through one text (or JSON-AST) surface.

A defining property of CAL is that its destructive surface is **shaped and
authorization-gated** (CAL 1.3): destruction takes a hash (`FORGET <hash>`),
an identity (`FORGET SUBJECT`), or an age (`PURGE OLDER THAN`) — never a
predicate — there are no `DELETE`/`DROP`-table tokens in the grammar, every
execution writes an audit Observation, sessions bound to a principal need
the matching `delete`/`erase` grant, and the whole surface can still be
switched off per-process (`--no-destructive-ops`) for untrusted input. See
[§8](#8-destruction-shaped-and-authorization-gated).
This reference documents the language as implemented in `areev-cal`.

For where CAL sits in the system, see [ARCHITECTURE.md](../ARCHITECTURE.md#5-cal-the-context-assembly-language).
For the security rationale, see the [security model](security-model.md).

---

## 1. Query structure

A CAL query is a version prefix (optional), a single statement, and a set of
optional modifiers:

```
[CAL/1]
[LET $name = <recall> ; ...]
<statement>
[| <pipeline stage> | <pipeline stage> ...]
[WITH <option>, <option> ...]
[FORMAT <spec>]
[WITH VARS { "key": "value", ... }]
```

- **Version prefix** — `CAL/1` (defaults to `CAL/1` if omitted).
- **`LET` bindings** — precompute sub-query results into `$parameters` (see
  [§7](#7-let-bindings)).
- **Statement** — exactly one (see [§3](#3-statement-types)).
- **Pipeline** — post-processing stages after `|` (see [§4](#4-the-pipeline)).
- **`WITH` options** — recall/behavior flags (see [§5](#5-with-options)).
- **`FORMAT`** — output rendering (see [§6](#6-output-formats)).
- **`WITH VARS`** — display-only string variables for template formats.

Comments start with `--` and run to end of line.

---

## 2. Grain types in CAL

Statements name grain types by their **plural** form for reads and **singular**
form for writes. The names are case-insensitive. A `—` in the write column
means the type is **engine-authored**: no host surface (CAL, CLI, bindings,
MCP) can create one, by design.

| Plural (read) | Singular (write) |
|---|---|
| `facts` | `fact` |
| `events` | `event` |
| `states` | `state` |
| `workflows` | `workflow` |
| `tools` | `tool` |
| `observations` | `observation` |
| `goals` | `goal` |
| `reasonings` | `reasoning` |
| `consensuses` | `consensus` |
| `consents` | `consent` |
| `skills` | `skill` |
| `recommendations` | — (query-only) |
| `*` / `grains` / `all` | — (wildcard: matches every type) |

`recommendations` (OMS 1.5 `0x0C`) is **read-only**: a recommendation is
engine-emitted and lifecycle-gated, so there is no `ADD recommendation`, and
`ADD`/`SUPERSEDE SET` never create or transition one. Queryable fields:
`target_ref`, `analyzer`, `severity`, `dedup_key`, `rec_status` — the last is
index-layer state rebuilt from the audit chain, never author-written.

---

## 3. Statement types

CAL's AST defines 39 statement variants (CAL 1.3 added the destruction,
control, governance, capture, time/provenance, and graph statements). The ones below are **reachable
from text queries**; access control and governance are in
[§9](#9-access-control--governance-cal-13), destruction in
[§8](#8-destruction-shaped-and-authorization-gated). (A couple of variants
exist in the AST for the JSON-CAL surface only — `FORGET SCOPE` stays out of
the text grammar.)

### 3.1 Read statements

#### `RECALL` — retrieve grains

```
RECALL <plural> [ABOUT "<free text>"] [WHERE <condition>]
       [RECENT <n>] [SINCE "..."] [UNTIL "..."] [LIKE "..."]
       [BETWEEN "..." AND "..."] [CONTRADICTIONS [OF (<sub-query>)]] [LIMIT <n>]
```

```sql
RECALL facts WHERE subject = "john" AND relation = "prefers"
RECALL facts WHERE subject = "john" RECENT 5
RECALL facts ABOUT "seating preferences" LIMIT 10
RECALL facts LIKE "window"
```

`RECALL *` is the explicit spelling of "any grain type" and means exactly the
same as omitting the type.

**The anchoring rule applies to *untyped* recalls only.** An untyped `RECALL`
(`*` / `grains` / `all`) must carry a subject filter or a free-text query
(`LIKE`/`ABOUT`), because `*` is not a *specific* grain type and
`LIMIT`/`RECENT` alone cannot bound an untyped scan; without one it is rejected
with `VAL-E001`. A **typed** recall (`RECALL facts`, `RECALL events RECENT 5`)
needs no anchor — it is bounded by `default_limit` (50) and `max_limit` (1000)
instead, which is what makes `RECALL facts WHERE namespace = "caller" | COUNT`
and the "reflect over recent experience" scans below legal.

This is a *bounding* rule, not an authorization one. If you accept model- or
user-authored CAL, a typed full-type scan up to `max_limit` is reachable —
tune `default_limit`/`max_limit` on the executor config rather than relying on
the anchoring rule to stop it.

- `ABOUT "..."` runs semantic/free-text search (requires a full-text and/or
  vector leg; without them it returns a clear "unsupported" result rather than
  wrong data).
- `LIKE "..."` is the **same free-text leg as `ABOUT`**, spelled for the
  SQL-familiar — not a substring or wildcard filter, and not a separate scan.
  Both set the one free-text query the recall takes, so the two cannot be
  combined in a single `RECALL` (the parser rejects it). Use `WHERE ... =` for
  exact structured matching.
- `WHERE` is a structured filter (see [§3.4](#34-the-where-clause)).
- `RECENT n` is shorthand for "newest n" (`ORDER BY created_at DESC LIMIT n`).
- `SINCE` / `UNTIL` / `BETWEEN ... AND ...` are temporal filters accepting
  absolute dates or relative expressions (`"3 days ago"`).
- `CONTRADICTIONS` keeps only *contested* grains — see below.

##### `CONTRADICTIONS` — what is still disputed

When two writers change the same `(subject, relation)` and later sync, both
versions survive as live **heads** (a fork — see ARCHITECTURE.md §3). Recall
then answers with a deterministically-elected *provisional* head, which looks
like a settled value. `CONTRADICTIONS` asks the opposite question:

```sql
-- only the grains that are currently contested
RECALL facts CONTRADICTIONS

-- scoped: contested grains among the keys the sub-query selected
RECALL facts CONTRADICTIONS OF (RECALL facts WHERE subject = "john")
```

Each returned grain carries `contested_by` — the hashes of the other live tips
for its key — so a model can see *what* it disagrees with, not merely that
something is disputed:

```json
{ "hash": "ecb1af…", "fields": { "object": "enterprise" },
  "contested_by": ["ef8c16…"] }
```

The clause applies **after** every other filter, so it composes with
`ABOUT`/`WHERE`/`SINCE` rather than overriding them, and **before** `LIMIT`:
`LIMIT` bounds the contested grains, not the search that found them, so
`CONTRADICTIONS LIMIT 10` means "ten contested grains" and never "contested
among the first ten". The candidate scan widens to the executor's `max_limit`
(1000 by default) rather than stopping at the recall's usual limit.

An empty result is therefore a meaningful answer: nothing in scope is
contested. The one exception is announced — if the scan itself hit `max_limit`,
the query carries `CAL-W012` and the empty result covers only the grains it
managed to examine. Narrow with `WHERE`/`ABOUT`/`SINCE` when you see it.

To keep your normal result set and merely mark the disputed parts of it, use
[`WITH contradiction_detection`](#5-with-options) instead.

This detects **structural** contradiction — divergent writes to one key. Two
independently-added facts that merely disagree in meaning ("prefers tea" vs
"prefers coffee") are not a fork and are not reported here; that is a semantic
judgement, and belongs to the Areev Loop verifier (`docs/loop.md`).

Resolve a fork with `areev merge` (or `Areev::merge_heads`), which supersedes
every tip with one merged grain and records all parents.

Set operations combine `RECALL`s:

```sql
RECALL facts WHERE subject = "john"
  INTERSECT
RECALL facts WHERE relation = "prefers"
```

`UNION`, `INTERSECT`, and `EXCEPT` are supported (up to 4 operands).

#### `EXISTS` — boolean existence check

```sql
EXISTS facts WHERE subject = "john" AND relation = "allergic_to"
```

Returns whether any matching grain exists, without materializing results.

#### `HISTORY` — version chain

```sql
HISTORY OF sha256:a1b2c3d4...
HISTORY WHERE subject = "john" AND relation = "prefers"
```

Walks the supersession chain for a grain (by content hash) or for a
`(subject, relation)` pair, newest to oldest. `HISTORY OF sha256:x DIFF
sha256:y` compares two versions.

#### `ASSEMBLE` — compose context

```
ASSEMBLE "<topic>" [FOR "<audience>"] FROM <sources> [WHERE ...]
         [BUDGET <n> [tokens|grains]]
         [PRIORITY label: weight, ... | PRIORITY a > b > c]
         [FORMAT <spec>] [WITH dedup(<field>)]
```

Clause order is fixed (it is the OMS §8.2 order): `FOR` directly after the topic,
`BUDGET` before `PRIORITY`, and `FORMAT` before `WITH dedup`. A clause written out
of order is not attached to the statement (e.g. `BUDGET` after `FORMAT` is
silently dropped — see the golden-test notes). `WHERE` applies to the
single-source form only. `PRIORITY` accepts weighted labels (`a: 0.5, b: 0.3`) or
an ordering chain (`a > b > c`, mapped to evenly spaced weights).

Single-source:

```sql
ASSEMBLE "caller profile" FROM facts WHERE subject = "john" FORMAT sml
```

Multi-source with per-source budgets and priorities:

```sql
ASSEMBLE "session prompt" FROM
  policies: (RECALL facts  WHERE namespace = "org.policies" RECENT 10),
  profile:  (RECALL facts  WHERE subject = "john"),
  recent:   (RECALL events WHERE session_id = "call-42" RECENT 10)
BUDGET 1500 tokens
PRIORITY profile: 0.5, recent: 0.3, policies: 0.2
FORMAT sml
WITH dedup(object)
```

Source labels are plain identifiers; `PRIORITY` labels must match them
(`CAL-E035`). `ASSEMBLE` is CAL's context-composition statement: it pulls from
labeled sources (including read-only [facade mounts](../ARCHITECTURE.md#55-assemble-and-facade-mounts),
addressed by the `alias.inner` *namespace string* inside a source's `RECALL` —
e.g. `WHERE namespace = "org.policies"`), applies per-source token budgets and
priorities, deduplicates, and renders one budgeted block. `STREAM ASSEMBLE ...` enables
streamed output. There is a 2000-grain post-dedup cap across all sources.

#### `COALESCE` — first non-empty fallback chain

```sql
COALESCE { RECALL facts WHERE subject = "john" AND relation = "seat" }
      OR { RECALL facts WHERE subject = "john" AND relation = "prefers" }
    ELSE { RECALL facts WHERE subject = "john" RECENT 1 }
```

Tries each branch in order, returning the first that yields results (with an
optional `ELSE` fallback).

#### Wave-2/3 reads (CAL 1.3): time, runs, provenance, forks, the graph

```
ENTITY "<subject>" RELATION "<relation>" AT <epoch-ms> [AXIS world|knowledge]
RUN TRACE "<run-id>" [LIMIT <n>]
RUNS TOUCHING sha256:<hash> [DEPTH <n>]
DERIVED FROM sha256:<hash>
SHOW FORKS
RELATED "<start>" VIA "<r1,r2>" [DIRECTION out|in|both] [DEPTH <n>] [LIMIT <n>]
NOVELTY "<text>" [SUBJECT "<s>"] [RELATION "<r>"] [LIMIT <k>]
DESCRIBE STATS
DESCRIBE INTEGRITY
```

- `ENTITY … AT` is the bitemporal as-of read: `world` answers "what was
  true at T" (validity windows), `knowledge` answers "what did the agent
  know at T" (the supersession chain). Distinct from `SINCE`/`UNTIL`, which
  *filter grains by timestamp* rather than resolving the head at T.
- `RUN TRACE` returns both halves of the run↔memory join in one statement:
  `recorded` (the grains stamped with the run id) and `produced` (what was
  distilled from them, via provenance). `RUNS TOUCHING` is the reverse
  walk — which runs produced or refined a grain; reads leave no grain and
  are never recorded.
- `DERIVED FROM` is reverse provenance (requires `read` on `*` — provenance
  spans namespaces), `SHOW FORKS` lists the open multi-head contradictions,
  and the two `DESCRIBE` reads are the store counters and the
  integrity/content-address recheck (`read` on `*`).
- `RELATED` is the bounded k-hop entity walk (depth 1–4; `in`/`both` only
  see relations the file declares entity-valued). `NOVELTY` is the
  paraphrase check — nearest existing grains to a candidate text; it needs
  a host-installed embedder and refuses cleanly without one.

#### `REPORT SUBJECT` — the DSAR read (OMS 1.6 draft)

```
REPORT SUBJECT "<id>" [WITH text_mentions]
```

```sql
REPORT SUBJECT "pat"
REPORT SUBJECT "pat" WITH text_mentions
```

Everything `FORGET SUBJECT` would erase for one identity — the exact
subject, its partition-style keys (`pat`, `pat#visit1` — never `patricia`),
and the full history — hydrated instead of erased: the GDPR Art. 15
(access) / Art. 20 (portability) answer. `WITH text_mentions` extends the
selection to grains whose indexed text mentions the identity, with the same
hard precondition as the erasure form (the text index must be on and fully
built — never a silent partial answer).

The report and the erasure run **one selector**, so "show me everything,
then delete it" is this statement followed by `FORGET SUBJECT` over exactly
the same set. A pure read: session-namespace-scoped, gated by the `read`
grant only, available on the token-less read-only console, and it writes no
audit grain — the audit obligation is on destruction, not access. The
result carries `identity_names` (every matched dictionary string) and
`grains` (`{hash, type, fields}`). Host spellings: `areev subject-report`
(with `--out`/`--bundle` for JSONL and portable-bundle export), MCP
`areev_subject_report`, Python `subject_report`/`subject_bundle`, Node
`subjectReport`/`subjectBundle`.

### 3.2 Write statements (append-only)

Every write requires a `REASON` (or `BECAUSE`) clause — the provenance of a
change is captured in the change itself. Writes never mutate existing grains;
they add new content-addressed grains.

#### `ADD` — add a grain

```
ADD <singular> SET <field> = <value> [SET <field> = <value> ...]
    [WITH <add option> ...] REASON "<why>"
```

```sql
ADD fact SET subject = "john" SET relation = "prefers" SET object = "window seat"
    SET confidence = 0.9 REASON "caller stated during booking"

ADD goal SET description = "confirm flight" SET goal_state = "open"
    REASON "opened at call start"
```

Omitting `SET namespace = ...` stores the grain in the session namespace.

`ADD` intelligence options: `WITH extract_memories` (decompose content into
atomic facts), `WITH extract_event_date`, `WITH sync`. `WITH auto_relate`
parses but is **not implemented** — it infers no relations and emits CAL-W013.
Link grains explicitly via `related_to`, or widen at read time with
`WITH multi_hop(n)`.
There is also an `ADD workflow "name" ... graph ... BIND ... REASON "..."`
form for DAG-shaped Workflow grains.

#### `MERGE` — close an open fork (CAL 1.3)

```
MERGE "<subject>" RELATION "<relation>" TO "<object>" [CONFIDENCE <n>] BECAUSE "<why>"
```

Writes the resolved value as a merge grain superseding **every** live tip
of the fork (all parents recorded in the grain). Requires an actual fork
(≥2 tips — a merge of one head would be a plain `SUPERSEDE`, so it refuses
instead), the `supersede` grant, and a written reason. `SHOW FORKS` before,
`MERGE` after — detection and resolution in the same language.

> **Recording tool calls: use `record_tool_call`, not raw `ADD tool`.**
> Grains are content-addressed, so byte-identical `ADD tool` writes collapse
> to one grain — five identical retries of a failing call become a single
> stored occurrence, which starves exactly the evidence
> `loop.tool_failure` clusters on. The host API (`record_tool_call` on
> every binding) stamps each call's own identity (`tool_call_id`) so retries
> stay distinct; raw `ADD tool` does not.

#### `REMEMBER` — capture free text (CAL 1.3)

```
REMEMBER "<content>" [WITH session("<id>"), role("<user|assistant|system|tool>"), run("<run-id>")]
```

Stores the text as an Event grain in the session namespace — the same
capture path as `areev remember` and the bindings' `capture`, so the Event
lands in the thread index (`session`) and the run↔memory join (`run`). The
observer is the bound session's principal, never statement text. LLM fact
extraction stays host configuration (`areev remember --model …`) — the
statement carries no model names. Requires the `write` grant.

#### `SUPERSEDE` — evolve a grain

```
SUPERSEDE sha256:<hash> SET <field> = <value> [SET <field> = <value> ...]
    BECAUSE "<why>"
```

```sql
SUPERSEDE sha256:a1b2c3d4... SET object = "aisle seat"
    BECAUSE "caller changed preference"
```

Writes a new version that supersedes the identified grain. The old version is
preserved as append-only history (visible via `HISTORY`), never deleted. A
matching `SUPERSEDE workflow` form exists for Workflow grains.

#### `ACCUMULATE` — numeric/last-writer-wins deltas

```sql
ACCUMULATE state WHERE subject = "john" AND relation = "call_count"
    ADD call_count = 1 REASON "another call handled"
```

Applies numeric `ADD field = delta` operations and `SET field = value`
replacements against the current tip (resolved by hash or by
`(subject, relation)` lookup), producing a new superseding grain.

#### `FORGET` — tombstone a single grain (gated)

```sql
FORGET sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d
```

Removes a single grain by content address (maps to `Areev::forget`). Unlike
`SUPERSEDE`, this is a genuine tombstone — the grain is gone, not versioned.
An optional `BECAUSE "<why>"` is recorded in the audit Observation. The
identity and age forms — `FORGET SUBJECT` and `PURGE OLDER THAN`, both with
mandatory BECAUSE — plus the authorization and audit story are in
[§8](#8-destruction-shaped-and-authorization-gated). All destruction is
refused inside saved-query bodies and capped by `allow_destructive_ops`.

### 3.3 Introspection & management

| Statement | Purpose |
|---|---|
| `DESCRIBE facts` / `DESCRIBE SCHEMA` | Describe a grain type or the whole schema |
| `DESCRIBE CAPABILITIES` | Report the CAL conformance level and supported features |
| `DESCRIBE FIELDS [type]` | List filterable/sortable fields |
| `DESCRIBE TEMPLATES` / `DESCRIBE QUERIES` | List registered templates / saved queries |
| `EXPLAIN <query>` | Return a query plan for a statement |
| `BATCH { stmt1 ; stmt2 ; ... }` | Run several statements as one batch (up to 10 entries; optional labels) |
| `DEFINE TEMPLATE <name> AS "<source>"` | Register a reusable output template (ELEMENT shorthand) |
| `DEFINE TEMPLATE <name> [EXTENDS <parent>] HEADER { } ELEMENT { } …` | Register a sectioned template (OMS CAL §10.6) |
| `DEFINE QUERY "name"($params) AS { body }` | Register a saved, parameterized query |
| `DROP TEMPLATE "name"` / `DROP QUERY "name"` | Remove a template or saved query |
| `RUN "name"($p = v, ...)` | Execute a saved query with bindings |

> **`DROP` is only ever `DROP TEMPLATE` / `DROP QUERY`.** The parser accepts no
> other `DROP` target. Templates and saved queries are host-managed metadata,
> not memory — dropping one removes a definition, never a grain.

Saved queries let prompt-assembly logic live as named, versioned CAL — hot-swappable
without redeploying the agent:

```sql
DEFINE QUERY "session_prompt"($user, $session)
  DESCRIPTION "standard session bootstrap"
AS {
  ASSEMBLE "session" FROM
    profile: (RECALL facts  WHERE subject = $user),
    recent:  (RECALL events WHERE session_id = $session RECENT 10)
  BUDGET 1200 FORMAT sml
}

RUN "session_prompt"($user = "john", $session = "call-42")
```

Saved-query limits: 100 per namespace, 8 KiB body, 10 parameters. Saved-query
bodies get an extra read-only verification pass, so a saved query can never
smuggle in a write or a blocked keyword.

Saved queries and custom templates are **host metadata carried by the memory
file**, not memories: they persist as `meta` rows (`qry:<name>`, `tpl:<name>`)
and so travel with the `.db` — the same set is visible from the CLI, MCP and
the web console. They are never grains, never content-addressed, and never
appear in recall results.

An entry the file carries that the running build cannot load — a template
written before a limit was tightened, a row from a newer version — is skipped
rather than failing the open, and reported: on stderr from `areev cal` / `repl`
/ `serve` / `ui` / `hub`, and in `warnings` on `GET /api/config`. It stays in
the file, so an older or newer build can still read it; overwriting the entry
is what loses it.

### 3.4 The `WHERE` clause

`WHERE` conditions combine with `AND`, `OR`, and `NOT` (up to nesting depth 8):

| Form | Example |
|---|---|
| Comparison | `confidence >= 0.8`, `subject = "john"`, `role != "system"` |
| Membership | `relation IN ("prefers", "likes")`, `subject NOT IN (...)` |
| Null checks | `deadline IS NULL`, `object IS NOT NULL` |
| Text | `object CONTAINS "seat"`, `subject STARTS WITH "caller:"` |
| Boolean logic | `subject = "john" AND (relation = "prefers" OR relation = "likes")` |

Comparators: `=`, `!=`, `>`, `>=`, `<`, `<=`. Values are strings (`"..."`),
numbers, booleans, arrays (`["a", "b"]`), content hashes (`sha256:abcdef...`), or
parameter references (`$name`). Membership sets are capped at 100 values.

`IN` fails **closed** in both directions, which matters because these filters
usually scope a recall to a tenant, a session, or a user:

- `subject IN $var` where `$var` bound to nothing selects **nothing**, not
  everything. "This user has no friends yet" is the natural outcome of the
  two-step pattern, and it must not widen to the whole table.
- An unbound `$var` is an error (`CAL-E008`), not an empty set — scoping to
  nothing silently is as wrong as scoping to everything silently.

`hash` is a filter field like any other (`WHERE hash = "<64-hex>"`, with or
without the `sha256:` prefix, and `hash IN (...)`), even though a grain's
content address lives on the envelope rather than among its fields.

---

## 4. The pipeline

Pipeline stages post-process a statement's result set, chained with `|` (up to
5 stages):

| Stage | Effect |
|---|---|
| `\| SELECT f1, f2` | Keep only these fields |
| `\| PROJECT f1 AS a, f2` | Select with renaming |
| `\| ORDER BY field [ASC\|DESC]` | Sort |
| `\| LIMIT n` / `\| OFFSET n` | Paginate (limit ≤ 1000) |
| `\| COUNT` | Return the count instead of the rows |
| `\| FIRST` | Return only the first result |
| `\| SUBJECTS` / `\| OBJECTS` | Extract the `subject`/`object` of each Fact |
| `\| HASHES` | Extract the content hash of each grain |
| `\| GROUP BY field` | Group results |

```sql
RECALL facts WHERE subject = "john" | SELECT relation, object | LIMIT 5
RECALL facts WHERE namespace = "caller" | COUNT
RECALL facts WHERE relation = "knows" | OBJECTS
```

There is **no `| WHERE` stage** — filtering is a statement clause, not a
pipeline stage. Write `RECALL … WHERE a = … AND b = …` instead. The parser
rejects `| WHERE` with `CAL-E002` and lists the stages it does accept, which is
the same list as the table above and as `DESCRIBE`'s `pipeline_stages`.

---

## 5. `WITH` options

> **Parsing is not support.** The grammar accepts the full CAL spec; what a
> given host *executes* is narrower. An accepted-but-inert option never
> changes results silently — it emits a `CAL-Wnnn` warning (returned under
> `warnings` on every surface, bindings included), and `DESCRIBE
> CAPABILITIES` reports exactly what the host supports before you rely on
> it.

`WITH` options tune recall behavior. There are roughly three dozen; a
representative selection:

| Option | Effect |
|---|---|
| `WITH superseded` | Include historical (superseded) grains, each stamped with the `superseded_by` hash of the version that replaced it. Applies to every recall leg — structural, free-text and vector — so text that survives only in an old version becomes findable. Forgotten grains stay gone. |
| `WITH provenance` | Include the provenance chain in results |
| `WITH include_sources` | Include `derived_from` source grains |
| `WITH explanation` | Stamp each grain with why it matched (which predicate anchored it, whether the free-text leg hit, whether it is history). Template-based, no LLM. |
| `WITH score_breakdown` | Per-leg ranking detail. **Not available on `RECALL`** — the recall path returns fused grains, not per-leg scores, so structural hits all carry the sentinel `1.0`. Passing it emits `CAL-W014` rather than changing the result. |
| `WITH diversity(0.5)` | Apply MMR diversity (optional lambda) |
| `WITH dedup(object)` | Deduplicate, optionally by a field |
| `WITH rerank` / `WITH rerank("model")` | Cross-encoder reranking (feature-gated) |
| `WITH query_expansion` / `WITH query_decompose` / `WITH hyde` | Query rewriting strategies |
| `WITH multi_hop(2)` | Entity-graph expansion (1–3 hops): follows the entities named by the first-pass results and adds what they anchor to the candidate pool, competing within `LIMIT` |
| `WITH recency_weight(0.3)` / `WITH min_score(0.6)` | Scoring controls |
| `WITH conflict_resolution` | Keep only the newest grain per `(subject, relation)` |
| `WITH contradiction_detection` | Keep everything, but stamp `contested_by` on grains that are live tips of an open fork |
| `WITH annotate_relative_time` | Add "2 weeks ago"-style labels |
| `WITH progressive_disclosure(summary)` | OMS progressive-disclosure level |

```sql
RECALL facts ABOUT "dietary restrictions" WITH rerank, diversity(0.4), min_score(0.5)
RECALL facts WHERE subject = "john" WITH superseded, provenance
```

Options requiring an unavailable backend (e.g. a reranker feature that is not
compiled in) return an honest error rather than silently degrading. An option
that parses but cannot change the result on the statement it is attached to
emits `CAL-W014` naming the option and the surface — a warning rather than an
error, because these have shipped for several releases and a hard failure would
break callers who pass them today. Either way the contract is that **silence
means the option did something**.

Warnings reach you as a `warnings` array of `CAL-Wnnn` strings in the result
payload from `cal()` in Python and Node and from the MCP `areev_cal` tool, and
alongside the payload on `POST /api/cal`. The key is present only when there is
something to report, so a clean query returns the shape it always had. The
`areev cal` CLI prints them to stderr instead, keeping stdout pure JSON.

`DESCRIBE`'s `with_options` lists the options that actually change a `RECALL`
result, so a client can introspect rather than guess.

---

## 6. Output formats

`FORMAT <spec>` renders the result set. A single format or a list (up to 5):

| Format | Output |
|---|---|
| `sml` | Structured Memory Language (compact, Claude-class) |
| `toon` | TOON compact tabular blocks |
| `markdown` | Markdown — one assertion per line (see below) |
| `json` | JSON |
| `yaml` | YAML |
| `text` / `table` / `csv` / `triples` | Plain text / Markdown table / CSV / `S R O` triples |
| `structured` / `readable` / `compact` / `data` | OMS §10.1 semantic presets — aliases for `sml` / `markdown` / `text` / `json` |
| `TEMPLATE <name>` | A registered template — **bare** name, quoting makes it a body |
| `TEMPLATE "<source>"` | An inline template body — **quoted** (ELEMENT shorthand) |
| `TEMPLATE { ELEMENT { … } }` | Inline sections |
| `preset "<name>"` | A registered template, older spelling of `TEMPLATE <name>` |

`FORMAT <fmt>` may also be written `AS <fmt>` (§7 `as_clause`), including the
bracketed multi-format list.

A `TEMPLATE` **name** follows the same rules as the name in
`DEFINE TEMPLATE <name>`, so words that happen to be CAL keywords
(`recent`, `scope`, …) work in both places. A *quoted* argument is never a
name — `FORMAT TEMPLATE "..."` is always an inline body.

### What `markdown` renders

`FORMAT markdown` produces one line per grain, stating what the memory asserts:

```
- **john** prefers window seat *(0.95, 2026-01-13)*
- **user**: what's the refund window?
- **stripe_refund** failed: rate_limited 429
```

The trailing italics carry only what changes how much weight to give the line —
confidence when it is below 1.0, the date, and `superseded`. Storage
bookkeeping (namespace, grain type, raw epochs, the content address) is left
out: this output exists to be pasted into a prompt, and none of it helps a
model reason. Use `FORMAT json` when you need the full envelope, or a
`FORMAT TEMPLATE` to choose the fields yourself. A `GROUP BY` render keeps its
group headings.

### Template sections and limits

A sectioned template's `ELEMENT` renders once per grain, `ELEMENT_SUMMARY`
replaces it when the budget squeezes the render below full disclosure, and
`ELEMENT_OMIT` accounts for grains the budget dropped. `SOURCE_BREAK` goes
between `ASSEMBLE` sources — not before the first. `HEADER`/`FOOTER` bracket
the whole render and are the only place `assembly.*` and `budget.*` make
sense; `source.*` is bound only inside an element run.

Sections you do not define are inherited from `EXTENDS <parent>`, defaulting
to `readable`. The three preset parents (`structured`, `readable`, `compact`)
define element-level sections only, so inheriting never adds a header you did
not ask for. `data` cannot be extended (`CAL-E119`).

| Limit (OMS CAL §10.8) | Value |
|---|---|
| Template body | 4096 bytes (`CAL-E040`) |
| Templates per namespace | 50 (`CAL-E118`) |
| Conditional nesting | 5 (`CAL-E117`) |
| `{{#each}}` iterations | 200 — emits `CAL-W011` when it truncates |
| Template name | 64 chars |
| Inheritance depth | 1 |

```sql
RECALL facts WHERE subject = "john" FORMAT sml
RECALL facts WHERE subject = "john" FORMAT [json AS data, markdown AS readable]
```

Rendering is budget-aware and uses progressive disclosure — as a `BUDGET` fills,
grains degrade from full form to summary to omitted rather than the block being
cut mid-token.

---

## 7. `LET` bindings

`LET` precomputes a sub-query into a `$parameter` that later clauses reference —
useful for two-step "find the set, then query within it" patterns:

```sql
LET $friends = SUBJECTS OF (RECALL facts WHERE relation = "knows" AND object = "john");
RECALL facts WHERE subject IN $friends AND relation = "prefers"
```

Extractors are `SUBJECTS`, `OBJECTS`, or `HASHES`. Limits: at most 5 `LET`
bindings per query, each capped at 1000 grains.

---

## 8. Destruction: shaped and authorization-gated

Destruction in CAL takes **a hash, an identity, or an age — never a
predicate** (CAL 1.3 §8.14). Three statements exist, and nothing mutates a
stored blob:

```
FORGET sha256:<hash> [BECAUSE "<why>"]
FORGET SUBJECT "<id>" [WITH text_mentions] BECAUSE "<why>"
PURGE OLDER THAN <n><d|h|m> [TYPE <grain-type>] [IN "<namespace>"] [LIMIT <n>] BECAUSE "<why>"
```

- `FORGET <hash>` — a single-grain tombstone. BECAUSE optional but recorded.
- `FORGET SUBJECT` — identity erasure in the session namespace: every grain
  referencing the identity (history and partition keys included) plus its
  dictionary entries; `WITH text_mentions` extends it to grains whose
  indexed text mentions the identity. BECAUSE mandatory.
- `PURGE OLDER THAN` — the retention sweep, scoped to one namespace (never
  an implicit all-namespace sweep) and optionally one grain type. BECAUSE
  mandatory. Ages read as one word: `90d`, `6h`, `30m`. `LIMIT n` bounds the
  sweep to the **oldest** n matches (default 1000) — how you pilot a new
  retention rule on a reviewable batch, and how you walk a large backlog
  forward one deterministic slice at a time.

**Every execution writes an audit Observation** in the reserved
`agent:authz` namespace — the session principal, the verb, the target, the
reason, and the erased count — recallable like any grain:
`RECALL observations WHERE namespace = "agent:authz"`.

The read-only mirror of `FORGET SUBJECT` is [`REPORT SUBJECT`](#report-
subject--the-dsar-read-oms-16-draft) — the same selector in show-me mode,
for DSAR access/portability before (or without) erasure.

Defense in depth, unchanged in spirit:

1. **Lexer blocklist.** `DELETE` has no token at all, and a set of
   destructive/credential keywords only ever lex as inert identifiers the
   parser hard-rejects: `DELETE ERASE DESTROY TRUNCATE INSERT CREATE WRITE
   STORE KEY ENCRYPT DECRYPT ROTATE MASTER DEK SECRET POLICY SEAL UNSEAL
   CONSENT RESTRICT SCHEMA PARTITION INDEX MIGRATION …`.
2. **Parser shape.** `FORGET USER`/`FORGET SCOPE` are refused (the one
   spelled identity form is `SUBJECT`); `DROP` accepts only
   `DROP TEMPLATE`/`DROP QUERY`; saved-query bodies get a separate
   read-only verification pass, so a stored query can never carry
   destruction.
3. **Authorization.** A session bound to a principal executes destruction
   only under its grants — `delete` for the hash form, `erase` for the bulk
   forms, per namespace; the refusal is `AUT-E001`/`CAL-E121` naming the
   missing verb. An unbound local session is the owner (everything).
4. **The process cap.** `allow_destructive_ops` (**on by default**) still
   turns all of it off per-process — `areev serve --mcp
   --no-destructive-ops` (likewise `areev ui` and `areev cal`) — and a cap
   set restrictive wins over any grant.

**The same primitives back every surface.** CAL `FORGET <hash>`, the Rust
API `forget`, and the MCP [`areev_forget`](mcp-reference.md#areev_forget)
tool tombstone one grain; `FORGET SUBJECT`/`PURGE OLDER THAN` call the same
store machinery as `areev forget-subject`/`areev purge-older-than` and the
bindings' `forget_subject`/`forget_older_than`. Superseded versions remain
as append-only history (via `HISTORY`) regardless.

Two Unicode invariants also run before tokenization:

- **Bidi-override rejection** — bidirectional control characters
  (U+202A–202E, U+2066–2069) are rejected, defeating visual query spoofing.
- **NFC normalization** — the query is Unicode-NFC-normalized before lexing (and
  again when computing the audit hash).

---

## 9. Access control & governance (CAL 1.3)

Sessions bind to a **principal** (`with_principal` in Rust, `--as` on the
CLI once wired; an unbound local session is the owner, with every right).
Rights are per-verb — `read, write, supersede, delete, erase, loop.run,
loop.review, loop.apply, run.execute, run.respond, run.cancel, admin` —
scoped to namespaces, and live **in the
memory file** as ordinary `mg:permits` Facts in the reserved `agent:authz`
namespace: they sync, replicate, and RECALL like anything else. A refused
statement returns `AUT-E001` (wrapped as `CAL-E121`) naming the missing
verb, the namespace, and the principal — exactly what a granting admin
needs.

### DCL

```
GRANT <verb>[, <verb>…] ON <ns|*> TO "<principal>" [WITH because("<why>")]
REVOKE <verb>[, <verb>…] ON <ns|*> FROM "<principal>" [WITH because("<why>")]
SHOW GRANTS [FOR "<principal>"]
DESCRIBE PRINCIPAL "<principal>"
```

Principals are quoted strings (`"agent:support-bot"` — they carry `:` and
`-`). `GRANT`/`REVOKE` require the `admin` grant. A grant grain records the
grantor and reason; `REVOKE` is **retraction by supersession** — a partial
revoke supersedes with the reduced grant, a full revoke leaves a retraction
record, and revoking narrower than a grant's scope is refused by name
(revoke at the grant's own scope). Grant history survives under
`WITH superseded`. A session's rights are fixed when it binds, like a
database connection. No DCL inside saved-query bodies.

### Governance — the loop lifecycle

```
RUN LOOP [FULL] [WITH min_new(N), if_stale("6h")]
DESCRIBE LOOP | ANALYZERS | OUTCOMES | POLICY
APPROVE  sha256:<rec> BECAUSE "<why>"
REJECT   sha256:<rec> BECAUSE "<why>"
APPLY    sha256:<rec> BECAUSE "<why>"
ROLLBACK sha256:<rec> BECAUSE "<why>"
```

`DESCRIBE LOOP` is the in-language `areev loop list`: health plus the
pending queue with the hashes a reviewer acts on. `BECAUSE` is **syntax** —
a parse error when missing. Identity never rides the statement: the actor,
scopes, and observer derive from the bound session, and the engine's four
gates run unchanged — `loop.review` to approve/reject, `loop.apply` to
apply/rollback (a destructive apply additionally needs the session's own
`delete`/`erase`), self-approval blocked against the creator *and* the
principal that triggered the run. `RUN LOOP` is the deterministic pass;
model-attached reflection stays on host surfaces where credentials live,
and the loop's *policy* is never writable from CAL. Governance statements
are refused inside saved-query bodies, and a surface that has not attached
a governance host answers "governance is not wired" instead of pretending.

---

## Safety limits

The parser and executor enforce these hard bounds:

| Limit | Value |
|---|---|
| Max query length | 64 KiB (65,536 bytes) |
| Max nesting depth | 8 |
| Max result `LIMIT` value | 1,000 |
| Max `IN (...)` set size | 100 |
| Max pipeline stages | 5 |
| Max set-operation operands | 4 |
| Max `BATCH` entries | 10 |
| Max `LET` bindings per query | 5 |
| Grain cap per `LET` binding | 1,000 |
| Post-dedup grain cap (`ASSEMBLE`) | 2,000 |
| Max `REASON` length | 500 |
| Max formats per `FORMAT` list | 5 |
| Max `WITH VARS` entries / size | 10 / 1 KiB each |
| Saved queries per namespace | 100 |
| Saved-query body size | 8 KiB |
| Saved-query parameters | 10 |

---

## Copy-pasteable examples

```sql
-- Current preferences for a caller, model-ready
RECALL facts WHERE subject = "john" AND relation = "prefers" FORMAT sml

-- Count everything in a namespace
RECALL facts WHERE namespace = "caller" | COUNT

-- Add a fact (REASON is mandatory)
ADD fact SET subject = "john" SET relation = "allergic_to" SET object = "peanuts"
    SET confidence = 1.0 REASON "stated at intake"

-- Evolve it; the old version is kept as history
SUPERSEDE sha256:<hash> SET confidence = 0.8 BECAUSE "unconfirmed on follow-up"

-- Version history for a (subject, relation)
HISTORY WHERE subject = "john" AND relation = "prefers"

-- Two-step: friends-of-john's preferences
LET $friends = SUBJECTS OF (RECALL facts WHERE relation = "knows" AND object = "john");
RECALL facts WHERE subject IN $friends AND relation = "prefers" | LIMIT 20

-- Assemble a budgeted session prompt from three sources
ASSEMBLE "session" FROM
  profile: (RECALL facts  WHERE subject = "john"),
  recent:  (RECALL events WHERE session_id = "call-42" RECENT 10)
BUDGET 1200 tokens
PRIORITY profile: 0.6, recent: 0.4
FORMAT sml

-- Existence check (boolean)
EXISTS facts WHERE subject = "john" AND relation = "allergic_to"

-- Delete a single grain by content address (gated; on by default,
-- off under --no-destructive-ops, and needs the `admin` scope on the server):
FORGET sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d

-- These FAIL by design — the destructive surface stays narrow:
--   DELETE facts WHERE subject = "john"     -- no token; rejected at the lexer
--   DROP TABLE grains                        -- DROP only accepts TEMPLATE/QUERY
--   FORGET USER "john"                       -- bulk erasure not reachable from text
--   PURGE STALE                              -- not reachable from text
```
</content>
