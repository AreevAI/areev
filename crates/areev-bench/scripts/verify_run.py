#!/usr/bin/env python3
"""Verify a committed selfimprove_aba run against its own raw transcripts.

The published A/B/A/B tables are summaries. This recomputes every one of their
numbers from the `task_outcome` rows in the transcripts that shipped alongside
them, so a reader can check the claim against the evidence rather than trusting
the summary — keylessly, offline, in CI.

It also checks the property the paired statistics silently depend on: every
state ran the SAME task ids. `aba_stats.py` pairs by task id, so a state that
lost or reordered tasks would still produce a p-value, just not a meaningful
one.

    verify_run.py RUN_DIR                 # verify (what CI runs)
    verify_run.py RUN_DIR --write-manifest # (re)generate MANIFEST.md

MANIFEST.md records, per run: the exact command the config implies, the models,
the git rev, and a SHA-256 of every file in the directory. Without it a rename
or an overwrite of published evidence is invisible in review — which has
already happened once in this repo's history, when the single-seed pilot's
transcripts were renamed and overwritten by the three-seed run.

Stdlib only, by workspace policy.
"""

import hashlib
import json
import os
import sys

MANIFEST = "MANIFEST.md"


def fail(msg):
    print(f"verify_run: {msg}", file=sys.stderr)
    return 1


def load_outcomes(path):
    """The `task_outcome` rows of one eval transcript, in file order."""
    rows = []
    with open(path, encoding="utf-8") as fh:
        for n, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                v = json.loads(line)
            except json.JSONDecodeError as e:
                raise SystemExit(fail(f"{path}:{n}: {e}"))
            if v.get("kind") == "task_outcome":
                rows.append(v)
    return rows


def recompute(rows):
    """Rebuild an EvalSummary from the per-task rows (mirrors `summarize`)."""
    per_rule = {}
    for r in rows:
        for rule in r.get("rules_exercised", []):
            per_rule.setdefault(rule, {"opportunities": 0, "failures": 0})
            per_rule[rule]["opportunities"] += 1
        for rule in r.get("mishandled", []):
            per_rule.setdefault(rule, {"opportunities": 0, "failures": 0})
            per_rule[rule]["failures"] += 1
    return {
        "n": len(rows),
        "successes": sum(1 for r in rows if r.get("success")),
        "tool_errors": sum(r.get("tool_errors", 0) for r in rows),
        "total_steps": sum(r.get("steps", 0) for r in rows),
        "prompt_tokens": sum(r.get("prompt_tokens", 0) for r in rows),
        "completion_tokens": sum(r.get("completion_tokens", 0) for r in rows),
        "per_rule": per_rule,
    }


def verify_report(run_dir, stem, problems):
    """Check one run's report.json against its transcripts.

    `stem` is the filename PREFIX, including its separator: `""` for a fresh
    run directory (`report.json`, `transcripts-eval-B.jsonl` — what `Reporter`
    writes) and `"seed1."` for a published one, where per-seed runs were
    collected into a single directory and prefixed. Both must work: the fresh
    form is what a re-run produces and therefore what anyone verifying their
    own run will point this at.
    """
    report_path = os.path.join(run_dir, f"{stem}report.json")
    with open(report_path, encoding="utf-8") as fh:
        report = json.load(fh)

    evals = report.get("evals", [])
    if not evals:
        problems.append(f"{stem}: report.json has no evals")
        return

    task_ids_by_state = {}
    for ev in evals:
        state = ev["state"]
        tpath = os.path.join(run_dir, f"{stem}transcripts-eval-{state}.jsonl")
        if not os.path.exists(tpath):
            problems.append(f"{stem}/{state}: no transcript at {os.path.basename(tpath)}")
            continue
        rows = load_outcomes(tpath)
        got = recompute(rows)
        task_ids_by_state[state] = [r["task_id"] for r in rows]

        for key in ("n", "successes", "tool_errors", "total_steps"):
            if got[key] != ev.get(key):
                problems.append(
                    f"{stem}/{state}: {key} — report says {ev.get(key)}, "
                    f"transcripts say {got[key]}"
                )

        rate = (got["successes"] / got["n"]) if got["n"] else 0.0
        if abs(rate - ev.get("success_rate", 0.0)) > 1e-9:
            problems.append(
                f"{stem}/{state}: success_rate — report says {ev.get('success_rate')}, "
                f"transcripts say {rate}"
            )

        for pr in ev.get("per_rule", []):
            rule = pr["rule"]
            mine = got["per_rule"].get(rule, {"opportunities": 0, "failures": 0})
            for key in ("opportunities", "failures"):
                if mine[key] != pr.get(key):
                    problems.append(
                        f"{stem}/{state}/{rule}: {key} — report says {pr.get(key)}, "
                        f"transcripts say {mine[key]}"
                    )

        # Usage: an arm that summarizes at write time (m-llm) adds its
        # summarizer's tokens to the eval total on purpose, so its reported
        # usage is legitimately HIGHER than the per-task sum. Every other
        # state must match exactly, and the surcharge must never be negative.
        rep_usage = ev.get("usage", {})
        for key in ("prompt_tokens", "completion_tokens"):
            reported, summed = rep_usage.get(key, 0), got[key]
            if state == "M-llm":
                if reported < summed:
                    problems.append(
                        f"{stem}/{state}: {key} — reported {reported} is below the "
                        f"per-task sum {summed}; the summarizer surcharge cannot be negative"
                    )
            elif reported != summed:
                problems.append(
                    f"{stem}/{state}: {key} — report says {reported}, "
                    f"transcripts say {summed}"
                )

    # The paired-test precondition: same tasks, same order, every state.
    states = sorted(task_ids_by_state)
    if states:
        ref_state = states[0]
        ref = task_ids_by_state[ref_state]
        for state in states[1:]:
            other = task_ids_by_state[state]
            if other != ref:
                if sorted(other) == sorted(ref):
                    problems.append(
                        f"{stem}: {state} ran the same tasks as {ref_state} in a "
                        f"different ORDER — pairing survives, determinism does not"
                    )
                else:
                    missing = sorted(set(ref) - set(other))
                    extra = sorted(set(other) - set(ref))
                    problems.append(
                        f"{stem}: {state} did not run the same task set as {ref_state} "
                        f"(missing {len(missing)}, extra {len(extra)}) — the paired "
                        f"statistics in paired-stats.txt do not hold"
                    )


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def command_for(config):
    """The invocation this run's config implies, as documentation.

    `--workdir` is normalized to a placeholder: the original is a scratch path
    on whoever ran it, so reprinting it would put an unreproducible absolute
    path in a document about reproduction. The raw value stays in report.json.
    """
    parts = [
        "cargo run --release -p areev-bench --bin selfimprove_aba --",
        f"  --workdir /tmp/aba-seed{config.get('seed')}",
        f"  --seed {config.get('seed')}",
        f"  --experience {config.get('experience')} --eval {config.get('eval')}",
        f"  --workers {config.get('workers')} --max-turns {config.get('max_turns')}",
    ]
    for flag in ("agent_cmd", "llm_cmd", "ground_cmd", "mllm_cmd", "context_cmd"):
        value = config.get(flag)
        if value:
            parts.append(f"  --{flag.replace('_', '-')} '{value}'")
    if config.get("arms"):
        parts.append(f"  --arms {','.join(config['arms'])}")
    return " \\\n".join(parts)


