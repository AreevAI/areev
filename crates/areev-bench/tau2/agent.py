#!/usr/bin/env python3
"""The retail agent: τ²-bench's orchestrator drives it, Areev's memory teaches it.

A `HalfDuplexAgent` so the shipped orchestrator handles the whole
agent ↔ user ↔ environment dance, the shipped user simulator plays the
customer, and the shipped evaluator scores the run. The only things this
class owns are the system prompt and the model call:

    system prompt = redacted policy + LESSONS assembled from live memory

`lessons_md` is read from the memory file on every episode, so Areev's own
apply/rollback is the only lever on behaviour and there is no harness flag
that changes it.

The model call goes through the bench's JSON-on-stdio adapter (one process
per call, no SDK), the same contract the rest of areev-bench uses.
"""
import json
import subprocess
import uuid

from tau2.agent.base_agent import HalfDuplexAgent
from tau2.data_model.message import AssistantMessage, ToolCall

SYSTEM_PROMPT = """You are a retail customer-service agent. You act on behalf of the store, using the tools available to you, and you speak to one customer at a time.

<policy>
{policy}
</policy>
{lessons}
Answer the customer directly when you are talking to them, and make a tool call when you need to act or look something up. Do not do both in the same message."""

LESSONS_BLOCK = """
<lessons>
These come from your supervisor, based on how earlier conversations actually went. They OVERRIDE nothing in the policy above, but the policy does not cover them and they are binding on you.
{rules}
</lessons>
"""


class AreevRetailAgent(HalfDuplexAgent):
    """A retail agent whose knowledge is (redacted policy + applied lessons)."""

    def __init__(self, tools, domain_policy, agent_cmd, lessons_md="", max_tokens=1024):
        super().__init__(tools=tools, domain_policy=domain_policy)
        self.agent_argv = agent_cmd.split()
        self.lessons_md = lessons_md or ""
        self.max_tokens = max_tokens
        self.calls = []  # every (request, reply) this episode, for the record
        self.malformed_calls = 0

    @property
    def system_prompt(self):
        lessons = ""
        if self.lessons_md.strip():
            lessons = LESSONS_BLOCK.format(rules=self.lessons_md.strip())
        return SYSTEM_PROMPT.format(policy=self.domain_policy, lessons=lessons)

    def get_init_state(self, message_history=None):
        return list(message_history or [])

    def generate_next_message(self, message, state):
        state = list(state) + [message]
        reply = self._call(state)
        state = state + [reply]
        return reply, state

    # ── the model call ────────────────────────────────────────────────────

    def _wire_messages(self, state):
        out = [{"role": "system", "content": self.system_prompt}]
        for m in state:
            role = getattr(m, "role", None)
            if role == "user":
                out.append({"role": "user", "content": m.content or ""})
            elif role == "assistant":
                msg = {"role": "assistant", "content": m.content or ""}
                if getattr(m, "tool_calls", None):
                    msg["tool_calls"] = [
                        {"id": tc.id, "type": "function",
                         "function": {"name": tc.name, "arguments": json.dumps(tc.arguments)}}
                        for tc in m.tool_calls
                    ]
                out.append(msg)
            elif role == "tool":
                out.append({"role": "tool", "tool_call_id": m.id, "content": m.content or ""})
            else:  # MultiToolMessage — one wire message per result
                for sub in getattr(m, "tool_messages", []) or []:
                    out.append({"role": "tool", "tool_call_id": sub.id, "content": sub.content or ""})
        return out

    def _call(self, state):
        req = {
            "op": "chat",
            "messages": self._wire_messages(state),
            "tools": [t.openai_schema for t in self.tools],
            "temperature": 0,
            "max_tokens": self.max_tokens,
        }
        p = subprocess.run(self.agent_argv, input=json.dumps(req).encode(),
                           capture_output=True, timeout=300)
        if p.returncode != 0:
            raise RuntimeError("agent adapter failed: %s" % p.stderr.decode()[:300])
        resp = json.loads(p.stdout.decode())
        msg = resp.get("message") or {}
        self.calls.append({"usage": resp.get("usage") or {}, "meta": resp.get("meta") or {}})

        tool_calls, malformed = [], 0
        for tc in msg.get("tool_calls") or []:
            fn = tc.get("function") or {}
            name = (fn.get("name") or "").strip()
            if not name:
                # A tool call with no function name is a malformed reply, not
                # an action: the store would reject it and so does the grain
                # writer. Dropped here, counted, and surfaced as a turn the
                # agent wasted rather than as a crash.
                malformed += 1
                continue
            args = fn.get("arguments")
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except json.JSONDecodeError:
                    args = {}
            if not isinstance(args, dict):
                args = {}
            tool_calls.append(ToolCall(id=tc.get("id") or ("call_%s" % uuid.uuid4().hex[:8]),
                                       name=name, arguments=args))
        self.malformed_calls += malformed
        content = msg.get("content") or None
        if not content and not tool_calls:
            # A reply that is neither text nor a tool call fails τ²'s own
            # validation; make it a visible, in-domain response instead of an
            # orchestrator crash, so the episode ends as a bad turn rather
            # than as infrastructure.
            content = "I'm sorry, could you say that again?"
        if tool_calls:
            content = None  # policy: never text and a tool call together
        return AssistantMessage(role="assistant", content=content,
                                tool_calls=tool_calls or None, cost=0.0)

    def usage(self):
        p = sum(int((c["usage"] or {}).get("prompt_tokens") or 0) for c in self.calls)
        c = sum(int((c["usage"] or {}).get("completion_tokens") or 0) for c in self.calls)
        return {"prompt_tokens": p, "completion_tokens": c, "calls": len(self.calls),
                "malformed_tool_calls": self.malformed_calls}
