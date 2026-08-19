# areev-run

The `areev run` DRIVER — a host over Areev (peer of areev-mcp), composing
the pure scheduler (`areev-run-core`) with the store. All store writes
happen on the driver thread in command order; parallelism lives exclusively
in the executor pool.

## The journal (§5.1) — existing vocabulary only

- **Intent** = Tool grain `status=Pending`, written BEFORE dispatch, carrying
  as content fields: `run_id`, `task_path`, `node`, `attempt`, `effect_seq`,
  `superstep`, `effect_kind`, the `mg:step_action:<node>` link, `tool_call_id`
  (= journal-key digest), `created_at` = the journaled clock.
- **Result** = a SUPERSESSION of the intent **re-stating all of the above**
  — supersession flips the old grain's link rows off the current set, so a
  result that forgets to re-state erases the node's execution record (pinned
  by `superseding_an_execution_record_needs_the_link_restated` in
  areev-store). Usage (`usage_*` extras incl. `usage_journal_bytes`) rides
  the result so budget recompute from the journal equals live accounting.
- **Gotcha that cost a debugging session**: a Tool's `content` field
  deserializes as **`tool_content`** (compact `cnt`), not `content`.
- **Checkpoint** = State grain (`checkpoint: true` extra):
  `context.scheduler` = serialized `SchedulerState`, `context.decisions` =
  the superstep's `DecisionRecord`; chained by `derived_from`.

## Crash windows (§5.3)

`RunOptions.inject_crash` (`CrashPoint::AfterIntent(n)` / `BeforeResult(n)`)
is the deterministic §5.5 gate-2 mechanism — the driver aborts at that exact
protocol point; never sleeps, never SIGKILL races. On resume, a dangling
intent is ADOPTED (lookup-before-write — one intent grain per key, ever),
re-dispatched under the SAME idempotency key, and the redelivery is journaled
as an Observation in `agent:harness`. A panicking host executor is converted
to a Failed effect by `catch_unwind` in the pool — a dead worker once left
`done_rx.recv()` waiting on nobody (the original integration run hung 9h on
exactly this).

## Verify (§5.5 gate 1)

