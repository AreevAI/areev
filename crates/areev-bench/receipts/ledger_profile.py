#!/usr/bin/env python3
"""The corpus profile: what the ledger wants, in what order, written how.

Everything the accountant knows and the agent does not lives here — the
fields, the order they become required in, and the filing conventions. The
agent's day-one instruction names ONE field; every later requirement and
every convention has to reach it the long way: the accountant says it, the
loop proposes it, a reviewer approves it, and the applied lesson renders into
the prompt. This module is imported by the accountant and the scorer, never
by the agent — the harness structurally cannot hand over what it measures.

One profile per public corpus. Adding a corpus is adding a profile plus a
builder that emits the shared JSONL record shape (see build_sroie.py).
"""

PROFILES = {
    # ICDAR 2019 SROIE (Malaysian retail receipts): company, date, address,
    # total. The ledger conventions are a bookkeeper's, not the receipt's —
    # dates DD/MM/YYYY, amounts as plain two-decimal numbers, names and
    # addresses exactly as printed — so "exact" measures learning how this
    # business files, and "semantic" measures reading the receipt at all.
    "sroie": {
        "fields": ["Invoice Date", "Vendor Name", "Amount", "Vendor Address"],
        "day_one": "Invoice Date",
        # (after_seq, fields_now_required, how the accountant says it). The
        # schedule is fixed before any run, never revealed opportunistically
        # when the agent fails — that is the first honesty rule.
        "arc": [
            (1, ["Invoice Date"], None),
            (2, ["Invoice Date", "Vendor Name", "Amount"],
             "Thanks — I also need the vendor and the amount on every one of "
             "these, otherwise I can't file it."),
            (8, ["Invoice Date", "Vendor Name", "Amount", "Vendor Address"],
             "Add the vendor's address as well; the tax file needs it."),
        ],
        "date_fields": ["Invoice Date"],
        "date_strftime": "%d/%m/%Y",
        "date_name": "DD/MM/YYYY",
        "amount_fields": ["Amount"],
        "name_fields": ["Vendor Name"],
        "loose_fields": ["Vendor Address"],
        # How the accountant phrases a right-value-wrong-format correction.
        # Stated once as a rule, the way a person does, not as a per-invoice
        # nag — and stated with the filed value as the example.
        "format_hint": {
            "Invoice Date": "write dates as DD/MM/YYYY, like {example}",
            "Amount": "write the amount as a plain number with two decimals and "
                      "no currency sign, like {example}",
            "Vendor Name": "copy the vendor's name exactly as printed on the "
                           "receipt, capitals and all, like {example}",
            "Vendor Address": "copy the address exactly as printed, like {example}",
        },
        "document_noun": "receipt",
    },
}


def get(name):
    try:
        return PROFILES[name]
    except KeyError:
        raise SystemExit("unknown profile %r (known: %s)" % (name, ", ".join(PROFILES)))


def required_fields(profile, seq):
    """What the accountant expects captured by this point in the arc."""
    req = profile["arc"][0][1]
    for after, fields, _ in profile["arc"]:
        if seq >= after:
            req = fields
    return list(req)


def new_requirement_message(profile, seq, categories_known=()):
    """The message introducing a requirement, if one starts at this invoice."""
    for after, _fields, msg in profile["arc"]:
        if seq == after and msg:
            return msg.format(categories=", ".join(categories_known) or "none yet")
    return None
