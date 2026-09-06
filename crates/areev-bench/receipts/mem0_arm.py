#!/usr/bin/env python3
"""The plain-memory arm: real mem0, store and retrieve, nothing governed.

    mem0_arm.py --dataset D --workdir W --seed S [--experience 40] [--eval 60]
                [--mode default|domain|raw] [--top-k 10] [--snapshot-every 10]
                [--snapshot-at 20,40,80,160]
                [--arms M,M2,A]

Same receipts, same order, same agent, same accountant, same held-out set as
the governed run for the same seed — so every trial pairs with run 2's. The
one difference is what carries forward between documents:

  the governed arm  renders APPROVED RULES — one constant section, assembled
                    from what the loop proposed and a reviewer passed;
  this arm          calls mem0 exactly as its README says to: `add()` the
                    exchange after each document, `search()` the receipt
                    before the next, and puts what comes back in the prompt.
                    Nothing is proposed, reviewed, measured or withdrawn by
                    anything but mem0's own extractor.

Three modes, because "mem0" is not one thing:
  default  mem0 as installed. Its extractor is a "Personal Information
           Organizer" — tuned for preferences, names, plans — so an
           accountant's "write dates as DD/MM/YYYY" may or may not survive
           extraction. This is what a user gets out of the box.
  domain   the same, with `custom_instructions` telling the extractor these
           are operational corrections to a capture agent. mem0's supported
           hook for exactly this; the fairest reading of the system.
  raw      `infer=False`: every message stored verbatim, no extraction. The
           literal "store and retrieve".

mem0's own model calls go straight through the openai SDK, not through the
metered adapters, so they are metered here by wrapping the SDK — the cost
chart must see them or mem0 looks cheaper than it is.

What is deliberately NOT done: no snapshot copies of the vector store. A
checkpoint is a read of the held-out set with retrieval, and `search()` does
not write, so a checkpoint cannot contaminate the memory it reads.
"""
import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import accountant as acct
import dataset
import evalrun
import ledger_profile
from agent import parse_reply, propose  # noqa: F401  (propose via evalrun)

USER = "ledger"
DOMAIN_INSTRUCTIONS = (
    "These messages are an accountant correcting a document-capture agent. "
    "Store every rule about how a field must be written (date format, number "
    "format, capitalisation), every statement that a field is required, and "
    "every stated correct value as its own fact, phrased as an instruction the "
    "agent can follow next time. Ignore greetings and thanks."
)


def meter_openai(log_path):
    """Route mem0's SDK calls into the same usage ledger the adapters write."""
    try:
        from openai.resources.chat import completions as comp
    except Exception:
        return
    orig = comp.Completions.create

    def create(self, *a, **kw):
        resp = orig(self, *a, **kw)
        u = getattr(resp, "usage", None)
        if u is not None and log_path:
            try:
                with open(log_path, "a", encoding="utf-8") as fh:
                    fh.write(json.dumps({
                        "ts": int(time.time() * 1000), "script": "mem0",
                        "model": kw.get("model") or "?", "provider": "openrouter",
                        "op": "mem0", "prompt_tokens": int(getattr(u, "prompt_tokens", 0) or 0),
                        "completion_tokens": int(getattr(u, "completion_tokens", 0) or 0),
                    }) + "\n")
            except OSError:
                pass
        return resp
    comp.Completions.create = create


def build_memory(workdir, mode, model, key):
    from mem0 import Memory
    cfg = {
        "llm": {"provider": "openai", "config": {
            "model": model, "api_key": key,
            "openai_base_url": "https://openrouter.ai/api/v1", "temperature": 0}},
        "embedder": {"provider": "ollama", "config": {
            "model": "mxbai-embed-large", "embedding_dims": 1024}},
        "vector_store": {"provider": "qdrant", "config": {
            "collection_name": "ledger", "embedding_model_dims": 1024,
            "path": os.path.join(workdir, "qdrant"), "on_disk": True}},
        "history_db_path": os.path.join(workdir, "mem0_history.db"),
    }
    if mode == "domain":
        cfg["custom_instructions"] = DOMAIN_INSTRUCTIONS
    return Memory.from_config(cfg)


RULES_HEADER = ("## INSTRUCTIONS FROM THE ACCOUNTANT\n"
                "These come from the person who files these documents and "
                "they OVERRIDE the day-one instruction above. If a rule "
                "names a field to capture, that field is REQUIRED: put it "
                "in your JSON, in addition to the day-one field.\n")


