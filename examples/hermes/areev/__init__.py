"""Areev as a Hermes Agent memory provider.

Hermes ships two always-on memory files (``MEMORY.md``, ``USER.md``) capped at
2,200 and 1,375 characters, plus an FTS5 index over session history. This
provider adds a third layer underneath: a single content-addressed Areev file
where nothing is ever overwritten.

What that buys, concretely:

* **Consolidation stops losing things.** When Hermes hits its character cap it
  merges and drops entries; a ``remove`` rewrites the file and the old text is
  gone. Every such write is mirrored here as an immutable grain, so "what did I
  used to believe, and when did that change" stays answerable.
* **Recall is assembled, not pasted.** ``prefetch()`` runs one CAL ``ASSEMBLE``
  that pulls the user's profile and recent turns, applies a token budget and
  per-source priorities, and renders one block. It is a single in-process call
  (~0.6 ms), which matters because ``prefetch()`` sits on the synchronous turn
  path — a network-backed provider pays its round trip there.

Install: copy or symlink this directory to ``$HERMES_HOME/plugins/areev`` and
set ``memory.provider: areev``. See ``../README.md``.

Config lives in ``$HERMES_HOME/areev.json`` (``hermes memory setup`` writes
it). Everything has a default; the file is optional.
"""

from __future__ import annotations

import json
import logging
import os
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

try:  # pragma: no cover - exercised only inside a Hermes install
    from agent.memory_provider import MemoryProvider
except ImportError:  # pragma: no cover
    # Importable outside Hermes so the provider can be unit-tested on its own
    # (see ../test_provider.py). Hermes's loader always supplies the real ABC;
    # this shim only needs to be a base class, not a behavioural stand-in.
    class MemoryProvider:  # type: ignore[no-redef]
        pass


CONFIG_FILE = "areev.json"

DEFAULTS = {
    # One memory = one file. Left unset, it lands under $HERMES_HOME.
    "db_path": "",
    "namespace": "hermes",
    # The BM25 text index, on by default (matching Areev's own default).
    #
    # This was off in the first version of this provider, because the index was
    # Turso's experimental FTS and it made every write cost time proportional
    # to the file's size — the one thing a per-turn writer cannot absorb.
    # Areev now uses its own inverted index, so writes are flat (~0.4 ms at
    # 4k grains) *and* free-text search works. Turn it off only for a
    # write-heavy edge profile that never searches by text.
    "index_text": True,
    # Same contract as the CLI's --embed-cmd: text on stdin, JSON array out.
    # Optional now that BM25 is available by default — it adds the vector leg
    # alongside, which catches paraphrase that lexical matching misses.
    "embed_cmd": "",
    "embed_model": "",
    # Token budget for the block injected each turn.
    "budget_tokens": 800,
    "recent_turns": 8,
    # Run Areev Loop's deterministic analyzers when a session ends. No LLM, no
    # network; it only ever files advisory recommendations, which never
    # auto-apply. Review them with `areev loop list`.
    "loop_on_session_end": True,
}


def _load_config(hermes_home: str) -> Dict[str, Any]:
    cfg = dict(DEFAULTS)
    path = os.path.join(hermes_home, CONFIG_FILE)
    try:
        with open(path, encoding="utf-8") as fh:
            cfg.update(json.load(fh) or {})
    except FileNotFoundError:
        pass
    except Exception as e:
        logger.warning("areev: ignoring unreadable %s: %s", path, e)
    return cfg


def _lit(value: str) -> str:
    """Quote a Python string as a CAL string literal."""
    return '"' + str(value).replace("\\", "\\\\").replace('"', '\\"') + '"'


