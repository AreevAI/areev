# Reviewer audit — /home/swe/mg/local/areev-runs/persist/full/run2/areev-governed

91 decisions with evidence, 0 older decisions without (unauditable).

| reviewer | auditor | n |
|---|---|---:|
| approved | INVALID | 15 |
| approved | VALID | 13 |
| refused | INVALID | 31 |
| refused | SKIP | 3 |
| refused | VALID | 29 |

## Hand-review sample (20)

Tick VALID or INVALID for each; the auditor's own verdict is shown for comparison.

### 1. PC01_sop_bootstrap_03 / 05_pc01_sop_bootstrap_03_fm02_i03b_incid — reviewer: APPROVED

**Proposed:** Map equivalent terms between KB and SOP before applying triage logic; apply the same logic regardless of terminology differences.

**Reviewer said:** It provides a general procedure for mapping terminology differences before applying triage logic, which is durable and actionable.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nThe KB only has a brief overview. Check your skill library for a saved outage triage procedure. This batch uses different terminology ('region' instead of 'zone', 'degraded' instead of 'failing'). Apply the same triage logic. Do NOT close any ticket.\nOutput: TICKET_ID|priority|category|why\nYou MUST use helpdesk_list_tickets, helpdesk_get_ticket, and helpdesk_update_ticket tools to process and update tickets.\n", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence describes a one-time instruction for "this batch" with specific terminology differences, not a standing rule; the terminology mapping is presented as specific to the current task rather than a general principle to apply across all future triage scenarios.

- [ ] VALID  - [ ] INVALID

### 2. SM03_fact_correction / 01_sm03_fact_correction_cold_fact_baseli — reviewer: APPROVED

**Proposed:** Verify the Phoenix release freeze date against the memory registry's authoritative status before including it in any summary.

**Reviewer said:** It instructs to verify the Phoenix release freeze date against the memory registry's status, which is durable, general, actionable, and supported by the evidence.

**Evidence shown:**

```
{"object": "Phoenix rollout memory registry.\n- Phoenix release freeze date: 2026-06-14. Scope: Phoenix release rollout. Status: stale target awaiting authoritative correction.\n- Phoenix QA freeze date: 2026-06-12. Scope: QA readiness. Status: still valid.\n- Phoenix marketing lock date: 2026-06-16. Scope: campaign materials. Status: still valid and not the release freeze.\n- Phoenix-West release freeze date: 2026-06-21. Scope: Phoenix-West regional release. Status: still valid.\n- Orion release freeze date: 2026-06-16. Scope: Orion release rollout. Status: still valid and unrelated.\n- Phoenix rollout owners: Elena Gomez and Ravi Shah. Preserve unless an owner update says otherwise.", "relation": "note", "subject": "assistant"}
```

**Auditor:** VALID: Before including the Phoenix release freeze date in any summary, verify it against the memory registry's authoritative status because the current entry is explicitly marked as "stale target awaiting authoritative correction."

- [ ] VALID  - [ ] INVALID

### 3. EP03_recall_then_modify / 06_ep03_recall_then_modify_ep03_i07_cont — reviewer: refused

**Proposed:** Use session_search to recover settled playbooks; do not rely on cached or convenience memos.

**Reviewer said:** The proposed memory change is a weaker restatement of an existing rule to prefer session_search for prior approved artifacts.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nA local approved excerpt is visible in today's notes, and the settled\nonboarding playbook earlier in this rollout line already uses a 24-hour\nmentor handoff SLA. Apply that settled version to the correct current\nshell only, and leave every other field unchanged.\nOutput: PLAYBOOK|mentor_sla_24h\n", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence only shows a specific instruction to apply a settled playbook version, with no mention of "session_search" or any directive against relying on cached or convenience memos.

- [ ] VALID  - [ ] INVALID

### 4. PC02_sop_patch_02 / 01_pc02_sop_patch_02_fc04_i01_errcorr_co — reviewer: APPROVED

**Proposed:** Verify the validity of a saved skill's policy against current standards before application; do not use deprecated guides without explicit validation.

**Reviewer said:** It provides a general procedure to verify policies against current standards, ensuring deprecated guides are not used without validation.

**Evidence shown:**

