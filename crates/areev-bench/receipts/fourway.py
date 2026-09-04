#!/usr/bin/env python3
"""Four ways to carry something forward, on one chart for accuracy and one
for cost.

    fourway.py --areev RUN --mem0 ROOT --slm ROOT --out STEM [--areev-cost RUN] [--json F]

Arms, and where each one's evidence lives:

  none       the day-one agent. Arm A of the governed run — the same prompt
             arm A produces by rollback — and mem0's own arm A, which must
             agree with it (a drift check across harnesses).
  mem0/<m>   ROOT/<mode>/seedN/at_NNN/trials.json (arm M) for the curve,
             ROOT/<mode>/seedN/eval/trials.json (M, M2, A) for the end.
  areev      RUN/seedN/at_NNN/trials.json (arm B) and RUN/seedN/eval/trials.json
             (B, B2, A) — the governed loop's rules in the prompt.
  slm        ROOT/seedN/eval-tuned/trials.json (arm B, agent = the LoRA) and
             eval-base (the untuned base under the same prompt). A point,
             not a curve: the model is trained once from the governed memory
             the areev arm left.

Accuracy is exact match on the same 60 held-out receipts per seed, so every
arm pairs per (document, field) with every other at the end, and the
comparisons are McNemar exact — a memory system has to survive the test a
learned rule does.

Cost comes from usage.jsonl, the ledger every model call writes to (see
cost.py). Two numbers per arm, because they answer different questions:

  inference  $ per document READ — the agent's call, whatever the agent is
  memory     $ per document LEARNED FROM — what the memory system itself
             spends to remember (mem0's extract/update calls; the loop's
             DISCOVER/GROUND/VERIFY and the reviewer)

The SLM's inference is priced at zero marginal (it ran on the laptop) AND at
a shadow hosted rate, with its one-time training time reported beside — so
the chart is not allowed to say "free" without also saying what free cost.
"""
import argparse
import glob
import json
import os
import re
import sys
from math import comb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cost as costmod

XS = [0, 10, 20, 30, 40]
ARM_ORDER = ["none", "mem0/default", "mem0/raw", "mem0/domain", "areev", "slm/base", "slm/tuned"]
LABEL = {"none": "no memory", "mem0/default": "mem0", "mem0/raw": "mem0 (raw)",
         "mem0/domain": "mem0 (domain)", "areev": "Areev — governed", "slm/base": "small model + rules",
         "slm/tuned": "Areev — tuned SLM"}

THEMES = {
    "light": {"bg": "none", "fg": "#1b1b1f", "muted": "#5f6470", "grid": "#e6e8ec", "axis": "#c9cdd4",
              "none": "#9aa0ac", "mem0/default": "#b8763a", "mem0/raw": "#d9a066", "mem0/domain": "#8a6d1f",
              "areev": "#1f6f4f", "slm/tuned": "#2f5f9e", "slm/base": "#7f9cc7"},
    "dark": {"bg": "none", "fg": "#e9eaee", "muted": "#9aa1ad", "grid": "#2a2e35", "axis": "#4a505a",
             "none": "#767d8a", "mem0/default": "#d99a5c", "mem0/raw": "#e8b98a", "mem0/domain": "#d3b04a",
             "areev": "#5fcd9b", "slm/tuned": "#7aa8e0", "slm/base": "#a9c3e8"},
}


