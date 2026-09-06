#!/usr/bin/env python3
"""Areev as a PAST-Bench persistence backend and runtime adapter.

PAST-Bench (Gen-Verse/PAST-Bench, arXiv 2608.04003) asks whether an agent
improves across fresh sessions by reusing what it persisted, and grades the
*mechanism* as well as the outcome. It ships adapters for Hermes, nanobot,
ZeroClaw and Agent-Zero. This module adds two agents that share ONE scaffold
— the benchmark's own OpenAI-compatible chat loop — and differ only in what
carries forward between episodes:

  areev-passive   every turn, tool call and saved entry is a grain in one
                  Areev file; the live notes, profile and skills are rendered
                  into the system prompt at session start; the model can
                  add/replace/remove entries and manage skills through tools
                  named exactly as Hermes names them, so the benchmark's
                  mechanism counters see them. Nothing is proposed, reviewed
                  or measured by anything but the model itself.
  areev-governed  the same, plus a loop pass at every episode close:
                  ANALYZE -> DISCOVER -> GROUND -> VERIFY, then a fixed-rubric
                  reviewer approves or refuses each proposal with a BECAUSE,
                  approved changes apply under supersession, and the previous
                  episode's graded score is journaled as an evalset run so
                  `outcome_review` can propose a revert of an applied change
                  that hurt — which the harness applies, because a measured
                  regression is the gate's own verdict.

The benchmark is not modified: `run.py` registers the adapter and the backend
and hands control to `past_bench.cli.main`.

Artifact contract. PAST-Bench scores the mechanism from a Hermes-shaped
snapshot — `memories/MEMORY.md` entries separated by "\\n§\\n", `memories/USER.md`,
`skills/<name>/SKILL.md`, and a `session_current.json` whose assistant
`tool_calls` are counted by name (`memory` with action add/replace/remove =
writes, get/read/list/search/view = reads; `skill_manage`; `session_search`;
`skill_view`; `skills_list`). The backend renders the Areev file into that
shape at snapshot time and the adapter writes the session file at close, so
one scorer grades every arm. Memory *injection* is inferred by the benchmark
from "a memory file existed before the episode", which is why an empty memory
renders no file.
"""
from __future__ import annotations

import gc
import json
import os
import re
import shutil
import time
import uuid
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from past_bench.models.content import TextBlock, ToolResultBlock
from past_bench.models.message import Message
from past_bench.models.tool import ToolSpec
from past_bench.runner.providers.openai_compat import OpenAICompatProvider
from past_bench.runner.self_evolve import (
    PersistenceBackend,
    _empty_artifact_summary,
    _snapshot_hermes_tree,
    _split_memory_entries,
)
from past_bench.runtime.adapters.base import RuntimeAdapter
from past_bench.runtime.protocol import StartSessionRequest, StepRequest, StepResponse, ToolCallAction

# Not `agent:<x>`: the loop's all-namespace evidence scan deliberately skips
# every `agent:*` namespace as governance metadata (`read_user_type` in the
# substrate adapter), so a memory living there is invisible to DISCOVER —
# the first smoke's loop passes saw zero evidence for exactly this reason.
NS = "desk:persist"
HARNESS_NS = "agent:harness"
ACTOR_AGENT = "agent:assistant"
ACTOR_RUNNER = "loop:runner"
ACTOR_REVIEWER = "user:reviewer"
DB_NAME = "memory.db"
NOTE_SUBJECT = "assistant"
USER_SUBJECT = "user"
ENTRY_DELIM = "\n§\n"
CAP = 500
MEM_TOOL_NAMES = {"memory", "skill_manage", "skills_list", "skill_view", "session_search"}
MAX_INTERNAL_ROUNDS = 8
MAX_INJECT_CHARS = 12_000
RETIRED = "retired"


# ---------------------------------------------------------------- the file

def with_memory(path, actor, fn):
    """Open as `actor`, run `fn(db)`, and guarantee the handle is released —
    the embedded backend is single-writer per process, and a handle that
    outlives its frame makes the next open fail (STO-E002)."""
    import areev
    db = areev.Areev(str(path), ns=NS, actor=actor)
    try:
        return fn(db)
    finally:
        del db
        gc.collect()


def _grains(db, noun, where=""):
    payload = json.loads(db.cal(
        'RECALL %s WHERE namespace = "%s"%s LIMIT %d FORMAT json' % (noun, NS, where, CAP)))
    grains = payload.get("grains", payload if isinstance(payload, list) else [])
    if len(grains) >= CAP:
        raise RuntimeError("memory scan hit the %d-grain cap; narrow the query" % CAP)
    return grains


def _fields(g):
    return g.get("fields") or {}


def live_facts(db):
    return [g for g in _grains(db, "facts") if _fields(g).get("relation") not in (RETIRED, "mg:eval_run")]


def live_notes(db):
    """What MEMORY.md renders: the assistant's own notes and every approved
    lesson, whatever subject the proposer named — plus any other durable
    fact the loop stored, written as `subject relation: object`."""
    out = []
    for g in live_facts(db):
        f = _fields(g)
        rel, subj, obj = f.get("relation") or "", f.get("subject") or "", f.get("object") or ""
        if not obj or subj.startswith("session:") or subj.startswith("evalset:"):
            continue
        if rel == "profile":
            continue
        if rel in ("note", "lesson"):
            out.append((g.get("hash"), obj))
        else:
            out.append((g.get("hash"), "%s %s: %s" % (subj, rel, obj)))
    return out


def live_profile(db):
    return [(g.get("hash"), _fields(g).get("object") or "") for g in live_facts(db)
            if _fields(g).get("relation") == "profile" and _fields(g).get("object")]


