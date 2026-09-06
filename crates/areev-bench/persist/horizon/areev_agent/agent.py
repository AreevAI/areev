"""AreevAgent — Horizon reference-style agent whose prior-session memory is
one Areev file, and whose learning from it is governed.

Ingestion
---------
The trace at ``/workdir/trace.jsonl`` is read once (``read_trace_file``) and
written into an Areev file on the host: every ``function_call`` +
``function_call_output`` pair becomes a **Tool grain** (name, arguments,
output, error flag, timestamp, one thread per UTC day), every ``reasoning``
and ``message`` an **Event** in that day's session. Nothing is summarised
and nothing is dropped; the raw trace is then deleted from the sandbox so
the only path back to it is the memory.

Learning (the governed variant)
-------------------------------
Before the task starts, Areev's loop runs over the ingested memory:
``tool_failure`` clusters the breakages (a broken ``curl`` is exactly its
input), DISCOVER proposes the lessons the trace implies and must cite the
grains it read, GROUND checks the premises, and a fixed-rubric reviewer
(``persist/reviewer.py``) approves or refuses each proposal with a reason.
Approved lessons are applied — they render into the system prompt as
"learned from earlier sessions". The passive variant skips the loop: the
same memory, searched on demand, nothing learned ahead of time.

Acting
------
The model sees ``session_search`` (hybrid search over the memory's Events
and Tool grains, plus a recent-sessions listing) and the task's own tools
from ``/.horizon/tools/tools.json`` through ``HorizonToolRegistry`` — the
same surface ``trace_rag`` gives it, with memory search in place of
embedding search. Emits an ATIF trajectory with cost split into chat, loop
legs (metered by the bench's adapters) and review.

Run it with::

    export OPENROUTER_API_KEY=... OPENROUTER_MANAGEMENT_KEY=...   # the latter optional locally
    export AREEV_LOOP_LLM_CMD="python3 .../openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507 --provider siliconflow/fp8 --seed 1"
    export AREEV_LOOP_GROUND_CMD="python3 .../openrouter_loop.py openai/gpt-4o-mini --provider openai --seed 1"
    export AREEV_REVIEW_CMD="python3 .../openrouter_toolcall.py openai/gpt-4o --provider openai --seed 1"
    PYTHONPATH=agents harbor run -p evals/01-example-catering-vendor \\
        --agent-import-path areev_agent.agent:AreevGovernedAgent -m qwen/qwen3-30b-a3b-instruct-2507
"""

from __future__ import annotations

import asyncio
import gc
import json
import os
import re
import sys
import time
import uuid
from collections import defaultdict
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from agent_utils import (
    HorizonToolRegistry,
    load_environment_tools,
    read_trace_file,
    summarize_call_log,
    timed_call,
    trial_subkey,
    usage_cost,
)
from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trajectories import (
    Agent,
    FinalMetrics,
    Metrics,
    Observation,
    ObservationResult,
    Step,
    ToolCall,
    Trajectory,
)

_PERSIST_DIR = Path(__file__).resolve().parents[2]  # crates/areev-bench/persist
if str(_PERSIST_DIR) not in sys.path:
    sys.path.insert(0, str(_PERSIST_DIR))

TRACE_PATH = "/workdir/trace.jsonl"
MAX_STEPS = 12
DEFAULT_CHAT_MODEL = "qwen/qwen3-30b-a3b-instruct-2507"
ATIF_VERSION = "ATIF-v1.4"
MAX_EXEC_OUTPUT_CHARS = 12_000
NS = "desk:horizon"
ACTOR_AGENT = "agent:assistant"
ACTOR_RUNNER = "loop:runner"
ACTOR_REVIEWER = "user:reviewer"
MAX_GRAIN_TEXT = 4000
SEARCH_K = 8
_ERROR_RE = re.compile(
    r"(^|\n)\s*(error|err:|traceback|exception|failed|failure|not found|permission denied|"
    r"command not found|no such file|exit code:\s*[1-9]\d*|non-zero exit)", re.I)

