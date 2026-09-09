# OMS 1.7 amendments — CAL expressiveness

> Companion to [`oms-1.6-amendments.md`](oms-1.6-amendments.md) (the
> compliance batch) and
> [`oms-1.6-amendments-triggers.md`](oms-1.6-amendments-triggers.md).

**Status:** amendment proposal, written 2026-09-08. One revision covering the
three reads that could not be expressed in CAL, so conformance moves once
rather than three times.

CAL syntax and its §10.5 template-variable set are OMS conformance contracts:
Areev's own invariants say no new CAL syntax without a spec-level decision.
This document is that decision, written down.

| # | Amendment | Issue | Conformance impact |
|---|---|---|---|
| B1 | `GROUP BY <field>` followed by `COUNT` projects one row per group | [#209](https://github.com/AreevAI/areev/issues/209) | Result payload; **no new syntax** |
| B2 | The `group.` template namespace (`key`, `count`) | #209 | §10.5 variable set |
| B3 | Six extracting filters and one navigating filter | [#210](https://github.com/AreevAI/areev/issues/210), [#211](https://github.com/AreevAI/areev/issues/211) | §10.7 filter set |
| B4 | Dotted field paths in `WHERE`, up to 8 segments | #211 | Grammar (relaxes an existing rule) |

All four ship in 1.7.4 ahead of ratification, because each closes a read a
host must otherwise perform by over-fetching — which also defeats `BUDGET`,
since the budget is spent on the rows the host is about to discard. They are
written here as the record of what an implementation must do to interoperate.

---

## The standing constraint, and why these fit inside it

CAL's value is that it is **not** a programming language. Saved-query bodies
get an extra read-only verification pass precisely because the surface is
narrow, and those bodies are handed to unattended agents. Four properties are
load-bearing, and every amendment below is shaped by them:

1. **Conformance surface.** Two implementations must render a grain
   identically. That is why §10.5's variables and the filter list are *closed*
   sets, and why every addition here extends a closed set rather than opening
   one.
2. **Cost and safety.** A template renders on every turn over host-supplied
   and model-supplied text. Anything added needs a bound.
3. **Auditability.** "What will this saved query show me?" must stay
   answerable by reading it. Arbitrary expressions make that a
   program-analysis question.
4. **The render is sometimes the wrong place.** If a value has to be taken
   apart at read time, the write often chose the wrong shape.

**What is therefore refused, and stays refused:** host-registered functions (a
template calling one would render differently depending on who opened the
file, which breaks the property that makes saved queries worth having — the
registry travels *with* the memory); a general expression language in
templates; and any transformation that parses, mutates and re-serialises. That
last one is the tempting case in #211 — "read `error.code`, remove that key,
re-emit the rest" — and it is declined. It is a program. Its main use,
reshaping on the way out, is better served by writing the right shape in,
which also makes the value **filterable**, as no amount of template machinery
does.

---

## B1 — `GROUP BY <field> COUNT` projects one row per group

```sql
RECALL tools WHERE is_error = true LIMIT 400 GROUP BY tool_name COUNT
```

**Semantics.** One row per group, carrying the group's key and size, ordered
**most frequent first** with ties broken by key ascending. The payload is
`group_counts`, and each row is a grain-shaped record with `grain_type
"group"`, fields `{key, count}`, and an **empty content address** — a group is
computed, not stored, and must never be mistaken for a grain (anything keying
on a hash, dedup above all, has to skip it).

**No new syntax, and that is the point.** `GROUP BY` and `COUNT` both already
exist. What the combination *did* was return the plain total — identical to
`COUNT` alone, silently discarding the grouping. Nothing could have wanted
that number: it is `COUNT` with extra words. So an implementation adopting
this amendment takes no meaningful answer away from any caller.

**Why it belongs in the language.** Frequency is how a memory says what
*matters*. "Which tool fails most", "which topic does this user raise most",
"which policy is cited most" are the ordinary summarisation reads, and every
one of them was host code. The ordering is part of the contract, not a
convenience: a deterministic tiebreak is what makes the answer reproducible
across backends and across runs.

**An `ASSEMBLE` source may be a grouped count.** So "the five errors this
agent hits most" can be a *section of a prompt* rather than a separate read
the host tallies and splices in itself.

## B2 — the `group.` template namespace

§10.5 gains a fourth namespace beside `grain.`, `assembly.`, `budget.` and
`source.`:

| Variable | Value |
|---|---|
| `{{group.key}}` | The group's key |
| `{{group.count}}` | How many grains fall in it |

```
DEFINE TEMPLATE top_failures ELEMENT {- ({{group.count}}x) {{group.key}}}
```
```
- (8x) simple_note.search_notes
- (6x) phone.search_contacts
```

**Bound only on a group row.** On an ordinary grain both resolve null, rather
than reading a field that happens to be called `key`. The set is closed at two
names; an implementation MUST NOT resolve any other `group.` variable.

No `GROUP` / `GROUP_BREAK` sections are added. `ELEMENT` over group rows
renders the list, and the existing sections keep their meaning — a second
sectioning axis would need its own inheritance and budget-tier rules for a
case `ELEMENT` already covers.

## B3 — the filter set grows by seven, and stays closed

§10.7's filter list gains six **extractors** (which *take* part of a value,
where the existing ten *format* the whole of one) and one **navigator**:

| Filter | Effect |
|---|---|
| `first_line` | Text up to the first line break |
| `split("<sep>", n)` | The nth field (0-indexed) after splitting |
| `strip_prefix("<s>")` / `strip_suffix("<s>")` | Remove a fixed affix if present |
| `between("<open>", "<close>")` | The text between the first `open` and the next `close` |
| `match("<pattern>"[, n])` | The whole match, or capture group `n` |
| `get("<a.b.c>")` | One value out of a JSON payload, by dotted path |

```
{{grain.object | between("[", "]")}}           → Q3 close handoff
{{grain.object | get("error.code")}}           → rate_limited
```

**Why extractors at all.** Memories store text people wrote. Titles, ticket
ids, error codes, subject lines and thread keys all live inside free text, and
every host that wanted one over-fetched to slice it in application code.
`get` closes a narrower and sharper gap: `record_tool_call` round-trips a
tool's `input` as parsed JSON, so a Python or Node host receives the structure
for free — it was specifically the **CAL** path that could not see inside.
(The existing `json` filter does not help despite its name: it *serialises* a
value, it does not parse one.)

**Requirements on an implementation.**

- **Totality.** Every one of these is total at render time: a filter that
  finds nothing yields empty, never an error. One unparseable grain must not
  fail the render of the other 199.
- **Validation is at authoring time, not render time.** A pattern that does
  not compile, a `split` index that is not a number, a `between` missing a
  delimiter, or a `get` path deeper than the bound MUST be refused when the
  template is defined. This is the auditability requirement: an unreadable
  saved query never gets stored, and by the time a pattern reaches the render
  path it is known to compile.
- **Patterns MUST use a non-backtracking engine.** A template renders on every
  turn over untrusted grain content; a backtracking regex there is a
  denial-of-service primitive. Areev uses Rust's `regex` (a finite-automaton
  engine), which is why the dialect has **no backreferences and no
  lookaround** — those are precisely the constructs that force backtracking.
  An implementation MAY choose another linear-time engine; it MUST NOT choose
  a backtracking one.