def live_skills(db):
    out = {}
    for g in _grains(db, "skills"):
        f = _fields(g)
        name = f.get("name") or ""
        if name and f.get("description") != RETIRED:
            out[name] = (g.get("hash"), f)
    return out


def session_titles(db):
    """One line per prior session, most recent first: its title (the first
    Event's bracketed title when the session was seeded) or its first words."""
    rows = _grains(db, "events")
    first: dict[str, str] = {}
    order: list[str] = []
    for g in rows:
        f = _fields(g)
        sid = f.get("session_id") or ""
        if not sid or sid in first:
            continue
        text = (f.get("content") or "").strip()
        m = re.match(r"^\[(.{3,120}?)\]\s", text)
        first[sid] = m.group(1) if m else text[:90].replace("\n", " ")
        order.append(sid)
    return [first[s] for s in reversed(order)]


def render_home(db_path, out_dir):
    """Render the live memory into the Hermes-shaped tree the benchmark
    snapshots. Empty stores render no file — the benchmark reads "a memory
    file exists" as "memory was injected"."""
    out_dir = Path(out_dir)
    for sub in ("memories", "skills"):
        d = out_dir / sub
        if d.exists():
            shutil.rmtree(d)
    if not Path(db_path).exists():
        return {"notes": 0, "profile": 0, "skills": 0}

    def go(db):
        return live_notes(db), live_profile(db), live_skills(db)

    notes, profile, skills = with_memory(db_path, ACTOR_AGENT, go)
    mem_dir = out_dir / "memories"
    if notes:
        mem_dir.mkdir(parents=True, exist_ok=True)
        (mem_dir / "MEMORY.md").write_text(ENTRY_DELIM.join(t for _, t in notes), encoding="utf-8")
    if profile:
        mem_dir.mkdir(parents=True, exist_ok=True)
        (mem_dir / "USER.md").write_text(ENTRY_DELIM.join(t for _, t in profile), encoding="utf-8")
    for name, (_, f) in skills.items():
        d = out_dir / "skills" / _safe_name(name)
        d.mkdir(parents=True, exist_ok=True)
        (d / "SKILL.md").write_text(_skill_markdown(f), encoding="utf-8")
    return {"notes": len(notes), "profile": len(profile), "skills": len(skills)}


def _skill_markdown(f):
    body = f.get("instructions") or ""
    head = "---\nname: %s\ndescription: %s\n" % (f.get("name", ""), f.get("description", ""))
    if f.get("when_to_use"):
        head += "when_to_use: %s\n" % f["when_to_use"]
    return head + "---\n\n" + body


def _safe_name(name):
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", name).strip("_") or "skill"


def import_hermes_home(db_path, src_dir):
    """Seed a memory from a Hermes-shaped fixture (what the benchmark hands
    every framework at family start and before pre-seeded episodes):
    MEMORY.md entries become notes, USER.md entries the profile, each
    skills/<name>/SKILL.md a Skill grain, and sessions/*.json prior-session
    Events. Idempotent: an entry already live is not added twice."""
    src_dir = Path(src_dir)
    mem_text = _read(src_dir / "memories" / "MEMORY.md")
    user_text = _read(src_dir / "memories" / "USER.md")
    skills = {}
    skills_dir = src_dir / "skills"
    if skills_dir.exists():
        for skill_md in sorted(skills_dir.rglob("SKILL.md")):
            skills[skill_md.parent.name] = _read(skill_md)
    sessions = []
    sessions_dir = src_dir / "sessions"
    if sessions_dir.exists():
        for p in sorted(sessions_dir.rglob("*.json")):
            try:
                sessions.append((p.stem, json.loads(_read(p))))
            except json.JSONDecodeError:
                continue
    # The benchmark pre-seeds prior sessions as ONE file, `session_seed.json`
    # ({"sessions": [{"id", "title", "messages": [...]}, …]}), which the
    # first version never read: the six information-gathering families
    # whose expected signal is `session_search` ran with no prior session
    # to search (run 1, defect #13). Each seeded session becomes its own
    # thread of Events, its title on the first one.
    seed = src_dir / "session_seed.json"
    if seed.exists():
        try:
            payload = json.loads(_read(seed)) or {}
        except json.JSONDecodeError:
            payload = {}
        for i, s in enumerate(payload.get("sessions") or [], start=1):
            if not isinstance(s, dict):
                continue
            sid = re.sub(r"[^A-Za-z0-9_.-]+", "_", str(s.get("id") or "seed_%03d" % i)).strip("_")
            msgs = list(s.get("messages") or [])
            if s.get("title") and msgs:
                first = dict(msgs[0])
                first["content"] = "[%s] %s" % (s["title"], first.get("content") or "")
                msgs[0] = first
            sessions.append((sid, {"messages": msgs}))
    if not (mem_text or user_text or skills or sessions):
        return {"notes": 0, "profile": 0, "skills": 0, "events": 0}

    def go(db):
        have_notes = {t for _, t in live_notes(db)}
        have_profile = {t for _, t in live_profile(db)}
        have_skills = live_skills(db)
        n = {"notes": 0, "profile": 0, "skills": 0, "events": 0}
        for entry in _split_memory_entries(mem_text):
            if entry not in have_notes:
                db.add("fact", json.dumps({"subject": NOTE_SUBJECT, "relation": "note", "object": entry}), ns=NS)
                n["notes"] += 1
        for entry in _split_memory_entries(user_text):
            if entry not in have_profile:
                db.add("fact", json.dumps({"subject": USER_SUBJECT, "relation": "profile", "object": entry}), ns=NS)
                n["profile"] += 1
        for name, content in skills.items():
            desc, body, when = _parse_skill_markdown(content)
            fields = {"name": name, "description": desc or name, "instructions": body}
            if when:
                fields["when_to_use"] = when
            if name in have_skills:
                old_hash, old = have_skills[name]
                if (old.get("instructions") or "") != body:
                    db.supersede(old_hash, "skill", json.dumps(fields), ns=NS)
                    n["skills"] += 1
            else:
                db.add("skill", json.dumps(fields), ns=NS)
                n["skills"] += 1
        for sid, data in sessions:
            msgs = data.get("messages") if isinstance(data, dict) else data
            if not isinstance(msgs, list):
                continue
            for m in msgs:
                if not isinstance(m, dict):
                    continue
                role = m.get("role") or "user"
                text = _text_of(m.get("content"))
                if not text or role not in ("user", "assistant", "system", "tool"):
                    continue
                db.add("event", json.dumps({"content": text[:4000], "role": role,
                                            "session_id": "seed:" + sid,
                                            "subject": "session:seed:" + sid}), ns=NS)
                n["events"] += 1
        return n

    return with_memory(db_path, ACTOR_AGENT, go)


