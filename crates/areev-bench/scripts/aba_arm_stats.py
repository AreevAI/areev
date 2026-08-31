#!/usr/bin/env python3
"""Paired statistics BETWEEN two selfimprove_aba configurations.

    aba_arm_stats.py --control DIR [DIR ...] --arm DIR [DIR ...]

`aba_stats.py` compares states WITHIN one run (A0 vs B vs A1 vs B2). This
compares the SAME state ACROSS two runs — the pre-registered loop+LLM arm
question: does adding LLM-authored lessons to the governed loop change the
outcome, measured on the same held-out tasks?

Runs are paired BY SEED (from `report.json`'s `config.seed`, falling back to
a `-s<N>` directory suffix), never by argument order, so a missing or
reordered directory cannot silently mispair two different task streams. Each
seed's tasks are then paired by `task_id` — the same instances under both
configs, so McNemar is the right test.

Reported per state:
  A0  the baseline sanity check — both configs learn nothing yet, so a
      significant difference here means provider drift, not an arm effect,
      and invalidates the comparison rather than supporting it.
  B   the headline: governed lessons vs governed + LLM-authored lessons.
  A1/B2 completeness.

Plus per-rule mishandling at state B for both configs: the accuracy delta
can be null while the arm moves failures BETWEEN rules — the seed-1 pilot
eliminated its target rule and regressed two others at the same time, which
a single success rate cannot show.

`mcnemar_exact` is imported from `aba_stats`, never reimplemented, so this
tool and the published within-run statistics can never disagree.

Stdlib only.
"""
import argparse
import json
import os
import re
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import aba_stats  # noqa: E402

# A0 and A0R are IGNORANT in both configurations, so neither can carry a
# treatment effect: a difference there is cross-run drift and is labelled as
# such below. A0R additionally reports each run's own floor, which is the
# scale any other row has to beat.
STATES = ["A0", "A0R", "B", "A1", "B2"]


def resolve(arg):
    """(directory, filename prefix) for either committed or fresh layout.

    A committed results set is ONE flat directory of prefixed files
    (`governed-s1.transcripts-eval-B.jsonl`); a fresh run is a directory of
    unprefixed ones. Both are addressed the same way here — pass the path up
    to and including the prefix (`results/SET/governed-s1`) or the run
    directory itself. Reading only the fresh layout is a real regression, not
    a hypothetical: `aba_stats.py` shipped that way and silently found
    nothing in `results/`, so the published statistics could not be
    regenerated from the evidence they were computed from.
    """
    if os.path.isdir(arg):
        return arg, ""
    return os.path.dirname(arg) or ".", os.path.basename(arg) + "."


def seed_of(arg):
    """The run's seed: report.json first, then a `-s<N>` suffix. None if neither."""
    d, prefix = resolve(arg)
    report = os.path.join(d, f"{prefix}report.json")
    if os.path.exists(report):
        try:
            with open(report, encoding="utf-8") as fh:
                seed = json.load(fh).get("config", {}).get("seed")
            if seed is not None:
                return int(seed)
        except (json.JSONDecodeError, OSError, TypeError, ValueError):
            pass
    m = re.search(r"-s(\d+)$", os.path.basename(os.path.normpath(arg)))
    return int(m.group(1)) if m else None


def load_outcomes(arg):
    """{state: {task_id: (success, frozenset(mishandled), frozenset(exercised))}}."""
    d, prefix = resolve(arg)
    out = {}
    for state in STATES:
        path = os.path.join(d, f"{prefix}transcripts-eval-{state}.jsonl")
        if not os.path.exists(path):
            continue
        rows = {}
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    r = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if r.get("kind") != "task_outcome":
                    continue
                rows[r["task_id"]] = (
                    bool(r.get("success")),
                    frozenset(r.get("mishandled") or []),
                    frozenset(r.get("rules_exercised") or []),
                )
        if rows:
            out[state] = rows
    return out


