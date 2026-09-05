#!/usr/bin/env python3
"""mem0 as a PAST-Bench persistence backend — the plain-memory arm.

Same scaffold as the Areev arms (the benchmark's own chat loop), and mem0
used exactly as its README says to: `search()` the task at session start
and put what comes back in the prompt, `add()` the session's exchange at
close. Nothing is proposed, reviewed, measured or withdrawn by anything but
mem0's own extractor, and the model has no memory tools of its own — that
is mem0's design, and the benchmark's mechanism counters see it as it is:
injection when a memory existed before the episode, entries in the rendered
memory file, no explicit reads or writes.

Three modes, as in FOURWAY.md, selected by agent name:
  mem0          as installed — the "Personal Information Organizer" extractor
  mem0-domain   with `custom_instructions` saying these are operating rules
  mem0-raw      `infer=False`: every message stored verbatim

mem0's own model calls go through the openai SDK; `receipts/mem0_arm.py`'s
`meter_openai` wraps it so they land in AREEV_USAGE_LOG like every leg.
"""
from __future__ import annotations

import gc
import json
import os
import sys
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from past_bench.models.content import TextBlock
from past_bench.models.message import Message
from past_bench.runner.providers.openai_compat import OpenAICompatProvider
from past_bench.runner.self_evolve import (
    PersistenceBackend,
    _empty_artifact_summary,
    _snapshot_hermes_tree,
    _split_memory_entries,
)
from past_bench.runtime.adapters.base import RuntimeAdapter
from past_bench.runtime.protocol import StartSessionRequest, StepRequest, StepResponse, ToolCallAction

_HERE = Path(__file__).resolve().parent
_RECEIPTS = _HERE.parents[1] / "receipts"
if str(_RECEIPTS) not in sys.path:
    sys.path.insert(0, str(_RECEIPTS))

USER = "assistant"
ENTRY_DELIM = "\n§\n"
DOMAIN_INSTRUCTIONS = (
    "These messages are a work assistant's sessions with the team it works for. Extract "
    "durable operating rules: preferences, standing instructions, corrections that replace "
    "an earlier rule, reusable procedures with their steps, and constraints about tools or "
    "people. Do not store the outcome or contents of one task.")


def _now():
    return datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")


def _text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for c in content:
            if isinstance(c, str):
                parts.append(c)
            elif getattr(c, "text", None):
                parts.append(str(c.text))
            elif isinstance(getattr(c, "content", None), list):
                parts.append(_text_of(c.content))
            elif isinstance(c, dict) and c.get("text"):
                parts.append(str(c["text"]))
        return "\n".join(p for p in parts if p)
    return ""


def build_memory(state_dir: Path, mode: str):
    from mem0 import Memory
    state_dir = Path(state_dir)
    state_dir.mkdir(parents=True, exist_ok=True)
    key = os.environ.get("OPENROUTER_API_KEY", "")
    model = os.environ.get("MEM0_LLM_MODEL") or "qwen/qwen3-30b-a3b-instruct-2507"
    cfg = {
        "llm": {"provider": "openai", "config": {
            "model": model, "api_key": key,
            "openai_base_url": "https://openrouter.ai/api/v1", "temperature": 0}},
        "embedder": {"provider": "ollama", "config": {
            "model": "mxbai-embed-large", "embedding_dims": 1024}},
        "vector_store": {"provider": "qdrant", "config": {
            "collection_name": "sessions", "embedding_model_dims": 1024,
            "path": str(state_dir / "qdrant"), "on_disk": True}},
        "history_db_path": str(state_dir / "mem0_history.db"),
    }
    if mode == "domain":
        cfg["custom_instructions"] = DOMAIN_INSTRUCTIONS
    return Memory.from_config(cfg)


def with_memory(state_dir, mode, fn):
    """One mem0 handle per operation, released before the benchmark clones
    or resets the state directory (qdrant's local store holds a lock)."""
    m = build_memory(state_dir, mode)
    try:
        return fn(m)
    finally:
        try:
            m.vector_store.client.close()
        except Exception:
            pass
        del m
        gc.collect()


def _scoped(m, method, *args, **kw):
    """mem0 2.x takes the entity scope as `filters={'user_id': …}` on the
    read side (`get_all` refuses the top-level kwarg outright); 1.x took
    `user_id=`. Try the 2.x form first, fall back to 1.x."""
    fn = getattr(m, method)
    try:
        return fn(*args, filters={"user_id": USER}, **kw)
    except (TypeError, ValueError):
        return fn(*args, user_id=USER, **kw)


def entries(m) -> list[str]:
    res = _scoped(m, "get_all")
    rows = res.get("results", res) if isinstance(res, dict) else res
    out = []
    for r in rows or []:
        t = (r.get("memory") if isinstance(r, dict) else str(r)) or ""
        t = t.strip()
        if t:
            out.append(t)
    return out


