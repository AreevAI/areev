# areev-run-core

The PURE scheduler of `areev run` (governed-agents proposal §4–§6). Sans-IO by
construction: exports `step(env, state, events) -> (commands, state)` and the
vocabulary around it; depends on nothing that can observe the world. The
driver (`areev-run`) owns the store, clock, executors, journal — it feeds
`EventIn`s and performs `Command`s. **Replay IS this function**: feed the
journaled events back, assert the same commands come out.

## Purity is enforced, not aspirational

- CI (`ci.yml` clippy job) fails if `cargo tree -p areev-run-core` contains
  `rand|getrandom|chrono|time|tokio|ureq|reqwest|hyper|mio`.
- This crate's `clippy.toml` disallows `SystemTime::now`/`Instant::now`; CI
  greps that the lint is never `allow`ed.
- **No `HashMap`/`HashSet` anywhere in scheduler state** — hash-iteration
  order is the classic silent replay-divergence source. `Vec` + `BTreeMap`
  only. `serde_json`'s map is its default `BTreeMap` (never enable
  `preserve_order`).
- The only clock the scheduler ever sees is `EventIn::ClockReading`,
  journaled by the driver in decision records.

## Module map

- `error.rs` — the `RUN-Ennn` domain (E001–E020, append-only; format and
  uniqueness pinned by tests).
- `cond.rs` — the frozen v1 condition grammar (`==`/`!=`/`exists`/truthy;
  strict JSON equality, NO coercion, `1 != 1.0` deliberately). Parse errors
  are load-time; **evaluation is total**.
- `plan.rs` — `PlanGraph::build`: V1–V6's pure validation (unique nodes,
  edge integrity, cond parse, Tarjan SCC + every-cycle-has-`max_cycles`,
  reachability) over an OMS §8.4 Workflow. Store-dependent V3/V7/V8 live in
  the driver.
- `state.rs` — `SchedulerState`: everything a checkpoint carries; serde
  byte-stability is what the replay-equivalence gate compares.
- `step.rs` — the BSP machine. Key semantics (v1, pinned):
  - **Re-entry generations**: an edge firing into a resolved node increments
    its generation and resets it — how bounded cycles iterate (the Rev-2
    design's edge states were all terminal and could not).
  - **AND-join**: Ready = no in-edge Pending, ≥1 Fired — edges must
    *resolve*, not fire, so a diamond's untaken arm completes the join via
    dead-path propagation.
  - **Fail-fast**: retries are per-node (never suppressed by other nodes'
    failures — that would be permutation-variant); on close with a
    permanent failure, no edges evaluate, the run drains and `Failed` names
    the LOWEST-index failed node (chosen at close, not arrival).
  - **Wall vs elapsed**: wall charges journaled active segments
    (open→park); the park→close tail is NEVER charged and elapsed accrues
    at close from the journaled close reading — accruing at response-apply
    would make state a function of when the operator typed `resume`, and
    verify could never reproduce it.
  - **Budgets**: every axis checked at superstep open (per-dispatch
    reservation arrives with Wave-2 LLM effects, which carry a reservable
    `max_tokens`).
- `types.rs` — `JournalKey` (`run/task_path/node/attempt/effect_seq/kind`);
  `tool_call_id` = the key's sha256 — unique per occurrence AND reproducible
  under scheduler permutation (random ids would break the permutation gate).

## Wave 2 semantics (pinned)

- **Abstract flows** (`AbstractFlow` in state): one LLM loop per node
  attempt; turns and model-issued tool calls share `effect_seq`. Effect
  resolution sets `FlowNeed`; `progress_open` EMITS in canonical node order
  (resolution has no command sink — emitting there would be arrival-order).
  Tool failures inside a flow are MODEL-VISIBLE error results, never
  scheduler retries. Unknown tool = ONE re-prompt then fail (§6.11);
  strict-arg violations re-prompt via `StepEnv.validate_args`, bounded by
  `max_effects_per_attempt`. A flow torn down mid-round leaves stragglers:
  `resolve_effect` guards resolved nodes so a late tool result cannot flip
  DoneFailed back to DoneOk.
- **Per-dispatch token reservation** (§6.7): `spent + llm_reserve_tokens`
  must fit BEFORE an LLM effect is emitted; on refusal the un-dispatched
  turn survives as a `FlowNeed` and `exhausted` drains the run — a fork
  with raised budgets picks the loop up exactly there.
- **Send** (§5.1): a result's reserved `$send` key = the spawn decision,
  extracted at CLOSE (before reducers; the key is stripped), validated
  all-or-nothing, journaled in `DecisionRecord.spawns`. Task paths are
  `parent/NNNN` with a MONOTONIC per-parent counter (`spawn_counter`) —
  re-entered spawners mint fresh paths, so keys never collide across
  generations; zero-padding makes lexicographic order spawn order. Targets
  must be Host nodes (v1), never the spawner. The target shows `Dispatched`
  while its batch runs and completes with a Null contribution when the
  batch drains — the join below a fan-out. Task retries are per-task
  attempts against the target's retry budget.
- **Reducers**: injected via `StepEnv.reduce` (the driver freezes the table
  in the manifest); merge order is static results by node index, then Send
  results by task path.

## Tests

`cargo test -p areev-run-core`. `tests/scheduler_tests.rs` is the mini-DST
harness: a simulated driver with an adversarial completion-order permutation
knob (hand-rolled xorshift — no rand even in dev-deps). The permutation test
IS the §5.5 gate-3 in miniature; extend it when the scheduler grows (Send,
subgraphs) or the gate proves the wrong thing.