SYSTEM_PROMPT_TEMPLATE = (
    "You are an autonomous agent continuing work you did in earlier sessions.\n\n"
    "You have tools:\n"
    "  - `session_search`: search your memory of the earlier sessions (what was said, "
    "which tools were run, what they returned). Call it with a query before assuming any "
    "prior-session detail; with no query it lists the most recent sessions. There is NO "
    "other access to the earlier sessions.\n"
    "  - Task tools: {tool_names}. Each matches a command from the earlier sessions; use "
    "them to act on the current world. There is no generic shell.\n\n"
    "{lessons}"
    "Workflow: ALWAYS start by inspecting the current environment (list what is pending) "
    "and then act on it, using `session_search` to inform the action with what you learned "
    "before. Search more than once, with different terms, when the first pass lacks the "
    "specific detail the request turns on. When you answer or act, carry the concrete "
    "identifying details from memory — names, amounts, dates, contacts, identifiers — rather "
    "than a summary of them. Do not summarise the past or ask the user what to do; find the "
    "pending work and complete it. Stop when the task's success condition is met."
)

SESSION_SEARCH_TOOL: dict[str, Any] = {
    "type": "function",
    "function": {
        "name": "session_search",
        "description": ("Search the memory of earlier sessions: messages, reasoning, and the "
                        "tool calls with their outputs. Returns the best-matching records with "
                        "their day. No query lists the most recent sessions."),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Natural-language query."},
                "k": {"type": "integer", "minimum": 1, "maximum": 20, "default": SEARCH_K},
            },
        },
    },
}


def _now_iso() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")


def _day_of(ts: str) -> str:
    try:
        return datetime.fromisoformat(str(ts).replace("Z", "+00:00")).astimezone(UTC).date().isoformat()
    except ValueError:
        return "undated"


