# examples/agents

Vertical agents built on Areev — end-to-end workflows where Areev is the memory
and the runtime, and the business system on either end is somebody else's.

The rest of [`examples/`](../) teaches **one seam at a time**: a policy file, the
`--llm-cmd` protocol, the analyzer contract. These solve a **job**: mail arrives,
a workflow runs, a human approves (and corrects, by reply, until they do), a
system of record gets written, and what happened is recallable afterwards —
then the loop reads those runs back and proposes a fix a person signs.

## The index

Each agent starts from a problem a real desk has, and its act scripts prove
the payoff rather than narrate it — every claim in the middle column below
is an assertion CI runs keyless on every release. Start from the problem
that looks like yours.

| Agent | The problem | What you get | Teaches |
|---|---|---|---|
| [`invoice-to-accounting/`](invoice-to-accounting/) | AP runs on email: invoices land in a mailbox, approvals crawl through reply chains, and every correction dies in someone's sent folder instead of teaching the system anything | Invoices posted to the books with a named approver on every row; corrections taken **by reply** around a bounded cycle until the approver says yes — and the desk reads its own history and proposes fixes a person signs, so week two runs on week one's lessons | the canonical agent shape — **one agent ×3 languages** (Python, TypeScript, Rust), one file each, minting one content-addressed plan |
| [`sanctions-screening/`](sanctions-screening/) | An examiner will eventually ask *which exact version of the rule screened this payment* — and when the rule is a script on a box, the answer is a changelog and a shrug. Nothing stops a desk quietly running last quarter's rule | Every ledger row names the exact rule bytes that decided it; a rule change is a signed chain (blob → tool → plan → trigger), and a desk whose rule has moved ahead of its operator's pin **refuses to run** rather than running stale — the revised rule then catches an exact match the old one was blind to | **code-carrying tools**: the rule is a CAS blob under a host pin |
| [`incident-response/`](incident-response/) | Every page starts from zero: the cause of last month's identical incident is in a postmortem nobody reopens at 3am, and alerting vendors retry — a duplicate page that restarts a remediation is how a bad night becomes a bad week | The next identical page arrives with its cause attached (*"seen 1 time on this service and signal; last cause: …"*), redelivered alerts start nothing, and every production action carries the engineer who approved it and their written reason. It still pages — the payoff is a better proposal at the same gate, not a removed human | **webhook triggers** — a push source needs no connector; plus `manual` replay |
| [`hiring-screening/`](hiring-screening/) | Screening candidates is high-risk under the EU AI Act, and Article 14 wants oversight you can *demonstrate* — a policy document cannot show that a person could intervene, or that one actually did | Every advance/reject parks for a named recruiter, and the Article 14 report is **measured from the run journal**: the gate, the authorized approvers, the ceilings, the kill switch's real cancel-to-drain time. Outcomes and human reviews are counted, and must be equal | the oversight report; approval grants living in the file |
| [`insurance-documents/`](insurance-documents/) | A claims file answers "what does the policy say now" — but the money question is *what cover was in force on the date of loss, and when did we come to know it*. Backdated endorsements make those diverge, and a one-clock memory pays the wrong amount, confidently | A coverage picture on two clocks: the claim is assessed at the 500,000 in force on the date of loss while the file's current picture says 750,000 — and "what was true in March" and "what we told the insured in March" both stay answerable after the file has moved on | **bi-temporal as-of reads** (`world` vs `knowledge`); the entity graph for accumulation |
| [`rcm-optimization/`](rcm-optimization/) | A payer remittance lands carrying a wall of denied claims — how many is unknowable when the plan is written — and the same denial causes recur for months because the fix lives in one biller's head | A screening task per denial, spawned at runtime and folded back deterministically, replayable byte-for-byte; one signed approval later, next week's remittance classifies itself — three of five denials never spend a person's attention, and the ones that do carry a written reason | **`$send` dynamic fan-out** + declared reducers: 6 nodes, 11 tasks |
| [`trade-surveillance/`](trade-surveillance/) | Neither feed is interesting alone — a block order is Tuesday, a rebalance notice is Tuesday. Alert on each and the desk drowns in false positives; correlate them yourself and you are building a correlation service before the first case opens | One case per correlated pair on the same instrument inside a window: eight signals become three cases, **zero auto-closed**, each parked for an analyst and arriving with how this *pattern* was dispositioned before | **composite triggers** with correlation windows |
| [`due-diligence/`](due-diligence/) | Research is open-ended and expensive, and most frameworks treat a spend cap as an exception: you catch it, you log it, you lose the work | A ceiling that ends the run cleanly with every finding journaled; the analyst forks under a raised ceiling or ships the partial report, and a partner signs it out — never the person who raised the ceiling. After one signed desk rule, **the same ceiling buys three times the material findings** | **`BudgetExhausted` as a resumable state**; the replay verbs `run_verify` / `run_shadow` / `run_oversight_report` |
| [`clinical-referrals/`](clinical-referrals/) | The desk cannot do its job without an outside coding service, and cannot use one without disclosing patient records to it — and privacy logic sprinkled through every integration is the version that drifts | One policy on one namespace, and every model-facing read leaves as typed placeholders: the verbatim wire log is checked against every identifier in every fixture — **zero on the wire** — while the clinician's letter comes back fully rehydrated and a signed correction becomes the clinic's own triage rule | **pseudonymization on egress**, audited on the wire; extending Tier-0 with a NER chain |
| [`data-subject-requests/`](data-subject-requests/) | A DSAR that discloses one set of data and erases another is a compliance failure — and when export and delete are separate code paths, nothing prevents it. Worse: an audit log that names the erased person has undone the erasure it records | The report and the erasure share **one selector**, asserted equal count-for-count per namespace; a requester who has not proven who they are is refused before anything is read; and the register proves every act with an approver, a written reason and a fingerprint — never the person | **erasure**: `subject_report` / `forget_subject` / `subject_bundle`, retention sweeps, consent withdrawal |