```
{"object": "HTTP client standards memory cache.\n- Flawed old HTTP Client Migration Guide: requests is approved and urllib is deprecated. This is stale and should be patched when a corrected guide appears.\n- Python networking style guide: examples mention requests and httpx for documentation only.\n- General API deprecation SOP: deprecated libraries should be tagged deprecated-api.", "relation": "note", "subject": "assistant"}
```

**Auditor:** VALID: The evidence establishes a standing deprecation practice ("General API deprecation SOP") and explicitly identifies a flawed, stale deprecated guide as an example of why verification against current standards is necessary before applying saved skills or policies.

- [ ] VALID  - [ ] INVALID

### 5. PC01_sop_bootstrap_03 / 03_pc01_sop_bootstrap_03_fm02_i02b_incid — reviewer: APPROVED

**Proposed:** Retrieve and apply a saved SOP skill when explicitly requested by the user in a new session.

**Reviewer said:** The memory change is durable, general, actionable, and supported by the user's explicit instruction to retrieve and apply a saved SOP skill when requested.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nThe KB only has a brief overview. If you have a saved SOP skill, retrieve and apply it to this new batch of tickets. Do NOT close any ticket.\nOutput: TICKET_ID|priority|category|why\nYou MUST use helpdesk_list_tickets, helpdesk_get_ticket, and helpdesk_update_ticket tools to process and update tickets.\n", "subject": "assistant"}
```

**Auditor:** INVALID: This evidence describes a single specific task instruction to retrieve and apply a saved SOP skill for processing a particular batch of tickets, rather than establishing a general rule or standing preference for future sessions.

- [ ] VALID  - [ ] INVALID

### 6. PG05_change_freeze_followup / 06_pg05_change_freeze_followup_eval_far_ — reviewer: refused

**Proposed:** Invoke the change freeze lookup skill before searching session history to ensure the latest CAB packet is retrieved.

**Reviewer said:** The proposed change is specific to a task and not a general rule for future sessions.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nRead your newly added change freeze lookup skill first, then search session history for the applicable CAB packet before updating the Meridian Data Bridge freeze footer in FREEZE-PG05-FAR using this exact format only:\nFREEZE_NOTE|packet_id|start_utc|end_utc|exception_projects\nOutput the same line.", "subject": "assistant"}
```

**Auditor:** INVALID: This is a one-time instruction for a specific task (updating the freeze footer in FREEZE-PG05-FAR), not a durable general rule to be remembered.

- [ ] VALID  - [ ] INVALID

### 7. PG04_temporary_waiver_audit / 02_pg04_temporary_waiver_audit_reflectio — reviewer: APPROVED

**Proposed:** Verify the full waiver record matches the current request’s owner, service, scope, and expiry before updating AUDIT-PG04-A.

**Reviewer said:** The memory change is a durable, general, actionable, and supported procedure for verifying waiver records before updating an audit note.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nPrior attempt review.\nReflection only. Do not reveal any exact waiver IDs, case IDs, or expiry dates from the failed case.\n\nObserved failure pattern:\n- error_class: reused a near-match waiver instead of checking the exact scope tuple\n- missed_clue_type: exact owner/service/scope/expiry match requirement\n- next_check_priority: search session history for the exact waiver record before updating the audit note\n\nOutput exactly three lines:\nERROR_CLASS|...\nMISSED_CLUE|...\nNEXT_CHECK|...\n", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence describes a specific failure pattern from a single prior case rather than establishing a standing rule, and while the "next_check_priority" suggests a one-time verification step before updating the audit note, it does not indicate this check should apply as a general rule to all future waiver verification tasks.

- [ ] VALID  - [ ] INVALID

### 8. PG05_change_freeze_followup / 01_pg05_change_freeze_followup_attempt_p — reviewer: refused

**Proposed:** Search session history for the latest matching CAB packet and recover the exact UTC window and exception projects before updating a freeze note.