def render_home(state_dir: Path, mode: str, out_dir: Path) -> int:
    out_dir = Path(out_dir)
    mem_dir = out_dir / "memories"
    if mem_dir.exists():
        for p in mem_dir.iterdir():
            p.unlink()
    if not (Path(state_dir) / "qdrant").exists():
        return 0
    rows = with_memory(state_dir, mode, entries)
    if rows:
        mem_dir.mkdir(parents=True, exist_ok=True)
        (mem_dir / "MEMORY.md").write_text(ENTRY_DELIM.join(rows), encoding="utf-8")
    return len(rows)


def _meter():
    try:
        import mem0_arm  # receipts/mem0_arm.py
        log = os.environ.get("AREEV_USAGE_LOG")
        if log and not getattr(_meter, "done", False):
            mem0_arm.meter_openai(log)
            _meter.done = True
    except Exception:
        pass


class Mem0Adapter(RuntimeAdapter):
    def __init__(self, spec, request: StartSessionRequest) -> None:
        super().__init__(spec, request)
        cfg = (request.model.extra_body or {}).get("mem0") or {}
        self.state_dir = Path(cfg["state_dir"]) if cfg.get("state_dir") else None
        self.artifacts_dir = Path(cfg["artifacts_dir"]) if cfg.get("artifacts_dir") else None
        self.persist = bool(cfg.get("persistence_enabled")) and self.state_dir is not None
        self.mode = str(cfg.get("mode") or "default")
        self.top_k = int(cfg.get("top_k") or 10)
        self.seed = int(cfg.get("seed") or 1)
        self._log: list[dict[str, Any]] = []
        self._usage = {"input_tokens": 0, "output_tokens": 0, "calls": 0}
        self._closed = False
        self._injected = 0
        body: dict[str, Any] = {"seed": self.seed}
        pin = cfg.get("provider_pin") or os.environ.get("AREEV_AGENT_PIN") or ""
        if pin:
            body["provider"] = {"order": [pin], "allow_fallbacks": False}
        self._provider = OpenAICompatProvider(
            model_id=request.model.model_id, api_key=request.model.api_key, base_url=request.model.base_url,
            extra_body=body, temperature=request.runtime_config.temperature)
        self._messages = [m.model_copy(deep=True) for m in request.initial_messages]
        self._tools = [t.model_copy(deep=True) for t in request.tools]
        if self.persist:
            _meter()
            self._inject()

    def _inject(self):
        prompt = "\n".join(_text_of(m.content) for m in self._messages if m.role == "user")
        if not (self.state_dir / "qdrant").exists() or not prompt.strip():
            return

        def go(m):
            res = _scoped(m, "search", prompt[:2000], top_k=self.top_k)
            rows = res.get("results", res) if isinstance(res, dict) else res
            return [r.get("memory", "").strip() for r in rows or [] if isinstance(r, dict) and r.get("memory")]

        mems = with_memory(self.state_dir, self.mode, go)
        if not mems:
            return
        section = ("\n## Relevant memories\nWhat you have stored from earlier sessions, most relevant first:\n"
                   + "\n".join("- " + x for x in mems))
        self._injected = len(section)
        for m in self._messages:
            if m.role == "system" and m.content and m.content[0].type == "text":
                m.content[0].text = m.content[0].text + section
                return
        self._messages.insert(0, Message(role="system", content=[TextBlock(text=section)]))

    def step(self, request: StepRequest) -> StepResponse:
        if request.tool_results:
            blocks = [b.model_copy(deep=True) for b in request.tool_results]
            for b in blocks:
                self._log.append({"role": "tool", "timestamp": _now(), "content": _text_of(b.content)[:2000]})
            self._messages.append(Message(role="user", content=blocks))
        started = time.monotonic()
        assistant, usage = self._provider.chat(self._messages, tools=self._tools)
        self._messages.append(assistant)
        self._usage["input_tokens"] += int(getattr(usage, "input_tokens", 0) or 0)
        self._usage["output_tokens"] += int(getattr(usage, "output_tokens", 0) or 0)
        self._usage["calls"] += 1
        entry = {"role": "assistant", "timestamp": _now(), "content": assistant.text or ""}
        calls = [b for b in assistant.content if b.type == "tool_use"]
        if calls:
            entry["tool_calls"] = [{"id": b.id, "type": "function",
                                    "function": {"name": b.name, "arguments": json.dumps(b.input or {})}}
                                   for b in calls]
        self._log.append(entry)
        elapsed = time.monotonic() - started
        if calls:
            return StepResponse(status="acting", assistant_message=assistant, usage=usage,
                                tool_calls=[ToolCallAction(tool_use_id=b.id, name=b.name, arguments=dict(b.input or {}))
                                            for b in calls], model_time_s=elapsed)
        return StepResponse(status="finished", assistant_message=assistant, usage=usage,
                            final_output=assistant.text, model_time_s=elapsed)

    def close(self, reason: str = "") -> None:
        if self._closed:
            return
        self._closed = True
        if self.artifacts_dir is None:
            return
        self.artifacts_dir.mkdir(parents=True, exist_ok=True)
        added = None
        if self.persist:
            convo = []
            for m in self._messages:
                if m.role == "system":
                    continue
                text = _text_of(m.content)
                if m.role == "assistant":
                    text = text or " ".join("[called %s]" % b.name for b in m.content if b.type == "tool_use")
                if text.strip():
                    convo.append({"role": "user" if m.role == "user" else "assistant", "content": text[:6000]})
            if convo:
                try:
                    added = with_memory(self.state_dir, self.mode,
                                        lambda mm: mm.add(convo, user_id=USER, infer=(self.mode != "raw")))
                    added = json.dumps(added, default=str)[:600]
                except Exception as exc:
                    # A memory arm whose writes fail is not a result: the
                    # first smoke scored mem0 at Δ 0.0 with every add()
                    # refused for a missing client library, and only the
                    # ledger said so. Fail the family-run instead.
                    added = "error: %s" % str(exc)[:300]
                    print("[mem0] add() failed: %s" % str(exc)[:300], file=sys.stderr, flush=True)
                    self._write_ledger(added, 0)
                    raise RuntimeError("mem0 add() failed; the arm's write path is broken: %s" % str(exc)[:200])
        (self.artifacts_dir / "session_current.json").write_text(json.dumps(
            {"session_id": self.request.session_id, "task_id": self.request.task_id, "finished_at": _now(),
             "persistence_enabled": self.persist, "mode": self.mode, "messages": self._log},
            ensure_ascii=False, indent=1), encoding="utf-8")
        n = render_home(self.state_dir, self.mode, self.artifacts_dir) if self.persist else 0
        self._write_ledger(added, n)

    def _write_ledger(self, added, n):
        (self.artifacts_dir / "mem0_ledger.json").write_text(json.dumps(
            {"usage": self._usage, "injected_chars": self._injected, "entries_after": n, "add_result": added},
            indent=1), encoding="utf-8")