def mcnemar(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def load_trials(path, arm):
    if not os.path.exists(path):
        return None
    return {"%s|%s" % (t["seq"], t["field"]): t for t in json.load(open(path)) if t["arm"] == arm}


def exact(tr):
    return sum(1 for t in tr.values() if t["exact"])


def paired(x, y):
    keys = set(x) & set(y)
    w = sum(1 for k in keys if x[k]["exact"] and not y[k]["exact"])
    l = sum(1 for k in keys if y[k]["exact"] and not x[k]["exact"])
    return {"n": len(keys), "wins": w, "losses": l, "p": round(mcnemar(w, l), 6)}


def seeds_in(root):
    return sorted(int(re.match(r"seed(\d+)", os.path.basename(d)).group(1))
                  for d in glob.glob(os.path.join(root, "seed*")) if os.path.isdir(d))


def curve_from(seed_dir, arm):
    """{x: trials} for a run laid out as at_NNN/ + eval/."""
    out = {}
    for d in glob.glob(os.path.join(seed_dir, "at_*")):
        m = re.match(r"at_(\d+)$", os.path.basename(d))
        tr = load_trials(os.path.join(d, "trials.json"), arm)
        if m and tr:
            out[int(m.group(1))] = tr
    final = load_trials(os.path.join(seed_dir, "eval", "trials.json"), arm)
    if final:
        out[40] = final
    return out


def usage_rows(root):
    rows = []
    for p in glob.glob(os.path.join(root, "**", "usage.jsonl"), recursive=True):
        for line in open(p, encoding="utf-8"):
            if line.strip():
                rows.append(json.loads(line))
    return rows


def cost_split(rows, memory_scripts, agent_model=None):
    """(inference $/call, memory $ total, agent calls, memory calls, shadow $/call)."""
    inf_usd = inf_calls = mem_usd = mem_calls = 0
    shadow_usd = 0.0
    for r in rows:
        usd, _ = costmod.price(r["model"], r["prompt_tokens"], r["completion_tokens"])
        is_mem = r["script"] in memory_scripts or (agent_model and r["model"] != agent_model
                                                    and not r["model"].startswith("mlx-slm"))
        if is_mem:
            mem_usd += usd
            mem_calls += 1
        else:
            inf_usd += usd
            inf_calls += 1
            if r["model"].startswith("mlx-slm"):
                shadow_usd += (r["prompt_tokens"] / 1e6 * costmod.SLM_SHADOW[0]
                               + r["completion_tokens"] / 1e6 * costmod.SLM_SHADOW[1])
    return {"inference_usd": inf_usd, "inference_calls": inf_calls,
            "inference_usd_per_call": inf_usd / inf_calls if inf_calls else None,
            "shadow_usd_per_call": shadow_usd / inf_calls if inf_calls and shadow_usd else None,
            "memory_usd": mem_usd, "memory_calls": mem_calls}


def assemble(args):
    out = {"arms": {}, "seeds": {}, "paired_at_end": {}, "cost": {}}
    agent_model = "qwen/qwen3-30b-a3b-instruct-2507"

    # ---- areev (governed) + none ----
    a_seeds = seeds_in(args.areev)
    for s in a_seeds:
        sd = os.path.join(args.areev, "seed%d" % s)
        a0 = load_trials(os.path.join(sd, "a0.trials.json"), "A0")
        cur = curve_from(sd, "B")
        armA = load_trials(os.path.join(sd, "eval", "trials.json"), "A")
        if a0:
            cur[0] = a0
        out["seeds"].setdefault(s, {})["areev"] = {str(x): exact(t) for x, t in sorted(cur.items())}
        out["seeds"][s]["none"] = {"0": exact(a0) if a0 else None, "40": exact(armA) if armA else None}
        out["seeds"][s]["_trials"] = {"areev": cur.get(40), "none": armA}

    # ---- mem0 modes ----
    for mode_dir in sorted(glob.glob(os.path.join(args.mem0, "*"))) if args.mem0 else []:
        mode = os.path.basename(mode_dir)
        for s in seeds_in(mode_dir):
            sd = os.path.join(mode_dir, "seed%d" % s)
            cur = curve_from(sd, "M")
            if not cur:
                continue
            key = "mem0/%s" % mode
            # mem0 runs do not journal A0; at zero documents the arm IS the
            # day-one agent on the same held-out set, so borrow that seed's.
            a0 = out["seeds"].get(s, {}).get("none", {}).get("0")
            pts = {str(x): exact(t) for x, t in sorted(cur.items())}
            if a0 is not None:
                pts["0"] = a0
            out["seeds"].setdefault(s, {})[key] = dict(sorted(pts.items(), key=lambda kv: int(kv[0])))
            out["seeds"][s].setdefault("_trials", {})[key] = cur.get(40)
            mA = load_trials(os.path.join(sd, "eval", "trials.json"), "A")
            if mA is not None:
                out["seeds"][s]["mem0_armA_%s" % mode] = exact(mA)

    # ---- slm ----
    for s in seeds_in(args.slm) if args.slm else []:
        sd = os.path.join(args.slm, "seed%d" % s)
        for kind in ("tuned", "base"):
            tr = load_trials(os.path.join(sd, "eval-%s" % kind, "trials.json"), "B")
            if tr:
                key = "slm/%s" % kind
                out["seeds"].setdefault(s, {})[key] = {"40": exact(tr)}
                out["seeds"][s].setdefault("_trials", {})[key] = tr
        man = os.path.join(sd, "adapter", "adapter.manifest.json")
        if os.path.exists(man):
            out["seeds"].setdefault(s, {})["slm_training"] = json.load(open(man))

    # ---- pooled curves + paired tests at the end ----
    present = [a for a in ARM_ORDER if any(a in d for d in out["seeds"].values())]
    common = sorted(s for s, d in out["seeds"].items() if all(a in d for a in present))
    out["pooled_over_seeds"] = common
    for arm in present:
        pts = {}
        for s in common:
            for x, v in (out["seeds"][s].get(arm) or {}).items():
                if v is not None:
                    pts.setdefault(int(x), []).append(v)
        if pts:
            out["arms"][arm] = {"pooled": {str(x): sum(v) for x, v in sorted(pts.items()) if len(v) == len(common)},
                                "seeds": len(common), "trials_per_seed": 240}
    for arm in ARM_ORDER:
        if arm == "areev":
            continue
        w = l = n = 0
        for s, d in out["seeds"].items():
            t = d.get("_trials", {})
            if t.get("areev") and t.get(arm):
                p = paired(t["areev"], t[arm])
                w += p["wins"]; l += p["losses"]; n += p["n"]
        if n:
            out["paired_at_end"]["areev_vs_%s" % arm] = {"n": n, "wins": w, "losses": l, "p": round(mcnemar(w, l), 6)}

    # ---- cost ----
    if args.areev_cost and os.path.isdir(args.areev_cost):
        out["cost"]["areev"] = cost_split(usage_rows(args.areev_cost), {"openrouter_loop.py"}, agent_model)
        out["cost"]["areev"]["source"] = args.areev_cost
        out["cost"]["areev"]["learned_documents"] = 40 * len(seeds_in(args.areev_cost))
    for mode_dir in sorted(glob.glob(os.path.join(args.mem0, "*"))) if args.mem0 else []:
        rows = usage_rows(mode_dir)
        if rows:
            c = cost_split(rows, {"mem0"}, agent_model)
            c["learned_documents"] = 40 * len(seeds_in(mode_dir))
            out["cost"]["mem0/%s" % os.path.basename(mode_dir)] = c
    if args.slm:
        for kind in ("tuned", "base"):
            rows = []
            for s in seeds_in(args.slm):
                rows += usage_rows(os.path.join(args.slm, "seed%d" % s, "eval-%s" % kind))
            if rows:
                c = cost_split(rows, set())
                lat = [r.get("latency_ms") for r in rows if r.get("latency_ms")]
                c["latency_ms_median"] = sorted(lat)[len(lat) // 2] if lat else None
                out["cost"]["slm/%s" % kind] = c
        secs = [d["slm_training"]["train_seconds"] for d in out["seeds"].values() if "slm_training" in d]
        if secs:
            out["cost"].setdefault("slm/tuned", {"inference_calls": 0, "inference_usd": 0.0,
                                                 "inference_usd_per_call": None, "shadow_usd_per_call": None,
                                                 "memory_usd": 0.0, "memory_calls": 0})
            out["cost"]["slm/tuned"]["train_seconds_per_seed"] = secs
    for s in out["seeds"].values():
        s.pop("_trials", None)
    return out


# ---------------------------------------------------------------- charts
W, H = 920, 400
PAD_L, PAD_T, PLOT_H = 58, 30, 300


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


def shelf(peak):
    """The smallest ceiling above the peak whose quarters are round numbers."""
    for top in (40, 80, 100, 120, 160, 200, 240, 300, 400, 480, 600, 720, 800, 1000, 1200, 1600, 2000):
        if top >= peak * 1.05:
            return top
    return int(peak * 1.15) + 1


def accuracy_svg(theme, res):
    t = THEMES[theme]
    arms = [a for a in ARM_ORDER if a in res["arms"]]
    seeds = max(res["arms"][a]["seeds"] for a in arms)
    total = 240 * seeds
    peak = max(v for a in arms for v in res["arms"][a]["pooled"].values())
    ymax = shelf(peak)
    CURVE_W = 700
    px = lambda v: PAD_L + CURVE_W * (v / 40.0)
    py = lambda v: PAD_T + PLOT_H - PLOT_H * (v / ymax)
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
         f'font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,sans-serif">']
    a = o.append
    ends = "; ".join("%s ends at %d" % (LABEL[x], list(res["arms"][x]["pooled"].values())[-1]) for x in arms)
    a(f'<title>{esc("Exact matches of %d held-out trials as the deployment proceeds: %s." % (total, ends))}</title>')
    a(f'<desc>{esc("The same held-out receipts are re-read after 0, 10, 20, 30 and 40 experience documents, pooled over %d seed(s). A memory system that stores what it is told but does not turn it into rules stays near the no-memory floor; the governed loop rises at the first checkpoint and holds. The tuned small model is a single point at 40, because it is trained once from the governed memory." % seeds)}</desc>')
    for i in range(5):
        v = ymax * i / 4
        a(f'<line x1="{PAD_L}" y1="{py(v):.1f}" x2="{PAD_L+CURVE_W}" y2="{py(v):.1f}" stroke="{t["grid"]}"/>')
        a(f'<text x="{PAD_L-9}" y="{py(v)+4:.1f}" text-anchor="end" font-size="11" fill="{t["muted"]}">{int(v)}</text>')
    for x in XS:
        a(f'<text x="{px(x):.1f}" y="{PAD_T+PLOT_H+19}" text-anchor="middle" font-size="11" fill="{t["muted"]}">{x}</text>')
    a(f'<text x="{PAD_L-32}" y="{PAD_T-12}" font-size="11" fill="{t["muted"]}">exact, of {total} trials pooled over {seeds} seed{"s" if seeds != 1 else ""}</text>')
    a(f'<text x="{PAD_L+CURVE_W/2:.0f}" y="{PAD_T+PLOT_H+40}" text-anchor="middle" font-size="11.5" fill="{t["muted"]}">experience documents seen</text>')
    labels = []
    for arm in arms:
        pts = sorted((int(x), v) for x, v in res["arms"][arm]["pooled"].items())
        col = t[arm]
        if len(pts) > 1:
            d = " ".join(("M" if i == 0 else "L") + f"{px(x):.1f} {py(v):.1f}" for i, (x, v) in enumerate(pts))
            a(f'<path d="{d}" fill="none" stroke="{col}" stroke-width="2.4" stroke-linejoin="round"/>')
        for x, v in pts:
            r = 5 if len(pts) == 1 else 3
            a(f'<circle cx="{px(x):.1f}" cy="{py(v):.1f}" r="{r}" fill="{col}"/>')
        labels.append([LABEL[arm], col, px(pts[-1][0]) + 10, py(pts[-1][1]) + 4])
    labels.sort(key=lambda e: e[3])
    for i in range(1, len(labels)):
        if labels[i][3] - labels[i - 1][3] < 15:
            labels[i][3] = labels[i - 1][3] + 15
    for lab, col, x, y in labels:
        a(f'<text x="{x:.1f}" y="{y:.1f}" font-size="12" font-weight="600" fill="{col}">{esc(lab)}</text>')
    a(f'<line x1="{PAD_L}" y1="{py(0):.1f}" x2="{PAD_L+CURVE_W}" y2="{py(0):.1f}" stroke="{t["axis"]}"/>')
    a("</svg>")
    return "\n".join(o)


def cost_svg(theme, res):
    t = THEMES[theme]
    rows = []
    for arm in ARM_ORDER:
        c = res["cost"].get(arm)
        if not c or not c.get("inference_calls"):
            continue
        inf = (c["inference_usd_per_call"] or 0) * 1000
        memv = (c["memory_usd"] / c["learned_documents"] * 1000) if c.get("learned_documents") else 0.0
        shadow = (c["shadow_usd_per_call"] or 0) * 1000 if c.get("shadow_usd_per_call") else None
        rows.append((arm, inf, memv, shadow))
    if not rows:
        return "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10'/>"
    peak = max(max(r[1], r[2], r[3] or 0) for r in rows)
    xmax = shelf(peak) if peak > 0 else 1
    LEFT, BW, ROWH = 190, 620, 44
    Hh = PAD_T + ROWH * len(rows) + 60
    bx = lambda v: LEFT + BW * (v / xmax)
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {Hh}" width="{W}" height="{Hh}" '
         f'font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,sans-serif">']
    a = o.append
    a(f'<title>{esc("Dollars per thousand documents, from journaled tokens: what each arm pays to read a document, and what its memory system pays to learn from one.")}</title>')
    a(f'<desc>{esc("Two bars per arm. The solid bar is inference, the agent call per document read. The hatched bar is the memory system itself: mem0 extract and update calls, or the governed loop and its reviewer. The tuned small model ran locally, so its inference bar is drawn at zero with a shadow marker at a hosted small-model rate.")}</desc>')
    a('<defs><pattern id="h" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">'
      f'<line x1="0" y1="0" x2="0" y2="6" stroke="{t["muted"]}" stroke-width="2"/></pattern></defs>')
    for i in range(5):
        v = xmax * i / 4
        a(f'<line x1="{bx(v):.1f}" y1="{PAD_T}" x2="{bx(v):.1f}" y2="{PAD_T+ROWH*len(rows)}" stroke="{t["grid"]}"/>')
        a(f'<text x="{bx(v):.1f}" y="{PAD_T+ROWH*len(rows)+18}" text-anchor="middle" font-size="11" fill="{t["muted"]}">${v:.2f}</text>')
    a(f'<text x="{LEFT+BW/2:.0f}" y="{PAD_T+ROWH*len(rows)+40}" text-anchor="middle" font-size="11.5" fill="{t["muted"]}">USD per 1,000 documents</text>')
    for i, (arm, inf, memv, shadow) in enumerate(rows):
        y = PAD_T + i * ROWH
        col = t[arm]
        a(f'<text x="{LEFT-10}" y="{y+18}" text-anchor="end" font-size="12" font-weight="600" fill="{col}">{esc(LABEL[arm])}</text>')
        a(f'<rect x="{LEFT}" y="{y+4}" width="{max(bx(inf)-LEFT, 1.5):.1f}" height="14" fill="{col}"/>')
        a(f'<text x="{bx(inf)+6:.1f}" y="{y+15}" font-size="11" fill="{t["fg"]}">read ${inf:.2f}</text>')
        if memv > 0:
            a(f'<rect x="{LEFT}" y="{y+21}" width="{max(bx(memv)-LEFT, 1.5):.1f}" height="14" fill="url(#h)"/>')
            a(f'<text x="{bx(memv)+6:.1f}" y="{y+32}" font-size="11" fill="{t["muted"]}">learn ${memv:.2f}</text>')
        if shadow is not None:
            a(f'<line x1="{bx(shadow):.1f}" y1="{y+2}" x2="{bx(shadow):.1f}" y2="{y+20}" stroke="{col}" stroke-width="2" stroke-dasharray="3 2"/>')
            a(f'<text x="{bx(shadow)+6:.1f}" y="{y+15}" font-size="11" fill="{t["muted"]}">hosted-rate shadow ${shadow:.2f}</text>')
    a("</svg>")
    return "\n".join(o)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--areev", required=True, help="governed run dir with seedN/ (trials)")
    ap.add_argument("--areev-cost", default=None, help="a METERED governed run dir for cost")
    ap.add_argument("--mem0", default=None)
    ap.add_argument("--slm", default=None)
    ap.add_argument("--out", required=True, help="SVG stem; writes STEM-accuracy-{light,dark}.svg and STEM-cost-*.svg")
    ap.add_argument("--json", default=None)
    args = ap.parse_args()
    res = assemble(args)

    print("| arm | " + " | ".join(str(x) for x in XS) + " | seeds |")
    print("|---|" + "---:|" * len(XS) + "---:|")
    for arm in ARM_ORDER:
        if arm in res["arms"]:
            p = res["arms"][arm]["pooled"]
            print("| %s | %s | %d |" % (LABEL[arm], " | ".join(str(p.get(str(x), "")) for x in XS), res["arms"][arm]["seeds"]))
    for k, v in res["paired_at_end"].items():
        print("%-26s %4d wins / %4d losses  p=%.4f  (n=%d)" % (k, v["wins"], v["losses"], v["p"], v["n"]))
    for arm, c in res["cost"].items():
        print("cost %-14s inference $%.5f/call over %d calls; memory $%.4f over %d calls%s"
              % (arm, c["inference_usd_per_call"] or 0, c["inference_calls"], c["memory_usd"], c["memory_calls"],
                 ("; shadow $%.5f/call" % c["shadow_usd_per_call"]) if c.get("shadow_usd_per_call") else ""))
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    for theme in ("light", "dark"):
        for kind, fn in (("accuracy", accuracy_svg), ("cost", cost_svg)):
            p = "%s-%s-%s.svg" % (args.out, kind, theme)
            open(p, "w", encoding="utf-8").write(fn(theme, res))
            print("wrote", p)
    if args.json:
        json.dump(res, open(args.json, "w"), indent=1, sort_keys=True)
        print("wrote", args.json)


if __name__ == "__main__":
    sys.exit(main())
