#!/usr/bin/env python3
"""Recompute the published receipts result from its committed evidence.

    verify.py <results-dir> [--write]     # recompute; --write refreshes RESULTS.json + MANIFEST.md
    verify.py <results-dir> --check       # CI: recomputed == committed, checksums intact

`--write` refuses a directory whose seeds disagree on which artifacts they
carry — the shape a run still flushing to disk has — because that silently
publishes a result with one seed's evidence missing. `--allow-ragged` writes
anyway, for a run genuinely cut short, and records what is absent.

A results directory holds one `seedN/` per seed, each as curve.sh left it:
`a0.summary.json`, `experience.summary.json`, `journal.jsonl`,
`eval/trials.json`, `regress/regress.summary.json`, `regress/regress.trials.json`,
and `at_NNN/trials.json` for the learning curve. Every number in
RECEIPTS.md comes from this script over those files — nothing is entered
by hand, and `--check` fails the build if a published number stops matching
its own evidence or a file is renamed or edited.
"""
import argparse
import collections
import hashlib
import json
import os
import re
import sys
from math import comb

METRICS = ("exact", "semantic")
PAIRS = (("B", "B2"), ("B", "A"), ("B2", "A"))


def mcnemar_exact(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def load_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def by_arm(trials):
    out = {}
    for t in trials:
        out.setdefault(t["arm"], {})[(t["seq"], t["field"])] = t
    return out


def paired(left, right, metric):
    keys = sorted(set(left) & set(right))
    b = sum(1 for k in keys if left[k][metric] and not right[k][metric])
    c = sum(1 for k in keys if right[k][metric] and not left[k][metric])
    return {"n": len(keys), "wins": b, "losses": c, "p": round(mcnemar_exact(b, c), 6)}


def seed_result(seed_dir):
    r = {"seed_dir": os.path.basename(seed_dir)}
    trials = load_json(os.path.join(seed_dir, "eval", "trials.json"))
    arms = by_arm(trials)
    r["held_out_documents"] = len({k[0] for k in arms["B"]})
    r["trials_per_arm"] = len(arms["B"])
    fields = sorted({k[1] for k in arms["B"]})
    for arm, d in sorted(arms.items()):
        r["passed_%s" % arm] = {m: sum(1 for t in d.values() if t[m]) for m in METRICS}
        cov, tot = collections.Counter(), collections.Counter()
        for k, t in d.items():
            tot[k[1]] += 1
            if str(t["got"]).strip():
                cov[k[1]] += 1
        r["coverage_%s" % arm] = {f: [cov.get(f, 0), tot.get(f, 0)] for f in fields}
    for m in METRICS:
        for left, right in PAIRS:
            if left in arms and right in arms:
                r["%s_%s_vs_%s" % (m, left, right)] = paired(arms[left], arms[right], m)
        wins = collections.Counter()
        for k, tb in arms["B"].items():
            ta = arms.get("A", {}).get(k)
            if ta and tb[m] and not ta[m]:
                wins[k[1]] += 1
        r["%s_wins_by_field_B_vs_A" % m] = dict(sorted(wins.items()))

    p = os.path.join(seed_dir, "a0.summary.json")
    if os.path.exists(p):
        a0 = load_json(p)
        r["A0"] = {"exact": a0["exact"], "semantic": a0["semantic"], "total": a0["total"]}
    p = os.path.join(seed_dir, "experience.summary.json")
    if os.path.exists(p):
        e = load_json(p)
        r["experience"] = {k: e[k] for k in ("documents", "learn_passes", "lessons_applied",
                                              "lessons_rejected", "prompt_tokens", "completion_tokens")
                           if k in e}
    # The lessons in force at the end, from the journal (what rendered).
    p = os.path.join(seed_dir, "journal.jsonl")
    if os.path.exists(p):
        # The review ledger travels: a proposal's text is the model's own
        # words and a reason is the reviewer's, neither of which is corpus
        # content. The journal's per-receipt rows do not travel.
        decisions = []
        for line in open(p, encoding="utf-8"):
            row = json.loads(line)
            for d in row.get("decisions", []) or []:
                decisions.append({"approved": d["approved"], "kind": d["kind"], "text": d["text"], "why": d["why"]})
        r["review_decisions"] = decisions
        r["lessons_in_prompt_at_end"] = max(
            (json.loads(l).get("lessons_in_prompt", 0) for l in open(p, encoding="utf-8")
             if "lessons_in_prompt" in l), default=0)
    p = os.path.join(seed_dir, "regress", "regress.summary.json")
    if os.path.exists(p):
        g = load_json(p)
        steps = {s["step"]: s for s in g["steps"]}
        r["regress"] = {
            "all_ok": g.get("all_ok"),
            "checks": {k: v["ok"] for k, v in g["checks"].items()},
            "verdicts_after_B": [{"verdict": o["verdict"], "baseline": o["baseline"], "current": o["current"]}
                                 for o in steps.get("verify", {}).get("outcomes", [])],
            "H": steps.get("measure-H", {}).get("summary"),
            "R": steps.get("measure-R", {}).get("summary"),
        }
        rt = os.path.join(seed_dir, "regress", "regress.trials.json")
        if os.path.exists(rt):
            ra = by_arm(load_json(rt))
            for m in METRICS:
                if "H" in ra and "B" in arms:
                    r["regress"]["%s_B_vs_H" % m] = paired(arms["B"], ra["H"], m)
                if "R" in ra and "H" in ra:
                    r["regress"]["%s_R_vs_H" % m] = paired(ra["R"], ra["H"], m)
                if "R" in ra and "B" in arms:
                    r["regress"]["%s_B_vs_R" % m] = paired(arms["B"], ra["R"], m)
    curve = {}
    for name in sorted(os.listdir(seed_dir)):
        mm = re.match(r"at_(\d+)$", name)
        tp = os.path.join(seed_dir, name, "trials.json")
        if mm and os.path.exists(tp):
            ca = by_arm(load_json(tp))
            if "B" in ca:
                curve[int(mm.group(1))] = {m: sum(1 for t in ca["B"].values() if t[m]) for m in METRICS}
    if curve:
        if "A0" in r:
            curve[0] = {"exact": r["A0"]["exact"], "semantic": r["A0"]["semantic"]}
        r["curve"] = {str(k): curve[k] for k in sorted(curve)}
    return r


def pooled(seeds):
    out = {}
    for m in METRICS:
        for left, right in PAIRS:
            key = "%s_%s_vs_%s" % (m, left, right)
            rows = [s[key] for s in seeds if key in s]
            if rows:
                b, c = sum(x["wins"] for x in rows), sum(x["losses"] for x in rows)
                out[key] = {"n": sum(x["n"] for x in rows), "wins": b, "losses": c,
                            "p": round(mcnemar_exact(b, c), 6)}
        for arm in ("A", "B", "B2"):
            key = "passed_%s" % arm
            rows = [s[key][m] for s in seeds if key in s]
            if rows:
                out.setdefault("passed", {}).setdefault(arm, {})[m] = sum(rows)
    out["trials_per_arm"] = sum(s["trials_per_arm"] for s in seeds)
    return out


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def manifest(root):
    """Checksums over the raw evidence. Those files stay LOCAL: `trials.json`
    and the journals embed the corpus's own values (vendor names, addresses,
    filed totals), and this repo redistributes none of it — only counts
    travel. The manifest is what lets an operator verify their own copy
    produced the published RESULTS.json."""
    rows = []
    for dirpath, _dirs, files in os.walk(root):
        for f in sorted(files):
            if f in ("MANIFEST.md", "RESULTS.json") or f.endswith((".db", ".db-wal")):
                continue
            p = os.path.join(dirpath, f)
            rows.append((os.path.relpath(p, root), sha256(p)))
    return sorted(rows)


def render_md(results):
    lines = ["| seed | trials | A exact | B exact | B2 exact | B vs A (wins/losses, p) | B vs B2 (noise) | A0 | curve (exact at 10/20/30/40) |",
             "|---|---|---|---|---|---|---|---|---|"]
    for s in results["seeds"]:
        ba, bb = s.get("exact_B_vs_A", {}), s.get("exact_B_vs_B2", {})
        curve = s.get("curve", {})
        lines.append("| %s | %d | %d | %d | %d | %d / %d, p=%.4f | %d | %s | %s |" % (
            s["seed_dir"], s["trials_per_arm"],
            s["passed_A"]["exact"], s["passed_B"]["exact"], s.get("passed_B2", {}).get("exact", 0),
            ba.get("wins", 0), ba.get("losses", 0), ba.get("p", 1.0),
            bb.get("wins", 0) + bb.get("losses", 0),
            s.get("A0", {}).get("exact", "-"),
            "/".join(str(curve[k]["exact"]) for k in sorted(curve, key=int) if k != "0") or "-"))
    p = results["pooled"]
    lines.append("| **pooled** | %d | %d | %d | %d | %d / %d, p=%.4f | %d | | |" % (
        p["trials_per_arm"], p["passed"]["A"]["exact"], p["passed"]["B"]["exact"],
        p["passed"].get("B2", {}).get("exact", 0),
        p["exact_B_vs_A"]["wins"], p["exact_B_vs_A"]["losses"], p["exact_B_vs_A"]["p"],
        p["exact_B_vs_B2"]["wins"] + p["exact_B_vs_B2"]["losses"]))
    return "\n".join(lines)


def raggedness(seeds):
    """Artifacts one seed has and another lacks.

    Twice now, `--write` has been run while a seed was still flushing its
    snapshot evals to disk: the seed contributed no learning curve (and once
    no `regress` block at all), every published number still looked right,
    and nothing said a seed was missing. A results file that silently drops
    one seed's evidence is the worst failure this script has, because it is
    invisible in its own output. So compare the seeds against each other and
    say so.

    A seed that *recorded* a failed leg is not ragged — the block is there,
    reporting its own failure, which is the harness working."""
    kinds = {}
    for k in ("A0", "curve", "regress"):
        have = {i for i, s in enumerate(seeds, 1) if s.get(k)}
        if have and len(have) != len(seeds):
            kinds[k] = sorted(set(range(1, len(seeds) + 1)) - have)
    # A seed with no curve at all is already named above; this catches the
    # subtler case of a curve that is present but short a checkpoint.
    widths = {i: len(s["curve"]) for i, s in enumerate(seeds, 1) if s.get("curve")}
    if len(set(widths.values())) > 1:
        full = max(widths.values())
        kinds["curve_points"] = sorted(i for i, w in widths.items() if w != full)
    return kinds


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--write", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--allow-ragged", action="store_true",
                    help="write even though the seeds disagree on which artifacts they have "
                         "(a run genuinely cut short, not one still being written)")
    args = ap.parse_args()

    seed_dirs = sorted(os.path.join(args.root, d) for d in os.listdir(args.root)
                       if re.match(r"seed\d+$", d))
    if not seed_dirs:
        raise SystemExit("no seedN/ directories under %s" % args.root)
    seeds = [seed_result(d) for d in seed_dirs]
    results = {"seeds": seeds, "pooled": pooled(seeds)}
    rag = raggedness(seeds)
    if rag:
        # Recorded in the results file too, so `--check` compares it like any
        # other number: evidence that shows up later makes the recomputation
        # differ from the committed file, which is exactly the alarm wanted.
        results["incomplete"] = rag
    md = render_md(results)
    print(md)
    if rag:
        print("\nWARNING: the seeds do not carry the same evidence.", file=sys.stderr)
        for kind, missing in sorted(rag.items()):
            print("  %-13s missing from seed%s"
                  % (kind, ", seed".join(str(i) for i in missing)), file=sys.stderr)
        print("  If the run is still writing, wait and re-run. If it was genuinely\n"
              "  cut short, pass --allow-ragged and say so where the result is published.",
              file=sys.stderr)

    res_path = os.path.join(args.root, "RESULTS.json")
    man_path = os.path.join(args.root, "MANIFEST.md")
    if args.write and rag and not args.allow_ragged:
        raise SystemExit("refusing to write a ragged results file; see the warning above")
    if args.write:
        with open(res_path, "w", encoding="utf-8") as fh:
            json.dump(results, fh, indent=1, sort_keys=True)
        with open(man_path, "w", encoding="utf-8") as fh:
            fh.write("# %s — manifest\n\nGenerated by `receipts/verify.py --write`. "
                     "`verify.py --check` recomputes RESULTS.json from these files and "
                     "re-derives every checksum.\n\n| file | sha256 |\n|---|---|\n" % os.path.basename(args.root.rstrip("/")))
            for rel, h in manifest(args.root):
                fh.write("| `%s` | `%s` |\n" % (rel, h))
        print("\nwrote %s and %s" % (res_path, man_path))
    if args.check:
        ok = True
        committed = load_json(res_path)
        if json.dumps(committed, sort_keys=True) != json.dumps(results, sort_keys=True):
            print("RESULTS.json does not match a recomputation from the trials", file=sys.stderr)
            ok = False
        want = {}
        for line in open(man_path, encoding="utf-8"):
            m = re.match(r"\| `(.+?)` \| `([0-9a-f]{64})` \|", line)
            if m:
                want[m.group(1)] = m.group(2)
        have = dict(manifest(args.root))
        for rel, h in want.items():
            if have.get(rel) != h:
                print("checksum mismatch or missing: %s" % rel, file=sys.stderr)
                ok = False
        for rel in have:
            if rel not in want:
                print("file not in manifest: %s" % rel, file=sys.stderr)
                ok = False
        print("check: %s" % ("ok" if ok else "FAILED"))
        sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