def _parse_skill_markdown(content):
    desc, when, body = "", "", content
    m = re.match(r"^---\n(.*?)\n---\n?", content, re.S)
    if m:
        for line in m.group(1).splitlines():
            if line.startswith("description:"):
                desc = line.split(":", 1)[1].strip()
            elif line.startswith("when_to_use:"):
                when = line.split(":", 1)[1].strip()
        body = content[m.end():].lstrip("\n")
    if not desc:
        first = next((ln.strip("# ").strip() for ln in body.splitlines() if ln.strip()), "")
        desc = first[:160]
    return desc, body, when


def _read(p):
    try:
        return Path(p).read_text(encoding="utf-8")
    except (FileNotFoundError, IsADirectoryError):
        return ""


def _text_of(content):
    """Text from a content value: a string, a list of strings, of dicts with
    `text`, or of the benchmark's pydantic blocks (TextBlock, ToolResultBlock
    with nested TextBlocks). The first version handled dicts only, so the
    user's instructions — pydantic TextBlocks — were never recorded and the
    loop had no human Observation to reason from."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for c in content:
            if isinstance(c, str):
                parts.append(c)
            elif isinstance(c, dict):
                if c.get("text"):
                    parts.append(str(c["text"]))
                elif isinstance(c.get("content"), list):
                    parts.append(_text_of(c["content"]))
            elif getattr(c, "text", None):
                parts.append(str(c.text))
            elif isinstance(getattr(c, "content", None), list):
                parts.append(_text_of(c.content))
        return "\n".join(p for p in parts if p)
    return ""


def _now():
    return datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")


# --------------------------------------------------------------- the tools

def persistence_tools(tool_config):
    """The persistence surface the model sees, named as Hermes names it so
    the benchmark's counters read our calls. Gated per episode exactly as
    the benchmark gates Hermes (`resolve_episode_tool_config`)."""
    tools = []
    if tool_config.get("memory_enabled") or tool_config.get("user_profile_enabled"):
        tools.append(ToolSpec(
            name="memory",
            description=("Your persistent notes. Later sessions start with an empty context and see "
                         "ONLY what is saved here, so save durable preferences, standing instructions, "
                         "corrections and constraints as you learn them, replace an entry when a rule "
                         "changes, and remove one that no longer holds. 'memory' holds working notes, "
                         "'user' holds facts about the person you work for. 'search' looks entries up."),
            input_schema={
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "replace", "remove", "search"],
                               "description": "The action to perform."},
                    "target": {"type": "string", "enum": ["memory", "user"],
                               "description": "Which store: 'memory' for notes, 'user' for the user profile."},
                    "content": {"type": "string", "description": "Entry text. Required for add and replace."},
                    "old_text": {"type": "string",
                                 "description": "Short unique substring identifying the entry to replace or remove."},
                    "query": {"type": "string", "description": "For search: what to look for."},
                },
                "required": ["action"],
            }))
    if tool_config.get("skills_enabled"):
        tools.append(ToolSpec(
            name="skill_manage",
            description=("Save or update a reusable procedure (a skill) — the ordered steps, the tools "
                         "each step uses, and pitfalls — so a later session can repeat the workflow "
                         "without rediscovering it. Patch the closest existing skill when a workflow "
                         "changes instead of creating a near-duplicate."),
            input_schema={
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "edit", "patch", "delete"]},
                    "name": {"type": "string", "description": "Skill name, e.g. weekly_expense_export."},
                    "content": {"type": "string",
                                "description": "For create/edit: the full skill text — a one-line description first, then the steps."},
                    "old_text": {"type": "string", "description": "For patch: the text to replace."},
                    "new_text": {"type": "string", "description": "For patch: the replacement."},
                },
                "required": ["action", "name"],
            }))
        tools.append(ToolSpec(name="skills_list", description="List the saved skills with their descriptions.",
                              input_schema={"type": "object", "properties": {}}))
        tools.append(ToolSpec(name="skill_view", description="Read a saved skill's full steps.",
                              input_schema={"type": "object", "properties": {"name": {"type": "string"}},
                                            "required": ["name"]}))
    if tool_config.get("session_search_enabled"):
        tools.append(ToolSpec(
            name="session_search",
            description=("Search your record of earlier sessions — handoffs, decisions, exceptions, "
                         "waivers, lookups you did before. This is your only access to them. USE IT "
                         "PROACTIVELY, before acting, whenever the task mentions a prior handoff, an "
                         "earlier decision, a policy note, 'as before', 'last time', or an identifier you "
                         "do not have in front of you. With a query it returns the matching records; "
                         "with no query it lists the most recent sessions."),
            input_schema={"type": "object", "properties": {"query": {"type": "string"}}}))
    return tools


# ------------------------------------------------------------- the adapter

class AreevAdapter(RuntimeAdapter):
    """One PAST-Bench session on the benchmark's own chat loop, with an Areev
    file as the only thing that outlives it."""

    def __init__(self, spec, request: StartSessionRequest) -> None:
        super().__init__(spec, request)
        cfg = (request.model.extra_body or {}).get("areev") or {}
        self.cfg = cfg
        self.db_path = Path(cfg["db_path"]) if cfg.get("db_path") else None
        self.artifacts_dir = Path(cfg["artifacts_dir"]) if cfg.get("artifacts_dir") else None
        self.persist = bool(cfg.get("persistence_enabled")) and self.db_path is not None
        self.governed = bool(cfg.get("governed"))
        self.tool_config = dict(cfg.get("tool_config") or {})
        self.family_id = str(cfg.get("family_id") or "")
        self.seed = int(cfg.get("seed") or 1)
        self.session_id = request.session_id
        self.task_id = request.task_id
        self._started = _now()
        self._closed = False
        self._log: list[dict[str, Any]] = []
        self._pending: dict[str, tuple[str, dict]] = {}
        self._held: list[ToolResultBlock] = []
        self._usage = {"input_tokens": 0, "output_tokens": 0, "calls": 0}
        self._ledger: dict[str, Any] = {"governance": None, "tool_calls": 0, "memory_ops": []}

        body: dict[str, Any] = {"seed": self.seed}
        pin = cfg.get("provider_pin") or os.environ.get("AREEV_AGENT_PIN") or ""
        if pin:
            body["provider"] = {"order": [pin], "allow_fallbacks": False}
        self._provider = OpenAICompatProvider(
            model_id=request.model.model_id,
            api_key=request.model.api_key,
            base_url=request.model.base_url,
            extra_body=body,
            temperature=request.runtime_config.temperature,
        )
        self._messages = [m.model_copy(deep=True) for m in request.initial_messages]
        self._task_tools = [t.model_copy(deep=True) for t in request.tools]
        self._mem_tools = persistence_tools(self.tool_config) if self.persist else []
        if self.persist:
            self.db_path.parent.mkdir(parents=True, exist_ok=True)
            if self.governed:
                self._journal_previous_outcome()
            self._inject_memory()
            self._record_prompt()

    # -- session start -----------------------------------------------------

    def _inject_memory(self):
        def go(db):
            return live_notes(db), live_profile(db), live_skills(db)
        notes, profile, skills = with_memory(self.db_path, ACTOR_AGENT, go) if self.db_path.exists() else ([], [], {})
        lines = ["", "## Persistent memory",
                 "Everything below was saved in earlier sessions; apply it without being asked. "
                 "This session's context is discarded at the end — only what you save through the "
                 "memory and skill tools carries forward."]
        if self.tool_config.get("memory_enabled") or self.tool_config.get("user_profile_enabled"):
            lines.append("### Notes")
            lines += ["- " + t for _, t in notes] or ["- (none yet)"]
            lines.append("### User profile")
            lines += ["- " + t for _, t in profile] or ["- (none yet)"]
        if self.tool_config.get("skills_enabled"):
            lines.append("### Skills (call skill_view for the steps)")
            lines += ["- %s — %s" % (n, f.get("description", "")) for n, (_, f) in skills.items()] or ["- (none yet)"]
        if self.tool_config.get("session_search_enabled"):
            # What Hermes gets from a zero-cost `session_search` with no
            # query — the recent sessions' titles — and what the first full
            # run showed the model needs: told only that sessions were
            # "searchable", it never searched once in the three families
            # whose answer lived in one (defect #16). The memory knows its
            # sessions; list them, most recent first.
            titles = with_memory(self.db_path, ACTOR_AGENT, session_titles) if self.db_path.exists() else []
            lines.append("### Earlier sessions (%d) — records this task may depend on; call session_search "
                         "BEFORE acting when the task refers to anything from before" % len(titles))
            lines += ["- " + t for t in titles[:30]] or ["- (none yet)"]
            if len(titles) > 30:
                lines.append("- … and %d more; session_search finds them by topic" % (len(titles) - 30))
        section = "\n".join(lines)
        if len(section) > MAX_INJECT_CHARS:
            section = section[:MAX_INJECT_CHARS] + "\n- (memory truncated to budget)"
        self._injected_chars = len(section)
        for m in self._messages:
            if m.role == "system" and m.content and m.content[0].type == "text":
                m.content[0].text = m.content[0].text + "\n" + section
                return
        self._messages.insert(0, Message(role="system", content=[TextBlock(text=section)]))

    def _record_prompt(self):
        text = "\n".join(_text_of(m.content) for m in self._messages if m.role == "user")
        if not text.strip():
            return

        # The framing of the evidence chooses the audience of the lesson (the
        # receipts programme learned this the hard way): a bare task text
        # reads as one job, an instruction attributed to a person reads as a
        # standing rule. The Observation says who said it and to whom.
        framed = "The user instructed the assistant, in a new session:\n%s" % text[:4000]

        def go(db):
            db.add("event", json.dumps({"content": text[:8000], "role": "user", "session_id": self.session_id,
                                        "subject": "session:" + self.session_id}), ns=NS)
            db.add("observation", json.dumps({"content": framed, "observer_id": "user",
                                              "observer_type": "human", "subject": NOTE_SUBJECT}), ns=NS)
        with_memory(self.db_path, ACTOR_AGENT, go)

    def _journal_previous_outcome(self):
        """The previous episode's graded score, as an evalset run under this
        family — the number `outcome_review` measures an applied change
        against. Read from the sibling episode directory the benchmark
        graded before starting this one; absent on the first episode."""
        if not self.artifacts_dir:
            return
        episode_dir = self.artifacts_dir.parent
        variant_dir = episode_dir.parent
        if not variant_dir.exists():
            return
        try:
            mine = int(episode_dir.name.split("_", 1)[0])
        except ValueError:
            return
        prev = None
        for d in sorted(variant_dir.iterdir()):
            try:
                idx = int(d.name.split("_", 1)[0])
            except ValueError:
                continue
            if idx < mine and d.is_dir() and (prev is None or idx > int(prev.name.split("_", 1)[0])):
                prev = d
        if prev is None:
            return
        score = None
        for trace in sorted(prev.glob("*.jsonl")):
            for line in _read(trace).splitlines()[::-1]:
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(row, dict) and "task_score" in row:
                    score = {"task_score": float(row["task_score"]), "passed": bool(row.get("passed")),
                             "episode": prev.name, "n": 1}
                    break
            if score:
                break
        if not score or not self.db_path.exists():
            return

        def go(db):
            db.add("fact", json.dumps({"subject": "evalset:" + (self.family_id or "family"),
                                       "relation": "mg:eval_run", "object": json.dumps(score)}), ns=HARNESS_NS)
        with_memory(self.db_path, ACTOR_RUNNER, go)
        self._ledger["journaled_outcome"] = score

    # -- the loop the benchmark drives ---------------------------------------

    def step(self, request: StepRequest) -> StepResponse:
        if request.tool_results:
            blocks = [b.model_copy(deep=True) for b in request.tool_results]
            for b in blocks:
                name, args = self._pending.pop(b.tool_use_id, ("?", {}))
                self._record_tool(name, args, b)
            self._messages.append(Message(role="user", content=self._held + blocks))
            self._held = []
        started = time.monotonic()
        rounds = 0
        while True:
            assistant, usage = self._provider.chat(self._messages, tools=self._task_tools + self._mem_tools)
            self._messages.append(assistant)
            self._usage["input_tokens"] += int(getattr(usage, "input_tokens", 0) or 0)
            self._usage["output_tokens"] += int(getattr(usage, "output_tokens", 0) or 0)
            self._usage["calls"] += 1
            self._log_assistant(assistant)
            calls = [b for b in assistant.content if b.type == "tool_use"]
            mem_calls = [b for b in calls if b.name in MEM_TOOL_NAMES]
            task_calls = [b for b in calls if b.name not in MEM_TOOL_NAMES]
            results = [self._run_mem_tool(b) for b in mem_calls]
            if mem_calls and not task_calls and rounds < MAX_INTERNAL_ROUNDS:
                self._messages.append(Message(role="user", content=results))
                rounds += 1
                continue
            self._held = results
            for b in task_calls:
                self._pending[b.id] = (b.name, dict(b.input or {}))
            elapsed = time.monotonic() - started
            if task_calls:
                return StepResponse(
                    status="acting", assistant_message=assistant, usage=usage,
                    tool_calls=[ToolCallAction(tool_use_id=b.id, name=b.name, arguments=dict(b.input or {}))
                                for b in task_calls],
                    model_time_s=elapsed)
            if self._held:
                # memory-only calls past the internal round cap: answer them and finish
                self._messages.append(Message(role="user", content=self._held))
                self._held = []
            self._flush_nudge()
            return StepResponse(status="finished", assistant_message=assistant, usage=usage,
                                final_output=assistant.text, model_time_s=elapsed)

    def _flush_nudge(self):
        """The save step, asked for once at session end — the counterpart of
        Hermes's memory-flush and skill-creation nudges, which the benchmark
        configures for it per family. Without it the model finished PC01's
        learn episodes with the procedure demonstrated and nothing saved
        (`skills_list` calls only). One extra model call; any memory or skill
        writes it makes are executed; the task's final answer is unchanged."""
        if not self.persist or not self._mem_tools or getattr(self, "_nudged", False):
            return
        self._nudged = True
        if any(op.get("ok") and op.get("action") in ("add", "replace", "create", "edit", "patch")
               for op in self._ledger["memory_ops"]):
            return  # it already saved something this session
        nudge = ("Before this session ends: is there anything from it that a later session must know "
                 "without being told — a preference, a standing instruction, a correction, or a reusable "
                 "procedure with its steps? If so, save it now with the memory or skill tools (one entry "
                 "per rule, the exact steps for a procedure). If nothing is durable, reply 'nothing to save'.")
        self._messages.append(Message(role="user", content=[TextBlock(text=nudge)]))
        try:
            assistant, usage = self._provider.chat(self._messages, tools=self._mem_tools)
        except Exception as exc:
            self._ledger["nudge"] = {"error": str(exc)[:200]}
            return
        self._messages.append(assistant)
        self._usage["input_tokens"] += int(getattr(usage, "input_tokens", 0) or 0)
        self._usage["output_tokens"] += int(getattr(usage, "output_tokens", 0) or 0)
        self._usage["calls"] += 1
        self._log_assistant(assistant)
        calls = [b for b in assistant.content if b.type == "tool_use" and b.name in MEM_TOOL_NAMES]
        results = [self._run_mem_tool(b) for b in calls]
        if results:
            self._messages.append(Message(role="user", content=results))
        self._ledger["nudge"] = {"saved": len([r for r in results if not r.is_error]),
                                 "reply": (assistant.text or "")[:200]}

    def _log_assistant(self, assistant):
        entry = {"role": "assistant", "timestamp": _now(), "content": assistant.text or ""}
        calls = []
        for b in assistant.content:
            if b.type == "tool_use":
                calls.append({"id": b.id, "type": "function",
                              "function": {"name": b.name, "arguments": json.dumps(b.input or {})}})
        if calls:
            entry["tool_calls"] = calls
        self._log.append(entry)
        if self.persist and (assistant.text or "").strip():
            text = assistant.text

            def go(db):
                db.add("event", json.dumps({"content": text[:8000], "role": "assistant",
                                            "session_id": self.session_id,
                                            "subject": "session:" + self.session_id}), ns=NS)
            with_memory(self.db_path, ACTOR_AGENT, go)

    def _record_tool(self, name, args, result_block):
        text = _text_of([c.model_dump() for c in result_block.content])
        self._log.append({"role": "tool", "timestamp": _now(), "name": name, "content": text[:2000]})
        self._ledger["tool_calls"] += 1
        if not self.persist:
            return
        is_error = bool(result_block.is_error)

        def go(db):
            db.record_tool_call(name, text[:8000], is_error, thread=self.session_id,
                                call_id=result_block.tool_use_id, input=json.dumps(args))
        with_memory(self.db_path, ACTOR_AGENT, go)

    # -- persistence tools, executed in-process --------------------------------

    def _run_mem_tool(self, block) -> ToolResultBlock:
        args = dict(block.input or {})
        try:
            out = with_memory(self.db_path, ACTOR_AGENT, lambda db: self._dispatch(db, block.name, args))
            self._ledger["memory_ops"].append({"tool": block.name, "action": args.get("action"), "ok": True})
            return ToolResultBlock(tool_use_id=block.id, content=[TextBlock(text=out)])
        except Exception as exc:  # the model sees the refusal, the ledger keeps it
            self._ledger["memory_ops"].append({"tool": block.name, "action": args.get("action"),
                                               "ok": False, "error": str(exc)[:200]})
            return ToolResultBlock(tool_use_id=block.id, is_error=True,
                                   content=[TextBlock(text="error: %s" % str(exc)[:300])])

    def _dispatch(self, db, name, args):
        if name == "memory":
            return self._memory(db, args)
        if name == "skill_manage":
            return self._skill_manage(db, args)
        if name == "skills_list":
            skills = live_skills(db)
            return "\n".join("- %s — %s" % (n, f.get("description", "")) for n, (_, f) in skills.items()) or "(no skills saved)"
        if name == "skill_view":
            skills = live_skills(db)
            hit = skills.get(str(args.get("name") or ""))
            if not hit:
                return "no skill named %r; saved: %s" % (args.get("name"), ", ".join(skills) or "(none)")
            return _skill_markdown(hit[1])
        if name == "session_search":
            return self._session_search(db, str(args.get("query") or ""))
        raise ValueError("unknown persistence tool %s" % name)

    def _memory(self, db, args):
        action = str(args.get("action") or "").strip()
        target = str(args.get("target") or "memory").strip()
        subject, relation = (USER_SUBJECT, "profile") if target == "user" else (NOTE_SUBJECT, "note")
        if action == "add":
            content = (args.get("content") or "").strip()
            if not content:
                raise ValueError("add needs content")
            existing = live_profile(db) if target == "user" else live_notes(db)
            if any(t == content for _, t in existing):
                return "already saved"
            db.add("fact", json.dumps({"subject": subject, "relation": relation, "object": content}), ns=NS)
            return "saved"
        if action in ("replace", "remove"):
            old_text = (args.get("old_text") or "").strip()
            if not old_text:
                raise ValueError("%s needs old_text" % action)
            entries = live_profile(db) if target == "user" else live_notes(db)
            hits = [(h, t) for h, t in entries if old_text in t]
            if not hits:
                raise ValueError("no %s entry contains %r" % (target, old_text[:80]))
            if len(hits) > 1:
                raise ValueError("old_text matches %d entries; be more specific" % len(hits))
            old_hash, _ = hits[0]
            if action == "replace":
                content = (args.get("content") or "").strip()
                if not content:
                    raise ValueError("replace needs content")
                db.supersede(old_hash, "fact", json.dumps({"subject": subject, "relation": relation,
                                                           "object": content}), ns=NS)
                return "replaced (the earlier wording stays in history)"
            try:
                db.forget(old_hash)
            except Exception:
                db.supersede(old_hash, "fact", json.dumps({"subject": subject, "relation": RETIRED,
                                                           "object": old_text}), ns=NS)
            return "removed"
        if action == "search":
            q = str(args.get("query") or "").strip()
            entries = live_notes(db) + live_profile(db)
            if not q:
                return "\n".join("- " + t for _, t in entries) or "(memory is empty)"
            words = [w for w in re.findall(r"[a-z0-9]+", q.lower()) if len(w) > 2]
            hits = [t for _, t in entries if any(w in t.lower() for w in words)]
            return "\n".join("- " + t for t in hits) or "(no entry matches)"
        raise ValueError("unknown memory action %r" % action)

    def _skill_manage(self, db, args):
        action = str(args.get("action") or "").strip()
        name = str(args.get("name") or "").strip()
        if not name:
            raise ValueError("skill_manage needs a name")
        skills = live_skills(db)
        if action == "create":
            content = (args.get("content") or "").strip()
            if not content:
                raise ValueError("create needs content")
            desc, body, when = _parse_skill_markdown(content)
            fields = {"name": name, "description": desc or name, "instructions": body}
            if when:
                fields["when_to_use"] = when
            if name in skills:
                db.supersede(skills[name][0], "skill", json.dumps(fields), ns=NS)
                return "skill %s updated (it already existed)" % name
            db.add("skill", json.dumps(fields), ns=NS)
            return "skill %s saved" % name
        if name not in skills:
            raise ValueError("no skill named %r; saved: %s" % (name, ", ".join(skills) or "(none)"))
        old_hash, old = skills[name]
        if action == "edit":
            content = (args.get("content") or "").strip()
            if not content:
                raise ValueError("edit needs content")
            desc, body, when = _parse_skill_markdown(content)
            fields = {"name": name, "description": desc or old.get("description") or name, "instructions": body}
            if when or old.get("when_to_use"):
                fields["when_to_use"] = when or old.get("when_to_use")
            db.supersede(old_hash, "skill", json.dumps(fields), ns=NS)
            return "skill %s updated" % name
        if action == "patch":
            old_text, new_text = str(args.get("old_text") or ""), str(args.get("new_text") or "")
            body = old.get("instructions") or ""
            if not old_text or old_text not in body:
                raise ValueError("old_text not found in skill %s" % name)
            fields = {k: v for k, v in old.items() if k in ("name", "description", "when_to_use")}
            fields["instructions"] = body.replace(old_text, new_text, 1)
            db.supersede(old_hash, "skill", json.dumps(fields), ns=NS)
            return "skill %s patched" % name
        if action == "delete":
            try:
                db.forget(old_hash)
            except Exception:
                db.supersede(old_hash, "skill", json.dumps({"name": name, "description": RETIRED,
                                                            "instructions": ""}), ns=NS)
            return "skill %s deleted" % name
        raise ValueError("unknown skill_manage action %r" % action)

    def _session_search(self, db, query):
        if not query:
            rows = _grains(db, "events", ' AND role = "user"')
            seen, out = set(), []
            for g in reversed(rows):
                f = _fields(g)
                sid = f.get("session_id") or ""
                if sid == self.session_id or sid in seen:
                    continue
                seen.add(sid)
                out.append("- [%s] %s" % (sid, (f.get("content") or "")[:160].replace("\n", " ")))
                if len(out) >= 10:
                    break
            return "\n".join(out) or "(no earlier sessions)"
        payload = json.loads(db.search(query, k=12, ns=NS))
        grains = payload.get("grains", payload) if isinstance(payload, dict) else payload
        out = []
        for g in grains or []:
            f = _fields(g) if isinstance(g, dict) else {}
            sid = f.get("session_id") or f.get("thread") or ""
            if sid == self.session_id:
                continue
            text = f.get("content") or f.get("object") or f.get("tool_content") or f.get("result") or ""
            if text:
                out.append("- [%s %s] %s" % (sid or "memory", f.get("role") or g.get("type") or "",
                                             text[:400].replace("\n", " ")))
            if len(out) >= 8:
                break
        return "\n".join(out) or "(nothing in earlier sessions matches)"

    # -- episode close --------------------------------------------------------

    def close(self, reason: str = "") -> None:
        if self._closed:
            return
        self._closed = True
        if self.artifacts_dir is None:
            return
        self.artifacts_dir.mkdir(parents=True, exist_ok=True)
        if self.persist and self.governed:
            try:
                self._ledger["governance"] = govern(self.db_path, self.family_id, self.seed)
            except Exception as exc:  # a governance failure is a finding, not a crash
                self._ledger["governance"] = {"error": "%s: %s" % (type(exc).__name__, str(exc)[:300])}
        session = {"session_id": self.session_id, "task_id": self.task_id, "started_at": self._started,
                   "finished_at": _now(), "persistence_enabled": self.persist, "governed": self.governed,
                   "messages": self._log}
        (self.artifacts_dir / "session_current.json").write_text(json.dumps(session, ensure_ascii=False, indent=1),
                                                                 encoding="utf-8")
        if self.persist:
            rendered = render_home(self.db_path, self.artifacts_dir)
        else:
            rendered = {"notes": 0, "profile": 0, "skills": 0}
        # The benchmark does not log its composed system prompt or the tool
        # schemas in the trace; the tuning corpus needs both (a model trained
        # on one prompt and evaluated on another measures the prompt). The
        # injected memory section is stripped so the row teaches behaviour
        # without the memory in context.
        system_text = ""
        for m in self._messages:
            if m.role == "system" and m.content and m.content[0].type == "text":
                system_text = re.sub(r"\n## Persistent memory\n.*\Z", "", m.content[0].text, flags=re.S)
                break
        self._ledger.update({"rendered": rendered, "usage": self._usage,
                             "injected_chars": getattr(self, "_injected_chars", 0),
                             "system_prompt": system_text,
                             "task_tools": [{"name": t.name, "description": t.description,
                                             "input_schema": t.input_schema} for t in self._task_tools]})
        (self.artifacts_dir / "areev_ledger.json").write_text(json.dumps(self._ledger, indent=1), encoding="utf-8")


