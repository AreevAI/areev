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
            "Invoice Date": "write dates as {date_name}, like {example}",
            "Amount": "write the amount as a plain number with two decimals and "
                      "no currency sign, like {example}",
            "Vendor Name": "copy the vendor's name exactly as printed on the "
                           "receipt, capitals and all, like {example}",
            "Vendor Address": "copy the address exactly as printed, like {example}",
        },
        "document_noun": "receipt",
    },
    # VRDU ad-buy forms (DeepForm): real US broadcast advertising invoices
    # filed with the FCC. Chosen to differ from SROIE on every axis that
    # could be a confound — country, document type, length, day-one field,
    # and filing convention. Its dates are ISO where the receipts ledger
    # wants day-first, so a rule that is right on one corpus is wrong on the
    # other and the agent has to learn THIS business's practice rather than
    # apply a prior.
    "vrdu": {
        "fields": ["Gross Amount", "Advertiser", "Contract Number",
                   "Flight From", "Flight To"],
        "day_one": "Gross Amount",
        "arc": [
            (1, ["Gross Amount"], None),
            (2, ["Gross Amount", "Advertiser", "Contract Number"],
             "Thanks — I also need the advertiser and the contract number on "
             "every one of these, otherwise I can't file it."),
            (8, ["Gross Amount", "Advertiser", "Contract Number",
                 "Flight From", "Flight To"],
             "Put the flight dates on as well, both ends; the accrual period "
             "goes by them."),
        ],
        "date_fields": ["Flight From", "Flight To"],
        "date_strftime": "%Y-%m-%d",
        "date_name": "YYYY-MM-DD",
        "amount_fields": ["Gross Amount"],
        "name_fields": ["Advertiser"],
        "loose_fields": [],
        "format_hint": {
            "Gross Amount": "write the amount as a plain number with two decimals, "
                            "no dollar sign and no thousands separator, like {example}",
            "Advertiser": "copy the advertiser's name exactly as printed on the "
                          "invoice, like {example}",
            "Contract Number": "copy the contract number exactly as printed, "
                               "like {example}",
            "Flight From": "write dates as {date_name}, like {example}",
            "Flight To": "write dates as {date_name}, like {example}",
        },
        "document_noun": "invoice",
    },
}

# The same real receipts, deployed against a business that does not hold
# still. Everything here is SROIE's ledger except the timeline: one new
# REQUIREMENT arrives mid-deployment, and one CONVENTION is replaced. The
# second is the hard half. When the group files ISO from document 81, every
# date rule the agent learned in its first eighty receipts is wrong, and
# getting better means retracting them — which is a thing you can only do if
# you measured whether they still help.
PROFILES["sroie_drift"] = dict(
    PROFILES["sroie"],
    arc=[
        (1, ["Invoice Date"], None),
        (2, ["Invoice Date", "Vendor Name", "Amount"],
         "Thanks — I also need the vendor and the amount on every one of "
         "these, otherwise I can't file it."),
        (41, ["Invoice Date", "Vendor Name", "Amount", "Vendor Address"],
         "One more thing from now on: put the vendor's address on as well, "
         "the tax file needs it."),
    ],
    # Same receipts as `sroie`; only the timeline differs, so it reads that
    # corpus rather than duplicating 612 documents.
    corpus="sroie",
    regimes=[
        (0, {"date_strftime": "%d/%m/%Y", "date_name": "DD/MM/YYYY"}),
        # Deliberately NOT ISO. Every model's prior for a date is ISO, so a
        # flip toward it would let the frozen arm improve for free and no
        # post-flip gain would be attributable to learning. A spelled-out
        # month is a real bookkeeping convention (it cannot be misread
        # day-first or month-first) and no model writes it unprompted, so
        # after document 81 the only way to score is to have been told and
        # to have retracted what came before.
        (81, {"date_strftime": "%d %B %Y", "date_name": "DD Month YYYY",
              "announce": "Change from today: the group that bought us wants "
                          "dates spelled out so nobody misreads them — write "
                          "every date like 20 March 2018 from now on, not "
                          "day-first with slashes."}),
    ],
)