def _render_content(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for item in content:
            if isinstance(item, str):
                parts.append(item)
            elif isinstance(item, dict):
                if item.get("type") == "thinking":
                    parts.append("thinking: " + str(item.get("thinking", "")))
                elif item.get("text"):
                    parts.append(str(item["text"]))
                else:
                    parts.append(json.dumps({k: v for k, v in item.items() if k not in ("signature", "id")},
                                            ensure_ascii=False))
        return "\n".join(p for p in parts if p)
    if isinstance(content, dict):
        return json.dumps({k: v for k, v in content.items() if k not in ("signature", "id")}, ensure_ascii=False)
    return str(content or "")


# ---------------------------------------------------------------- the file

def _with_memory(path, actor, fn):
    import areev
    db = areev.Areev(str(path), ns=NS, actor=actor)
    try:
        return fn(db)
    finally:
        del db
        gc.collect()


def ingest_trace(db_path: Path, trace_text: str) -> dict[str, int]:
    """The trace as grains: Tool grains for call/output pairs, Events for the
    rest, one session per UTC day. Lossless: nothing summarised."""
    events: list[dict[str, Any]] = []
    for line in trace_text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    n = {"lines": len(events), "events": 0, "tools": 0, "tool_errors": 0, "days": 0}
    pending: dict[str, dict[str, Any]] = {}
    days: set[str] = set()

    def go(db):
        for ev in events:
            ts = str(ev.get("timestamp") or "")
            day = _day_of(ts)
            days.add(day)
            data = ev.get("message_data") or {}
            etype = data.get("type") or "message"
            if etype == "function_call":
                cid = str(data.get("call_id") or data.get("id") or uuid.uuid4().hex)
                pending[cid] = {"name": data.get("name") or "tool", "arguments": data.get("arguments") or "{}",
                                "ts": ts, "day": day}
                continue
            if etype == "function_call_output":
                cid = str(data.get("call_id") or "")
                call = pending.pop(cid, None) or {"name": "tool", "arguments": "{}", "ts": ts, "day": day}
                output = str(data.get("output") or "")
                is_error = bool(_ERROR_RE.search(output[:2000]))
                db.record_tool_call(str(call["name"]), output[:MAX_GRAIN_TEXT], is_error,
                                    thread="day:" + call["day"], call_id=cid or None,
                                    input=str(call["arguments"])[:MAX_GRAIN_TEXT])
                n["tools"] += 1
                n["tool_errors"] += int(is_error)
                continue
            if etype == "reasoning":
                text = _render_content(data.get("summary"))
                role = "assistant"
            elif etype == "message":
                text = _render_content(data.get("content"))
                role = str(data.get("role") or "user")
                if role not in ("user", "assistant", "system", "tool"):
                    role = "user"
            else:
                text = json.dumps({k: v for k, v in data.items() if k not in ("signature",)}, ensure_ascii=False)
                role = "system"
            if not text.strip():
                continue
            db.add("event", json.dumps({"content": ("[%s] " % ts if ts else "") + text[:MAX_GRAIN_TEXT],
                                        "role": role, "session_id": "day:" + day,
                                        "subject": "session:day:" + day}), ns=NS)
            n["events"] += 1
            # A person's own words are the rarest evidence and the loop
            # reserves a share of DISCOVER's bundle for human Observations;
            # an Event with role=user never reaches it. Record them twice.
            if etype == "message" and role == "user":
                db.add("observation", json.dumps({"content": text[:MAX_GRAIN_TEXT], "observer_id": "user",
                                                  "observer_type": "human", "subject": "assistant"}), ns=NS)
                n["observations"] = n.get("observations", 0) + 1
        # calls that never got an output are still evidence of what was tried
        for cid, call in pending.items():
            db.record_tool_call(str(call["name"]), "(no output recorded)", False,
                                thread="day:" + call["day"], call_id=cid,
                                input=str(call["arguments"])[:MAX_GRAIN_TEXT])
            n["tools"] += 1
        n["days"] = len(days)
        return n

    return _with_memory(db_path, ACTOR_AGENT, go)


def live_lessons(db) -> list[str]:
    payload = json.loads(db.cal('RECALL facts WHERE namespace = "%s" LIMIT 500 FORMAT json' % NS))
    out = []
    for g in payload.get("grains", []):
        f = g.get("fields") or {}
        rel, obj = f.get("relation") or "", f.get("object") or ""
        if not obj or rel == "mg:eval_run":
            continue
        out.append(obj if rel in ("lesson", "note") else "%s %s: %s" % (f.get("subject") or "", rel, obj))
    return out


def govern(db_path: Path, seed: int) -> dict[str, Any]:
    """One governed pass before the task: propose under the runner, decide
    under the reviewer; both recorded."""
    import reviewer as rv

    llm_cmd = os.environ.get("AREEV_LOOP_LLM_CMD") or None
    ground_cmd = os.environ.get("AREEV_LOOP_GROUND_CMD") or None
    review_cmd = os.environ.get("AREEV_REVIEW_CMD") or None
    policy_path = db_path.parent / "loop-policy.json"
    policy_path.write_text(json.dumps({"discover_objective": "learner"}), encoding="utf-8")
    rep = json.loads(_with_memory(
        db_path, ACTOR_RUNNER,
        lambda db: db.loop_run(llm_cmd=llm_cmd, ground_cmd=ground_cmd, policy=str(policy_path), full_sweep=True)))
    out = {"funnel": rep.get("llm_funnel"), "proposed": rep.get("proposed"), "stored": rep.get("stored"),
           "analyzers_run": rep.get("analyzers_run"), "pending": 0, "applied": 0, "rejected": 0,
           "decisions": [], "errors": [], "llm": bool(llm_cmd), "reviewer": bool(review_cmd)}
    judge = rv.make_judge(review_cmd)

    def review(db):
        in_force = live_lessons(db)
        pend = json.loads(db.recommendations('{"status":"pending"}'))
        out["pending"] = len(pend)
        by_hash = {}
        for noun in ("observations", "facts", "tools", "events"):
            try:
                for g in json.loads(db.cal('RECALL %s WHERE namespace = "%s" LIMIT 1000 FORMAT json' % (noun, NS))).get("grains", []):
                    by_hash[g.get("hash")] = g.get("fields") or {}
            except Exception:
                continue
        # `recommendations()` carries no evidence list; the stored record
        # (a Fact in the `areev-loop` namespace) does — see PERSIST.md #18
        cited = {}
        try:
            for g in json.loads(db.cal('RECALL facts WHERE namespace = "areev-loop" AND relation = "loop_recommendation" '
                                       'LIMIT 1000 FORMAT json')).get("grains", []):
                try:
                    o = json.loads((g.get("fields") or {}).get("object") or "{}")
                except json.JSONDecodeError:
                    continue
                cited[g.get("hash")] = o.get("evidence") or []
        except Exception:
            pass
        for rec in pend:
            rec_evidence = rec.get("evidence") or cited.get(rec.get("hash")) or []
            summary = str(rec.get("summary") or "")
            m = re.search(r'record lesson:\s*"(.*)"\s*$', summary, re.S)
            m2 = re.search(r"record fact:\s*(.*)$", summary, re.S)
            if m:
                kind, text = "lesson", m.group(1).strip()
            elif m2:
                kind, text = "fact", m2.group(1).strip().strip('"')
            else:
                kind, text = "advisory", summary.strip()
            evidence = []
            for h in rec_evidence[:6]:
                f = by_hash.get(h)
                if f:
                    evidence.append(json.dumps({k: v for k, v in f.items()
                                                if k in ("content", "object", "relation", "tool_name",
                                                         "input", "tool_content", "error", "is_error")},
                                               ensure_ascii=False)[:900])
            if kind in ("lesson", "fact"):
                ok, because = rv.review(text, "\n".join(evidence), in_force, judge)
            else:
                ok, because = False, "advisory only — asks for no change"
            try:
                if ok:
                    db.apply_recommendation(rec["hash"], because)
                    out["applied"] += 1
                    in_force.append(text)
                else:
                    db.dismiss_recommendation(rec["hash"], because)
                    out["rejected"] += 1
                out["decisions"].append({"hash": rec["hash"], "analyzer": rec.get("analyzer"), "kind": kind,
                                         "text": text[:300], "approved": bool(ok), "because": because[:300]})
            except Exception as exc:
                out["errors"].append("%s: %s" % (str(rec.get("hash", ""))[:12], str(exc)[:160]))

    _with_memory(db_path, ACTOR_REVIEWER, review)
    return out


def session_search(db_path: Path, query: str, k: int) -> dict[str, Any]:
    def go(db):
        if not query:
            rows = json.loads(db.cal('RECALL events WHERE namespace = "%s" LIMIT 500 FORMAT json' % NS)).get("grains", [])
            seen, out = set(), []
            for g in reversed(rows):
                f = g.get("fields") or {}
                sid = f.get("session_id") or ""
                if sid in seen:
                    continue
                seen.add(sid)
                out.append({"session": sid, "preview": (f.get("content") or "")[:200]})
                if len(out) >= 10:
                    break
            return {"recent_sessions": out}
        # Hybrid search returns Event grains only (measured on the first
        # smoke: three queries, events every time, never one of the 13 Tool
        # grains naming the vendor), and the tool output lives in
        # `tool_content`. So the tool records get their own pass — a keyword
        # score over every Tool grain, cheap at this scale — and the answer
        # interleaves the two kinds so it shows both what was thought and
        # what the tools actually returned.
        payload = json.loads(db.search(query, k=max(k * 2, 16), ns=NS))
        grains = payload.get("grains", payload) if isinstance(payload, dict) else payload
        event_hits = []
        for g in grains or []:
            f = (g.get("fields") or {}) if isinstance(g, dict) else {}
            if f.get("tool_name"):
                continue
            text = "%s: %s" % (f.get("role") or "note", str(f.get("content") or f.get("object") or "")[:900])
            event_hits.append({"session": f.get("session_id") or "", "text": text})
        words = {w for w in re.findall(r"[a-z0-9]+", query.lower()) if len(w) > 2}
        tools_all = json.loads(db.cal('RECALL tools WHERE namespace = "%s" LIMIT 500 FORMAT json' % NS)).get("grains", [])
        scored = []
        for g in tools_all:
            f = g.get("fields") or {}
            body = "%s %s %s" % (f.get("tool_name") or "", f.get("input") or "", f.get("tool_content") or f.get("content") or "")
            low = body.lower()
            score = sum(low.count(w) for w in words)
            if score:
                scored.append((score, str(f.get("created_at") or ""), f))
        scored.sort(key=lambda x: (-x[0], x[1]))

        def render_tool(f, cap):
            return "tool %s(%s) -> %s%s" % (f.get("tool_name"), str(f.get("input") or "")[:200],
                                            "ERROR " if str(f.get("is_error")).lower() == "true" else "",
                                            str(f.get("tool_content") or f.get("content") or f.get("error") or "")[:cap])

        tools_hits = [{"session": f.get("session_id") or f.get("thread") or "", "text": render_tool(f, 1200)}
                      for _, _, f in scored[:k]]
        hits: list[dict[str, Any]] = []
        while len(hits) < k and (tools_hits or event_hits):
            if tools_hits:
                hits.append(tools_hits.pop(0))
            if event_hits and len(hits) < k:
                hits.append(event_hits.pop(0))
        # The day around the best hit, in order. A keyword hit lands on the
        # record that names the topic (an inbox listing), while the detail
        # the request turns on sits in the next record of the same day (the
        # email that was then read). trace_rag gets this for free by
        # retrieving whole-day chunks; here the same day is reconstructed
        # from the grains, bounded.
        # The whole day around the best hits — tool records AND the notes
        # between them, in order — which is what trace_rag's per-day chunk
        # gives its model and what a keyword hit alone does not: the record
        # that names the topic sits next to the record that holds the detail.
        day_digest = None
        if scored or event_hits:
            day_score: dict[str, int] = defaultdict(int)
            for sc, _, f in scored:
                day_score[f.get("session_id") or ""] += sc
            for h in event_hits[:6]:
                day_score[h.get("session") or ""] += 1
            best_day = max(day_score, key=day_score.get)
            events_all = json.loads(db.cal('RECALL events WHERE namespace = "%s" AND session_id = "%s" LIMIT 500 FORMAT json'
                                           % (NS, best_day))).get("grains", [])
            records = []
            for g in tools_all:
                f = g.get("fields") or {}
                if (f.get("session_id") or "") == best_day:
                    records.append((str(f.get("created_at") or ""), render_tool(f, 700)))
            for g in events_all:
                f = g.get("fields") or {}
                records.append((str(f.get("created_at") or ""),
                                "%s: %s" % (f.get("role") or "note", str(f.get("content") or "")[:500])))
            records.sort(key=lambda x: x[0])
            budget, parts = 9000, []
            for _, t in records:
                if budget - len(t) < 0:
                    break
                budget -= len(t)
                parts.append(t)
            day_digest = {"session": best_day, "records": parts}
        return {"hits": hits, "best_day": day_digest}
    return _with_memory(db_path, ACTOR_AGENT, go)


def _loop_cost(usage_log: Path) -> dict[str, Any]:
    """Tokens the loop and review legs metered, by model — priced later by
    cost.py from the pinned table; here only counted."""
    out: dict[str, dict[str, int]] = defaultdict(lambda: {"calls": 0, "prompt_tokens": 0, "completion_tokens": 0})
    if not usage_log.exists():
        return {}
    for line in usage_log.read_text(encoding="utf-8").splitlines():
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        m = str(r.get("model") or "?")
        out[m]["calls"] += 1
        out[m]["prompt_tokens"] += int(r.get("prompt_tokens") or 0)
        out[m]["completion_tokens"] += int(r.get("completion_tokens") or 0)
    return dict(out)


# --------------------------------------------------------------- the agent

class AreevAgentBase(BaseAgent):
    SUPPORTS_ATIF = True
    GOVERNED = True

    def version(self) -> str | None:
        return "0.1.0"

    async def setup(self, environment: BaseEnvironment) -> None:
        if not os.environ.get("OPENROUTER_API_KEY"):
            raise RuntimeError("AreevAgent requires OPENROUTER_API_KEY")
        import areev  # noqa: F401 — fail loudly before any spend if the binding is absent

    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        from openai import AsyncOpenAI

        management_key = os.environ.get("OPENROUTER_MANAGEMENT_KEY", "")
        chat_model = self.model_name or DEFAULT_CHAT_MODEL
        seed = int(os.environ.get("SEED", "1") or 1)
        pin = os.environ.get("AREEV_AGENT_PIN", "")
        t_start = time.monotonic()
        call_log: list[dict] = []
        steps: list[Step] = []
        total_prompt = total_completion = 0
        chat_cost_usd = 0.0
        db_path = self.logs_dir / "memory.db"
        usage_log = self.logs_dir / "usage.jsonl"
        os.environ["AREEV_USAGE_LOG"] = str(usage_log)
        ingest: dict[str, Any] = {}
        governance: dict[str, Any] | None = None
        lessons: list[str] = []
        t_ingest_done = t_learn_done = t_end = t_start

        async with trial_subkey(management_key=management_key,
                                label=f"horizon-areev-{uuid.uuid4().hex[:8]}") as tk:
            client = AsyncOpenAI(base_url="https://openrouter.ai/api/v1", api_key=tk.key)
            async with timed_call(call_log, "download", "load tools.json"):
                tool_registry: HorizonToolRegistry = await load_environment_tools(environment)
            async with timed_call(call_log, "download", "download trace.jsonl"):
                trace_text = await read_trace_file(environment, TRACE_PATH)
            async with timed_call(call_log, "ingest", "trace -> areev grains"):
                ingest = await asyncio.to_thread(ingest_trace, db_path, trace_text)
            t_ingest_done = time.monotonic()
            async with timed_call(call_log, "exec", "rm trace.jsonl"):
                try:
                    await environment.exec(f"rm -f {TRACE_PATH}", timeout_sec=10)
                except Exception:
                    pass
            if self.GOVERNED:
                async with timed_call(call_log, "loop", "governed pass"):
                    try:
                        governance = await asyncio.to_thread(govern, db_path, seed)
                    except Exception as exc:
                        governance = {"error": "%s: %s" % (type(exc).__name__, str(exc)[:300])}
                lessons = await asyncio.to_thread(lambda: _with_memory(db_path, ACTOR_AGENT, live_lessons))
            t_learn_done = time.monotonic()

            lessons_block = ""
            if lessons:
                lessons_block = ("Learned from the earlier sessions (proposed by review of the memory, approved "
                                 "by a reviewer) — apply these without being asked:\n"
                                 + "\n".join("  - " + x for x in lessons) + "\n\n")
            system_prompt = SYSTEM_PROMPT_TEMPLATE.format(
                tool_names=", ".join(f"`{n}`" for n in tool_registry.names) or "(none)", lessons=lessons_block)
            user_message = (f"Task:\n\n{instruction}\n\nYou have no direct view of the earlier sessions. "
                            "Call `session_search` to recall what matters before acting.")
            messages: list[dict[str, Any]] = [{"role": "system", "content": system_prompt},
                                              {"role": "user", "content": user_message}]
            ingest_note = ("Ingested %d trace lines into one Areev file: %d events, %d tool calls (%d errors) "
                           "over %d days; %s; trace deleted from the sandbox." % (
                               ingest.get("lines", 0), ingest.get("events", 0), ingest.get("tools", 0),
                               ingest.get("tool_errors", 0), ingest.get("days", 0),
                               ("%d lesson(s) approved" % len(lessons)) if self.GOVERNED else "no loop (passive)"))
            self.logger.info(ingest_note)
            steps.extend([Step(step_id=1, timestamp=_now_iso(), source="system", message=ingest_note),
                          Step(step_id=2, timestamp=_now_iso(), source="user", message=user_message)])

            tools_schema = [SESSION_SEARCH_TOOL, *tool_registry.openrouter_tools]
            extra_body: dict[str, Any] = {"usage": {"include": True}, "seed": seed}
            if pin:
                extra_body["provider"] = {"order": [pin], "allow_fallbacks": False}
            async def chat_with_retries():
                # StreamLake answered 429 "temporarily rate-limited upstream"
                # on one public trial; a provider's bad minute is not a
                # result. Five attempts, exponential backoff, then the
                # trial fails as it would have.
                last: Exception | None = None
                for attempt in range(5):
                    try:
                        return await client.chat.completions.create(
                            model=chat_model, messages=messages, tools=tools_schema, temperature=0,
                            extra_body=extra_body)
                    except Exception as exc:  # 429 / 5xx / socket
                        last = exc
                        status = getattr(exc, "status_code", None)
                        if status is not None and status < 500 and status != 429:
                            raise
                        await asyncio.sleep(min(2 ** attempt * 2, 30))
                raise last if last else RuntimeError("chat failed")

            for turn_idx in range(MAX_STEPS):
                async with timed_call(call_log, "chat", f"chat turn {turn_idx + 1}"):
                    resp = await chat_with_retries()
                if resp.usage:
                    total_prompt += resp.usage.prompt_tokens or 0
                    total_completion += resp.usage.completion_tokens or 0
                chat_cost_usd += usage_cost(resp)
                step_metrics = Metrics(prompt_tokens=(resp.usage.prompt_tokens if resp.usage else 0) or 0,
                                       completion_tokens=(resp.usage.completion_tokens if resp.usage else 0) or 0)
                choice = resp.choices[0].message
                tool_calls = list(choice.tool_calls or [])
                messages.append({"role": "assistant", "content": choice.content,
                                 "tool_calls": [{"id": tc.id, "type": "function",
                                                 "function": {"name": tc.function.name,
                                                              "arguments": tc.function.arguments}}
                                                for tc in tool_calls] or None})
                if not tool_calls:
                    steps.append(Step(step_id=len(steps) + 1, timestamp=_now_iso(), source="agent",
                                      model_name=chat_model, message=(choice.content or "(done)"),
                                      metrics=step_metrics))
                    break
                atif_calls: list[ToolCall] = []
                observations: list[ObservationResult] = []
                for tc in tool_calls:
                    try:
                        args = json.loads(tc.function.arguments or "{}")
                    except json.JSONDecodeError:
                        args = {"query": tc.function.arguments or ""}
                    name = tc.function.name
                    if name == "session_search":
                        async with timed_call(call_log, "search", f"session_search (turn {turn_idx + 1})"):
                            payload = await asyncio.to_thread(
                                session_search, db_path, str(args.get("query") or "").strip(),
                                int(args.get("k") or SEARCH_K))
                    elif name in tool_registry:
                        async with timed_call(call_log, "exec", f"{name} turn {turn_idx + 1}"):
                            payload = await tool_registry.call(environment, name, args,
                                                               output_char_cap=MAX_EXEC_OUTPUT_CHARS)
                    else:
                        payload = {"exit_code": 127, "stdout": "",
                                   "stderr": f"unknown tool: {name}. Available: session_search, "
                                             f"{', '.join(tool_registry.names) or '(none)'}."}
                    atif_calls.append(ToolCall(tool_call_id=tc.id, function_name=name, arguments=args))
                    observations.append(ObservationResult(source_call_id=tc.id, content=json.dumps(payload)))
                    messages.append({"role": "tool", "tool_call_id": tc.id, "content": json.dumps(payload)})
                steps.append(Step(step_id=len(steps) + 1, timestamp=_now_iso(), source="agent",
                                  model_name=chat_model, message=choice.content or "", tool_calls=atif_calls,
                                  observation=Observation(results=observations), metrics=step_metrics))
            t_end = time.monotonic()

        loop_usage = _loop_cost(usage_log)
        trajectory = Trajectory(
            schema_version=ATIF_VERSION, session_id=str(uuid.uuid4()),
            agent=Agent(name=self.name(), version=self.version() or "unknown", model_name=chat_model),
            steps=steps,
            final_metrics=FinalMetrics(total_prompt_tokens=total_prompt, total_completion_tokens=total_completion,
                                       total_steps=len(steps)),
            extra={
                "ingest": ingest, "governed": self.GOVERNED, "governance": governance, "lessons": lessons,
                "tools": tool_registry.names,
                "loop_usage_by_model": loop_usage,
                "timing_seconds": {"ingest": round(t_ingest_done - t_start, 3),
                                   "learn": round(t_learn_done - t_ingest_done, 3),
                                   "chat": round(t_end - t_learn_done, 3), "total": round(t_end - t_start, 3)},
                "cost_usd": tk.cost_usd_dict(direct_total=chat_cost_usd,
                                             breakdown={"chat_completions": round(chat_cost_usd, 6),
                                                        "loop_and_review": "see loop_usage_by_model (priced by cost.py)"}),
                "call_summary": summarize_call_log(call_log), "call_log": call_log,
            },
        )
        (self.logs_dir / "trajectory.json").write_text(json.dumps(trajectory.to_json_dict(), indent=2))
        context.n_input_tokens = total_prompt
        context.n_output_tokens = total_completion


class AreevGovernedAgent(AreevAgentBase):
    GOVERNED = True

    @staticmethod
    def name() -> str:
        return "areev-governed"


class AreevPassiveAgent(AreevAgentBase):
    GOVERNED = False

    @staticmethod
    def name() -> str:
        return "areev-passive"