def collect(dirs, label):
    """{seed: outcomes} — refuses a duplicate or unidentifiable seed."""
    by_seed = {}
    for d in dirs:
        seed = seed_of(d)
        if seed is None:
            sys.exit(f"aba_arm_stats: cannot determine the seed of {d} "
                     f"(no report.json config.seed, no -s<N> suffix)")
        if seed in by_seed:
            sys.exit(f"aba_arm_stats: two {label} runs claim seed {seed}")
        outcomes = load_outcomes(d)
        if not outcomes:
            print(f"aba_arm_stats: no task_outcome rows in {d} — skipped",
                  file=sys.stderr)
            continue
        by_seed[seed] = outcomes
    return by_seed


def compare_state(control, arm, state):
    """(pooled b, c, n, per-seed lines). b = arm won, c = control won."""
    pb = pc = pn = 0
    lines = []
    for seed in sorted(set(control) & set(arm)):
        cs, as_ = control[seed].get(state), arm[seed].get(state)
        if not cs or not as_:
            continue
        shared = sorted(set(cs) & set(as_))
        if not shared:
            continue
        b = sum(1 for t in shared if not cs[t][0] and as_[t][0])
        c = sum(1 for t in shared if cs[t][0] and not as_[t][0])
        pb, pc, pn = pb + b, pc + c, pn + len(shared)
        lines.append(
            f"    seed {seed}: n={len(shared)} "
            f"control={sum(cs[t][0] for t in shared)} arm={sum(as_[t][0] for t in shared)} "
            f"b={b} c={c} p={aba_stats.mcnemar_exact(b, c):.4f}"
        )
    return pb, pc, pn, lines


def per_rule(by_seed, state):
    """{rule: (mishandled, exercised)} pooled over seeds for one state."""
    mis, ex = defaultdict(int), defaultdict(int)
    for outcomes in by_seed.values():
        for _, (_, mishandled, exercised) in outcomes.get(state, {}).items():
            for rule in exercised:
                ex[rule] += 1
            for rule in mishandled:
                mis[rule] += 1
    return {rule: (mis[rule], ex[rule]) for rule in sorted(ex)}


def selftest():
    """Keyless check of the load → pair → count chain on synthetic runs.

    This script computes published numbers, so a silent break here corrupts
    a claim rather than failing a build. Writes real transcript files and
    reads them back through the ordinary path — an in-memory test would skip
    the parsing and seed-discovery this actually has to get right.
    """
    import tempfile

    def write(d, state, rows):
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"transcripts-eval-{state}.jsonl"), "w",
                  encoding="utf-8") as fh:
            for task, (ok, mishandled) in rows.items():
                fh.write(json.dumps({
                    "kind": "task_outcome", "task_id": task, "success": ok,
                    "mishandled": mishandled, "rules_exercised": ["R4", "R5"],
                }) + "\n")
            fh.write('{"kind":"other","ignored":true}\n')  # non-outcome rows skipped

    with tempfile.TemporaryDirectory() as tmp:
        ctl, arm = os.path.join(tmp, "c-s7"), os.path.join(tmp, "a-s7")
        # t1: arm wins (b). t2, t3: control wins (c). t4: both pass (concordant).
        write(ctl, "B", {"t1": (False, ["R5"]), "t2": (True, []),
                         "t3": (True, []), "t4": (True, [])})
        write(arm, "B", {"t1": (True, []), "t2": (False, ["R4"]),
                         "t3": (False, ["R4"]), "t4": (True, [])})
        c, a = collect([ctl], "control"), collect([arm], "arm")
        assert list(c) == [7] and list(a) == [7], "seed from -s<N> suffix"
        b, cc, n, _ = compare_state(c, a, "B")
        assert (b, cc, n) == (1, 2, 4), f"b/c/n orientation wrong: {(b, cc, n)}"
        assert abs(aba_stats.mcnemar_exact(1, 2) - 1.0) < 1e-9, "exact p"
        # Per-rule: the arm mishandles R4 more (+50 pts), R5 less (-25 pts).
        cr, ar = per_rule(c, "B"), per_rule(a, "B")
        assert cr["R5"] == (1, 4) and ar["R5"] == (0, 4), (cr, ar)
        assert cr["R4"] == (0, 4) and ar["R4"] == (2, 4), (cr, ar)
        # A run whose seed cannot be determined must fail loud, never pair
        # by argument order — two different task streams would silently mix.
        nameless = os.path.join(tmp, "nameless")
        write(nameless, "B", {"t1": (True, [])})
        try:
            collect([nameless], "control")
        except SystemExit:
            pass
        else:  # pragma: no cover
            raise AssertionError("an unidentifiable seed must exit, not pair")

        # The COMMITTED layout: one flat directory of prefixed files, which
        # is what `results/` actually holds. Reading only the fresh layout
        # would mean published numbers cannot be recomputed from the evidence
        # they came from — the regression aba_stats.py already shipped once.
        flat = os.path.join(tmp, "committed")
        os.makedirs(flat, exist_ok=True)
        for name, rows in (("ctl-s9", {"t1": (True, [])}),
                           ("arm-s9", {"t1": (False, ["R4"])})):
            with open(os.path.join(flat, f"{name}.transcripts-eval-B.jsonl"), "w",
                      encoding="utf-8") as fh:
                for task, (ok, mis) in rows.items():
                    fh.write(json.dumps({
                        "kind": "task_outcome", "task_id": task, "success": ok,
                        "mishandled": mis, "rules_exercised": ["R4"],
                    }) + "\n")
        pc = collect([os.path.join(flat, "ctl-s9")], "control")
        pa = collect([os.path.join(flat, "arm-s9")], "arm")
        assert list(pc) == [9] and list(pa) == [9], "seed from a prefixed path"
        b, cc, n, _ = compare_state(pc, pa, "B")
        assert (b, cc, n) == (0, 1, 1), f"prefixed layout pairing: {(b, cc, n)}"
    print("aba_arm_stats: selftest OK")