`Runner::verify` re-drives the run from the **manifest's input** (never a
checkpoint's post-reducer context) with the clock scripted from journaled
readings (decision records' open/close), every effect answered from the
journal, writing NOTHING — and byte-compares every commanded checkpoint
against the stored chain. Divergence verdicts name the differing fields
(`diff_fields`). Parked spans replay by feeding the stored close reading
before `ResponseSettled` — see run-core's wall/elapsed pin for why.

## The clock-reading contract on cancellation

`step()`'s doc comment: "any call that may open or close a superstep
includes a fresh `ClockReading`." The live `drive()` loop's cancel poll used
to violate this — it pushed `CancelSeen` alone when `cancel_marker` first
found the operator's Fact, so a freshly-detected cancel (which closes the
run's FINAL superstep) journaled `clock_close_ms` from whatever the last
**completed wave** happened to read, which can predate the cancel Fact's own
`created_at` when the cancel lands in the gap between one wave finishing and
the next loop iteration noticing it — inverting the audit trail
`oversight-report`'s <5-minute kill-switch measurement reads (it already
silently drops inverted samples rather than trusting them, which is how this
went unnoticed). The fix: take a fresh reading in the SAME batch as
`CancelSeen`, mirroring what `verify()`'s own replay already does at both its
`cancel_peek` call sites (a `ClockReading` immediately preceding
`CancelSeen`). `cancel_marker` only returns once its store read has observed
the write `cancel()` performed, so a reading taken right after is monotone
at or after it. Pinned two ways: `wave6_tests.rs`'s
`kill_switch_drill_measures_cancel_to_drain_under_the_clause` under real
concurrency (the shape that caught it), and the deterministic, non-racy pair
in `areev-run-core/tests/scheduler_tests.rs` —
`cancel_drains_without_new_dispatches` (now also asserts `clock_close_ms`
takes the batch's reading) and `cancel_without_a_fresh_reading_journals_a_stale_close`
(pins the contract's negative: skip the reading, get a stale close — the
exact shape the old driver code produced).

## HITL (§6.6)

`respond` validates in order (pausable → known ask by `tool_call_id`, NEVER
index → unexpired → separation of duties: an `approval` ask structurally
refuses responder == triggering principal → schema) and journals REJECTED
responses as Observations before erroring — a losing approval is audit
evidence. Responding and resuming are separate acts.

## Wave 2 (agent-grade)

- **Abstract nodes** (§6.2/§6.11): manifest resolution falls through
  bound → named-Definition → abstract (needs `Runner.llm`, else `RUN-E006`);
  offered tools = the manifest's HOST pins. The pool runs LLM turns through
  `ToolCallLlm` (streaming via `call_streaming` when observed); the DRIVER
  prepares requests (transcript translation + pinned Definitions) because
  pool workers never touch the store. `load_arg_schemas` wires the §6.11
  strict-argument validator into `StepEnv.validate_args` — same table live
  and on verify, or replay diverges.
- **Result grains re-state the DISPATCHED executor** (`DispatchDone.executor`)
  — a flow tool inside an abstract node runs as Host while the node-level
  executor says Abstract; journaling the node-level one would rename the
  result `mg:llm`.
- **Subgraphs** run INLINE on the driver thread (the child needs the store);
  child id = `{parent}~{tool_call_id[..16]}` — deterministic, so replays and
  permutations agree. A child that parks fails the parent node (v1: no ask
  bubbling). Parallel subgraph siblings serialize (documented bound).
- **Typed reducers** (§6.5): Workflow grain `reducers: {key: name}` →
  validated at resolve → FROZEN in the manifest; builtins in `reducers.rs`
  (append-only names). Undeclared keys LWW.
- **Forks** (§5.4): `Runner::fork` — same-plan inherits pins + scheduler
  state (run_id rewritten); new-plan = migration (full re-validation, graph
  restarts at entry with inherited CONTEXT as input). Seed checkpoint
  `derived_from`s the base checkpoint; `mg:fork_of` Fact indexes lineage;
  verify replays the fork's TAIL from the seed. `--at N` picks the CLOSED
  (Idle) checkpoint at that superstep — the terminal checkpoint shares its
  superstep number. `BudgetExhausted` terminals re-open on fork (the
  continue-under-raised-budgets path). Parked checkpoints refuse (asks
  address parent keys).
- **Streaming** (§6.10, `stream.rs`): observational only. Bounded
  drop-oldest bus, delivery on its own thread, `emit` never blocks;
  `RunFinished` carries the dropped counter. TokenChunk sinks ride
  `PreparedLlm.on_token` into the pool (only when observed — otherwise the
  plain call). The §6.10 check: journals identical with no/normal/slow
  subscriber.

## Not yet (documented gaps)

- F7 owner-nonce copy detection needs an op-cursor read API; v1 ships taint
  detection + explicit forks only.
- D10 `--override-hold` on FORGET SUBJECT lands with the compliance wave.
- Subgraph ask-bubbling; `run_trace` fork splicing (the `mg:fork_of` Fact is
  the index; the CLI splice view is not built yet).

## Tests

`cargo test -p areev-run` — `tests/runner_tests.rs`: end-to-end pipeline +
verify, crash-window redelivery (same idempotency key, one intent, one
recorded redelivery), executor-panic-is-a-failed-effect, HITL
separation-of-duties + losing-responder journaling, cancel drain,
verify-catches-forgery, cross-run determinism. `tests/wave2_tests.rs`: the
abstract-node loop (journal shape, token accounting, verify-never-calls-the-
model), reservation refusal, model-visible tool failures, unknown-tool
one-strike, strict-args re-prompt, Send fan-out/join/retry/malformed,
append/sum reducers, forks (time-travel + migration), §6.10
identical-journals and TokenChunk wiring.