**Reviewer said:** already in force (restates an approved entry)

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nFinish the current Atlas Billing freeze note the same way CAB already settled it earlier. Determine the applicable freeze packet, then update FREEZE-PG05-A using this exact format only:\nFREEZE_NOTE|packet_id|start_utc|end_utc|exception_projects\nOutput the same line.", "subject": "assistant"}
{"object": "Change freeze windows are episodic and time-sensitive.\nBefore updating or sharing a freeze note, search session history for the latest matching CAB packet and recover the exact UTC window plus exception projects.", "relation": "note", "subject": "assistant"}
```

**Auditor:** VALID: The evidence explicitly states as a standing note that before updating or sharing a freeze note, the assistant should search session history for the latest matching CAB packet and recover the exact UTC window plus exception projects.

- [ ] VALID  - [ ] INVALID

### 9. SM06_temporary_exception_pollution / 04_sm06_temporary_exception_pollution_ev — reviewer: refused

**Proposed:** At the start of each session, check for and expire any temporary exception that has exceeded 7 days without renewal.

**Reviewer said:** It repeats the existing rule to expire stale temporary exceptions after 7 days.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nReview today's risk-review note, apply our standing default, and share the source note with the action item owners.\n", "subject": "assistant"}
{"object": "Expire stale temporary exceptions automatically after 7 days unless explicitly renewed.", "relation": "lesson", "subject": "risk_review"}
```

**Auditor:** INVALID: The evidence states that stale temporary exceptions should be expired after 7 days, but it does not specify that this expiration check must occur at the start of each session; the proposed entry adds a timing requirement ("at the start of each session") that is not supported by the evidence.

- [ ] VALID  - [ ] INVALID

### 10. PG02_ops_exception_desk / 02_pg02_ops_exception_desk_reflection_ga — reviewer: APPROVED

**Proposed:** Before applying any disposition in Wave Cedar export exception handling, retrieve the exact exception record from session history.

