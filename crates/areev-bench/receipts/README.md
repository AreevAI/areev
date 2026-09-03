# receipts — governed self-improvement on a public receipt corpus

The harness behind [`../RECEIPTS.md`](../RECEIPTS.md): an agent that fills a
ledger row from a receipt, an accountant that corrects it, and Areev's loop
turning the corrections into governed, reviewable, revertible rules. The
public port of the private expense-agent measurement in
[`../EXPENSE.md`](../EXPENSE.md).

| file | role |
|---|---|
| `build_sroie.py` | fetch SROIE (cached) and emit the filed ledger as JSONL — the one corpus-specific builder |
| `ledger_profile.py` | what the ledger wants, in what order, written how — never imported by the agent |
| `dataset.py` | the seeded split: a seed permutes the corpus and assigns experience / held-out |
| `agent.py` | the capture agent: day-one instruction + whatever LESSONS memory holds |
| `accountant.py` | the scripted reviewer of rows, and the rubric judge of proposed rules |
| `memory.py` | the Areev bridge: record → loop → review → apply/rollback → lesson assembly |
| `run.py` | the experience phase |
| `evaluate.py` | the paired held-out evaluation (arms B, B2, A) |
| `stats.py`, `summarize.py` | McNemar over the paired trials; the published counts |
| `mock_agent.py`, `mock_judge.py`, `fixtures/` | the keyless floor `dryrun.sh` runs |
| `env.sh`, `learn.sh`, `eval.sh`, `curve.sh`, `dryrun.sh` | drivers; every model leg pinned and seeded |

Python 3 stdlib plus the `areev` binding built from this tree. No SDKs: the
model legs are the bench's JSON-on-stdio adapters in `../scripts/`.
