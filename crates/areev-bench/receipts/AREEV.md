# receipts — how this bench uses Areev

The reference track. If you are starting a new bench, read
[`../BENCH-TEMPLATE.md`](../BENCH-TEMPLATE.md) and then this file: receipts is
the one that adopts every surface, so its gaps are gaps in the *domain*, not in
the harness.

A capture agent reads documents into ledger rows. An accountant corrects it.
The loop proposes rules, the accountant approves them, and the approved rules
are the only thing that changes the prompt.

## Namespaces

| namespace | what lives there |
|---|---|
| `ledger` | everything the agent may read: rules, conventions, the accountant's words, the filed rows |
| `agent:harness` | eval-run scores only (`journal_eval_run`) |

Flat, deliberately: a receipt corpus has no natural parts to make child
namespaces out of. A multi-corpus successor (SROIE + VRDU + CORD in one memory)
should use `ledger.sroie` / `ledger.vrdu` and read `"ledger.*"`.

## Grains

| grain | relation / shape | written by |
|---|---|---|
| `fact` | `lesson` — a rule the LLM leg proposed and the accountant approved | the loop, applied by the reviewer |
| `fact` | `fails_with` — a failure signature the deterministic leg found | the loop |
| `fact` | a learned convention on the capture entity (`date_format = DD/MM/YYYY`) | the loop |
| `fact` | the filed row, on a `document_NNNN` subject | `record_correction` |
| `observation` | the accountant's own sentence, `observer_type = "human"`, `seq`-ordered | `record_correction` |
| `fact` | `mg:eval_run`, in `agent:harness` | the harness |

`document_NNNN` subjects and the `reading_result` / `correction` /
`capture_attempt` relations are **evidence for the loop, never instruction for
the agent**, and the prompt queries exclude them by construction rather than by
a Python filter.

## The prompt

Three saved queries and three templates, registered in the memory file by
`memory.py`'s `REGISTRY` and installed on every writable open.

| saved query | renders | used by |
|---|---|---|
| `ledger_rules($ns)` | `## INSTRUCTIONS FROM THE ACCOUNTANT` + one line per approved rule | arm B |
| `ledger_conventions($ns, $subject)` | `## CONVENTIONS` + `relation: object` per learned convention | arm B |
| `ledger_said($ns)` | `## WHAT THE ACCOUNTANT HAS TOLD YOU` + every human observation, oldest first | arm C |

```sql
ASSEMBLE "operating rules" FOR "the capture agent" FROM
  rules: (RECALL facts WHERE namespace = $ns AND relation IN ("lesson", "fails_with")
          ORDER BY object ASC LIMIT 500)
FORMAT TEMPLATE ledger_rules_tpl
WITH dedup(object)
```

`ORDER BY object ASC` inside the source is the `sorted(...)`, `WITH
dedup(object)` is the `set(...)`, and the template's
`{{#if assembly.grain_count}}` guard is what makes arm A — rules rolled back —
render the **empty string** instead of a heading with nothing under it.

**Byte parity is gated.** `scripts/parity_check.py receipts` seeds a memory,
renders each block through CAL and through the retired hand-rolled renderer
(kept verbatim in that file, imported by nothing), and asserts the strings are
equal. This matters here more than anywhere: `structure.py` measured that CAL's
*default* `FORMAT markdown` scores 35 against the hand-assembled prompt's 141
on these grains, because it prefixes every rule with its subject and relation.
The templates exist to emit the published bytes exactly.

## The governed pass

`memory.py::learn` proposes under `agent:receipt-capture` and decides under
`user:accountant` — the Review gate's separation of duties. The same pass as an
`areev run` workflow (`propose → review → apply → evaluate`, the review node
parking for a human) is `scripts/bench_run.py`; drive it with

```bash
python3 scripts/selftest_run.py     # keyless, receipts-seeded
```

which is also the proof that the runtime, not the harness, refuses a responder
equal to the principal that triggered the ask (`RUN-E012`).

## The governed pass, and what the reviewer reads

`memory.learn()` delegates to `bench_run.learn()` — there is no second,
unjournaled review loop. `propose → review → apply`, the review node a client
executor the run parks on, answered by `user:accountant` through
`run_respond`, which structurally refuses a responder equal to the principal
that triggered the ask (`RUN-E012`).

`review_pending` is handed the whole ask, not just the pending batch:

| key | from | why the reviewer needs it |
|---|---|---|
| `pending` | `recommendations({"status":"pending"})` | what is being decided |
| `prior` | `bench_review_history` (saved query, `SINCE "90d"`) | what this accountant already declined, so a reworded restatement is declined again with the earlier reason instead of going to a fresh judge call |
| `outcomes` | `bench_outcomes` (saved query) | the held-out series, so approvals can be judged against whether the last ones moved anything |

The engine already covers the generation half — it dedupes candidates on
`dedup_key` and starts an exponential rejection cooldown (7d, 14d, 28d, capped
at 90) whenever a recommendation is dismissed. A **reworded** proposal carries
a different key, so the cooldown never sees it; `prior` is the half that does.

## Skipped, and why

| surface | status |
|---|---|
| ASSEMBLE | adopted, all three model-facing blocks |
| saved queries | adopted, all three |
| CAL rendering | adopted, via `DEFINE TEMPLATE`; the default renderers are measured against it in `structure.py` |
| tool-call lifecycle | **not applicable** — this agent calls no tools; it reads a document and returns JSON |
| prefix namespaces | **not applicable** — one corpus, no parts. See above for the multi-corpus shape |

One ordering caveat, stated because it is not visible from the output: the
conventions section orders by `relation`, while the retired renderer sorted the
composed `"relation: object"` string. These agree unless two conventions share a
relation, which supersession makes impossible in practice.