class Mem0PersistenceBackend(PersistenceBackend):
    state_root_name = "mem0_state"

    def __init__(self, agent_name: str) -> None:
        self.agent_name = agent_name
        self.mode = {"mem0": "default", "mem0-domain": "domain", "mem0-raw": "raw"}.get(agent_name, "default")

    def materialize_inputs(self, *, state_root, initial_home_fixture_dir, preseed_artifacts_dir) -> None:
        state_root = Path(state_root)
        state_root.mkdir(parents=True, exist_ok=True)
        texts = []
        for src in (initial_home_fixture_dir, preseed_artifacts_dir):
            if not src or not Path(src).exists():
                continue
            src = Path(src)
            for name in ("MEMORY.md", "USER.md"):
                p = src / "memories" / name
                if p.exists():
                    texts += _split_memory_entries(p.read_text(encoding="utf-8"))
            for skill_md in sorted((src / "skills").rglob("SKILL.md")) if (src / "skills").exists() else []:
                texts.append("Skill %s:\n%s" % (skill_md.parent.name, skill_md.read_text(encoding="utf-8")[:3000]))
            for sess in sorted((src / "sessions").rglob("*.json")) if (src / "sessions").exists() else []:
                try:
                    data = json.loads(sess.read_text(encoding="utf-8"))
                except json.JSONDecodeError:
                    continue
                msgs = data.get("messages") if isinstance(data, dict) else data
                for m in msgs or []:
                    if isinstance(m, dict) and isinstance(m.get("content"), str) and m["content"].strip():
                        texts.append("%s: %s" % (m.get("role", "user"), m["content"][:2000]))
        if not texts:
            return
        _meter()
        have = set(with_memory(state_root, self.mode, entries)) if (state_root / "qdrant").exists() else set()
        new = [t for t in texts if t not in have]
        if new:
            with_memory(state_root, self.mode, lambda m: [m.add(t, user_id=USER, infer=False) for t in new])

    def build_extra_body(self, *, state_root, artifacts_dir, persistence_enabled, sequence, family_id,
                         review_wait_s, tool_config) -> dict[str, Any]:
        del sequence, review_wait_s, tool_config
        return {"mem0": {"state_dir": str(state_root), "artifacts_dir": str(artifacts_dir),
                         "persistence_enabled": bool(persistence_enabled), "mode": self.mode,
                         "family_id": family_id, "seed": int(os.environ.get("SEED", "1") or 1),
                         "provider_pin": os.environ.get("AREEV_AGENT_PIN", ""),
                         "top_k": int(os.environ.get("MEM0_TOP_K", "10") or 10)}}

    def snapshot_before(self, state_root, *, include_contents: bool = False) -> dict[str, Any]:
        state_root = Path(state_root)
        if not (state_root / "qdrant").exists():
            return _empty_artifact_summary(state_root)
        rendered = state_root / "rendered"
        render_home(state_root, self.mode, rendered)
        return _snapshot_hermes_tree(rendered, include_contents=include_contents)

    def snapshot_after(self, artifacts_dir) -> dict[str, Any]:
        artifacts_dir = Path(artifacts_dir)
        if not artifacts_dir.exists():
            return _empty_artifact_summary(artifacts_dir)
        session = artifacts_dir / "session_current.json"
        return _snapshot_hermes_tree(artifacts_dir, session_path=session if session.exists() else None,
                                     include_contents=True)


def make_backend(agent_name: str) -> Mem0PersistenceBackend:
    return Mem0PersistenceBackend(agent_name)
