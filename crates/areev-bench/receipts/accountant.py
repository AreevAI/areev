#!/usr/bin/env python3
"""The accountant: reviews a proposed row and replies the way a person would.

It stands in for the person who files these receipts and pushes back when
the agent gets something wrong. It is scripted against the filed ledger,
which is the ground truth it is entitled to see.

Two honesty rules, because this component is where a benchmark like this
usually cheats:

1. It never names the field the agent should have captured UNLESS the agent
   already produced something for it or the requirement has been introduced.
   Requirements are revealed on the profile's fixed schedule, not
   opportunistically when the agent fails.

2. Every reply is recorded verbatim in the run journal. Realistic replies
   sometimes state a general rule rather than one value — which teaches
   faster and is what a real accountant does — so the record has to let a
   reader judge for themselves whether a lesson was earned or handed over.
"""
import json
import re
from datetime import datetime

import ledger_profile as prof


# --- value comparison -------------------------------------------------------

DATE_FORMATS = ("%d/%m/%Y", "%m/%d/%Y", "%Y-%m-%d", "%B %d, %Y", "%b %d, %Y",
                "%d %B %Y", "%d %b %Y", "%d-%m-%Y", "%Y/%m/%d", "%d/%m/%y",
                "%d.%m.%Y", "%B %d %Y", "%d %b %y", "%d/%b/%Y", "%d-%b-%Y",
                "%d-%b-%y", "%d/%B/%Y", "%m/%d/%y")


def norm_date(s):
    """A date's identity, independent of how it was written."""
    s = (s or "").strip()
    if not s:
        return None
    s = re.split(r"\s+\d{1,2}:\d{2}", s)[0]
    for f in DATE_FORMATS:
        try:
            return datetime.strptime(s, f).date()
        except ValueError:
            pass
    return None


def norm_amount(s):
    s = re.sub(r"[^0-9.\-]", "", (s or "").replace(",", ""))
    try:
        return round(float(s), 2)
    except ValueError:
        return None


def norm_text(s):
    return re.sub(r"\s+", " ", (s or "").strip().lower())


def compare(profile, field, got, want):
    """(exact, semantic) — exact is what lands in the ledger; semantic is
    whether the agent actually read the right thing. Tracking both is what
    separates 'learned the convention' from 'learned to read the receipt'."""
    got_s = ("" if got is None else str(got)).strip()
    want_s = ("" if want is None else str(want)).strip()
    exact = got_s == want_s and got_s != ""
    if field in profile["date_fields"]:
        g, w = norm_date(got_s), norm_date(want_s)
        semantic = bool(g and w and g == w)
    elif field in profile["amount_fields"]:
        g, w = norm_amount(got_s), norm_amount(want_s)
        semantic = bool(g is not None and w is not None and abs(g - w) < 0.01)
    elif field in profile["name_fields"]:
        g, w = norm_text(got_s), norm_text(want_s)
        semantic = bool(g and w and (g == w or g in w or w in g))
    elif field in profile["loose_fields"]:
        g, w = norm_text(got_s), norm_text(want_s)
        semantic = bool(g and w and (g == w or g in w or w in g))
    else:
        semantic = norm_text(got_s) == norm_text(want_s) and got_s != ""
    return exact, semantic


def review(profile, seq, proposal, truth, categories_known=()):
    """The accountant's reply.

    Returns (approved, message, corrections) where corrections is the set of
    (field, correct_value) they explicitly stated. The message is what gets
    recorded into memory as a human observation.
    """
    req = prof.required_fields(profile, seq)
    fields = proposal.get("fields") or {}
    parked = proposal.get("park")

    intro = prof.new_requirement_message(profile, seq, categories_known)
    # A convention change is announced the same way a new requirement is:
    # once, in the accountant's own words, on the document it takes effect.
    # It is stated even when the agent got everything right, because the
    # agent cannot see it coming and every rule it already holds about that
    # convention has just gone stale.
    regime = prof.regime_change_message(profile, seq)
    parts = []
    corrections = {}

    # An unreadable document is a legitimate park; the accountant fills it in
    # by hand and says so, exactly as they would in a real mailbox.
    if parked:
        msg = intro or ""
        vals = ", ".join("%s is %s" % (k, truth[k]) for k in req if truth.get(k))
        msg = (msg + " " if msg else "") + (
            "I've keyed this one in myself: %s." % vals if vals else "")
        for k in req:
            if truth.get(k):
                corrections[k] = truth[k]
        return False, msg.strip(), corrections

    wrong_format, wrong_value, missing = [], [], []
    for k in req:
        want = truth.get(k, "")
        if not want:
            continue
        got = fields.get(k, "")
        exact, semantic = compare(profile, k, got, want)
        if exact:
            continue
        if not (got or "").strip():
            missing.append(k)
        elif semantic:
            wrong_format.append(k)
        else:
            wrong_value.append(k)
        corrections[k] = want

    if not corrections and not intro and not regime:
        return True, "", {}

    if regime:
        parts.append(regime)
    if intro:
        parts.append(intro)
    # A real accountant states the rule once, not the same correction forever.
    for k in wrong_format:
        hint = profile["format_hint"].get(k, "use exactly what's in the ledger, e.g. {example}")
        # `date_name` is passed because a profile with regimes changes it
        # partway through a deployment; a hint that hard-coded the convention
        # would keep stating the retired one.
        parts.append("The %s is right but %s."
                     % (k, hint.format(example=truth[k],
                                       date_name=profile.get("date_name", ""))))
    if wrong_value:
        parts.append(" ".join(
            "%s should be %s, not %s." % (k, truth[k], fields.get(k) or "blank")
            for k in wrong_value))
    if missing:
        parts.append(" ".join("%s is %s." % (k, truth[k]) for k in missing))

    return False, " ".join(parts).strip(), corrections