# VRDU registration forms: real FARA filings, 1975-2023, 640 registrants. The
# first corpus here with a genuine timeline (`order_by`) and an entity key
# for a held-out set drawn from organisations the agent never saw
# (`holdout_key`) -- the split that designs memorisation out of the tuned
# model's evaluation instead of checking for it afterwards.
PROFILES["vrdu_reg"] = {
    "fields": ["Registration Number", "Registrant Name", "File Date", "Signer Name"],
    "day_one": "Registration Number",
    "arc": [
        (1, ["Registration Number"], None),
        (2, ["Registration Number", "Registrant Name", "File Date"],
         "Thanks — I also need who registered and the date it was filed, on "
         "every one of these."),
        (8, ["Registration Number", "Registrant Name", "File Date", "Signer Name"],
         "Add who signed it as well, on every one of these."),
    ],
    "date_fields": ["File Date"],
    "date_strftime": "%Y-%m-%d",
    "date_name": "YYYY-MM-DD",
    "amount_fields": [],
    "name_fields": ["Registrant Name", "Signer Name"],
    "loose_fields": [],
    "format_hint": {
        "File Date": "write dates as {date_name}, like {example}",
        "Registration Number": "write the registration number as digits only, like {example}",
        "Registrant Name": "copy the registrant's name exactly as printed on the form, like {example}",
        "Signer Name": "copy the signer's name exactly as printed, like {example}",
    },
    "document_noun": "registration form",
    "corpus": "vrdu_reg",
    "holdout_key": "entity",
    "order_by": "filed_at",
}

# The keyless gate's compressed timeline (dryrun_drift.sh). The flip here IS
# to ISO, and for the opposite reason: mock_agent.py writes ISO only when NO
# date rule is in the prompt, so the mock can score after the flip if and
# only if the stale rule was actually retracted. That makes the plumbing test
# mechanical. It proves the retraction path fires, never a learning claim.
PROFILES["sroie_drift_tiny"] = dict(
    PROFILES["sroie"],
    corpus="sroie",
    arc=[
        (1, ["Invoice Date"], None),
        (2, ["Invoice Date", "Vendor Name", "Amount"],
         "Thanks — I also need the vendor and the amount on every one of "
         "these, otherwise I can't file it."),
    ],
    regimes=[
        (0, {"date_strftime": "%d/%m/%Y", "date_name": "DD/MM/YYYY"}),
        (7, {"date_strftime": "%Y-%m-%d", "date_name": "YYYY-MM-DD",
             "announce": "Change from today: write every date as YYYY-MM-DD "
                         "from now on, not day-first."}),
    ],
)


def get(name):
    try:
        return PROFILES[name]
    except KeyError:
        raise SystemExit("unknown profile %r (known: %s)" % (name, ", ".join(PROFILES)))


def as_of(profile, seq):
    """The profile as it stands at document `seq`.

    A business does not hold still. `arc` already lets a REQUIREMENT arrive
    partway through a deployment; `regimes` lets a CONVENTION change — the
    harder case, because every rule the agent already learned about that
    convention is now wrong, and improving means retracting them rather than
    adding to them. A profile without `regimes` is unaffected, so every
    existing run is byte-identical under this."""
    reg = profile.get("regimes")
    if not reg:
        return profile
    merged = dict(profile)
    for after, changes in reg:
        if seq >= after:
            merged.update({k: v for k, v in changes.items() if k != "announce"})
    return merged


def regime_change_message(profile, seq):
    """What the accountant says on the document a convention changes, if any.

    Announced once, as a person would, and never repeated — the agent has to
    carry it into memory rather than being re-told on every document."""
    for after, changes in profile.get("regimes", []):
        if seq == after and changes.get("announce"):
            return changes["announce"]
    return None


def refile(profile_at, field, value):
    """Re-file a canonical ledger value under the conventions in force now.

    The dataset stores one filed value per field, built once. When a regime
    changes how a field is written, the ground truth has to move with it, and
    it moves deterministically: parse what the builder stored, re-emit it in
    the current format. Only date fields are re-filed today; a value that
    cannot be parsed is left exactly as the builder wrote it rather than
    guessed at."""
    if field not in profile_at.get("date_fields", ()):
        return value
    fmt = profile_at.get("date_strftime")
    if not fmt:
        return value
    d = _parse_filed_date(value)
    return d.strftime(fmt) if d else value


def _parse_filed_date(value):
    from datetime import datetime
    s = (value or "").strip()
    for f in ("%d/%m/%Y", "%Y-%m-%d", "%m/%d/%Y", "%d-%m-%Y", "%Y/%m/%d",
              "%d %B %Y", "%d %b %Y"):
        try:
            return datetime.strptime(s, f)
        except ValueError:
            continue
    return None


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


if __name__ == "__main__":
    import sys
    # `ledger_profile.py corpus <profile>` — env.sh asks which dataset a
    # profile reads, so a derived profile that only re-times an existing
    # corpus does not need its own copy of the data.
    if len(sys.argv) == 3 and sys.argv[1] == "corpus":
        print(get(sys.argv[2]).get("corpus", sys.argv[2]))
    else:
        raise SystemExit("usage: ledger_profile.py corpus <profile>")