# ---------------------------------------------------------------- the loop

def parse_proposal(summary):
    """What a recommendation asks a reviewer to do, from the engine's own
    rendering of it: `— record lesson: "..."`, `— record fact: rel = "..."`,
    a plan or query revision, or nothing (advisory)."""
    s = summary or ""
    m = re.search(r'record lesson:\s*"(.*)"\s*$', s, re.S)
    if m:
        return "lesson", m.group(1).strip()
    m = re.search(r"record fact:\s*(.*)$", s, re.S)
    if m:
        return "fact", m.group(1).strip().strip('"')
    if "SUPERSEDE" in s and "workflow" in s:
        return "plan_revision", s.strip()
    if "DEFINE QUERY" in s or "DEFINE TEMPLATE" in s:
        return "query_revision", s.strip()
    return "advisory", s.strip()


def govern(db_path, family_id, seed):
    """One governed pass at episode close: propose under the runner, decide
    under the reviewer, both recorded. Returns the ledger entry."""
    import reviewer as rv  # persist/reviewer.py, on sys.path via run.py

    llm_cmd = os.environ.get("AREEV_LOOP_LLM_CMD") or None
    ground_cmd = os.environ.get("AREEV_LOOP_GROUND_CMD") or None
    review_cmd = os.environ.get("AREEV_REVIEW_CMD") or None
    policy = {"discover_objective": "learner",
              "outcome_evalset": {"hash": family_id or "family", "field": "task_score", "higher_is_better": True}}
    policy_path = Path(db_path).parent / "loop-policy.json"
    policy_path.write_text(json.dumps(policy), encoding="utf-8")

    rep = json.loads(with_memory(
        db_path, ACTOR_RUNNER,
        lambda db: db.loop_run(llm_cmd=llm_cmd, ground_cmd=ground_cmd, policy=str(policy_path))))
    out = {"funnel": rep.get("llm_funnel"), "findings": len(rep.get("recommendations") or rep.get("findings") or []),
           "pending": 0, "applied": 0, "rejected": 0, "reverted": 0, "decisions": [], "errors": [],
           "llm": bool(llm_cmd), "reviewer": bool(review_cmd)}
    judge = rv.make_judge(review_cmd)

    def evidence_text(db, rec):
        # The first version looked the cited hashes up with `RECALL grains
        # WHERE hash = …`, which is not a CAL noun; every lookup failed
        # silently, the reviewer was handed "(none)" and refused every
        # proposal as unsupported — including three correct SOP lessons on
        # PC01. Index what the loop can cite (observations, facts, tools,
        # events in this namespace) by hash instead.
        by_hash = {}
        for noun in ("observations", "facts", "tools", "events"):
            try:
                for g in _grains(db, noun):
                    by_hash[g.get("hash")] = _fields(g)
            except Exception:
                continue
        parts = []
        for h in (rec.get("evidence") or [])[:6]:
            f = by_hash.get(h)
            if f:
                parts.append(json.dumps({k: v for k, v in f.items()
                                         if k in ("content", "object", "relation", "subject", "role",
                                                  "tool_name", "input", "tool_content", "is_error")},
                                        ensure_ascii=False)[:900])
        return "\n".join(parts)

    def review(db):
        in_force = [t for _, t in live_notes(db)] + [t for _, t in live_profile(db)]
        pend = json.loads(db.recommendations('{"status":"pending"}'))
        out["pending"] = len(pend)
        for rec in pend:
            kind, text = parse_proposal(str(rec.get("summary") or ""))
            analyzer = str(rec.get("analyzer") or "")
            because = ""
            evidence = ""
            if analyzer == "outcome_review":
                ok, because = True, "the gate measured a regression on this family's graded episodes"
            elif kind in ("lesson", "fact", "plan_revision", "query_revision"):
                evidence = evidence_text(db, rec)
                ok, because = rv.review(text, evidence, in_force, judge)
            else:
                ok, because = False, "advisory only — asks for no change"
            try:
                if ok:
                    db.apply_recommendation(rec["hash"], because)
                    out["applied"] += 1
                    if analyzer == "outcome_review":
                        out["reverted"] += 1
                    elif kind in ("lesson", "fact"):
                        in_force.append(text)
                else:
                    db.dismiss_recommendation(rec["hash"], because)
                    out["rejected"] += 1
                # the evidence the reviewer was shown travels with the
                # decision, so a refusal can be audited without reopening
                # the memory (67 refusals in run 1 could not be)
                out["decisions"].append({"hash": rec["hash"], "analyzer": analyzer, "kind": kind,
                                         "text": text[:300], "approved": bool(ok), "because": because[:300],
                                         "evidence": evidence[:1500]})
            except Exception as exc:
                out["errors"].append("%s: %s" % (rec.get("hash", "")[:12], str(exc)[:160]))

    with_memory(db_path, ACTOR_REVIEWER, review)
    return out