def _clip(text: str, limit: int) -> str:
    text = " ".join(str(text).split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


class AreevMemoryProvider(MemoryProvider):
    """One Areev file per agent identity, one namespace per user."""

    def __init__(self) -> None:
        self._db = None
        self._cfg: Dict[str, Any] = dict(DEFAULTS)
        self._ns = DEFAULTS["namespace"]
        self._subject = "user:local"
        self._identity = "default"
        self._session_id = ""
        # Non-primary contexts (cron ticks, subagents, flush passes) read the
        # same memory but must not write to it: their "user turn" is a system
        # prompt, and letting that land would poison the profile.
        self._writable = True
        self._free_text = False

    @property
    def name(self) -> str:
        return "areev"

    # -- lifecycle ----------------------------------------------------------

    def is_available(self) -> bool:
        try:
            import areev  # noqa: F401
        except ImportError:
            logger.debug("areev: package not installed (pip install areev)")
            return False
        return True

    def initialize(self, session_id: str, **kwargs) -> None:
        import areev

        hermes_home = kwargs.get("hermes_home") or os.path.expanduser("~/.hermes")
        self._cfg = _load_config(hermes_home)
        self._session_id = session_id

        context = kwargs.get("agent_context", "primary")
        self._writable = context == "primary"

        identity = kwargs.get("agent_identity") or "default"
        user = kwargs.get("user_id") or kwargs.get("user_id_alt") or "local"
        self._identity = identity
        self._subject = f"user:{user}"
        # One namespace per user keeps several people on one gateway from
        # reading each other's profile, while sharing the file's op-log.
        self._ns = f"{self._cfg['namespace']}/{user}"

        db_path = self._cfg.get("db_path") or os.path.join(
            hermes_home, "areev", f"{identity}.db"
        )
        os.makedirs(os.path.dirname(db_path) or ".", exist_ok=True)

        self._db = areev.Areev(
            db_path,
            ns=self._ns,
            actor=f"hermes:{identity}",
            index_text=bool(self._cfg["index_text"]),
        )
        for warning in json.loads(self._db.open_warnings()):
            logger.warning("areev: %s", warning)

        if self._cfg.get("embed_cmd"):
            try:
                self._db.set_embedder_command(
                    self._cfg["embed_cmd"], self._cfg.get("embed_model") or None
                )
            except Exception as e:
                # A broken embedder must not take the agent down with it; the
                # structural legs still answer.
                logger.warning("areev: embed_cmd failed, vector leg off: %s", e)

        self._free_text = bool(self._cfg["index_text"]) or bool(self._cfg.get("embed_cmd"))
        logger.info(
            "areev: %s (ns=%s, free-text=%s, writable=%s)",
            db_path, self._ns, self._free_text, self._writable,
        )

    def shutdown(self) -> None:
        self._db = None

    def on_session_switch(self, new_session_id: str, **kwargs) -> None:
        self._session_id = new_session_id

    # -- recall -------------------------------------------------------------

    def system_prompt_block(self) -> str:
        if not self._db:
            return ""
        return (
            "Areev holds this user's durable memory. It is append-only: an "
            "update supersedes rather than overwrites, so past versions stay "
            "queryable. Use the areev_memory tool to search it, to read the "
            "history of a belief, or to check what is currently contested."
        )

    def _assemble(self, query: str) -> str:
        """Build the per-turn ASSEMBLE.

        A saved query named ``hermes_prefetch`` in the memory file wins if one
        is defined — assembly logic is host metadata that travels with the
        file, so it can be re-tuned with `areev cal` without touching this
        plugin or restarting the agent.
        """
        budget = int(self._cfg["budget_tokens"])
        recent = int(self._cfg["recent_turns"])
        # `recent` deliberately excludes this session. Hermes already has the
        # current session's turns in context, so re-injecting them spends
        # budget restating what the model can already see; what it cannot see
        # is what happened in *previous* sessions. The audit leg is
        # observations, which `RECALL events` never returns.
        prior = (
            f"  recent:  (RECALL events WHERE "
            f"session_id != {_lit(self._session_id)} RECENT {recent})"
        )
        sources = [
            f"  profile: (RECALL facts WHERE subject = {_lit(self._subject)}),",
            prior,
        ]
        priority = "PRIORITY profile: 0.5, recent: 0.5"
        # The free-text source is only meaningful with a lexical or vector leg;
        # without one it would be an empty source that still eats budget.
        if self._free_text and query.strip():
            sources[-1] += ","
            sources.append(f"  related: (RECALL facts ABOUT {_lit(_clip(query, 240))} LIMIT 8)")
            priority = "PRIORITY profile: 0.4, related: 0.4, recent: 0.2"
        return (
            f'ASSEMBLE "hermes session" FROM\n'
            + "\n".join(sources)
            + f"\nBUDGET {budget} tokens\n{priority}\nFORMAT markdown"
        )

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        """Runs on the synchronous turn path — keep it to one call."""
        if not self._db:
            return ""
        try:
            if self._has_saved_prefetch():
                cal = (
                    f'RUN "hermes_prefetch"($user = {_lit(self._subject)}, '
                    f"$query = {_lit(_clip(query, 240))})"
                )
            else:
                cal = self._assemble(query)
            payload = json.loads(self._db.cal(cal))
        except Exception as e:
            # Recall must fail open: a memory provider that raises here would
            # take down the turn it was supposed to enrich.
            logger.debug("areev: prefetch failed (non-fatal): %s", e)
            return ""

        text = (payload.get("text") or "").strip()
        if not text:
            return ""
        return "## Durable memory (Areev)\n" + text

    def _has_saved_prefetch(self) -> bool:
        if not hasattr(self, "_saved_prefetch"):
            try:
                described = json.loads(self._db.cal("DESCRIBE QUERIES"))
                queries = described.get("info", {}).get("queries", [])
                # Only a user-defined query counts — the file ships built-ins.
                self._saved_prefetch = any(
                    q.get("name") == "hermes_prefetch" and not q.get("builtin")
                    for q in queries
                )
            except Exception:
                self._saved_prefetch = False
        return self._saved_prefetch

    # -- write --------------------------------------------------------------

    def sync_turn(
        self,
        user_content: str,
        assistant_content: str,
        *,
        session_id: str = "",
        messages: Optional[List[Dict[str, Any]]] = None,
    ) -> None:
        """Persist one exchange. Hermes calls this off the turn thread."""
        if not self._db or not self._writable:
            return
        session = session_id or self._session_id
        for role, content in (("user", user_content), ("assistant", assistant_content)):
            if not content or not str(content).strip():
                continue
            try:
                self._db.add("event", json.dumps({
                    "content": _clip(content, 4000),
                    "role": role,
                    "session_id": session,
                }))
            except Exception as e:
                logger.warning("areev: sync_turn (%s) failed: %s", role, e)

    def on_memory_write(
        self,
        action: str,
        target: str,
        content: str,
        metadata: Optional[Dict[str, Any]] = None,
    ) -> None:
        """Mirror a built-in memory edit as immutable grains.

        This is the point of the integration, and also the source of the
        ``profile`` block injected each turn: Hermes's ``MEMORY.md`` and
        ``USER.md`` are the agent's own curated view of the user, so mirroring
        them is better material than anything this provider could extract from
        raw turns on its own.

        Both files are character-capped, so consolidation *replaces* entries to
        make room and the previous text is gone from the file. Each mirror is a
        separate immutable grain, so the earlier wording is still recallable
        afterwards, and `areev` can show the whole sequence.

        **Known limit** (Hermes 0.16.0): the bridge that calls this only fires
        for ``add`` and ``replace`` — see the `action in {"add", "replace"}`
        guard in ``agent/agent_runtime_helpers.py``. A plain ``remove`` never
        reaches any provider. The removed *content* is still here from when it
        was added; what is not recorded is the fact that it was removed, or
        when. ``remove`` is handled below anyway, so this starts working the
        day that guard widens.
        """
        if not self._db or not self._writable:
            return
        session = (metadata or {}).get("session_id") or self._session_id
        try:
            # Audit leg: every edit, including the actions no longer
            # reachable. An OBSERVATION, not an Event — Event roles are
            # validated to the conversation vocabulary (areev >= 1.2), and
            # `RECALL events` naturally excludes observations, so the
            # per-turn recall block never sees this bookkeeping.
            self._db.add("observation", json.dumps({
                "observer": "hermes",
                "observation_type": "memory_write",
                "content": f"{action} {target}: {_clip(content, 2000)}",
                "session_id": session,
            }))
            # Profile leg: the entry itself, as a fact worth recalling. Several
            # entries share (subject, relation) and all stay recallable —
            # Areev only collapses a chain on an explicit supersede.
            if action in ("add", "replace") and content.strip():
                subject = self._subject if target == "user" else f"agent:{self._identity}"
                self._db.add_fact(subject, "note", _clip(content, 2000), 0.9,
                                  idempotent=True)
        except Exception as e:
            logger.warning("areev: on_memory_write failed: %s", e)

    def on_pre_compress(self, messages: List[Dict[str, Any]]) -> str:
        """Persist what compression is about to discard, then stay out of the
        summary prompt (returning text here would spend context to say
        something the memory already holds)."""
        if not self._db or not self._writable:
            return ""
        kept = 0
        for msg in messages or []:
            content = msg.get("content")
            role = msg.get("role")
            if role not in ("user", "assistant") or not isinstance(content, str):
                continue
            if len(content.strip()) < 40:  # greetings and acks are not memory
                continue
            try:
                self._db.add("event", json.dumps({
                    "content": _clip(content, 4000),
                    "role": role,
                    "session_id": self._session_id,
                }))
                kept += 1
            except Exception:
                break
        if kept:
            logger.info("areev: kept %d message(s) that compression discarded", kept)
        return ""

    def on_session_end(self, messages: List[Dict[str, Any]]) -> None:
        """Run the loop's deterministic analyzers over the session.

        Deterministic because that is what measured stronger on the
        self-improvement benchmark, not because avoiding a model is a goal
        in itself; it also keeps this hook free of keys and network. Wire
        `--llm-cmd` on the CLI to add verified LLM reflection. Findings are
        advisory recommendation grains that never auto-apply."""
        if not self._db or not self._writable:
            return
        if not self._cfg.get("loop_on_session_end"):
            return
        try:
            outcome = json.loads(self._db.loop_run())
            if outcome.get("stored"):
                logger.info(
                    "areev: loop filed %s recommendation(s) — `areev loop list`",
                    outcome["stored"],
                )
        except Exception as e:
            logger.debug("areev: loop run skipped: %s", e)

    # -- tools --------------------------------------------------------------

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        return [{
            "name": "areev_memory",
            "description": (
                "Durable append-only memory for this user. Nothing here is ever "
                "overwritten, so you can ask what was believed before.\n"
                "  search  — find memories by free text\n"
                "  recall  — everything known about one subject (e.g. 'user:alice')\n"
                "  history — every past version of one belief, newest first\n"
                "  cal     — a raw read-only CAL query, for anything else"
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["search", "recall", "history", "cal"],
                    },
                    "query": {"type": "string", "description": "Text for 'search', CAL for 'cal'."},
                    "subject": {"type": "string", "description": "Subject for 'recall'/'history'."},
                    "relation": {"type": "string", "description": "Relation for 'history'."},
                    "k": {"type": "integer", "description": "Max results (default 8)."},
                },
                "required": ["action"],
            },
        }]

    def handle_tool_call(self, tool_name: str, args: Dict[str, Any], **kwargs) -> str:
        if tool_name != "areev_memory":
            return json.dumps({"error": f"unknown tool {tool_name}"})
        if not self._db:
            return json.dumps({"error": "areev is not initialized"})

        action = (args or {}).get("action")
        k = int(args.get("k") or 8)
        try:
            if action == "search":
                if not self._free_text:
                    return json.dumps({
                        "error": "this memory has no free-text leg (index_text off, no "
                                 "embedder) — use action='recall' with a subject",
                    })
                return self._db.search(args.get("query") or "", k=k)
            if action == "recall":
                return self._db.recall(args.get("subject") or self._subject, k=k)
            if action == "history":
                subject = args.get("subject")
                relation = args.get("relation")
                if not subject or not relation:
                    return json.dumps({"error": "history needs both subject and relation"})
                return self._db.history(subject, relation)
            if action == "cal":
                return self._db.cal(args.get("query") or "")
            return json.dumps({"error": f"unknown action {action!r}"})
        except Exception as e:
            return json.dumps({"error": str(e)})

    # -- `hermes memory setup` ---------------------------------------------

    def get_config_schema(self) -> List[Dict[str, Any]]:
        return [
            {"key": "db_path", "description":
                "Memory file path (default: $HERMES_HOME/areev/<profile>.db)"},
            {"key": "namespace", "description": "Namespace prefix", "default": "hermes"},
            {"key": "index_text", "description":
                "Enable the BM25 text index (recommended). Turn off only for a "
                "write-heavy profile that never searches by text.",
             "default": True, "choices": [True, False]},
            {"key": "embed_cmd", "description":
                "Optional embedder command for the vector leg — text on stdin, JSON "
                "array of floats on stdout. Catches paraphrase that BM25 misses; "
                "required for free-text search if index_text is off."},
            {"key": "budget_tokens", "description":
                "Token budget for the block injected each turn", "default": 800},
            {"key": "loop_on_session_end", "description":
                "Run Areev Loop's deterministic analyzers at session end (advisory only)",
             "default": True, "choices": [True, False]},
        ]

    def save_config(self, values: Dict[str, Any], hermes_home: str) -> None:
        path = os.path.join(hermes_home, CONFIG_FILE)
        current = _load_config(hermes_home)
        current.update({k: v for k, v in (values or {}).items() if v != ""})
        os.makedirs(hermes_home, exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(current, fh, indent=2, sort_keys=True)
        logger.info("areev: wrote %s", path)


def register(ctx) -> None:
    """Plugin entry point — Hermes's loader calls this first."""
    ctx.register_memory_provider(AreevMemoryProvider())