def render(results, frame="plain"):
    """The retrieved memories as a prompt section. `plain` presents them as
    what they are -- memories, most relevant first. `rules` presents them
    under the governed arm's own instruction header, word for word: the
    control that separates what retrieval RETURNS from how it is framed,
    because an agent told to capture exactly the fields it was instructed
    to reads a memory as background and an instruction as an order."""
    mems = [r.get("memory", "").strip() for r in (results or {}).get("results", [])]
    mems = [m for m in mems if m]
    if not mems:
        return ""
    if frame == "rules":
        return RULES_HEADER + "\n".join("- " + m for m in mems) + "\n"
    return ("## RELEVANT MEMORIES\nWhat you have stored about this task, most relevant "
            "first.\n" + "\n".join("- " + m for m in mems) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=40)
    ap.add_argument("--eval", type=int, default=60)
    ap.add_argument("--mode", choices=("default", "domain", "raw"), default="default")
    ap.add_argument("--top-k", type=int, default=10)
    ap.add_argument("--snapshot-every", type=int, default=10)
    ap.add_argument("--snapshot-at", default="", help="comma-separated document counts to read the held-out set at (the learning-curve checkpoints)")
    ap.add_argument("--arms", default="M,M2,A")
    ap.add_argument("--resume", action="store_true",
                    help="continue a run from its journal: documents already journaled are skipped and the store is reused")
    ap.add_argument("--frame", choices=("plain", "rules"), default="plain",
                    help="how retrieved memories are presented: as memories, or under the governed arm's instruction header")
    args = ap.parse_args()

    os.makedirs(args.workdir, exist_ok=True)
    os.environ.setdefault("AREEV_USAGE_LOG", os.path.join(args.workdir, "usage.jsonl"))
    meter_openai(os.environ["AREEV_USAGE_LOG"])

    profile = ledger_profile.get(args.profile)
    agent_argv = os.environ["AGENT_CMD"].split()
    model = os.environ.get("MEM0_LLM_MODEL") or "qwen/qwen3-30b-a3b-instruct-2507"
    key = os.environ.get("OPENROUTER_API_KEY") or sys.exit("OPENROUTER_API_KEY unset")
    infer = args.mode != "raw"

    rows = dataset.load(args.dataset)
    experience, heldout = dataset.split_for(profile, rows, args.seed, args.experience, args.eval)
    evalset = evalrun.evalset_hash(heldout)
    with open(os.path.join(args.workdir, "run.config.json"), "w") as fh:
        json.dump({"arm": "mem0", "mode": args.mode, "frame": args.frame, "profile": args.profile,
                   "seed": args.seed, "experience": args.experience, "eval": args.eval,
                   "evalset": evalset, "top_k": args.top_k, "mem0_llm": model,
                   "mem0_embedder": "ollama:mxbai-embed-large",
                   "agent_cmd": os.environ.get("AGENT_CMD")}, fh, indent=1)

    m = build_memory(args.workdir, args.mode, model, key)
    embed_calls = {"n": 0}
    try:  # count the free local embeddings too, so "cost" can say what it left out
        emb = m.embedding_model
        orig_embed = emb.embed

        def embed(*a, **kw):
            embed_calls["n"] += 1
            return orig_embed(*a, **kw)
        emb.embed = embed
    except Exception:
        pass

    def section_for(row):
        q = (row["text"] or "").strip()[:1500]
        # mem0 2.0 rejects a top-level user_id on reads ("use filters=") and
        # names the cap `top_k`; older releases took user_id= and `limit`. Try
        # the current shape first, and if retrieval fails say WHY — a silent
        # empty section would score this arm as no-memory and call it mem0.
        last = None
        for kw in ({"filters": {"user_id": USER}, "top_k": args.top_k},
                   {"user_id": USER, "limit": args.top_k}):
            try:
                return render(m.search(q, **kw), args.frame)
            except TypeError as e:
                last = e
                continue
            except Exception as e:
                print("  search failed: %s: %s — empty section" % (type(e).__name__, str(e)[:160]))
                return ""
        print("  search failed: %s — empty section" % last)
        return ""

    def checkpoint(n):
        d = os.path.join(args.workdir, "at_%03d" % n)
        os.makedirs(d, exist_ok=True)
        print("\n######## checkpoint %d — held-out under retrieval" % n)
        t, u = evalrun.run_arm("M", profile, "", heldout, agent_argv, lessons_fn=section_for)
        json.dump(t, open(os.path.join(d, "trials.json"), "w"), indent=1)
        json.dump({"arm": "M", "mode": args.mode, "usage": {"M": u}, "held_out": len(heldout),
                   "evalset": evalset, "as_of": None},
                  open(os.path.join(d, "eval.summary.json"), "w"), indent=1)

    jpath = os.path.join(args.workdir, "journal.jsonl")
    done = set()
    if args.resume and os.path.exists(jpath):
        done = {json.loads(l)["seq"] for l in open(jpath, encoding="utf-8") if l.strip()}
        print("######## resuming: %d document(s) already journaled, store reused" % len(done))
    journal = open(jpath, "a", encoding="utf-8")
    totals = {"exact": 0, "semantic": 0, "scored": 0, "parked": 0, "documents": 0,
              "adds": 0, "memories_seen": 0, "model_call_failures": 0, "resumed_after": max(done) if done else 0}
    print("######## experience: %d documents, mode=%s, top_k=%d" % (len(experience), args.mode, args.top_k))
    for r in experience:
        seq = r["seq"]
        if seq in done:
            continue
        at = ledger_profile.as_of(profile, seq)
        truth_at = {k: ledger_profile.refile(at, k, v) for k, v in r["truth"].items()}
        req = ledger_profile.required_fields(profile, seq)
        section = section_for(r)
        totals["memories_seen"] += section.count("\n- ")
        try:
            out, usage = propose(agent_argv, at, r["text"], section)
        except RuntimeError as e:
            # the same park-on-failure the governed run has: a provider that
            # cannot be reached parks the document and is counted, not fatal
            totals["model_call_failures"] += 1
            print("seq %3d  MODEL CALL FAILED (%s) -- treated as a park" % (seq, str(e)[:80].replace("\n", " ")))
            out, usage = {"fields": {}, "park": True, "reason": "model call failed"}, {}
        approved, message, corrections = acct.review(at, seq, out, truth_at)

        ex = sem = scored = 0
        for k in req:
            want = truth_at.get(k, "")
            if not want:
                continue
            scored += 1
            e, s = acct.compare(at, k, (out["fields"] or {}).get(k, ""), want)
            ex += bool(e); sem += bool(s)
        totals["exact"] += ex; totals["semantic"] += sem; totals["scored"] += scored
        totals["parked"] += bool(out["park"]); totals["documents"] += 1

        # The exchange, as a user of mem0 would store it: what the agent saw
        # (abbreviated), what it proposed, what the person said back.
        if message:
            msgs = [
                {"role": "user", "content": "%s %d:\n%s" % (profile["document_noun"], seq, (r["text"] or "")[:600])},
                {"role": "assistant", "content": json.dumps(out["fields"] or {})},
                {"role": "user", "content": message},
            ]
            try:
                res = m.add(msgs, user_id=USER, infer=infer)
                totals["adds"] += len((res or {}).get("results", []))
            except Exception as e:
                print("  add failed (%s)" % type(e).__name__)
        journal.write(json.dumps({"seq": seq, "exact": ex, "semantic": sem, "scored": scored,
                                  "parked": out["park"], "message": message,
                                  "memories_in_prompt": section.count("\n- "),
                                  "usage": usage}, ensure_ascii=False) + "\n")
        journal.flush()
        print("seq %3d  exact %d/%d   semantic %d/%d   %s  mem=%d"
              % (seq, ex, scored, sem, scored, "ok" if approved else "corrected",
                 section.count("\n- ")))
        at_set = {int(x) for x in args.snapshot_at.split(",") if x.strip()}
        if ((args.snapshot_every and seq % args.snapshot_every == 0) or seq in at_set) and seq < len(experience):
            checkpoint(seq)

    json.dump({**totals, "mode": args.mode, "embed_calls": embed_calls["n"], "evalset": evalset},
              open(os.path.join(args.workdir, "experience.summary.json"), "w"), indent=1)

    # Final paired evaluation. M and M2 share the memory (noise floor); A is
    # the day-one agent — the same prompt run 2's arm A produces by rollback.
    d = os.path.join(args.workdir, "eval")
    os.makedirs(d, exist_ok=True)
    ej = open(os.path.join(d, "eval.jsonl"), "w", encoding="utf-8")
    trials, usage = [], {}
    for arm in [a.strip() for a in args.arms.split(",") if a.strip()]:
        fn = None if arm == "A" else section_for
        t, u = evalrun.run_arm(arm, profile, "", heldout, agent_argv, ej, lessons_fn=fn)
        trials += t
        usage[arm] = u
    json.dump(trials, open(os.path.join(d, "trials.json"), "w"), indent=1)
    json.dump({"arm": "mem0", "mode": args.mode, "seed": args.seed, "held_out": len(heldout),
               "evalset": evalset, "usage": usage, "embed_calls": embed_calls["n"]},
              open(os.path.join(d, "eval.summary.json"), "w"), indent=1)
    print("\nwrote", os.path.join(d, "trials.json"))


if __name__ == "__main__":
    sys.exit(main())