def main():
    if "--selftest" in sys.argv[1:]:
        selftest()
        return
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--control", nargs="+", required=True, metavar="DIR")
    ap.add_argument("--arm", nargs="+", required=True, metavar="DIR")
    ap.add_argument("--selftest", action="store_true",
                    help="run the keyless self-check and exit")
    args = ap.parse_args()

    control = collect(args.control, "control")
    arm = collect(args.arm, "arm")
    shared_seeds = sorted(set(control) & set(arm))
    if not shared_seeds:
        sys.exit("aba_arm_stats: no seed appears in both configurations")
    only_c = sorted(set(control) - set(arm))
    only_a = sorted(set(arm) - set(control))
    print(f"paired across configurations on seed(s): "
          f"{', '.join(str(s) for s in shared_seeds)}")
    if only_c or only_a:
        print(f"  UNPAIRED (excluded): control-only {only_c or '-'}, arm-only {only_a or '-'}")
    print("  b = the arm passed a task the control failed; c = the reverse.\n")

    for state in STATES:
        b, c, n, lines = compare_state(control, arm, state)
        if n == 0:
            continue
        p = aba_stats.mcnemar_exact(b, c)
        direction = "arm ahead" if b > c else ("control ahead" if c > b else "tied")
        note = (
            "  ← baseline sanity: a significant result here is drift"
            if state in ("A0", "A0R")
            else ""
        )
        print(f"  {state}: n={n} b={b} c={c} p={p:.4f}  ({direction}){note}")
        if len(lines) > 1:
            for line in lines:
                print(line)

    print("\nper-rule mishandling at state B (mishandled / exercised):")
    cr, ar = per_rule(control, "B"), per_rule(arm, "B")
    rules = sorted(set(cr) | set(ar))
    if rules:
        print(f"  {'rule':<6} {'control':>12} {'arm':>12}   delta")
        for rule in rules:
            cm, ce = cr.get(rule, (0, 0))
            am, ae = ar.get(rule, (0, 0))
            crate = cm / ce if ce else 0.0
            arate = am / ae if ae else 0.0
            print(f"  {rule:<6} {f'{cm}/{ce}':>12} {f'{am}/{ae}':>12}   "
                  f"{(arate - crate) * 100:+.1f} pts")
        print("  (negative = the arm mishandles that rule LESS often)")


if __name__ == "__main__":
    main()
