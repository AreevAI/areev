# Results moved

The evidence from every published run — per-trial records, governance
ledgers, token and cost meters, statistical workings and manifests — now
lives in its own repository:

**https://github.com/AreevAI/areev-benchmark** (`results/`)

The harness stays here, in [`crates/areev-bench/`](..), because it is a
workspace member that gates this repository's CI and has to move in lockstep
with the engine. The evidence only ever grows, so it was most of this
repository's weight and none of its build.

Every design document beside this one links directly to the run that backs
its numbers.