def build_manifest(run_dir, stems):
    lines = [
        f"# {os.path.basename(run_dir.rstrip('/'))} — run manifest",
        "",
        "Generated by `scripts/verify_run.py --write-manifest`. Every number in",
        "`../RESULTS.md` for this run is recomputable from the transcripts below:",
        "",
        "```bash",
        f"python3 crates/areev-bench/scripts/verify_run.py \\",
        f"  crates/areev-bench/results/{os.path.basename(run_dir.rstrip('/'))}",
        "```",
        "",
    ]
    for stem in stems:
        with open(os.path.join(run_dir, f"{stem}report.json"), encoding="utf-8") as fh:
            config = json.load(fh).get("config", {})
        lines += [
            f"## {stem.rstrip('.') or 'run'}",
            "",
            f"- git rev: `{config.get('git_rev', '(unrecorded)')}`",
            f"- seed {config.get('seed')} · {config.get('experience')} experience · "
            f"{config.get('eval')} held-out · arms: "
            f"{', '.join(config.get('arms') or ['(none)'])}",
            "",
            "```",
            command_for(config),
            "```",
            "",
        ]
    lines += ["## Checksums", "", "```"]
    for name in sorted(os.listdir(run_dir)):
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path) or name == MANIFEST:
            continue
        lines.append(f"{sha256(path)}  {name}")
    lines += ["```", ""]
    return "\n".join(lines)


def check_manifest(run_dir, stems, problems):
    path = os.path.join(run_dir, MANIFEST)
    if not os.path.exists(path):
        problems.append(
            f"{MANIFEST} is missing — regenerate with --write-manifest"
        )
        return
    with open(path, encoding="utf-8") as fh:
        committed = fh.read()
    if committed != build_manifest(run_dir, stems):
        problems.append(
            f"{MANIFEST} does not match the directory — a published file changed, "
            f"or the manifest is stale. Regenerate with --write-manifest and review "
            f"WHICH checksum moved before committing."
        )


def main(argv):
    if len(argv) < 2:
        return fail(__doc__.strip().splitlines()[-1])
    run_dir = argv[1]
    write = "--write-manifest" in argv[2:]
    if not os.path.isdir(run_dir):
        return fail(f"{run_dir} is not a directory")

    # Prefix, separator included: "" for a fresh run dir, "seed1." for a
    # published one. `report.json` does NOT end with ".report.json", so a
    # suffix match alone would silently skip every fresh run.
    stems = sorted(
        f[: -len("report.json")]
        for f in os.listdir(run_dir)
        if f == "report.json" or f.endswith(".report.json")
    )
    if not stems:
        return fail(f"{run_dir} contains no *.report.json")

    problems = []
    for stem in stems:
        verify_report(run_dir, stem, problems)

    if write:
        with open(os.path.join(run_dir, MANIFEST), "w", encoding="utf-8") as fh:
            fh.write(build_manifest(run_dir, stems))
        print(f"wrote {os.path.join(run_dir, MANIFEST)}")
    else:
        check_manifest(run_dir, stems, problems)

    if problems:
        for p in problems:
            print(f"verify_run: FAIL {p}", file=sys.stderr)
        return 1
    print(
        f"verify_run: OK — {len(stems)} run(s) in {run_dir}; every published "
        f"number recomputed from the transcripts"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