Every agent runs the same way — `python/smoke.sh` then `python/improve.sh`,
keyless, from committed fixtures — or all at once with
[`run-smokes.sh`](run-smokes.sh), which is what CI runs.
`invoice-to-accounting` additionally ships TypeScript and Rust stacks. Two
rows carry standing caveats: [`sanctions-screening/`](sanctions-screening/)
and [`trade-surveillance/`](trade-surveillance/) are teaching examples of
their mechanisms, **not** compliant compliance programmes — each README
carries the full caveat. And [`data-subject-requests/`](data-subject-requests/)
deliberately has **no trigger**: starting an irreversible erasure from an
unauthenticated mailbox is the wrong default.

The **Teaches** column is a uniqueness promise: the capability each agent
exercises appears in no other example, so between them the ten cover what the
[how-to guide](../how-to-create-an-areev-agent.md) claims. New agents follow
the same shape. Start from the invoice agent — its
[`CLAUDE.md`](invoice-to-accounting/CLAUDE.md) is the working contract — and
[`sanctions-screening/`](sanctions-screening/) for the newer conventions
(code as a grain, a pin derived from the workshop, a revision that walks its
whole reference chain).

## One agent, one or more languages

An agent ships as parallel single-file stacks — `python/agent.py`,
`typescript/agent.mts`, `rust/src/main.rs` — each embedding Areev through
its own binding and all exposing the same subcommands. The agent-level
`smoke.sh`/`improve.sh` hold the assertions **once**; per-language wrappers
are three lines, so adding a language is a wrapper plus an agent file.

`invoice-to-accounting` is the one that ships in all three today; the other
nine are Python, which is why their act scripts are written
language-neutrally — porting one adds a stack without touching an assertion.
Because every seeder pins `created_at`, all stacks of one agent must mint the
identical plan hash, and [`run-smokes.sh`](run-smokes.sh) asserts it: a stack
cannot silently drift from its siblings.

## What every agent here is made of

Three seams, one shape — JSON on stdin, JSON on stdout, one process per
invocation:

| Leg | Seam | Contract |
|---|---|---|
| **Inbound** — what wakes it up | a `polling` Trigger + a connector, or a `webhook` / `manual` Trigger and **no connector at all** (your listener calls `trigger deliver`) | [`docs/triggers.md`](../../docs/triggers.md), providers: [`docs/email-providers.md`](docs/email-providers.md) |
| **Work** — what it does | a Workflow grain run by `areev run`, host tools via `--tool-cmd` | [`docs/run.md`](../../docs/run.md) |
| **Model** — where judgment needs language | `--llm-cmd`, optional | [`../llm/`](../llm/) |

So an agent example adds **no dependencies to this repo**. A vendor SDK, if you
need one, lives in your copy of the connector script — never here. That is the
same posture the core takes (no clap, no HTTP framework, no MCP SDK), applied
to examples.

## Two paths, always

- **Keyless** — mock connector, mock tools, committed fixtures. No credentials,
  no network, no model key. This is what the smokes and CI run, and it is the
  reason these examples stay correct across releases instead of quietly
  rotting. Run them all: [`run-smokes.sh`](run-smokes.sh); how it works:
  [`docs/testing.md`](docs/testing.md).
- **Live** — real connectors and real tools, opt-in behind env vars
  ([`docs/email-providers.md`](docs/email-providers.md)); deployment,
  scheduling, and the embedded-vs-Postgres decision:
  [`docs/deploy.md`](docs/deploy.md).

## Where the human goes

Each one parks on a `client` node — an approval a person answers with
`areev run respond --as <principal>` (here: by replying to an email) —
because these are agents that spend money or sign things. The principal who
triggered the ask structurally cannot answer it (separation of duties), and
the approver's identity *is* the audit record. A correction the human makes
on the way to "yes" is recorded as memory, which is where self-improvement
actually starts. An agent example that approves its own work is teaching the
wrong lesson.

## Adding one

Layout, the keyless-floor rule, the no-new-dependency rule, and the indexes
that need updating in the same commit are in the `areev-examples` skill
(`.claude/skills/areev-examples/`); the test contract is
[`docs/testing.md`](docs/testing.md).