# -------------------------------------------------------------- the backend

class AreevPersistenceBackend(PersistenceBackend):
    """What the benchmark clones, resets and snapshots between episodes: a
    directory holding one Areev file. Anchors and branches are directory
    copies, exactly as for Hermes's home."""

    state_root_name = "areev_state"

    def __init__(self, agent_name: str) -> None:
        self.agent_name = agent_name
        self.governed = agent_name.endswith("governed")

    def db_path(self, state_root: Path) -> Path:
        return Path(state_root) / DB_NAME

    def materialize_inputs(self, *, state_root, initial_home_fixture_dir, preseed_artifacts_dir) -> None:
        state_root = Path(state_root)
        state_root.mkdir(parents=True, exist_ok=True)
        for src in (initial_home_fixture_dir, preseed_artifacts_dir):
            if src and Path(src).exists():
                import_hermes_home(self.db_path(state_root), Path(src))

    def build_extra_body(self, *, state_root, artifacts_dir, persistence_enabled, sequence, family_id,
                         review_wait_s, tool_config) -> dict[str, Any]:
        del sequence, review_wait_s
        return {"areev": {
            "db_path": str(self.db_path(Path(state_root))),
            "artifacts_dir": str(artifacts_dir),
            "persistence_enabled": bool(persistence_enabled),
            "governed": self.governed,
            "tool_config": dict(tool_config or {}),
            "family_id": family_id,
            "seed": int(os.environ.get("SEED", "1") or 1),
            "provider_pin": os.environ.get("AREEV_AGENT_PIN", ""),
        }}

    def snapshot_before(self, state_root, *, include_contents: bool = False) -> dict[str, Any]:
        state_root = Path(state_root)
        db = self.db_path(state_root)
        if not db.exists():
            return _empty_artifact_summary(state_root)
        rendered = state_root / "rendered"
        render_home(db, rendered)
        return _snapshot_hermes_tree(rendered, include_contents=include_contents)

    def snapshot_after(self, artifacts_dir) -> dict[str, Any]:
        artifacts_dir = Path(artifacts_dir)
        if not artifacts_dir.exists():
            return _empty_artifact_summary(artifacts_dir)
        session = artifacts_dir / "session_current.json"
        return _snapshot_hermes_tree(artifacts_dir, session_path=session if session.exists() else None,
                                     include_contents=True)


def make_backend(agent_name: str) -> AreevPersistenceBackend:
    return AreevPersistenceBackend(agent_name)
