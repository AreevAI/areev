#!/usr/bin/env python3
"""What the agent was built not knowing — and where that knowledge lives.

The scenario this harness measures is an agent deployed on an incomplete
understanding of its own domain, which then meets work the missing rules
govern, learns them from what goes wrong, and has a person approve them
before they take effect. The manipulation is therefore: remove a named set
of rules from everything the agent can read, run it, and see whether the
loop puts them back.

**τ²-bench's tool docstrings carry most of the policy**, which is worth
stating because it makes the obvious version of this experiment a no-op.
Of nine policy constraints checked mechanically (`--audit`), seven appear
in the tool descriptions the agent is handed: the cancellation-reason enum,
the once-only modify, the no-product-type-change rule, the refund
destination, the gift-card balance requirement, the confirmation
requirement, and the pending-status precondition. Redacting `policy.md`
alone leaves all seven in the prompt. So a withheld clause is withheld from
**both** the policy text and the tool descriptions, and this module is the
single place that says which and does both — the audit below is what
stops a future clause being "withheld" while its text sits in a schema.

The environment is untouched: the tools still execute normally and the
reward is computed by τ²-bench's own evaluator against its own gold
actions. Only the agent's view is redacted.
"""
import re

# Each clause: the exact policy sentence(s) to drop, and the tool-description
# fragments that restate it. `probe` is a lowercase substring used by --audit
# to assert the clause really is gone from what the agent sees.
CLAUSES = {
    "authenticate": {
        "why": "the agent must locate the user id via email or name+zip before "
               "anything else; nothing in any tool description says so",
        "policy": [
            "At the beginning of the conversation, you have to authenticate the user "
            "identity by locating their user id via email, or via name + zip code. "
            "This has to be done even when the user already provides the user id.",
        ],
        "tools": {},
        "probe": "authenticate the user identity",
    },
    "cancel_reason": {
        "why": "cancellation takes one of exactly two reasons; the tool "
               "description states the enum, so the policy alone is not the "
               "agent's only source",
        "policy": [
            "The user needs to confirm the order id and the reason (either 'no longer "
            "needed' or 'ordered by mistake') for cancellation. Other reasons are not "
            "acceptable.",
        ],
        "tools": {
            "cancel_pending_order": [
                ", which should be either 'no longer needed' or 'ordered by mistake'",
            ],
        },
        "probe": "no longer needed",
    },
    "modify_once": {
        "why": "modify/exchange fires once per order, so every item has to be "
               "collected before the call — the classic 'nobody told me I get "
               "one shot' failure",
        "policy": [
            "Exchange or modify order tools can only be called once per order. Be sure "
            "that all items to be changed are collected into a list before making the "
            "tool call!!!",
            "This action can only be called once, and will change the order status to "
            "'pending (items modifed)'. The agent will not be able to modify or cancel "
            "the order anymore. So you must confirm all the details are correct and be "
            "cautious before taking this action. In particular, remember to remind the "
            "customer to confirm they have provided all the items they want to modify.",
        ],
        "tools": {
            "modify_pending_order_items": [
                " For a pending order, this function can only be called once.",
            ],
            "exchange_delivered_order_items": [
                " For a delivered order, return or exchange can be only done once by the agent.",
            ],
        },
        "probe": "can only be called once",
    },
    "refund_destination": {
        "why": "a refund goes to the original payment method or an existing "
               "gift card, and nowhere else",
        "policy": [
            "The refund must either go to the original payment method, or an existing "
            "gift card.",
        ],
        "tools": {
            "return_delivered_order_items": [
                "The payment method should be either the original payment method or an existing gift card.",
            ],
        },
        "probe": "original payment method or an existing gift card",
    },
}

DEFAULT_WITHHELD = ["authenticate", "modify_once"]


def _drop(text, fragments):
    for frag in fragments:
        if frag in text:
            text = text.replace(frag, "")
    # Collapse the blank lines a removed paragraph leaves behind, so the
    # redacted policy reads like a policy rather than like a redacted one —
    # an agent that can see where a rule was removed is being told a rule
    # exists, which is half the thing being withheld.
    return re.sub(r"\n{3,}", "\n\n", text).strip() + "\n"


def redact_policy(policy, withheld):
    for name in withheld:
        policy = _drop(policy, CLAUSES[name]["policy"])
    return policy


def redact_tools(tools_json, withheld):
    """Redact the OpenAI-format tool schemas the agent is shown. Returns a new
    list; the environment's own tools are never touched."""
    import copy
    out = copy.deepcopy(tools_json)
    for name in withheld:
        for tool_name, frags in CLAUSES[name]["tools"].items():
            for t in out:
                if t.get("function", {}).get("name") != tool_name:
                    continue
                fn = t["function"]
                fn["description"] = _drop(fn.get("description", ""), frags)
                for p in (fn.get("parameters", {}).get("properties") or {}).values():
                    if isinstance(p.get("description"), str):
                        p["description"] = _drop(p["description"], frags)
    return out


def audit(policy, tools_json, withheld):
    """Every withheld clause must be absent from BOTH surfaces the agent can
    read. Returns a list of failures; empty means the redaction is real."""
    import json as _json
    blob = (redact_policy(policy, withheld) + "\n"
            + _json.dumps(redact_tools(tools_json, withheld))).lower()
    return [name for name in withheld if CLAUSES[name]["probe"].lower() in blob]


def leak_report(tools_json):
    """Which clauses the shipped tool descriptions restate — the reason a
    policy-only redaction is not a manipulation."""
    import json as _json
    blob = _json.dumps(tools_json).lower()
    return {name: (c["probe"].lower() in blob) for name, c in CLAUSES.items()}
