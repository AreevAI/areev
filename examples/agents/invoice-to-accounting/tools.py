#!/usr/bin/env python3
"""Host tools for the invoice-to-accounting example.

One process per invocation, the seam every Areev surface uses:

    the tool's input JSON arrives on **stdin** (the run's merged state),
    `AREEV_TOOL_NAME` says which tool this is,
    the result JSON leaves on **stdout**,
    a non-zero exit is a Failed effect (stderr becomes the failure detail).

Nothing here opens a socket. That is the point — the whole example runs with
no credentials and no model key, so CI can prove it on every release. Swap
this file for one that calls your real accounting API and the plan, the
journal, and the approval gate do not change.
"""

import json
import os
import sys

# Both thresholds are also stored as facts in `accounting.rules`, where the
# loop can propose changing them. They are duplicated here only because this
# mock does not read the memory.
REVIEW_THRESHOLD_USD = 2500.0
CONFIDENCE_FLOOR = 0.75

SHEET = os.environ.get("SHEET_OUT", "out/sheet.jsonl")
OUTBOX = os.environ.get("OUTBOX_OUT", "out/outbox.jsonl")


def emit(obj):
    json.dump(obj, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


def append(path, obj):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(obj, sort_keys=True) + "\n")


def main():
    state = json.load(sys.stdin)
    tool = os.environ.get("AREEV_TOOL_NAME", "")

    if tool == "parse_attachments":
        # A photographed invoice has no text layer. Failing loudly is the
        # correct behaviour: a silent empty extraction posts a blank row.
        if state.get("scanned"):
            sys.stderr.write(
                "pdftotext produced 0 characters - attachment is a scanned image\n"
            )
            return 1
        emit({"texts": [{"filename": state.get("attachment", "invoice.pdf"), "chars": 4180}]})

    elif tool == "extract_rows":
        # The real one calls a model. This one reads the fixture's own fields,
        # which keeps the example deterministic and the assertions meaningful.
        emit(
            {
                "rows": 1,
                "vendor": state.get("vendor", "unknown"),
                "amount": state.get("amount", 0),
                "currency": state.get("currency", "USD"),
                "category": state.get("category", "Software"),
                "field_confidence": state.get("confidence", 0.95),
            }
        )

    elif tool == "validate_rows":
        amount = float(state.get("amount", 0))
        confidence = float(state.get("field_confidence", 1.0))
        needs_review = amount >= REVIEW_THRESHOLD_USD or confidence < CONFIDENCE_FLOOR
        emit(
            {
                "needs_review": needs_review,
                # (message id, invoice index, amount) — the same key a
                # redelivered message computes, so a replayed mailbox page
                # posts one row, not two.
                "row_key": "%s#%s@%s" % (state.get("message_id", "?"), 0, amount),
                "reason": "amount at or above threshold"
                if amount >= REVIEW_THRESHOLD_USD
                else ("field confidence below floor" if confidence < CONFIDENCE_FLOOR else "clear"),
            }
        )

    elif tool == "send_ask":
        # Always the approver on the internal thread, never the external
        # sender. Getting that backwards emails your vendor an approval link.
        append(OUTBOX, {"to": os.environ.get("APPROVER", "dev@northwind.example"),
                        "subject": "Approve expense row: %s" % state.get("row_key", "?"),
                        "amount": state.get("amount"), "vendor": state.get("vendor")})
        emit({"ask_sent": True, "thread": state.get("thread", "thr-ap-0000")})

    elif tool == "append_sheet":
        row = {
            "row_key": state.get("row_key"),
            "vendor": state.get("vendor"),
            "amount": state.get("amount"),
            "currency": state.get("currency"),
            "category": state.get("category"),
            "approved_by": state.get("responder", "auto"),
        }
        append(SHEET, row)
        emit({"appended": 1, "row_key": row["row_key"]})

    elif tool == "reply_email":
        append(OUTBOX, {"to": state.get("sender", "ap@northwind.example"),
                        "subject": "Re: %s" % state.get("message_id", "?"),
                        "outcome": "posted" if state.get("approved", True) else "rejected"})
        emit({"sent": True})

    else:
        sys.stderr.write("unknown tool: %r\n" % tool)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