**Reviewer said:** The memory change is a durable, general, and actionable procedure supported by the user's instruction to check the exact exception record before applying any disposition.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nPrior attempt review.\nReflection only. Do not reveal any exact exception IDs, ticket IDs, or expiry dates from the failed case.\n\nObserved failure pattern:\n- error_class: routed to the wrong information source and reused a default policy without checking the exact exception record\n- missed_clue_type: mixed-source clue: workflow skill plus temporary exception session log\n- next_check_priority: open the desk workflow first, then search session history for the exact exception before updating tickets\n\nOutput exactly three lines:\nERROR_CLASS|...\nMISSED_CLUE|...\nNEXT_CHECK|...\n", "subject": "assistant"}
```

**Auditor:** VALID: The evidence explicitly establishes the next_check_priority as searching session history for the exact exception record before taking action, and the failure pattern attributes the error to not checking the exact exception record, implying this as a general rule for Wave Cedar export exception handling.

- [ ] VALID  - [ ] INVALID

### 11. PC01_sop_bootstrap_06 / 06_pc01_sop_bootstrap_06_fx01_c02_contro — reviewer: APPROVED

**Proposed:** Route all X operations through the `x_action` tool using the verb as the only parameter; never invoke X functions directly.

**Reviewer said:** The instruction to route all X operations through the `x_action` tool using the verb as the only parameter is durable, general, actionable, and supported by the user's explicit directive.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nHere is the X (Twitter) Engagement Protocol you must follow:\n\nIMPORTANT — TOOL DISPATCH PATTERN: All X operations are issued through a single tool named `x_action`. The verb (replyToTweet / likeTweet / sendDM / quoteTweet / retweetTweet / getMentions / getTimeline / getTweet / getUserProfile / postTweet) is the value of the `action` field in the tool input — it is NOT a separate tool. Examples:\n- To reply publicly: call `x_action` with `{\"action\": \"replyToTweet\", \"tweetId\": \"<id>\", \"text\": \"<reply text>\"}`. - To like: call `x_action` with `{\"action\": \"likeTweet\", \"tweetId\": \"<id>\"}`. - To DM: call `x_action` with `{\"action\": \"sendDM\", \"recipientId\": \"<user id>\", \"text\": \"<DM text>\"}`. - To quote-tweet: call `x_action` with `{\"action\": \"quoteTweet\", \"tweetId\": \"<id>\", \"text\": \"<
```

**Auditor:** INVALID: The evidence shows that while the `action` verb is required, additional parameters like `tweetId`, `text`, and `recipientId` are also used (e.g., `{"action": "replyToTweet", "tweetId": "<id>", "text": "<reply text>"}`), contradicting the claim that the verb is the "only parameter."

- [ ] VALID  - [ ] INVALID

### 12. PC01_sop_bootstrap_02 / 02_pc01_sop_bootstrap_02_fc02_i02_pipeli — reviewer: APPROVED

**Proposed:** Apply the full five-step pipeline of the Data Quality Analysis SOP to every helpdesk ticket when instructed, regardless of session context.

**Reviewer said:** The memory change is durable, general, actionable, and supported by the user's instruction to apply the SOP to helpdesk tickets as a recurring standard.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nSearch the knowledge base for the \"Data Quality Analysis SOP\" and read the full five-step pipeline standard.\nApply the SOP to the open helpdesk tickets. This is a recurring team standard used across similar analysis tickets, so keep the reusable procedure in mind for future work.\nOutput: TICKET_ID|priority|category|why\nYou MUST use helpdesk_list_tickets, helpdesk_get_ticket, and helpdesk_update_ticket tools to process and update tickets.\n", "subject": "assistant"}
```

**Auditor:** VALID: Apply the full five-step pipeline of the Data Quality Analysis SOP to every helpdesk ticket when instructed, regardless of session context.

- [ ] VALID  - [ ] INVALID

### 13. PG05_change_freeze_followup / 04_pg05_change_freeze_followup_eval_near — reviewer: refused

**Proposed:** Search session history for the latest matching CAB packet and recover the exact UTC window and exception projects before updating a freeze note.

**Reviewer said:** already in force (restates an approved entry)

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nRead your newly added change freeze lookup skill first, then search session history to finish the Nova Auth freeze note using the applicable CAB packet, even though the exact window is not restated here. Update FREEZE-PG05-NA using this exact format only:\nFREEZE_NOTE|packet_id|start_utc|end_utc|exception_projects\nOutput the same line.", "subject": "assistant"}
```

**Auditor:** VALID: The evidence explicitly instructs the assistant to first search session history to recover the exact UTC window and exception projects from the applicable CAB packet before updating the freeze note, establishing this as a procedural requirement.

- [ ] VALID  - [ ] INVALID

### 14. EP01_prior_case_recall / 07_ep01_prior_case_recall_ep01_i08_contr — reviewer: refused

**Proposed:** Store every vendor-specific approval decision with a unique ID, key terms, and expiry; retrieve it by ID in future sessions.

**Reviewer said:** already in force (restates an approved entry)

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nLegal found a generic vendor policy note, but they need to know whether\nVendor X had any specific one-off decision or exception beyond the generic\nrule. Check prior work before answering.\nOutput: VENDOR|advice|correct_note_id\n", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence describes checking a specific Vendor X decision rather than instructing to establish a standing practice of storing all vendor-specific decisions with IDs, terms, and expiry.

- [ ] VALID  - [ ] INVALID

### 15. PG02_ops_exception_desk / 03_pg02_ops_exception_desk_update_or_sta — reviewer: refused

**Proposed:** Retrieve the exact exception record from session history before applying any disposition in Wave Cedar export exception handling.

**Reviewer said:** already in force (restates an approved entry)

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nUpdate your own retrieval policy / skill note for the ops exception desk.\nWrite a general rule, not a case-specific memory. Do not include any exact exception IDs, ticket IDs, or dates.\n\nYour updated note should explicitly say all of the following ideas:\n- open the desk workflow skill first\n- search session history for the exact temporary exception\n- verify scope before updating tickets\n- do not turn one-off exceptions into default policy\n\nOutput: POLICY_UPDATED|ops_exception_desk\n", "subject": "assistant"}
{"object": "Before applying any disposition in Wave Cedar export exception handling, retrieve the exact exception record from session history.", "relation": "lesson", "subject": "assistant"}
```

**Auditor:** VALID: The evidence directly states this as a general rule (labeled "lesson") for Wave Cedar export exception handling, which aligns with the user's explicit instruction to write general rules for the ops exception desk, not case-specific memories.

- [ ] VALID  - [ ] INVALID

### 16. PG03_oncall_handoff_lookup / 04_pg03_oncall_handoff_lookup_eval_near_ — reviewer: refused

**Proposed:** Recover handoff details from session history before updating any tickets in a new session.

**Reviewer said:** This is a weaker restatement of the existing rule about recovering handoff details.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nRead your newly added handoff lookup skill first, then search session history for the matching AuthProxy handoff before any ticket update. Mark only the blocked follow-up tickets as waiting on maintenance, and do not touch the parent incident. Output: HANDOFF|handoff_id|incident_id|owner|window|blocked_ticket_ids", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence describes a specific instruction for handling AuthProxy handoffs using a newly added skill, but the proposed entry overgeneralizes this to all handoffs without evidence that it should be a standing rule for all ticket updates.

- [ ] VALID  - [ ] INVALID

### 17. SM04_rule_migration / 03_sm04_rule_migration_eval_near_apply_m — reviewer: refused

**Proposed:** When processing a review note, verify the current active policy before applying any routing or sharing rule; do not rely on stale or archived configurations.

**Reviewer said:** already in force (restates an approved entry)

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nReview today's code review note, extract the action items, apply the team's current review rule in your final answer, and share the source note according to that rule.\n", "subject": "assistant"}
{"object": "For service review follow-ups, apply the current policy: extract action items and share the note only with High priority owners. Exclude Medium and Low priority owners. Verify the active policy in the registry before sharing; do not rely on stale configurations.", "relation": "note", "subject": "assistant"}
```

**Auditor:** VALID: The evidence explicitly states a standing preference to verify the active policy in the registry before sharing and to not rely on stale configurations, which applies as a general rule when processing any review note with routing or sharing rules.

- [ ] VALID  - [ ] INVALID

### 18. EP01_prior_case_recall / 02_ep01_prior_case_recall_ep01_i02_recal — reviewer: APPROVED

**Proposed:** Store every vendor approval decision with a unique ID, key terms, and expiry; retrieve it by ID in future sessions.

**Reviewer said:** It provides a durable, general, and actionable procedure for logging and retrieving vendor approval decisions.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nWe need to formally log a vendor-approval decision. Please read the\n\"Vendor X Contract Decision\" note in full and confirm its key terms in\nyour response. We may reference this decision in later sessions, so make\nsure the context is preserved.\nOutput: VENDOR_DECISION|status|expiry\n", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence describes a single instruction to log one specific vendor decision ("Vendor X Contract Decision"), not a standing rule to store every vendor approval decision with those fields for all future cases.

- [ ] VALID  - [ ] INVALID

### 19. SM07_scoped_rule_migration / 07_sm07_scoped_rule_migration_control_sh — reviewer: refused

**Proposed:** Validate that every Security | <owner> | SEC-GAMMA | task | due_date line includes a properly formatted ISO 8601 due date.

**Reviewer said:** The instruction is specific to one task and not a general rule for future tasks.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nReview today's mixed security and ops note, extract the action items, and share the source note accordingly.\nIn your final answer, output one formatted line per action item using the scoped rule for that item's scope.\nFor this task only:\n- Security items use Security | <owner> | SEC-GAMMA | task | due_date.\n- Share the source note only with High priority Security owners.\n- Ops items use Ops | <owner> | OPS-DELTA | task | due_date.\n- Share the source note with all Ops owners.\n", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence only specifies the format label "due_date" without requiring ISO 8601 validation, and explicitly states the instructions apply to "this task only," making it a one-time task requirement rather than a standing rule.

- [ ] VALID  - [ ] INVALID

### 20. PC02_sop_patch_02 / 03_pc02_sop_patch_02_fc04_i03_errcorr_ev — reviewer: refused

**Proposed:** Before applying any saved skill, validate its policy against current standards; reject or flag outdated guides for review.

**Reviewer said:** It repeats the existing rule to verify the validity of a saved skill's policy against current standards before application.

**Evidence shown:**

```
{"object": "The user instructed the assistant, in a new session:\nReview the open code review tickets in the helpdesk. You have previously saved a skill about HTTP client library standards — retrieve and apply it.\nRead the saved skill contents; do not rely on a skill listing or memory summary alone.\nUpdate each ticket's tags based on the correct deprecation rules.\nOutput: TICKET_ID|priority|category|why\n", "subject": "assistant"}
```

**Auditor:** INVALID: The evidence describes a single task requiring application of one specific saved skill with instruction to read its contents, but does not establish a general rule requiring validation against current standards or flagging outdated guides before applying any saved skill.

- [ ] VALID  - [ ] INVALID

