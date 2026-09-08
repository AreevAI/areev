# receipts — governed self-improvement on public document corpora

The harness behind [`../RECEIPTS.md`](../RECEIPTS.md) and
[`../ADBUY.md`](../ADBUY.md): an agent that fills a ledger row from a
document, an accountant that corrects it, and Areev's loop turning the
corrections into governed, reviewable, revertible rules. The public port of
the private expense-agent measurement in [`../EXPENSE.md`](../EXPENSE.md).

Two corpora run through it, selected by `--profile`: `sroie` (Malaysian
retail receipts) and `vrdu` (US broadcast ad-buy invoices). A corpus is a
builder plus a row in `ledger_profile.py`; nothing else in the harness knows
which one it is running.

| file | role |
|---|---|
| `build_sroie.py` | fetch ICDAR-SROIE retail receipts (cached) and emit the filed ledger as JSONL |
| `build_vrdu.py` | the same for VRDU ad-buy invoices — the second corpus, deliberately unlike the first ([`../ADBUY.md`](../ADBUY.md)) |
| `ledger_profile.py` | what the ledger wants, in what order, written how — never imported by the agent |
| `dataset.py` | the seeded split: a seed permutes the corpus and assigns experience / held-out |
| `agent.py` | the capture agent: day-one instruction + whatever LESSONS memory holds |
| `accountant.py` | the scripted reviewer of rows, and the rubric judge of proposed rules |
| `memory.py` | the Areev bridge: record → loop → review → apply/rollback → lesson assembly |
| `run.py` | the experience phase |
| `evaluate.py` | the paired held-out evaluation (arms B, B2, A) |
| `evalrun.py` | one held-out pass, and the `mg:eval_run` journal entry that makes it evidence the Verify gate can read |
| `regress.py` | the verify-then-revert leg: measure the applied rules, admit a harmful one on purpose, watch the gate catch and revert it |
| `stats.py`, `summarize.py` | McNemar over the paired trials; the published counts |
| `verify.py` | recompute every published number from the trials, and checksum the raw files (`--check`) |
| `learners.py` | the diagnostic: what each candidate learner authors from one fixed memory |
| `mock_agent.py`, `mock_judge.py`, `fixtures/` | the keyless floor `dryrun.sh` runs |
| `env.sh`, `learn.sh`, `eval.sh`, `curve.sh`, `dryrun.sh` | drivers; every model leg pinned and seeded |

Python 3 stdlib plus the `areev` binding built from this tree. No SDKs: the
model legs are the bench's JSON-on-stdio adapters in `../scripts/`.

## Running against a Postgres memory

Every phase opens its memory through `areev.Areev(...)`, which takes a file
path or a `postgres://…?schema=…` DSN. `AREEV_BENCH_DB` (or `run.py --db`)
names the memory and is passed through **verbatim**, so a Cloud-provisioned
schema — owner role with no `CREATE`, `?provision=never` on the DSN — runs the
same harness the file-backed results were produced with. Unset, nothing
changes: the published file-backed runs are unaffected.

What a schema cannot do is be copied, and three things copy the file:

- **`evaluate.py`'s arms.** On a file, arms B and A each get their own copy
  and A is produced by `rollback_all` on its copy. On a DSN every arm reads
  the **one** memory, and arm A is produced by rolling the applied
  recommendations back on that memory — so A runs last, whatever `--arms`
  says, and the rollback is real: when the evaluation ends the memory holds
  no applied lessons. The harness prints this before it starts. It is also
  why `dryrun.sh` reorders the legs on a DSN: B and B2 (journaling B, which
  regress's verify needs), then regress, then arm A alone with
  `evaluate.py --append` — added to the trials already taken, last of all,
  because after it there is nothing applied left to verify.
- **`run.py --snapshot-every` / `--snapshot-at`** copy the memory aside for
  the learning curve. Refused on a DSN; run the curve on a file.
- **`learners.py`** copies the memory once per pass. Refused on a DSN.

`run.py` refuses a DSN whose schema already holds grains (the file-path rule,
"a stale memory would poison the run", asked of the schema). The loop policy
each leg records (`loop-policy.json`, `regress-policy.json`) lands in that
leg's `--workdir`, since a DSN has no "beside", and `regress.py` reads the
run's `run.config.json` from the parent of its work dir (`--run-config` to
say otherwise). A DSN is printed and recorded
(`run.config.json` → `memory`) with its password redacted, and only when one
was given.