# --- the accountant's review of a proposed rule ---------------------------
#
# The loop proposes; a person decides. Three earlier versions of this gate were
# lexical — keyword lists for "is this an instruction" and "is this about
# invoice data" — and each one misclassified a rule that mattered:
#
#   * "Verify all required invoice fields (Date, Vendor, Amount, Currency)"
#     rejected because "verify" was absent from a hand-written marker list;
#   * a Category rule rejected because "categories" does not contain
#     "category";
#   * "Confirm all required fields are present ... even after partial
#     corrections" rejected because it contains the substring "corrections".
#
# Each miss cost a run, and each patch would have meant choosing vocabulary
# after seeing which rules I wanted admitted — which is how a gate gets tuned
# toward its result. So the judgment is made the way the real one is: the
# accountant reads the rule and answers.
#
# The rubric below is FIXED and was written before the run it judges. The
# reviewer is a different model from the proposer, and it is deliberately NOT
# given the ledger: it judges from the same thing a person would — the rule,
# and the field names in use. It can therefore reject a rule that is wrong
# for the business, exactly as a human reviewer would, which means the human
# gate is part of what is being measured and is reported as such.
#
# Mechanical dedup stays mechanical (below): "I already told you this" is a
# lookup, not a judgment.

REVIEW_RUBRIC = """You are the accountant who files these {noun}s.
An assistant reads each {noun} and fills one ledger row. It has proposed a RULE to follow on all FUTURE {noun}s, and you decide whether to adopt it.

The ledger columns are: {fields}.

Approve the rule only if BOTH hold:
  (a) it tells the assistant WHICH fields to capture from a {noun}, or HOW to write a value it captures. A formatting convention counts here: a date format, a number style, or how a name should be written are all "how to write a value"; and
  (b) following it would be correct for this business.

Reject the rule if any of these hold:
  - it tells someone OTHER than the assistant what to do — typically the person who checks or corrects rows after they are filed;
  - it presupposes a stage the assistant does not have. The assistant reads one {noun} and returns the row in a single step: there is no separate submission, review or resubmission stage it can act in, and no way for it to revisit an earlier {noun}. A rule built on "resubmit", "re-enter" or "check previous rejections" is asking for something impossible. But a rule that merely SAYS "before submission" while telling the assistant which fields to capture or how to write them is simply saying "before you answer" — that is a capture rule, and you should approve it;
  - it fixes a value for one particular {noun} or vendor instead of stating a general practice;
  - it would produce a wrong value (for example, altering a vendor's name as printed, or converting an amount into another currency).

Answer with JSON only: {{"approve": true|false, "reason": "<one short sentence, addressed to the assistant>"}}"""


def review_recommendation(profile, lesson_text, already_approved, judge=None):
    """(approve: bool, reason: str) — would the accountant adopt this rule?

    `already_approved` is the set of normalized rules already in force.
    `judge` is a callable taking (system, user) and returning the model's text.
    Without one, only the mechanical checks run and everything else is held
    back rather than waved through — a review nobody performed is not an
    approval.
    """
    t = (lesson_text or "").strip()
    if not t:
        return False, "empty rule"

    norm = normalize_rule(t)
    if norm in already_approved:
        return False, "already in force — this restates a rule I've approved"

    near = _too_similar(norm, already_approved)
    if near is not None:
        return False, "I've already told you this — it restates: %s" % near[:70]

    if judge is None:
        return False, "no reviewer available to approve this"

    system = REVIEW_RUBRIC.format(noun=profile["document_noun"],
                                  fields=", ".join(profile["fields"]))
    raw = judge(system, t)
    try:
        body = raw[raw.index("{"):raw.rindex("}") + 1]
        verdict = json.loads(body)
    except (ValueError, json.JSONDecodeError):
        # An unreadable verdict is not an approval.
        return False, "reviewer returned no usable verdict"
    return bool(verdict.get("approve")), \
        (verdict.get("reason") or "").strip()[:200] or "reviewed"


def normalize_rule(text):
    """A rule's identity for dedup: case and whitespace do not make it new."""
    return re.sub(r"[^a-z0-9 ]", "", (text or "").lower())


# Content words only: two rules restating one idea share these, while the
# scaffolding ("all", "every", "the") is shared by every rule ever written.
_STOP = set("a an the to of in on for and or is are be as at by with all any "
            "every each from into that this it its not no do does use using "
            "when if then than must should always never ensure make sure "
            "before after even they them their there which who whom".split())


def _content_words(norm):
    return {w for w in norm.split() if w not in _STOP and len(w) > 2}


def _too_similar(norm, already_approved, threshold=0.6):
    """The rule in force this one is a restatement of, or None.

    Exact-text dedup is not enough: one run accumulated five rules that all
    said the same thing about vendor names, each worded differently, and
    together they crowded the prompt. A person would answer the fourth with
    "I've already told you this."
    """
    mine = _content_words(norm)
    if not mine:
        return None
    for other in already_approved:
        theirs = _content_words(other)
        if not theirs:
            continue
        j = len(mine & theirs) / len(mine | theirs)
        if j >= threshold:
            return other
    return None
