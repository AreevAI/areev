# Which grain?

Areev stores every memory as a **grain**: one immutable, content-addressed
record with a type. There are thirteen types — the twelve OMS 1.5 defines
plus `Trigger` from OMS 1.6 — and most confusion about Areev is really one
question: *which type do I write this as?* This page is the one answer.
Every other doc that touches the question points here, and the "Use it
for" column below is quoted verbatim from the engine's own type registry
(`crates/areev-core/src/types/registry.rs`), test-pinned so the page and
`DESCRIBE <type>` cannot disagree.

The rule that decides most cases:

> **If it's true, Fact. If it happened, Event. If it's measured,
> Observation. If it's intended, Goal. If it's procedure, Workflow. If it's
> capability, Tool.** When two fit, pick the one a later recall query or
> loop analyzer will consume — grains are written to be read.

Everything is a Fact only until it isn't. The quickstart shows a Fact
because it is the shortest write, not because the other types are advanced.

## 1. The decision table

| Grain | Use it for | Don't use it for | Write it with |
|---|---|---|---|
| **Fact** | Durable structured knowledge as subject-relation-object (a preference, an attribute, a setting): what the agent holds as true right now | Free prose (that's an Event), telemetry (an Observation), or anything with a "happened at" rather than an "is" | `areev add S R O` · CAL `ADD fact SET …` · `add_fact` / `addFact` · MCP `areev_add` |
| **Event** | Something that happened at a moment (a message, a decision, an episode): the transcript unit, thread-indexed and never the current-state lookup | Current-state lookups — recall the Fact, not the transcript | `areev remember --content` · CAL `REMEMBER "…"` · `remember()` on every binding · MCP `areev_remember` |
| **State** | A checkpoint or counter that evolves by supersession with its history kept: the escape hatch when no other type fits, not the default | Anything another type models better; a value that is really a Fact | JSON `add("state", {context, plan, history})`, then `supersede` to evolve it; CAL `ACCUMULATE` targets these |
| **Workflow** | A plan: a directed graph of steps bound to tools, immutable and content-addressed, so every edit mints a new hash | Run state or trigger schedules — both point *at* the plan; it stores neither | CAL `ADD workflow "name" … graph` · JSON `add("workflow", …)` for bounded cycles and reducers · the console canvas |
| **Tool** | A tool definition (what can run: schema, executor, locked params) or one execution record (what did run), split by kind | Hand-writing execution records with raw `ADD tool` — identical retries collapse into one grain and starve the loop's evidence | Definition: JSON `add("tool", {kind: "definition", …})`. Execution: `record_tool_call` / `areev record-tool-call` / MCP `areev_record_tool_call` |
| **Observation** | Telemetry, measurements and audit: what an observer noticed, unconfirmed, and what the loop's analyzers read | Knowledge you would recall into a prompt — that's a Fact | CAL `ADD observation SET content = … SET value = … SET unit = …` · JSON `add("observation", …)` |
| **Goal** | The intent of a task: a description, its criteria, and whether it is still open | Progress logging (Events) or metrics (Observations) | CAL `ADD goal SET description = …` · JSON `add("goal", …)`; `related_to`-link the plan to it |
| **Reasoning** | A recorded chain of inference from premises to a conclusion, kept because it will be cited later | Routine step logging — the run journal already captures execution | JSON `add("reasoning", {premises, conclusion, reasoning_type})` |
| **Consensus** | An agreement reached across several observers or agents, with the threshold that made it one | A single agent's decision (Event) or a vote count (Observation) | JSON `add("consensus", {threshold, agreement_count, participating_observers})` |
| **Consent** | A subject's recorded permission: granted or withdrawn, scoped by purpose, the GDPR trail | Authorization grants for CAL — those are `mg:permits` Facts | JSON `add("consent", {subject_did, user_id, consent_action, purpose})` |
| **Skill** | A capability the agent has learned, with a proficiency that tracks practice | A tool catalog — that's Tool definitions | CAL `ADD skill SET name = … SET description = …` · JSON `add("skill", …)` |
| **Recommendation** | A governed proposal the loop made to change memory or configuration: engine-written, never authored by hand | Ad-hoc TODOs — there is no `ADD recommendation` on any surface | `areev loop run` writes them; you `approve` / `apply` / `reject` |
| **Trigger** | A standing rule that starts a workflow (cron, a watched source, a composite gate): the cadence as data, not a daemon | Anything the plan itself should decide — a trigger fires runs, it doesn't branch | `areev trigger add --type KIND --workflow HASH --because "…"` |

Where the table says JSON `add(...)`, that is the generic write both
bindings expose — `add(grain_type, fields_json)` on Python and Node, scalars
in and a hash out. The CLI reaches the same builders through
`areev cal 'ADD …'`, and MCP through `areev_add` with a `type`. Only Fact,
Observation, Goal and Skill can be expressed as flat `ADD <type> SET k = v`
pairs; the rest are graphs, lifecycles or nested records, so they go through
the JSON builders and a bare `SET` form returns `Unsupported` rather than a
permission error.

## 2. The pairs people mix up

**Fact vs Event.** *"John prefers a window seat"* is a Fact: recall it into
a prompt, supersede it when it changes, ask its history. *"John said he
prefers a window seat on the 3 pm call"* is an Event: it happened once, it
belongs to a thread, and it is evidence for the Fact rather than the Fact
itself. `remember` writes the Event and, with an extractor configured,
distils the Fact from it — that is the intended relationship between the
two, not a choice between them.

**Fact vs Observation.** Both are structured, but a Fact is something the
agent asserts and a query relies on; an Observation is something an observer
noticed, with a `value` and a `unit`, that nobody has confirmed. Latency of
a tool call, a run's outcome, an authz decision, a sensor reading — all
Observations. The loop's analyzers read Observations; recall renders Facts.

**Event vs Observation.** Both are timestamped records of the past. The
difference is who is speaking: an Event is a participant's turn (a role, a
session, content), an Observation is an instrument's reading (an observer,
a value). If you would show it in a transcript, Event. If you would plot
it, Observation.

