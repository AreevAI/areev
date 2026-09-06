#!/usr/bin/env python3
"""Read a tuning learning curve: quality against corpus size, two ways to tune,
two held-out sets, and the loss curves that say whether anything overfit.

    curve_stats.py <runs-root> [--write]

Layout, per seedN/ (curve_tune.sh): ck_NNN/{adapter_scratch,adapter_continual}
each with adapter.manifest.json (val/train loss, kept checkpoint), and
ck_NNN/eval_{scratch,continual,llm}_{unseen,seen}/trials.json (arm B);
eval_base_{unseen,seen}/trials.json once per seed.

Reads, per checkpoint: exact of N on the UNSEEN set (organisations the agent
never learned from -- the number that can claim generalisation) and on the
SEEN set (the gap is what familiarity buys); scratch against continual,
paired per (document, field); each against the LLM carrying the same rules;
and validation loss at the kept checkpoint against the last one, which is
the overfitting receipt.
"""
import argparse
import collections
import glob
import json
import os
import re
import sys
from math import comb

MODES = ("llm", "scratch", "continual")
HOLD = ("unseen", "seen", "next")


def mcnemar(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def trials(path):
    """Arm-B trials keyed by DOCUMENT and field -- not by seq. Until
    2026-09-05 the unseen set's order varied per process (dataset.py), so two
    reads of one adapter carried different documents at the same seq and a
    seq-keyed pairing counted order noise as wins and losses (72/66 between
    two reads that differed on 9 outputs). A document id is the filename cut
    to 40 characters and is not unique in the corpus (two filings by one
    registrant on one day), so a repeat takes an occurrence index in file
    order -- stable across arms now that the split is, and the one set read
    before the fix (seed 1, checkpoint 20, unseen) has no duplicate ids."""
    if not os.path.exists(path):
        return None
    out, seen = {}, collections.Counter()
    for t in json.load(open(path)):
        if t["arm"] != "B":
            continue
        key = "%s|%s" % (t["id"], t["field"])
        seen[key] += 1
        if seen[key] > 1:
            key = "%s#%d" % (key, seen[key])
        out[key] = t
    return out


def exact(tr):
    return sum(1 for t in tr.values() if t["exact"])


def paired(x, y):
    keys = set(x) & set(y)
    w = sum(1 for k in keys if x[k]["exact"] and not y[k]["exact"])
    l = sum(1 for k in keys if y[k]["exact"] and not x[k]["exact"])
    return {"n": len(keys), "wins": w, "losses": l, "p": round(mcnemar(w, l), 6)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()

    out = {"seeds": {}, "pooled": {}}
    pooled = collections.defaultdict(lambda: [0, 0])                                   # (ck, mode, hold) -> [exact, n]
    pairs = collections.defaultdict(lambda: [0, 0])                                     # (ck, a, b, hold) -> [w, l]
    for sd in sorted(glob.glob(os.path.join(args.root, "seed*"))):
        if not os.path.isdir(sd):
            continue  # seedN.log sits beside seedN/ and would overwrite it with an empty record
        s = int(re.search(r"seed(\d+)", sd).group(1))
        rec = {"checkpoints": {}, "base": {}}
        for h in HOLD:
            tr = trials(os.path.join(sd, "eval_base_%s" % h, "trials.json"))
            if tr:
                rec["base"][h] = {"exact": exact(tr), "n": len(tr)}
                pooled[("base", "base", h)][0] += exact(tr); pooled[("base", "base", h)][1] += len(tr)
        for ck in sorted(glob.glob(os.path.join(sd, "ck_*"))):
            k = int(os.path.basename(ck)[3:])
            row = {"modes": {}, "train": {}}
            got = {}
            for mode in MODES:
                for h in HOLD:
                    tr = trials(os.path.join(ck, "eval_%s_%s" % (mode, h), "trials.json"))
                    if tr:
                        got[(mode, h)] = tr
                        row["modes"]["%s|%s" % (mode, h)] = {"exact": exact(tr), "n": len(tr)}
                        pooled[(k, mode, h)][0] += exact(tr); pooled[(k, mode, h)][1] += len(tr)
            # the validation selector's alternative: the latest saved checkpoint, where an earlier one was kept
            for mode in ("scratch", "continual"):
                lt = trials(os.path.join(ck, "eval_%s_latest_unseen" % mode, "trials.json"))
                if lt and (mode, "unseen") in got:
                    p = paired(lt, got[(mode, "unseen")])
                    lm = os.path.join(ck, "adapter_%s_latest" % mode, "adapter.manifest.json")
                    lman = json.load(open(lm)) if os.path.exists(lm) else {}
                    row.setdefault("selector", {})[mode] = {"kept": lman.get("kept_by_selector"), "latest": lman.get("latest_saved_checkpoint"),
                                                            "kept_exact": exact(got[(mode, "unseen")]), "latest_exact": exact(lt), "n": len(lt),
                                                            "latest_over_kept": p}
                    pooled[(k, mode + "-latest", "unseen")][0] += exact(lt); pooled[(k, mode + "-latest", "unseen")][1] += len(lt)
                    # the kept read of the SAME seeds, so the two columns pool over the same reads
                    kp = got[(mode, "unseen")]
                    pooled[(k, mode + "-kept", "unseen")][0] += exact(kp); pooled[(k, mode + "-kept", "unseen")][1] += len(kp)
                    pairs[(k, mode + "-latest", mode, "unseen")][0] += p["wins"]; pairs[(k, mode + "-latest", mode, "unseen")][1] += p["losses"]
            prev_k = max([kk for kk in rec["checkpoints"] if kk < k], default=0)
            for mode in ("scratch", "continual"):
                man = os.path.join(ck, "adapter_%s" % mode, "adapter.manifest.json")
                if os.path.exists(man):
                    m = json.load(open(man))
                    last_val = m["val_loss"][-1][1] if m.get("val_loss") else None
                    row["train"][mode] = {"rows": m["corpus"]["train"], "iters": m["iters"], "seconds": m["train_seconds"],
                                          "best_val_iter": m.get("best_val_iter"), "best_val_loss": m.get("best_val_loss"),
                                          "last_val_loss": last_val, "kept": m.get("kept_checkpoint"),
                                          "resumed_from": m.get("resumed_from"),
                                          "train_loss_at_best": m.get("train_loss_at_best"),
                                          "loss_gap_at_best": m.get("loss_gap_at_best"),
                                          "effective_epochs": m.get("effective_epochs"),
                                          "trainable_percent": (m.get("trainable") or {}).get("percent")}
                # the adapter on its own training rows: memorisation, and forgetting for continual
                tt = trials(os.path.join(ck, "eval_%s_train" % mode, "trials.json"))
                if tt:
                    old_rows = {kk: t for kk, t in tt.items() if t["seq"] <= prev_k}
                    new_rows = {kk: t for kk, t in tt.items() if t["seq"] > prev_k}
                    unseen = got.get((mode, "unseen"))
                    row.setdefault("overfit", {})[mode] = {
                        "train_exact": exact(tt), "train_n": len(tt),
                        "train_rate": round(exact(tt) / len(tt), 3),
                        "unseen_rate": round(exact(unseen) / len(unseen), 3) if unseen else None,
                        "memorisation_gap": (round(exact(tt) / len(tt) - exact(unseen) / len(unseen), 3) if unseen else None),
                        "old_rows": {"exact": exact(old_rows), "n": len(old_rows)} if old_rows else None,
                        "new_rows": {"exact": exact(new_rows), "n": len(new_rows)} if new_rows else None,
                    }
                    pooled[(k, mode + "-train", "train")][0] += exact(tt); pooled[(k, mode + "-train", "train")][1] += len(tt)
                    if old_rows:
                        pooled[(k, mode + "-old", "train")][0] += exact(old_rows); pooled[(k, mode + "-old", "train")][1] += len(old_rows)
            for h in HOLD:
                for a, b in (("scratch", "llm"), ("continual", "llm"), ("scratch", "continual")):
                    if (a, h) in got and (b, h) in got:
                        p = paired(got[(a, h)], got[(b, h)])
                        row.setdefault("paired", {})["%s_over_%s|%s" % (a, b, h)] = p
                        pairs[(k, a, b, h)][0] += p["wins"]; pairs[(k, a, b, h)][1] += p["losses"]
            rec["checkpoints"][k] = row
        # the governed agent as it ran, over the same next window: rules as
        # they evolved inside it, so this is the deployment's own record
        jp = os.path.join(sd, "journal.jsonl")
        if os.path.exists(jp):
            byseq = {}
            for line in open(jp, encoding="utf-8"):
                j = json.loads(line)
                if "seq" in j and "scored" in j:
                    byseq[j["seq"]] = j
            for k in list(rec["checkpoints"]):
                win = [byseq[q] for q in range(k + 1, k + 21) if q in byseq]
                if win:
                    ex = sum(j["exact"] for j in win); n = sum(j["scored"] for j in win)
                    rec["checkpoints"][k]["live_next"] = {"exact": ex, "n": n, "documents": len(win)}
                    pooled[(k, "live", "next")][0] += ex; pooled[(k, "live", "next")][1] += n
        out["seeds"][s] = rec

    def wilson(x, n, z=1.96):
        if not n:
            return (None, None)
        p = x / n; d = 1 + z * z / n; c = (p + z * z / (2 * n)) / d; hw = z * ((p * (1 - p) / n + z * z / (4 * n * n)) ** 0.5) / d
        return (round(c - hw, 3), round(c + hw, 3))
    fields = collections.defaultdict(lambda: [0, 0, 0])  # (ck, mode, field) -> [exact, semantic, n] on unseen
    for sd in sorted(glob.glob(os.path.join(args.root, "seed*"))):
        if not os.path.isdir(sd):
            continue
        for ck in sorted(glob.glob(os.path.join(sd, "ck_*"))):
            k = int(os.path.basename(ck)[3:])
            for mode in MODES:
                tr = trials(os.path.join(ck, "eval_%s_unseen" % mode, "trials.json"))
                for t in (tr or {}).values():
                    f = fields[(k, mode, t["field"])]; f[0] += t["exact"]; f[1] += t["semantic"]; f[2] += 1
    cks = sorted({k for (k, _m, _h) in pooled if k != "base"})
    helper = {key for key in pooled if key[1].endswith("-kept")}
    for k in cks:
        out["pooled"][k] = {"%s|%s" % (m, h): {"exact": v[0], "n": v[1], "rate": round(v[0] / v[1], 3) if v[1] else None}
                            for (kk, m, h), v in pooled.items() if kk == k and (kk, m, h) not in helper}
        out["pooled"][k]["paired"] = {"%s_over_%s|%s" % (a, b, h): {"wins": v[0], "losses": v[1], "p": round(mcnemar(*v), 6)}
                                      for (kk, a, b, h), v in pairs.items() if kk == k}
    out["pooled"]["base"] = {h: {"exact": v[0], "n": v[1], "rate": round(v[0] / v[1], 3) if v[1] else None}
                             for (kk, _m, h), v in pooled.items() if kk == "base"}
    # the overfitting record, pooled
    out["overfitting"] = {}
    for k in cks:
        rec_k = {}
        for mode in ("scratch", "continual"):
            u = pooled.get((k, mode, "unseen")); sn = pooled.get((k, mode, "seen")); tr_ = pooled.get((k, mode + "-train", "train")); od = pooled.get((k, mode + "-old", "train"))
            e = {}
            if u and u[1]:
                e["unseen"] = {"rate": round(u[0] / u[1], 3), "ci95": wilson(u[0], u[1]), "n": u[1]}
            if sn and sn[1]:
                e["seen"] = {"rate": round(sn[0] / sn[1], 3), "ci95": wilson(sn[0], sn[1]), "n": sn[1]}
            if u and sn and u[1] and sn[1]:
                e["familiarity_gap"] = round(sn[0] / sn[1] - u[0] / u[1], 3)
            if tr_ and tr_[1]:
                e["train"] = {"rate": round(tr_[0] / tr_[1], 3), "n": tr_[1]}
                if u and u[1]:
                    e["memorisation_gap"] = round(tr_[0] / tr_[1] - u[0] / u[1], 3)
            if od and od[1]:
                e["train_old_rows"] = {"rate": round(od[0] / od[1], 3), "n": od[1]}
            e["per_field_unseen"] = {fld: {"exact": v[0], "semantic": v[1], "n": v[2]} for (kk, m, fld), v in fields.items() if kk == k and m == mode}
            rec_k[mode] = e
        out["overfitting"][k] = rec_k

    # the verify leg: the loop's verdicts on the deployment's own checkpoint reads
    out["verify"] = {}
    for sd in sorted(glob.glob(os.path.join(args.root, "seed*"))):
        vp = os.path.join(sd, "verify", "verify.summary.json")
        if not os.path.isdir(sd) or not os.path.exists(vp):
            continue
        v = json.load(open(vp))
        s = int(re.search(r"seed(\d+)", sd).group(1))
        rec = {"checks_ok": v.get("all_ok")}
        for st in v.get("steps", []):
            if st["step"] == "verify":
                rec["verdicts"] = [{"verdict": x["verdict"], "baseline": x["baseline"], "current": x["current"], "lesson": x["lesson"][:120]} for x in st["verdicts"]]
                rec["reverts_proposed"] = len(st.get("reverts", []))
            if st["step"] == "revert":
                rec["rules_before"] = st["rules_before"]; rec["rules_after"] = st["rules_after"]
            if st["step"] == "measure-R":
                rec["final_exact"] = st["final_before_revert"]["exact"]; rec["r_exact"] = st["summary"]["exact"]; rec["n"] = st["summary"]["total"]
        out["verify"][s] = rec

    n_seeds = len(out["seeds"])
    print("Tuning learning curve, %d seed(s). Exact-match RATE; (wins/losses) paired against the LLM carrying the same rules.\n" % n_seeds)
    for h in HOLD:
        print("### held-out: %s\n" % (h if h != "next" else "next -- the 20 stream documents after the checkpoint, its own era (prequential)"))
        print("| documents learned from | LLM + rules | tuned from scratch | tuned continually | scratch vs continual |")
        print("|---|---:|---:|---:|---:|")
        for k in cks:
            p = out["pooled"][k]
            def cell(mode):
                c = p.get("%s|%s" % (mode, h))
                if not c or not c["n"]:
                    return "—"
                q = p["paired"].get("%s_over_llm|%s" % (mode, h))
                return "%.0f%% (%d/%d)" % (100 * c["rate"], q["wins"], q["losses"]) if q else "%.0f%%" % (100 * c["rate"])
            llm = p.get("llm|%s" % h)
            sc = p["paired"].get("scratch_over_continual|%s" % h)
            live = p.get("live|%s" % h)
            print("| %d | %s | %s | %s | %s |%s" % (k, ("%.0f%%" % (100 * llm["rate"])) if llm and llm["n"] else "—",
                                                cell("scratch"), cell("continual"),
                                                ("%d/%d p=%.3f" % (sc["wins"], sc["losses"], sc["p"])) if sc else "—",
                                                (" governed agent as it ran: %.0f%% |" % (100 * live["rate"])) if live and live["n"] else ""))
        b = out["pooled"]["base"].get(h)
        if b and b["n"]:
            print("| untuned base, final rules | %.0f%% | | | |" % (100 * b["rate"]))
        print()
    print("### overfitting receipt (per seed, per checkpoint): kept = lowest-validation checkpoint; last = final iteration\n")
    print("| seed | documents | mode | rows | iters | best val (iter) | last val | kept |")
    print("|---|---:|---|---:|---:|---:|---:|---|")
    for s, rec in sorted(out["seeds"].items()):
        for k, row in sorted(rec["checkpoints"].items()):
            for mode, t in row["train"].items():
                print("| %d | %d | %s | %d | %d | %s (%s) | %s | %s |" % (
                    s, k, mode, t["rows"], t["iters"],
                    ("%.3f" % t["best_val_loss"]) if t["best_val_loss"] is not None else "—", t["best_val_iter"],
                    ("%.3f" % t["last_val_loss"]) if t["last_val_loss"] is not None else "—", t["kept"]))
    print("\n### overfitting metrics, pooled (rates; gaps in points)\n")
    print("| documents | mode | unseen [95% CI] | seen | familiarity gap | train-set | memorisation gap | old rows (forgetting) | loss gap @kept | eff. epochs |")
    print("|---:|---|---|---:|---:|---:|---:|---:|---:|---:|")
    for k in cks:
        for mode in ("scratch", "continual"):
            e = out["overfitting"].get(k, {}).get(mode, {})
            if not e.get("unseen"):
                continue
            gaps = [rec["checkpoints"].get(k, {}).get("train", {}).get(mode, {}) for rec in out["seeds"].values()]
            lg = [g.get("loss_gap_at_best") for g in gaps if g and g.get("loss_gap_at_best") is not None]
            ep = [g.get("effective_epochs") for g in gaps if g and g.get("effective_epochs") is not None]
            print("| %d | %s | %.0f%% [%.0f–%.0f] | %s | %s | %s | %s | %s | %s | %s |" % (
                k, mode, 100 * e["unseen"]["rate"], 100 * e["unseen"]["ci95"][0], 100 * e["unseen"]["ci95"][1],
                ("%.0f%%" % (100 * e["seen"]["rate"])) if e.get("seen") else "—",
                ("%+.0f" % (100 * e["familiarity_gap"])) if e.get("familiarity_gap") is not None else "—",
                ("%.0f%%" % (100 * e["train"]["rate"])) if e.get("train") else "—",
                ("%+.0f" % (100 * e["memorisation_gap"])) if e.get("memorisation_gap") is not None else "—",
                ("%.0f%%" % (100 * e["train_old_rows"]["rate"])) if e.get("train_old_rows") else "—",
                ("%.3f" % (sum(lg) / len(lg))) if lg else "—", ("%.1f" % (sum(ep) / len(ep))) if ep else "—"))
    if out["verify"]:
        print("\n### the verify leg: the loop's verdicts on the checkpoint reads, per seed\n")
        print("| seed | lessons | held | regressed | reverts proposed | rules before -> after | final read -> after the reverts |")
        print("|---:|---:|---:|---:|---:|---|---|")
        for s, v in sorted(out["verify"].items()):
            vd = v.get("verdicts", [])
            held = sum(1 for x in vd if x["verdict"] == "held"); reg = sum(1 for x in vd if x["verdict"] == "regressed")
            r = ("%d -> %d of %d" % (v["final_exact"], v["r_exact"], v["n"])) if "r_exact" in v else "no revert, no read"
            print("| %d | %d | %d | %d | %d | %s -> %s | %s |" % (s, len(vd), held, reg, v.get("reverts_proposed", 0), v.get("rules_before", "—"), v.get("rules_after", "—"), r))
    sel = [(k, m) for (k, m, h) in pooled if h == "unseen" and m.endswith("-latest")]
    if sel:
        print("\n### the validation selector: kept checkpoint vs the latest saved one, unseen (only where they differ)\n")
        print("| documents | mode | kept | latest saved | latest over kept (wins/losses) |")
        print("|---:|---|---:|---:|---:|")
        for k, m in sorted(sel):
            mode = m[:-len("-latest")]
            kp = pooled.get((k, mode + "-kept", "unseen")); lt = pooled.get((k, m, "unseen")); pr = pairs.get((k, m, mode, "unseen"))
            print("| %d | %s | %.0f%% | %.0f%% | %d/%d p=%.3f |" % (k, mode, 100 * kp[0] / kp[1], 100 * lt[0] / lt[1], pr[0], pr[1], mcnemar(*pr)))
    if args.write:
        def strkeys(o):
            # checkpoints are ints and "base" is a string in one dict; sorted JSON needs one key type
            if isinstance(o, dict):
                return {str(k): strkeys(v) for k, v in o.items()}
            if isinstance(o, (list, tuple)):
                return [strkeys(v) for v in o]
            return o
        p = os.path.join(args.root, "CURVE.json")
        json.dump(strkeys(out), open(p, "w"), indent=1, sort_keys=True, default=str)
        print("\nwrote", p)


if __name__ == "__main__":
    sys.exit(main())