- **Bounds.** Extractor input is clipped (Areev: 64 KiB), patterns are length-
  capped (512 chars) and compiled through a bounded cache, and `get` paths are
  depth-capped (8 segments). These sit beside the existing §10.8 limits
  (`MAX_TEMPLATE_SIZE` 4096, `MAX_EACH_ITERATIONS` 200).

**`get` deliberately does not transform.** No wildcards, no predicates, no
arithmetic, and no "remove a key and re-serialise the rest". See the standing
constraint above.

## B4 — dotted field paths in `WHERE`

```sql
RECALL tools WHERE input.app = "phone"
RECALL facts WHERE object.error.code = "rate_limited"
```

**Semantics.** A field name may be a dotted path of up to **8 segments**. The
first segment names the grain field; the rest navigate its JSON. A numeric
segment indexes an array (`items.0.name`), so no separate syntax is needed for
it. The base field is validated against the grain type as usual (`CAL-E060`);
the *path* cannot be validated, because the shape lives in the payload rather
than in the schema.

**A value stored as a JSON string navigates identically to a parsed one.** A
Tool's `input` is already an object; a failure signature is often a JSON
document stored *as a string*. Navigating one and not the other would make the
accessor depend on how the writer happened to type the field.

**A path that does not resolve is UNKNOWN, not false** — it inherits the
fails-closed rule (`ARCHITECTURE.md` §10, "WHERE fails closed", amended
2026-09-08 for [#207](https://github.com/AreevAI/areev/issues/207)) rather
than adding one. So `WHERE input.app = "phone"` matches neither the grains
whose payload says otherwise nor the grains that have no such key, and
`input.app != "phone"` does not widen to everything.

**This relaxes an existing rule rather than adding one.** `parse_field_name`
already accepted one dot (`metadata.source`); a structured payload nests, and
one level did not reach `object.error.code` — the shape a stored error
envelope actually has. Indexing is unaffected: like every other type-specific
key, a path is an executor post-filter over the widened scan, so `CAL-W015`
still reports a scan that filled.

**No `ORDER BY` on a path.** Sorting by a payload value would need the same
post-filter widening plus a total order over heterogeneous JSON; it is not
part of this amendment.

---

## What this batch does not do

- **No `| WHERE` stage, no new pipeline stages.** B1 reuses two that exist.
- **No first-of-group.** #210's motivating case wants "one row per session,
  titled from its first event" — B1 gives per-group counts, not a per-group
  representative row. That is a separate amendment if it is wanted.
- **No write-side change.** The paved road for a new corpus is still to store
  the shape you want to read: two fields rather than one payload, because that
  makes the value filterable as well as renderable. B3 and B4 exist for the
  corpora already written, which cannot be reshaped — a grain's fields are
  part of its content address, so changing them changes every hash.