**State vs Fact.** A Fact is one triple with one current head per
(subject, relation). A State is a whole context snapshot — a resumable
checkpoint, a run manifest, a counter with structure — that you evolve by
superseding the entire thing. Reach for State only when the value has no
sensible subject-relation-object shape; if it does, it is a Fact and recall
will rank it better.

**Tool vs Skill.** A Tool definition is what *can run*: a schema, an
executor, its code by content address. A Skill is what the agent has
*learned to do well*, with a proficiency that rises with practice and that
`skill_stall` watches. A tool is installed; a skill is earned.

**Tool execution vs Event.** A tool call is a Tool grain of kind
`execution`, written through `record_tool_call` so each call keeps its own
`tool_call_id` and retries stay distinct. The Event that *mentions* the
call in a transcript is a separate grain. Write the execution record; the
loop's `tool_failure` analyzer clusters on it.

**Reasoning vs Event.** A Reasoning grain is a chain you expect to cite:
premises, a conclusion, and why. The step-by-step of a run is already in
the journal and does not need a Reasoning grain per step.

**Workflow vs Trigger.** The Workflow is the plan; the Trigger is the
standing rule that starts it. The plan never branches on time or on an
external source — that is the trigger's job — and the trigger never
decides what happens inside a run.

## 3. Ask the engine

Every surface that can run CAL can ask the registry directly, so a client
never has to read this table:

```sql
DESCRIBE goals
```

returns the type's `purpose` (the sentence in the table above), its
`required_fields` (what the write path refuses to build without — `goal`
needs `description`, `observation` needs `content`; both come from the
same registry row the validator enforces, test-pinned), its
`specific_fields` (what `WHERE` can filter on), and the `common_fields`
every type shares. `DESCRIBE FIELDS <type>` lists
only the filterable set. The MCP tool `areev_add` quotes the rule of thumb
in its own description and enumerates the addable types from the same
registry.

## 4. Two field traps

**Unknown fields are accepted silently.** A misspelled key is copied into
`extra_fields` and does nothing — a typo never errors, it just quietly
fails to work. Worse, a *recognized* name may have no builder behind it and
be dropped: `goal` recognizes eighteen names but its builder consumes only
`description` (or `object` as a fallback), `subject` and `object`, so
`criteria`, `priority`, `progress` and the rest are accepted and discarded.
There are similar gaps on `observation`, `reasoning` and `consent`.

The dangerous case is a dropped field whose *default* then appears in
recall, because that reads as success. Write a goal with
`goal_state = "open"` and recall it: `goal_state` comes back `"active"` —
the constructor's default, not your value.

So check what round-trips before you build on a field. `DESCRIBE <type>`
lists what filters; a write followed by a `RECALL` shows what was actually
kept. (Some names are not even writable from CAL text: `progress` is a
reserved word, so `SET progress = …` is a `CAL-E002` parse error rather
than a silent drop.)

**Two names are not what you'd guess.** `tool` takes `input_schema` /
`output_schema` — there is no bare `schema` — and workflow edges are
`src` / `dst`, never `from` / `to`.

## 5. `add` vs `supersede` — the same choice in time

Choosing the type is half the decision; the other half is whether the new
grain *replaces* an old one. A grain carries two clocks: `valid_from` /
`valid_to` (when it was true in the world) and the system clock (when you
came to know it), and `entity_at(..., axis="world"|"knowledge")` reads them
separately. The rule that falls out is sharp:

- a **variation** — a new state that coexists with the old one in its own
  time window — is an **`add`**. The world axis picks among *live* grains by
  their validity window, so both windows must stay live.
- a **restatement** — you were wrong, or you learned late — is a
  **`supersede`**. The knowledge axis walks the supersession chain, so a
  correction has to be linked to what it corrects.

Get it backwards and the reads go quiet rather than wrong: superseding a
still-valid window hides it from the world axis forever, and adding a
correction as a fresh grain leaves the knowledge axis unable to find it.
`system_valid_from` is not settable — the store copies `created_at` into
it. [`examples/agents/insurance-documents/`](../examples/agents/insurance-documents/)
turns this into the difference between telling an insured they are covered
and telling them they are 112,000 short.

## 6. Where each type is specified

The format contract — header byte, required fields, the shared envelope —
is [`ARCHITECTURE.md` §2.3](../ARCHITECTURE.md#23-the-12-grain-types). CAL's
plural/singular spellings are [`cal-reference.md` §2](cal-reference.md#2-grain-types-in-cal).
Workflow, Tool and Trigger have their own references:
[`run.md`](run.md), the code-as-a-grain section of
[`how-to-create-an-areev-agent.md`](../examples/how-to-create-an-areev-agent.md),
and [`triggers.md`](triggers.md). Recommendations are
[`loop.md`](loop.md); Consent and the erasure trail are
[`gdpr.md`](gdpr.md) and [`erasure.md`](erasure.md).
