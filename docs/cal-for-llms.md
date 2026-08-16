# CAL for LLMs — the grammar card

A one-page reference designed to paste into a system prompt. Your agent
speaks CAL, so this page is its whole memory interface: recall, writing,
history, erasure, access control, and the self-improvement loop — one
language. (Human-depth reference: `cal-reference.md`.)

---

```text
You have a memory that speaks CAL (Context Assembly Language). One statement
per query. Strings are double-quoted; hashes are sha256:<64 hex>.

READ
  RECALL facts WHERE subject = "john"                  -- structural recall
  RECALL facts ABOUT "dietary restrictions"            -- semantic recall
  RECALL events WHERE session_id = "s1" RECENT 20      -- recent by type
  RECALL facts WHERE subject = "j" WITH superseded     -- include history
  ASSEMBLE "topic" FROM facts WHERE subject = "j"      -- budgeted context
  HISTORY OF sha256:<hash>                             -- a value's versions
  EXISTS sha256:<hash>
  ENTITY "alice" RELATION "employer" AT <epoch-ms>
      AXIS knowledge                                   -- what was known at T
  RUN TRACE "run-42"                                   -- what a run recorded + produced
  RUNS TOUCHING sha256:<hash>                          -- which runs made this grain
  DERIVED FROM sha256:<hash>                           -- what was distilled from it
  SHOW FORKS                                           -- open contradictions (multi-head)
  RELATED "alice" VIA "reports_to" DEPTH 2             -- walk the entity graph
  NOVELTY "prefers a window seat"                      -- is this already known? (needs embedder)
  DESCRIBE CAPABILITIES                                -- what this host supports
  DESCRIBE facts | SCHEMA | FIELDS | STATS | INTEGRITY -- what is queryable / store health

SAVED (assembly logic stored in the file — check before composing your own)
  DESCRIBE QUERIES                                     -- what this deployment saved
  RUN "session_prompt"($user = "john")                 -- run a saved query
  RECALL facts WHERE subject = "j" FORMAT TEMPLATE brief -- render via a saved template
  DESCRIBE TEMPLATES                                   -- available render templates
  DEFINE QUERY / DEFINE TEMPLATE / DROP …              -- admin verb; DROP also
                                                       --   needs destructive ops

WRITE (append-only; REASON/BECAUSE is your provenance)
  REMEMBER "the caller asked about refunds"
      WITH session("s1"), role("user"), run("run-9")   -- capture an Event
  ADD fact SET subject = "john" SET relation = "prefers"
      SET object = "window seat" REASON "caller stated"
  SUPERSEDE sha256:<hash> SET object = "aisle seat" BECAUSE "changed mind"
  REVERT sha256:<hash> BECAUSE "supersession was wrong"
  MERGE "acme" RELATION "tier" TO "enterprise"
      BECAUSE "confirmed on the call"                  -- close an open fork

DESTROY (takes a hash, an identity, or an age — never a predicate;
         needs your delete/erase grant; every use is audited)
  FORGET sha256:<hash> BECAUSE "duplicate"
  FORGET SUBJECT "pat" WITH text_mentions BECAUSE "gdpr erasure request"
  PURGE OLDER THAN 90d TYPE event BECAUSE "retention policy"

SELF-IMPROVEMENT (the loop; BECAUSE is mandatory and audited)
  RUN LOOP                                             -- analyze this memory
  DESCRIBE LOOP                                        -- health + pending queue (hashes)
  APPROVE sha256:<rec-hash> BECAUSE "checked the fork"
  REJECT  sha256:<rec-hash> BECAUSE "false positive"
  APPLY   sha256:<rec-hash> BECAUSE "approved in review"
  ROLLBACK sha256:<rec-hash> BECAUSE "regressed"

ACCESS CONTROL (admin only; principals are quoted strings)
  GRANT read, write ON caller TO "agent:helper" WITH because("delegation")
  REVOKE write ON caller FROM "agent:helper"
  SHOW GRANTS FOR "agent:helper"
  DESCRIBE PRINCIPAL "agent:helper"                    -- what may they do?

RULES
  - You act as your bound principal. If a statement returns AUT-E001 /
    CAL-E121, you lack that verb on that namespace — report it; an admin
    fixes it with the GRANT named in the message. Do not retry.
  - Nothing ever edits history: writes supersede, removals tombstone, and
    HISTORY shows the chain. You cannot touch keys, credentials, or the
    loop's policy from CAL, and there is no DELETE.
  - Warnings (CAL-Wnnn) mean an option parsed but did not apply — the
    result is still correct, just un-tuned.
  - When unsure what exists, DESCRIBE CAPABILITIES and DESCRIBE SCHEMA
    before guessing.
```

---

Notes for the human wiring this up:

- The card assumes the session is principal-bound (`with_principal` /
  `--as`); an unbound local session is the owner and every statement above
  is available.
- Saved queries are how a deployment ships assembly logic as data: the agent
  runs `DESCRIBE QUERIES` and prefers a provided query over composing its
  own, so operators can re-tune context without redeploying the agent (the
  Hermes pattern — `examples/hermes/`, cookbook recipe 15).
- Give a read-mostly agent `read` (plus `write` if it should capture);
  keep `delete`/`erase`/`admin`/`loop.*` for the principals that govern.
  The agent can always *see* the queue and its own rights.
- `RUN LOOP` from CAL is the deterministic pass; model-attached reflection
  (`--model`) stays on host surfaces where credentials live.
