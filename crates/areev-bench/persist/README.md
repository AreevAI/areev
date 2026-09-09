# persist — governed self-improvement across sessions

The harness behind [`../PERSIST.md`](../PERSIST.md): Areev as the
persistence layer of two public cross-session benchmarks, with the same
arms as [`../FOURWAY.md`](../FOURWAY.md) — nothing, mem0, Areev passive,
Areev governed — beside each benchmark's own reference agent.

| path | role |
|---|---|
| `reviewer.py` | the fixed-rubric reviewer — the human gate of the governed arm, written before any run; never sees a task's expectations |
| `pastbench/areev_backend.py` | Areev as a PAST-Bench runtime adapter + persistence backend: one Areev file per family, Hermes-named memory/skill tools, a loop pass at episode close (governed), Hermes-shaped rendering so the benchmark's own scorer grades it |
| `pastbench/mem0_backend.py` | mem0 as a PAST-Bench backend, used as its README says: `search()` into the prompt, `add()` at close; three modes |
| `pastbench/run.py` | `past-bench` with the Areev and mem0 agents registered at import time |
| `pastbench/patch_pastbench.py` | the one edit the benchmark needs: `areev*`/`mem0*` in `evolve`'s agent allowlist |
| `pastbench/agents.yaml`, `config.persist.yaml` | the agents' registry and the judge config (MiniMax-M2.7 through OpenRouter) |
| `pastbench/evolve.sh` | one family for one agent, both persistence conditions, every leg pinned and seeded |
| `pastbench/pilot.sh` | families × agents in sequence, key usage read before and after each run |
| `pastbench/summarize.py` | one table from a run root: the benchmark's Δ and mechanism, tokens per episode, loop tokens, key-usage bound |
| `pastbench/selftest.py` | the keyless floor: fixture import, tools, rendering, counters, diff, retrieval signals, anchors — no model, no judge |
| `horizon/areev_agent/` | the Horizon (Harbor) agent: the trace as Tool and Event grains, a governed pass before the task, `session_search` over the memory |
| `horizon/run.sh` | one Horizon run for one agent, the public set or one task |
| `tune/train_lora.py`, `slm_train_cuda.sh`, `slm_serve_cuda.sh` | the CUDA twin of `receipts/slm_train.sh` (transformers + peft) and a vLLM server for the tuned model — what `areev tune --cmd` plugs into on the office box |

Runs live on the office box under `~/mg/local/areev-runs/persist/`; only
counts, manifests and checksums travel into `../results/persist-<date>/`.

## Reproduce (office box)

```sh
# PAST-Bench at f822351 in its own venv, the one-line allowlist patch applied,
# the areev binding built into that venv (maturin develop)
python pastbench/patch_pastbench.py ~/mg/local/PAST-Bench
python pastbench/selftest.py                                   # keyless floor
FAMILIES="memory_ability/SM01_preference_adoption" AGENTS="areev-governed areev-passive hermes" \
  ROOT=~/mg/local/areev-runs/persist/pilot sh pastbench/pilot.sh
python pastbench/summarize.py ~/mg/local/areev-runs/persist/pilot

# Horizon: Harbor 0.22 with the areev wheel, agents/areev_agent linked into the checkout
AGENT=areev-governed OUT=~/mg/local/areev-runs/persist/hz sh horizon/run.sh
```

## Running against a Postgres memory

The PAST-Bench backend (`pastbench/areev_backend.py`) derives each family's
memory as `<state_root>/areev_state/memory.db`. With `AREEV_BENCH_DB` set to a
`postgres://…?schema=…` DSN it opens that instead, verbatim — the Cloud leg,
one provisioned schema per family. Two consequences, stated because they are
the only places the run differs: the benchmark's own file operations on the
state root (`reset_state`, `clone_state`) do not touch a Postgres memory, so
provisioning and resetting the schema between families is the caller's job;
and the loop policy the governed close records goes to the run's artifacts
directory rather than beside the file. A DSN is never written unredacted.
