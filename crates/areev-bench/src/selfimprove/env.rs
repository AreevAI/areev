//! The hidden-rules support-desk environment (SELFIMPROVE.md "The environment").
//!
//! Fully deterministic and in-process: everything is drawn from the crate's
//! seeded `Xorshift`, timestamps are generated data on a fixed 2026 base
//! (never a clock read), and nothing observable depends on hash-map
//! iteration order (`BTreeMap`/`Vec` only). The agent gets `tool_schemas()`
//! and a task prompt; the six operating rules live only in `Env::execute`
//! and surface as the FROZEN error codes of the module contract (mod.rs).
//!
//! Split discipline: `Experience` and `Eval` draw names/emails from disjoint
//! pools on distinct generator streams, and `Eval` prompts are paraphrased
//! templates — the eval set is held-out by construction, fixed per seed.
//!
//! Prompt shapes (stable — agent.rs's deterministic mock parses these):
//!   refund_small / refund_large
//!     exp : Customer {name} ({email}) was double-charged on their last
//!           invoice. Refund ${amount} to their account and confirm what you did.
//!     eval: {name} <{email}> reports a duplicate charge. Please put
//!           ${amount} back on their account and summarize the outcome.
//!   refund_and_cancel
//!     exp : Customer {name} ({email}) is closing their account. Refund
//!           ${amount} for the unused period and cancel their subscription.
//!     eval: {name} <{email}> has decided to leave. Issue a ${amount} refund
//!           for the remaining period and shut down their subscription.
//!   log_case
//!     exp : Customer {name} ({email}) called about {topic}. Log a case with
//!           the note '{note}', timestamped {YYYY-MM-DD HH:MM}, we're UTC{±H:MM}.
//!     eval: {name} <{email}> rang regarding {topic}. Record a case with note
//!           '{note}' for {YYYY-MM-DD HH:MM} local time (our office is UTC{±H:MM}).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use super::{RuleId, ToolOutcome};
use crate::Xorshift;

/// `search_customers` page size; pools are 6–10, so page 1 always says
/// `has_more: true` and page 2 holds 1–5 customers.
const PAGE_SIZE: usize = 5;
/// R3 threshold: refunds strictly over $100 need an approval token.
const APPROVAL_CENTS: u64 = 100 * 100;
/// R6: the value carried in every `rate_limited` error, and the minimum
/// `wait(seconds)` that opens the limiter.
const RETRY_AFTER_S: u64 = 2;

const TEMPLATES: [&str; 4] = ["refund_small", "refund_large", "refund_and_cancel", "log_case"];
/// The desk's account mix. `enterprise` appears twice deliberately: R8 is a
/// SEGMENT rule, so the segment's prevalence is what decides whether it can
/// be measured at all, and a uniform third of a quarter of the tasks left it
/// exercised by 9 of 100 held-out tasks — a per-rule table nobody should
/// believe (`every_rule_gets_enough_opportunities_to_be_measurable` is the
/// guard). One draw either way, so the RNG stream and every prompt are
/// unchanged; only the `plan` column moves.
const PLANS: [&str; 4] = ["basic", "pro", "enterprise", "enterprise"];

// Disjoint per-split name pools — the split-holdout guarantee is structural,
// not statistical. Emails also diverge on the per-split domain.
const EXP_FIRST: [&str; 16] = [
    "Alice", "Bob", "Carol", "Dave", "Erin", "Frank", "Grace", "Heidi", "Ivan", "Judy", "Karl",
    "Lena", "Marta", "Nils", "Otto", "Priya",
];
const EXP_LAST: [&str; 12] = [
    "Alvarez", "Brennan", "Chen", "Dumont", "Eriksen", "Fontana", "Gruber", "Hoshino", "Iyer",
    "Jansen", "Kowalski", "Lindqvist",
];
const EVAL_FIRST: [&str; 16] = [
    "Quinn", "Rosa", "Sam", "Tara", "Ulrich", "Vera", "Wade", "Ximena", "Yusuf", "Zoe", "Anouk",
    "Boris", "Cleo", "Dario", "Esme", "Farid",
];
const EVAL_LAST: [&str; 12] = [
    "Moreau", "Novak", "Okafor", "Petrov", "Quispe", "Rahman", "Sato", "Toledo", "Ueda", "Vance",
    "Whitfield", "Zhukov",
];

/// R10's threshold: a refund above this must also be escalated as a
/// high-priority case, once a supervisor has said so.
const ESCALATE_CENTS: u64 = 500 * 100;

/// R10, in the words a person would actually use. Delivered ONCE, during the
/// experience phase, as a note from a human — not as an error, not as a
/// pattern across outcomes.
///
/// This is the archetype the other nine miss. R1-R6 announce themselves with
/// an error code and R7-R9 with a correlation across many outcomes, but both
/// are things the desk INFERS from its own failures. A great deal of real
/// learning is not inferred at all: somebody tells you. A supervisor says
/// "from now on, escalate these", and a desk worth the name applies it to
/// every future ticket of that shape — from one sentence, before it has ever
/// failed for that reason.
pub const SUPERVISOR_NOTE: &str = "Note from the billing lead, after reviewing \
this week's refunds: from now on any refund over $500 must ALSO be logged as a \
case with priority high, so finance sees it in the escalation queue. Apply this \
to every future refund of that size, not just this one.";

/// R9: which [`TOPICS`] are regulated and must be filed at high priority. A
/// recurring class of work the desk has no branch for — the distributional
/// archetype. Two of the four, for the same measurability reason as [`PLANS`]:
/// one topic left the rule exercised by 7 of 100 held-out tasks. Real
/// regulated categories are rarely singular either.
const REGULATED_TOPICS: [usize; 2] = [0, 3];

const TOPICS: [(&str, &str); 4] = [
    ("a billing discrepancy", "customer disputes the last invoice"),
    ("login trouble", "customer locked out after password rotation"),
    ("a renewal question", "customer asks if the discount survives renewal"),
    ("a data export request", "customer wants a full account data export"),
];

/// (prompt display, minutes east of UTC). All nonzero, so the naive
/// pass-the-local-time-through answer is always wrong.
const OFFSETS: [(&str, i64); 8] = [
    ("+5:30", 330),
    ("-8:00", -480),
    ("+1:00", 60),
    ("+9:00", 540),
    ("-5:00", -300),
    ("+10:00", 600),
    ("-3:30", -210),
    ("+3:00", 180),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Experience,
    Eval,
}

#[derive(Debug, Clone)]
struct Customer {
    id: String,
    name: String,
    email: String,
    sub_id: String,
    plan: &'static str,
    balance: u32,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    pub template: &'static str,
    /// The rules THIS task can actually trip — the template's rule surface
    /// narrowed by the seeded conditions (R1 only when the target sits on
    /// page 2, R3 only when the amount crosses $100, R6 only when the
    /// limiter is armed), so per-rule `opportunities` counts stay honest.
    pub rules_exercised: Vec<RuleId>,
    // Hidden spec — the ground truth Env scores against. Never in the prompt.
    pool: Vec<Customer>,
    target: usize,
    amount_cents: u64,
    expected_timestamp: String,
    limiter_armed: bool,
    /// R10: this task carries the supervisor's note. Exactly one experience
    /// task does; it is recorded into memory, never shown in the prompt.
    supervisor_note: Option<&'static str>,
    /// R10: this task must escalate. Only ever true on the HELD-OUT split —
    /// the note announces a policy that takes effect going forward, so the
    /// experience phase contains no instance of it mattering. That is what
    /// makes it a clean test: no amount of outcome correlation over the
    /// experience data can recover it, because the data does not contain it.
    /// Only reading what the person wrote works.
    escalate_required: bool,
    /// R9: this case is about the regulated topic, so it must be filed at
    /// high priority. Nothing in the prompt says so and no tool refuses a
    /// case without it — the whole point of the silent archetypes.
    regulated_topic: bool,
}

impl Task {
    /// The supervisor's note, when this task carries one.
    pub fn supervisor_note(&self) -> Option<&'static str> {
        self.supervisor_note
    }

    /// Everything about this task that a re-run must reproduce, on one line.
    ///
    /// Deliberately covers the HIDDEN spec (pool, target, amount, expected
    /// timestamp, armed limiter) as well as the prompt: two tasks can read
    /// identically to the model and still score differently, so pinning only
    /// the visible half would let ground truth drift under a published run
    /// without any gate noticing. Used by `tests/reproducibility.rs`.
    pub fn fingerprint(&self) -> String {
        let pool: Vec<String> = self
            .pool
            .iter()
            .map(|c| format!("{}:{}:{}:{}:{}", c.id, c.email, c.sub_id, c.plan, c.balance))
            .collect();
        format!(
            "{}|{}|rules={}|target={}|amount_cents={}|ts={}|limiter={}|regulated={}|escalate={}|note={}|pool={}|prompt={}",
            self.id,
            self.template,
            self.rules_exercised.join(","),
            self.target,
            self.amount_cents,
            self.expected_timestamp,
            self.limiter_armed,
            self.regulated_topic,
            self.escalate_required,
            self.supervisor_note.is_some(),
            pool.join(";"),
            self.prompt,
        )
    }
}

/// Deterministic task generation: same (seed, split, n) → identical tasks.
pub fn gen_tasks(seed: u64, split: Split, n: usize) -> Vec<Task> {
    let (salt, prefix, firsts, lasts, domain): (u64, &str, &[&str], &[&str], &str) = match split {
        Split::Experience => (0xE58A, "exp", &EXP_FIRST, &EXP_LAST, "novabank.example"),
        Split::Eval => (0x11A7, "eval", &EVAL_FIRST, &EVAL_LAST, "meridianco.example"),
    };
    // `| 1` keeps xorshift off its zero fixed point; the warmup decorrelates
    // nearby seeds.
    let mut rng = Xorshift((seed ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1);
    for _ in 0..8 {
        rng.next();
    }
    (0..n).map(|i| gen_task(&mut rng, i, prefix, split, firsts, lasts, domain)).collect()
}

#[allow(clippy::too_many_arguments)] // internal; all args flow from gen_tasks
fn gen_task(
    rng: &mut Xorshift,
    i: usize,
    prefix: &str,
    split: Split,
    firsts: &[&str],
    lasts: &[&str],
    domain: &str,
) -> Task {
    // Rotation keeps template coverage balanced at any n.
    let template = TEMPLATES[i % TEMPLATES.len()];
    let pool_size = 6 + (rng.next() % 5) as usize; // 6..=10

    let mut used_pairs = BTreeSet::new();
    let mut used_ids = BTreeSet::new();
    let mut pool: Vec<Customer> = Vec::with_capacity(pool_size);
    while pool.len() < pool_size {
        let fi = (rng.next() % firsts.len() as u64) as usize;
        let li = (rng.next() % lasts.len() as u64) as usize;
        if !used_pairs.insert((fi, li)) {
            continue; // names must be unique within a pool (prompts name the target)
        }
        let id = format!("cus_{:08x}", rng.next() as u32);
        if !used_ids.insert(id.clone()) {
            continue;
        }
        pool.push(Customer {
            id,
            name: format!("{} {}", firsts[fi], lasts[li]),
            email: format!("{}.{}@{}", firsts[fi].to_lowercase(), lasts[li].to_lowercase(), domain),
            sub_id: format!("sub_{:08x}", rng.next() as u32),
            plan: PLANS[(rng.next() % PLANS.len() as u64) as usize],
            balance: (10 + rng.next() % 491) as u32,
        });
    }

    let target_on_page2 = rng.next().is_multiple_of(2); // R1 armed on ~50% of tasks
    let target = if target_on_page2 {
        PAGE_SIZE + (rng.next() % (pool_size - PAGE_SIZE) as u64) as usize
    } else {
        (rng.next() % PAGE_SIZE as u64) as usize
    };
    let limiter_armed = rng.next() % 10 < 4; // R6 armed on ~40% of tasks

    let amount_dollars: u64 = match template {
        "refund_small" => 10 + rng.next() % 91,                       // 10..=100
        "refund_large" | "refund_and_cancel" => 101 + rng.next() % 800, // 101..=900
        _ => 0,
    };

    let name = pool[target].name.clone();
    let email = pool[target].email.clone();
    let target_plan = pool[target].plan;
    let mut expected_timestamp = String::new();
    let mut regulated_topic = false;
    let prompt = if template == "log_case" {
        let (off_disp, off_min) = OFFSETS[(rng.next() % OFFSETS.len() as u64) as usize];
        let month = 1 + (rng.next() % 12) as u32;
        // Day 2..=27 keeps the UTC conversion inside the month for every
        // offset in the table, so no calendar arithmetic is needed.
        let day = 2 + (rng.next() % 26) as u32;
        let hour = (rng.next() % 24) as u32;
        let minute = [0u32, 15, 30, 45][(rng.next() % 4) as usize];
        expected_timestamp = utc_from_local(month, day, hour, minute, off_min);
        let local = format!("2026-{month:02}-{day:02} {hour:02}:{minute:02}");
        let topic_idx = (rng.next() % TOPICS.len() as u64) as usize;
        let (topic, note) = TOPICS[topic_idx];
        regulated_topic = REGULATED_TOPICS.contains(&topic_idx);
        match split {
            Split::Experience => format!(
                "Customer {name} ({email}) called about {topic}. Log a case with the note \
                 '{note}', timestamped {local}, we're UTC{off_disp}."
            ),
            Split::Eval => format!(
                "{name} <{email}> rang regarding {topic}. Record a case with note '{note}' \
                 for {local} local time (our office is UTC{off_disp})."
            ),
        }
    } else if template == "refund_and_cancel" {
        match split {
            Split::Experience => format!(
                "Customer {name} ({email}) is closing their account. Refund ${amount_dollars} \
                 for the unused period and cancel their subscription."
            ),
            Split::Eval => format!(
                "{name} <{email}> has decided to leave. Issue a ${amount_dollars} refund for \
                 the remaining period and shut down their subscription."
            ),
        }
    } else {
        match split {
            Split::Experience => format!(
                "Customer {name} ({email}) was double-charged on their last invoice. Refund \
                 ${amount_dollars} to their account and confirm what you did."
            ),
            Split::Eval => format!(
                "{name} <{email}> reports a duplicate charge. Please put ${amount_dollars} \
                 back on their account and summarize the outcome."
            ),
        }
    };

    let amount_cents = amount_dollars * 100;
    let mut rules: Vec<RuleId> = Vec::new();
    if target_on_page2 {
        rules.push("R1");
    }
    rules.push("R2"); // every template acts on ids, so id discipline is always in play
    if amount_cents > APPROVAL_CENTS {
        rules.push("R3");
    }
    if template == "refund_and_cancel" {
        rules.push("R4");
    }
    if template == "log_case" {
        rules.push("R5");
    }
    if limiter_armed {
        rules.push("R6");
    }
    // ---- the SILENT archetypes (R7-R9): no tool ever errors for these ----
    // R7 rides every closure; R8 only BITES under $100 (over it, R3 already
    // forces the token, so the rule is satisfied for free and this task is
    // no opportunity to learn it); R9 rides the regulated topic only.
    if template == "refund_and_cancel" {
        rules.push("R7");
    }
    if template != "log_case" && target_plan == "enterprise" && amount_cents <= APPROVAL_CENTS {
        rules.push("R8");
    }
    if regulated_topic {
        rules.push("R9");
    }
    // R10 — the INSTRUCTED rule. The note lands once, in the experience
    // phase; the requirement only ever binds on the held-out split, because
    // the supervisor is announcing a policy for future work. An agent that
    // never read the note has no way to know, and no failure to learn from.
    let is_refund = template != "log_case";
    let escalate_required =
        split == Split::Eval && is_refund && amount_cents > ESCALATE_CENTS;
    if escalate_required {
        rules.push("R10");
    }
    let supervisor_note = if split == Split::Experience && i == NOTE_TASK_INDEX {
        Some(SUPERVISOR_NOTE)
    } else {
        None
    };

    Task {
        id: format!("{prefix}-{i:04}"),
        prompt,
        template,
        rules_exercised: rules,
        pool,
        target,
        amount_cents,
        expected_timestamp,
        limiter_armed,
        regulated_topic,
        supervisor_note,
        escalate_required,
    }
}

/// Which experience task carries the supervisor's note. Early enough that
/// every later task could have benefited, which is the point: a desk that
/// only learns from its own failures had 290-odd chances to notice and could
/// not, because nothing failed.
const NOTE_TASK_INDEX: usize = 7;

/// Convert a 2026 local wall time to `YYYY-MM-DDTHH:MM:SSZ`. Callers keep
/// `day` in 2..=27 so the shift never leaves the month.
fn utc_from_local(month: u32, day: u32, hour: u32, minute: u32, offset_min: i64) -> String {
    let local = (day as i64 - 1) * 1440 + (hour as i64) * 60 + minute as i64;
    let utc = local - offset_min;
    let d = utc.div_euclid(1440) + 1;
    let rem = utc.rem_euclid(1440);
    format!("2026-{:02}-{:02}T{:02}:{:02}:00Z", month, d, rem / 60, rem % 60)
}

/// Strict `YYYY-MM-DDTHH:MM:SSZ` shape check (R5). Format-only: a
/// well-formed but un-converted timestamp is accepted here and fails
/// scoring instead — exactly the trap a naive "just append Z" agent hits.
fn is_utc_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 20 {
        return false;
    }
    const DIGITS: [usize; 14] = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    if !DIGITS.iter().all(|&i| b[i].is_ascii_digit()) {
        return false;
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z'
    {
        return false;
    }
    let n2 = |i: usize| (b[i] - b'0') as u32 * 10 + (b[i + 1] - b'0') as u32;
    (1..=12).contains(&n2(5)) && (1..=31).contains(&n2(8)) && n2(11) <= 23 && n2(14) <= 59 && n2(17) <= 59
}

/// OpenAI-style tools array for the seven tools in the module contract.
/// Descriptions stay rule-silent: the schemas are given, the rules are not.
pub fn tool_schemas() -> Value {
    let f = |name: &str, desc: &str, params: Value| {
        json!({ "type": "function", "function": { "name": name, "description": desc, "parameters": params } })
    };
    Value::Array(vec![
        f(
            "search_customers",
            "Search customers by name or email. Returns a page of matches.",
            json!({ "type": "object", "properties": {
                "query": { "type": "string", "description": "name or email to search for" },
                "page": { "type": "integer", "description": "result page, starting at 1" }
            }, "required": ["query"] }),
        ),
        f(
            "get_customer",
            "Fetch one customer record by id.",
            json!({ "type": "object", "properties": {
                "id": { "type": "string", "description": "customer id" }
            }, "required": ["id"] }),
        ),
        f(
            "request_approval",
            "Request a manager approval token.",
            json!({ "type": "object", "properties": {
                "reason": { "type": "string", "description": "why approval is needed" }
            }, "required": ["reason"] }),
        ),
        f(
            "refund",
            "Issue a refund to a customer.",
            json!({ "type": "object", "properties": {
                "customer_id": { "type": "string" },
                "amount": { "type": "number", "description": "refund amount in dollars" },
                "approval_token": { "type": "string", "description": "approval token, if you have one" }
            }, "required": ["customer_id", "amount"] }),
        ),
        f(
            "cancel_subscription",
            "Cancel a customer's subscription.",
            json!({ "type": "object", "properties": {
                "customer_id": { "type": "string" }
            }, "required": ["customer_id"] }),
        ),
        f(
            "log_case",
            "Log a support case for a customer.",
            json!({ "type": "object", "properties": {
                "customer_id": { "type": "string" },
                "note": { "type": "string" },
                "timestamp": { "type": "string", "description": "when the case happened" },
                "priority": { "type": "string", "description": "case priority, e.g. low, normal, high" }
            }, "required": ["customer_id", "note", "timestamp"] }),
        ),
        f(
            "wait",
            "Wait for a number of seconds.",
            json!({ "type": "object", "properties": {
                "seconds": { "type": "number" }
            }, "required": ["seconds"] }),
        ),
    ])
}

/// One task's live tool environment: per-task state, the hidden rules, and
/// the ground-truth scorer.
#[derive(Debug, Clone)]
pub struct Env {
    template: &'static str,
    pool: Vec<Customer>,
    target: usize,
    amount_cents: u64,
    expected_timestamp: String,
    limiter_armed: bool,
    limiter_open: bool,
    /// R8: the target's plan, and whether the refund that landed carried an
    /// approval token. Both are ordinary desk facts; neither raises an error.
    target_plan: &'static str,
    refund_authorized: bool,
    /// R9: this case must be filed high-priority.
    regulated_topic: bool,
    /// R10: a refund on this task must also be escalated as a high-priority
    /// case, because a person said so during the experience phase.
    escalate_required: bool,
    /// The endpoint R6 fronts: `refund` on refund templates, `log_case` on
    /// log tasks.
    limited_tool: &'static str,
    /// Issued, not-yet-consumed approval tokens (single-use, per task).
    tokens: BTreeSet<String>,
    token_seq: u32,
    refunds: Vec<(String, u64)>,
    cancelled: BTreeSet<String>,
    /// (customer, note, timestamp, priority) — priority is empty unless the
    /// agent set one.
    cases: Vec<(String, String, String, String)>,
    failures: BTreeMap<RuleId, u32>,
}

fn ok_body(v: Value) -> ToolOutcome {
    ToolOutcome { body: v.to_string(), is_error: false, rule: None, code: None }
}

/// Harness-level arg/shape errors: not attributable to a hidden rule, so
/// `rule` stays `None` (mod.rs: "when it is one").
fn bad_request(msg: &str) -> ToolOutcome {
    ToolOutcome {
        body: json!({ "error": { "code": "bad_request", "message": msg } }).to_string(),
        is_error: true,
        rule: None,
        code: Some("bad_request".to_string()),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Live models sometimes send numbers as strings ("140", "$140"); accept
/// both so bad_request noise never masks a hidden-rule signal.
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_start_matches('$').parse::<f64>().ok(),
        _ => None,
    }
}

fn is_canonical_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 12
        && id.starts_with("cus_")
        && b[4..].iter().all(|&c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

impl Env {
    pub fn new(task: &Task) -> Env {
        Env {
            template: task.template,
            pool: task.pool.clone(),
            target: task.target,
            amount_cents: task.amount_cents,
            expected_timestamp: task.expected_timestamp.clone(),
            limiter_armed: task.limiter_armed,
            limiter_open: false,
            target_plan: task.pool[task.target].plan,
            refund_authorized: false,
            regulated_topic: task.regulated_topic,
            escalate_required: task.escalate_required,
            limited_tool: if task.template == "log_case" { "log_case" } else { "refund" },
            tokens: BTreeSet::new(),
            token_seq: 0,
            refunds: Vec::new(),
            cancelled: BTreeSet::new(),
            cases: Vec::new(),
            failures: BTreeMap::new(),
        }
    }

    /// Execute one tool call. Never panics on agent-supplied input.
    pub fn execute(&mut self, name: &str, args_json: &str) -> ToolOutcome {
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => return bad_request("arguments must be a JSON object"),
            Err(_) => return bad_request("arguments are not valid JSON"),
        };
        // R6 fronts the whole endpoint: until an adequate wait() the limited
        // tool answers 429 to everything, and only then do the other rules
        // surface. Immediate retries keep failing by design.
        if name == self.limited_tool && self.limiter_armed && !self.limiter_open {
            return self.rate_limited();
        }
        match name {
            "search_customers" => self.search_customers(&args),
            "get_customer" => self.get_customer(&args),
            "request_approval" => self.request_approval(&args),
            "refund" => self.refund(&args),
            "cancel_subscription" => self.cancel_subscription(&args),
            "log_case" => self.log_case(&args),
            "wait" => self.wait(&args),
            _ => bad_request(&format!("unknown tool {name}")),
        }
    }

    /// Template-specific predicate over final env state — the programmatic
    /// ground truth (no LLM judge). `failure_reason` is a short
    /// machine-greppable phrase, empty on success.
    pub fn score(&self, final_answer: &str) -> (bool, String) {
        let target = &self.pool[self.target].id;
        if final_answer.trim().is_empty() {
            // An empty answer means the loop hit its turn cap.
            return (false, "no_final_answer".to_string());
        }
        if self.refunds.iter().any(|(id, _)| id != target) {
            return (false, "refunded_wrong_customer".to_string());
        }
        if self.cancelled.iter().any(|id| id != target) {
            return (false, "cancelled_wrong_customer".to_string());
        }
        if self.cases.iter().any(|(id, _, _, _)| id != target) {
            return (false, "case_on_wrong_customer".to_string());
        }
        match self.template {
            "refund_small" | "refund_large" | "refund_and_cancel" => {
                if self.refunds.is_empty() {
                    return (false, "no_refund".to_string());
                }
                if self.refunds.len() > 1 {
                    return (false, "duplicate_refund".to_string());
                }
                if self.refunds[0].1 != self.amount_cents {
                    return (false, "wrong_amount".to_string());
                }
                if self.template == "refund_and_cancel" {
                    if !self.cancelled.contains(target) {
                        return (false, "not_cancelled".to_string());
                    }
                    // R7 (silent): closing an account is an auditable act —
                    // desk policy is that it leaves a case behind. Every tool
                    // call here succeeded; only the final state is wrong.
                    if self.cases.is_empty() {
                        return (false, "no_closure_case".to_string());
                    }
                } else if !self.cancelled.is_empty() {
                    return (false, "unrequested_cancellation".to_string());
                }
                // R8 (silent): an enterprise account needs a manager behind
                // ANY refund, not only one over $100. Over the threshold R3
                // already forced a token, so this only ever bites below it.
                if self.target_plan == "enterprise" && !self.refund_authorized {
                    return (false, "unapproved_enterprise_refund".to_string());
                }
                // R10 (instructed): a person said large refunds get escalated.
                // No tool objects; the desk simply did not do what it was told.
                if self.escalate_required
                    && !self.cases.iter().any(|c| c.3 == "high")
                {
                    return (false, "refund_not_escalated".to_string());
                }
                (true, String::new())
            }
            "log_case" => {
                if !self.refunds.is_empty() {
                    return (false, "unrequested_refund".to_string());
                }
                if !self.cancelled.is_empty() {
                    return (false, "unrequested_cancellation".to_string());
                }
                if self.cases.is_empty() {
                    return (false, "no_case_logged".to_string());
                }
                if self.cases.len() > 1 {
                    return (false, "duplicate_case".to_string());
                }
                if self.cases[0].2 != self.expected_timestamp {
                    return (false, "wrong_timestamp".to_string());
                }
                // R9 (silent): this topic is regulated and its cases are
                // filed high-priority. `log_case` took the case happily.
                if self.regulated_topic && self.cases[0].3 != "high" {
                    return (false, "case_missing_priority".to_string());
                }
                (true, String::new())
            }
            _ => (false, "unknown_template".to_string()),
        }
    }

    /// Error counts per hidden rule tripped this run (sorted by rule id).
    /// R1 has a silent shape no error can carry: the target sat past page 1
    /// and a side effect landed on someone else — the calls all "succeeded",
    /// the state is just wrong. Attribute it here so the recurrence table
    /// sees pagination failures that never raised `customer_not_found`.
    pub fn rule_failures(&self) -> Vec<(RuleId, u32)> {
        let mut out = self.failures.clone();
        let target = &self.pool[self.target].id;
        if self.target >= PAGE_SIZE
            && (self.refunds.iter().any(|(id, _)| id != target)
                || self.cancelled.iter().any(|id| id != target)
                || self.cases.iter().any(|(id, _, _, _)| id != target))
        {
            *out.entry("R1").or_insert(0) += 1;
        }
        // R7-R9 have NO error shape at all — every call that trips them
        // returned 200. Without env-side attribution they would be invisible
        // to the recurrence table as well as to the analyzers, and the
        // difference between "the loop cannot see this" and "nothing
        // happened" is the whole experiment.
        if self.template == "refund_and_cancel"
            && self.cancelled.contains(target)
            && self.cases.is_empty()
        {
            *out.entry("R7").or_insert(0) += 1;
        }
        if self.target_plan == "enterprise"
            && !self.refunds.is_empty()
            && !self.refund_authorized
        {
            *out.entry("R8").or_insert(0) += 1;
        }
        if self.regulated_topic
            && self.cases.first().is_some_and(|c| c.3 != "high")
        {
            *out.entry("R9").or_insert(0) += 1;
        }
        if self.escalate_required
            && !self.refunds.is_empty()
            && !self.cases.iter().any(|c| c.3 == "high")
        {
            *out.entry("R10").or_insert(0) += 1;
        }
        out.into_iter().collect()
    }

    fn rule_err(&mut self, rule: RuleId, code: &str, msg: &str) -> ToolOutcome {
        *self.failures.entry(rule).or_insert(0) += 1;
        ToolOutcome {
            body: json!({ "error": { "code": code, "message": msg } }).to_string(),
            is_error: true,
            rule: Some(rule),
            code: Some(code.to_string()),
        }
    }

    fn rate_limited(&mut self) -> ToolOutcome {
        *self.failures.entry("R6").or_insert(0) += 1;
        ToolOutcome {
            body: json!({ "error": {
                "code": "rate_limited",
                "message": "rate limited, try again later",
                "retry_after_s": RETRY_AFTER_S
            } })
            .to_string(),
            is_error: true,
            rule: Some("R6"),
            code: Some("rate_limited".to_string()),
        }
    }

    /// R2 then R1: a malformed id is `invalid_id`; a well-formed id that
    /// names nobody in the pool is the downstream `customer_not_found`.
    fn resolve_id(&mut self, id: &str) -> Result<usize, ToolOutcome> {
        if !is_canonical_id(id) {
            return Err(self.rule_err(
                "R2",
                "invalid_id",
                &format!("id '{id}' is not a canonical cus_ id from search_customers"),
            ));
        }
        match self.pool.iter().position(|c| c.id == id) {
            Some(i) => Ok(i),
            None => Err(self.rule_err("R1", "customer_not_found", &format!("no customer with id {id}"))),
        }
    }

    fn search_customers(&mut self, args: &Value) -> ToolOutcome {
        if str_arg(args, "query").is_none() {
            return bad_request("query (string) is required");
        }
        let page = match args.get("page") {
            None | Some(Value::Null) => 1,
            Some(v) => match num(v) {
                Some(p) if p >= 1.0 && p.fract() == 0.0 => p as u64,
                _ => return bad_request("page must be a positive integer"),
            },
        };
        // The query is accepted but not used to filter: the desk search
        // returns the whole fuzzy-matched book of business, PAGE_SIZE per
        // page — exhausting pages is the hidden rule (R1).
        let split_at = self.pool.len().min(PAGE_SIZE);
        let (rows, has_more) = match page {
            1 => (&self.pool[..split_at], self.pool.len() > split_at),
            2 => (&self.pool[split_at..], false),
            _ => (&self.pool[0..0], false),
        };
        let customers: Vec<Value> =
            rows.iter().map(|c| json!({ "id": c.id, "name": c.name, "email": c.email })).collect();
        let mut out = json!({ "customers": customers, "has_more": has_more });
        if has_more {
            out["next_page"] = json!(2);
        }
        ok_body(out)
    }

    fn get_customer(&mut self, args: &Value) -> ToolOutcome {
        let id = match str_arg(args, "id") {
            Some(s) => s.to_string(),
            None => return bad_request("id (string) is required"),
        };
        let i = match self.resolve_id(&id) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let c = &self.pool[i];
        let status = if self.cancelled.contains(&c.id) { "cancelled" } else { "active" };
        ok_body(json!({
            "id": c.id, "name": c.name, "email": c.email, "balance": c.balance,
            "subscription": { "id": c.sub_id, "status": status, "plan": c.plan }
        }))
    }

    fn request_approval(&mut self, args: &Value) -> ToolOutcome {
        match str_arg(args, "reason") {
            Some(r) if !r.trim().is_empty() => {}
            _ => return bad_request("reason (non-empty string) is required"),
        }
        self.token_seq += 1;
        let token = format!("appr_{:08x}", 0x0a00_0000u32 + self.token_seq);
        self.tokens.insert(token.clone());
        ok_body(json!({ "approval_token": token }))
    }

    fn refund(&mut self, args: &Value) -> ToolOutcome {
        let id = match str_arg(args, "customer_id") {
            Some(s) => s.to_string(),
            None => return bad_request("customer_id (string) is required"),
        };
        let cents = match args.get("amount").and_then(num) {
            Some(a) if a.is_finite() && a > 0.0 && a <= 1_000_000.0 => (a * 100.0).round() as u64,
            _ => return bad_request("amount must be a positive number of dollars"),
        };
        let i = match self.resolve_id(&id) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let cid = self.pool[i].id.clone();
        // R4: a cancelled account's billing is closed — the refund had to
        // come first.
        if self.cancelled.contains(&cid) {
            return self.rule_err(
                "R4",
                "cancel_before_refund",
                "subscription already cancelled; refunds must be processed before cancellation",
            );
        }
        // R3: tokens are single-use — consumed on the refund they authorize.
        // A token presented under the threshold is still consumed and still
        // authorizes: that is what makes R8 satisfiable at all.
        let presented = str_arg(args, "approval_token")
            .is_some_and(|t| self.tokens.remove(t));
        if cents > APPROVAL_CENTS && !presented {
            return self.rule_err(
                "R3",
                "approval_required",
                "refunds over $100 require a valid approval_token from request_approval",
            );
        }
        self.refund_authorized = presented;
        self.refunds.push((cid, cents));
        ok_body(json!({ "refund_id": format!("re_{:08x}", 0x0e00_0000u32 + self.refunds.len() as u32) }))
    }

    fn cancel_subscription(&mut self, args: &Value) -> ToolOutcome {
        let id = match str_arg(args, "customer_id") {
            Some(s) => s.to_string(),
            None => return bad_request("customer_id (string) is required"),
        };
        let i = match self.resolve_id(&id) {
            Ok(i) => i,
            Err(e) => return e,
        };
        // Idempotent: cancelling twice is a no-op, not an error.
        let cid = self.pool[i].id.clone();
        self.cancelled.insert(cid);
        ok_body(json!({ "status": "cancelled" }))
    }

    fn log_case(&mut self, args: &Value) -> ToolOutcome {
        let id = match str_arg(args, "customer_id") {
            Some(s) => s.to_string(),
            None => return bad_request("customer_id (string) is required"),
        };
        let note = match str_arg(args, "note") {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return bad_request("note (non-empty string) is required"),
        };
        let ts = match str_arg(args, "timestamp") {
            Some(s) => s.to_string(),
            None => return bad_request("timestamp (string) is required"),
        };
        let i = match self.resolve_id(&id) {
            Ok(i) => i,
            Err(e) => return e,
        };
        if !is_utc_timestamp(&ts) {
            return self.rule_err(
                "R5",
                "invalid_timestamp",
                "timestamp must be UTC ISO-8601: YYYY-MM-DDTHH:MM:SSZ",
            );
        }
        let priority = str_arg(args, "priority").unwrap_or("").trim().to_lowercase();
        let cid = self.pool[i].id.clone();
        self.cases.push((cid, note, ts, priority));
        ok_body(json!({ "case_id": format!("case_{:08x}", 0x0c00_0000u32 + self.cases.len() as u32) }))
    }

    fn wait(&mut self, args: &Value) -> ToolOutcome {
        let secs = match args.get("seconds").and_then(num) {
            Some(s) if s.is_finite() && s >= 0.0 => s,
            _ => return bad_request("seconds must be a non-negative number"),
        };
        // Modeled, never slept — the env has no clock. An adequate wait
        // opens the limiter for the rest of the task; otherwise a no-op.
        if secs >= RETRY_AFTER_S as f64 {
            self.limiter_open = true;
        }
        ok_body(json!({ "ok": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> Vec<Customer> {
        (0..6u32)
            .map(|i| Customer {
                id: format!("cus_{:08x}", 0xa0 + i),
                name: format!("Test Person{i}"),
                email: format!("test.person{i}@pool.example"),
                sub_id: format!("sub_{i:08x}"),
                plan: "basic",
                balance: 100,
            })
            .collect()
    }

    /// Hand-built task: full control over the seeded flags. Local datetime
    /// "2026-03-14 09:30" at UTC+5:30 → expected "2026-03-14T04:00:00Z".
    fn task(template: &'static str, page2: bool, limited: bool, amount_cents: u64) -> Task {
        Task {
            id: "t-0000".to_string(),
            prompt: "scripted".to_string(),
            template,
            rules_exercised: vec![],
            pool: test_pool(),
            target: if page2 { 5 } else { 0 },
            amount_cents,
            expected_timestamp: if template == "log_case" {
                "2026-03-14T04:00:00Z".to_string()
            } else {
                String::new()
            },
            limiter_armed: limited,
            regulated_topic: false,
            supervisor_note: None,
            escalate_required: false,
        }
    }

    /// A held-out refund large enough that the supervisor's note binds.
    fn escalating_task(template: &'static str, amount_cents: u64) -> Task {
        let mut t = task(template, false, false, amount_cents);
        t.escalate_required = true;
        t
    }

    /// The same scripted task with a silent rule armed: an enterprise target
    /// (R8) and/or the regulated topic (R9). R7 rides `refund_and_cancel`
    /// unconditionally, so it needs no flag.
    fn silent_task(
        template: &'static str,
        enterprise: bool,
        regulated: bool,
        amount_cents: u64,
    ) -> Task {
        let mut t = task(template, false, false, amount_cents);
        if enterprise {
            t.pool[0].plan = "enterprise";
        }
        t.regulated_topic = regulated;
        t
    }

    fn call(env: &mut Env, tool: &str, args: Value) -> ToolOutcome {
        env.execute(tool, &args.to_string())
    }

    fn body(o: &ToolOutcome) -> Value {
        serde_json::from_str(&o.body).unwrap()
    }

    #[test]
    fn gen_is_deterministic() {
        for split in [Split::Experience, Split::Eval] {
            let a = gen_tasks(42, split, 24);
            let b = gen_tasks(42, split, 24);
            assert_eq!(a.len(), 24);
            for (x, y) in a.iter().zip(&b) {
                assert_eq!(x.id, y.id);
                assert_eq!(x.prompt, y.prompt);
                assert_eq!(x.template, y.template);
                assert_eq!(x.rules_exercised, y.rules_exercised);
                assert_eq!(x.target, y.target);
                assert_eq!(x.limiter_armed, y.limiter_armed);
                let ex: Vec<&String> = x.pool.iter().map(|c| &c.email).collect();
                let ey: Vec<&String> = y.pool.iter().map(|c| &c.email).collect();
                assert_eq!(ex, ey);
            }
        }
    }

    #[test]
    fn splits_draw_disjoint_pools() {
        let exp = gen_tasks(7, Split::Experience, 32);
        let eval = gen_tasks(7, Split::Eval, 32);
        let exp_emails: BTreeSet<&String> =
            exp.iter().flat_map(|t| t.pool.iter().map(|c| &c.email)).collect();
        let eval_emails: BTreeSet<&String> =
            eval.iter().flat_map(|t| t.pool.iter().map(|c| &c.email)).collect();
        assert!(exp_emails.intersection(&eval_emails).next().is_none());
        let exp_names: BTreeSet<&String> =
            exp.iter().flat_map(|t| t.pool.iter().map(|c| &c.name)).collect();
        let eval_names: BTreeSet<&String> =
            eval.iter().flat_map(|t| t.pool.iter().map(|c| &c.name)).collect();
        assert!(exp_names.intersection(&eval_names).next().is_none());
        // Paraphrased templates + disjoint entities: no prompt is shared.
        let exp_prompts: BTreeSet<&String> = exp.iter().map(|t| &t.prompt).collect();
        assert!(eval.iter().all(|t| !exp_prompts.contains(&t.prompt)));
    }

    #[test]
    fn rules_exercised_match_the_seeded_spec() {
        for t in gen_tasks(3, Split::Experience, 40) {
            let has = |r: &str| t.rules_exercised.contains(&r);
            assert!(has("R2"), "{}: R2 is always in play", t.id);
            assert_eq!(has("R1"), t.target >= PAGE_SIZE, "{}", t.id);
            assert_eq!(has("R3"), t.amount_cents > APPROVAL_CENTS, "{}", t.id);
            assert_eq!(has("R4"), t.template == "refund_and_cancel", "{}", t.id);
            assert_eq!(has("R5"), t.template == "log_case", "{}", t.id);
            assert_eq!(has("R6"), t.limiter_armed, "{}", t.id);
            // Prompts always name the target by email; refund prompts carry
            // the exact dollar amount.
            assert!(t.prompt.contains(&t.pool[t.target].email), "{}", t.id);
            if t.template != "log_case" {
                assert!(t.prompt.contains(&format!("${}", t.amount_cents / 100)), "{}", t.id);
            }
        }
        // Both sides of each seeded coin appear in a modest sample.
        let ts = gen_tasks(3, Split::Experience, 40);
        assert!(ts.iter().any(|t| t.target >= PAGE_SIZE) && ts.iter().any(|t| t.target < PAGE_SIZE));
        assert!(ts.iter().any(|t| t.limiter_armed) && ts.iter().any(|t| !t.limiter_armed));
    }

    #[test]
    fn r1_page2_target_and_customer_not_found() {
        let t = task("refund_small", true, false, 50 * 100);
        let mut env = Env::new(&t);
        let target_email = t.pool[5].email.clone();

        let p1 = body(&call(&mut env, "search_customers", json!({"query": "test"})));
        assert_eq!(p1["has_more"], json!(true));
        assert_eq!(p1["next_page"], json!(2));
        assert_eq!(p1["customers"].as_array().unwrap().len(), 5);
        assert!(!p1.to_string().contains(&target_email), "target must not be on page 1");

        let p2 = body(&call(&mut env, "search_customers", json!({"query": "test", "page": 2})));
        assert_eq!(p2["has_more"], json!(false));
        assert!(p2.to_string().contains(&target_email));

        // Well-formed id, nobody in the pool → the downstream R1 code.
        let o = call(&mut env, "get_customer", json!({"id": "cus_deadbeef"}));
        assert!(o.is_error);
        assert_eq!(o.code.as_deref(), Some("customer_not_found"));
        assert_eq!(o.rule, Some("R1"));

        // Refunding a real-but-wrong customer succeeds at the tool and
        // fails at scoring — the silent R1 shape, attributed from final
        // state by rule_failures (so this env counts both shapes: the
        // customer_not_found error above + the wrongful side effect).
        let wrong = t.pool[0].id.clone();
        let o = call(&mut env, "refund", json!({"customer_id": wrong, "amount": 50}));
        assert!(!o.is_error);
        let (ok, reason) = env.score("done");
        assert!(!ok);
        assert_eq!(reason, "refunded_wrong_customer");
        assert_eq!(env.rule_failures(), vec![("R1", 2)]);
    }

    #[test]
    fn r2_non_canonical_ids_rejected_everywhere() {
        let t = task("refund_small", false, false, 50 * 100);
        let mut env = Env::new(&t);
        for (tool, args) in [
            ("get_customer", json!({"id": "12345"})),
            ("get_customer", json!({"id": "CUS_000000A0"})),
            ("refund", json!({"customer_id": "cus_xyz", "amount": 50})),
            ("cancel_subscription", json!({"customer_id": "cus_000000a"})),
            ("log_case", json!({"customer_id": "alice", "note": "n", "timestamp": "2026-01-02T00:00:00Z"})),
        ] {
            let o = call(&mut env, tool, args);
            assert!(o.is_error, "{tool}");
            assert_eq!(o.code.as_deref(), Some("invalid_id"), "{tool}");
            assert_eq!(o.rule, Some("R2"), "{tool}");
        }
        assert_eq!(env.rule_failures(), vec![("R2", 5)]);
    }

    #[test]
    fn r3_approval_required_and_tokens_single_use() {
        let t = task("refund_large", false, false, 250 * 100);
        let mut env = Env::new(&t);
        let target = t.pool[0].id.clone();

        let o = call(&mut env, "refund", json!({"customer_id": target, "amount": 250}));
        assert!(o.is_error);
        assert_eq!(o.code.as_deref(), Some("approval_required"));
        assert_eq!(o.rule, Some("R3"));

        let a = body(&call(&mut env, "request_approval", json!({"reason": "large refund"})));
        let token = a["approval_token"].as_str().unwrap().to_string();
        let o = call(
            &mut env,
            "refund",
            json!({"customer_id": target, "amount": 250, "approval_token": token}),
        );
        assert!(!o.is_error);
        assert!(body(&o)["refund_id"].is_string());

        // Consumed on use: the same token authorizes nothing further.
        let o = call(
            &mut env,
            "refund",
            json!({"customer_id": target, "amount": 250, "approval_token": token}),
        );
        assert!(o.is_error);
        assert_eq!(o.code.as_deref(), Some("approval_required"));

        let (ok, reason) = env.score("refunded");
        assert!(ok, "{reason}");
        assert_eq!(env.rule_failures(), vec![("R3", 2)]);

        // ≤ $100 needs no token at all.
        let t = task("refund_small", false, false, 100 * 100);
        let mut env = Env::new(&t);
        let o = call(&mut env, "refund", json!({"customer_id": t.pool[0].id, "amount": 100}));
        assert!(!o.is_error);
        assert!(env.score("done").0);
    }

    #[test]
    fn r4_cancel_before_refund_and_the_correct_order() {
        let t = task("refund_and_cancel", false, false, 250 * 100);
        let target = t.pool[0].id.clone();

        // Naive order: cancel first, refund fails, task fails.
        let mut env = Env::new(&t);
        assert!(!call(&mut env, "cancel_subscription", json!({"customer_id": target})).is_error);
        let a = body(&call(&mut env, "request_approval", json!({"reason": "r"})));
        let o = call(&mut env, "refund", json!({
            "customer_id": target, "amount": 250, "approval_token": a["approval_token"]
        }));
        assert!(o.is_error);
        assert_eq!(o.code.as_deref(), Some("cancel_before_refund"));
        assert_eq!(o.rule, Some("R4"));
        let (ok, reason) = env.score("done");
        assert!(!ok);
        assert_eq!(reason, "no_refund");
        // R7 rides along honestly: the account WAS closed and no case was
        // filed. The recurrence table is unaffected — `mishandled_rules`
        // attributes "no_refund" to R3/R4/R6 — this is the raw per-rule count.
        assert_eq!(env.rule_failures(), vec![("R4", 1), ("R7", 1)]);

        // Right order, still incomplete: approve → refund → cancel scores
        // FALSE now, and every call returned 200. This is R7's whole shape —
        // there is no error anywhere for a clusterer to find.
        let mut env = Env::new(&t);
        let a = body(&call(&mut env, "request_approval", json!({"reason": "r"})));
        let o = call(&mut env, "refund", json!({
            "customer_id": target, "amount": 250, "approval_token": a["approval_token"]
        }));
        assert!(!o.is_error);
        assert!(!call(&mut env, "cancel_subscription", json!({"customer_id": target})).is_error);
        let (ok, reason) = env.score("done");
        assert!(!ok);
        assert_eq!(reason, "no_closure_case");
        assert_eq!(env.rule_failures(), vec![("R7", 1)]);

        // Complete: the closure leaves a case behind.
        assert!(!call(&mut env, "log_case", json!({
            "customer_id": target, "note": "account closed", "timestamp": "2026-01-01T00:00:00Z"
        }))
        .is_error);
        let (ok, reason) = env.score("done");
        assert!(ok, "{reason}");
        assert!(env.rule_failures().is_empty());
    }

    /// How many tasks each rule is even ABLE to trip, at the published
    /// config. This is a POWER guard, not a behaviour test: a rule exercised
    /// by a handful of held-out tasks can never produce a per-rule result
    /// anyone should believe, and the 2x2 died of exactly that (SELFIMPROVE.md,
    /// "the 2x2 that could not be run"). Better to find it here than after
    /// paying for a run.
    #[test]
    fn every_rule_gets_enough_opportunities_to_be_measurable() {
        use std::collections::BTreeMap;
        let mut counts: BTreeMap<RuleId, usize> = BTreeMap::new();
        let tasks = gen_tasks(1, Split::Eval, 100);
        for t in &tasks {
            for r in &t.rules_exercised {
                *counts.entry(r).or_default() += 1;
            }
        }
        eprintln!("eval n=100 opportunities: {counts:?}");
        for rule in ["R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9", "R10"] {
            let n = counts.get(rule).copied().unwrap_or(0);
            assert!(
                n >= 10,
                "rule {rule} is exercised by only {n} of 100 held-out tasks —                  too thin for any per-rule claim; raise its arming rate in                  gen_task rather than publishing an underpowered table"
            );
        }
    }

    #[test]
    fn r10_a_large_refund_must_be_escalated_once_a_person_has_said_so() {
        // $700, above the $500 the note names. Nothing errors: the desk
        // simply did not do what it was told.
        let t = escalating_task("refund_large", 700 * 100);
        let target = t.pool[0].id.clone();

        let mut env = Env::new(&t);
        let a = body(&call(&mut env, "request_approval", json!({"reason": "r"})));
        let o = call(&mut env, "refund", json!({
            "customer_id": target, "amount": 700, "approval_token": a["approval_token"]
        }));
        assert!(!o.is_error, "R10 raises no error — a person's instruction never does");
        let (ok, reason) = env.score("done");
        assert!(!ok);
        assert_eq!(reason, "refund_not_escalated");
        assert_eq!(env.rule_failures(), vec![("R10", 1)]);

        // With the escalation filed, it passes.
        assert!(!call(&mut env, "log_case", json!({
            "customer_id": target, "note": "escalated",
            "timestamp": "2026-01-01T00:00:00Z", "priority": "high"
        }))
        .is_error);
        let (ok, reason) = env.score("done");
        assert!(ok, "{reason}");
        assert!(env.rule_failures().is_empty());
    }

    /// The note is delivered ONCE, on the experience split, and the
    /// requirement binds ONLY on the held-out split. That asymmetry is the
    /// whole design: the experience data contains no instance of the rule
    /// mattering, so nothing statistical can recover it.
    #[test]
    fn the_supervisor_note_is_experience_only_and_the_requirement_is_eval_only() {
        let exp = gen_tasks(1, Split::Experience, 300);
        let eval = gen_tasks(1, Split::Eval, 100);

        let noted: Vec<_> = exp.iter().filter(|t| t.supervisor_note.is_some()).collect();
        assert_eq!(noted.len(), 1, "exactly one experience task carries the note");
        assert!(noted[0].supervisor_note.unwrap().contains("over $500"));

        assert!(
            exp.iter().all(|t| !t.escalate_required),
            "the requirement must never bind during experience — otherwise the \
             rule is learnable by counting failures and stops testing instruction"
        );
        assert!(
            eval.iter().all(|t| t.supervisor_note.is_none()),
            "the note is never delivered on the held-out split"
        );
        let bound = eval.iter().filter(|t| t.escalate_required).count();
        assert!(bound >= 10, "R10 exercised by only {bound} of 100 held-out tasks");
        assert!(
            eval.iter().filter(|t| t.escalate_required).all(|t| t.amount_cents > ESCALATE_CENTS),
            "only refunds above the stated threshold bind"
        );
    }

    #[test]
    fn r8_enterprise_refund_needs_a_token_below_the_threshold_and_errors_never_fire() {
        // $40 is under R3's $100 line, so nothing in the refund path objects.
        let t = silent_task("refund_small", true, false, 40 * 100);
        let target = t.pool[0].id.clone();

        let mut env = Env::new(&t);
        let o = call(&mut env, "refund", json!({"customer_id": target, "amount": 40}));
        assert!(!o.is_error, "R8 raises no error — that is the whole shape");
        let (ok, reason) = env.score("done");
        assert!(!ok);
        assert_eq!(reason, "unapproved_enterprise_refund");
        assert_eq!(env.rule_failures(), vec![("R8", 1)]);

        // With a manager behind it the same refund passes. A token under the
        // threshold has to be consumed and count, or the rule is unlearnable.
        let mut env = Env::new(&t);
        let a = body(&call(&mut env, "request_approval", json!({"reason": "enterprise"})));
        let o = call(&mut env, "refund", json!({
            "customer_id": target, "amount": 40, "approval_token": a["approval_token"]
        }));
        assert!(!o.is_error);
        let (ok, reason) = env.score("done");
        assert!(ok, "{reason}");
        assert!(env.rule_failures().is_empty());

        // A non-enterprise account is unaffected: the rule is a segment rule.
        let t = silent_task("refund_small", false, false, 40 * 100);
        let target = t.pool[0].id.clone();
        let mut env = Env::new(&t);
        assert!(!call(&mut env, "refund", json!({"customer_id": target, "amount": 40})).is_error);
        assert!(env.score("done").0);
    }

    #[test]
    fn r9_regulated_cases_need_high_priority_and_log_case_never_objects() {
        let t = silent_task("log_case", false, true, 0);
        let target = t.pool[0].id.clone();
        let stamped = |priority: Option<&str>| {
            let mut a = json!({
                "customer_id": target,
                "note": "export requested",
                "timestamp": "2026-03-14T04:00:00Z",
            });
            if let Some(p) = priority {
                a["priority"] = json!(p);
            }
            a
        };

        // No priority: accepted by the tool, rejected by the desk.
        let mut env = Env::new(&t);
        let o = call(&mut env, "log_case", stamped(None));
        assert!(!o.is_error);
        let (ok, reason) = env.score("done");
        assert!(!ok);
        assert_eq!(reason, "case_missing_priority");
        assert_eq!(env.rule_failures(), vec![("R9", 1)]);

        // The wrong priority is just as silent, and just as wrong.
        let mut env = Env::new(&t);
        assert!(!call(&mut env, "log_case", stamped(Some("normal"))).is_error);
        assert_eq!(env.score("done").1, "case_missing_priority");

        // High priority (case-insensitively) passes.
        let mut env = Env::new(&t);
        assert!(!call(&mut env, "log_case", stamped(Some("High"))).is_error);
        let (ok, reason) = env.score("done");
        assert!(ok, "{reason}");
        assert!(env.rule_failures().is_empty());

        // An unregulated case needs nothing — high priority on it is fine too.
        let t = silent_task("log_case", false, false, 0);
        let mut env = Env::new(&t);
        assert!(!call(&mut env, "log_case", json!({
            "customer_id": t.pool[0].id, "note": "n", "timestamp": "2026-03-14T04:00:00Z"
        }))
        .is_error);
        assert!(env.score("done").0);
    }

    #[test]
    fn r5_timestamp_format_and_the_unconverted_trap() {
        let t = task("log_case", false, false, 0);
        let target = t.pool[0].id.clone();

        // The naive pass-through (no T, no Z) and the offset form both fail
        // the format check with the frozen code.
        for bad in ["2026-03-14 09:30", "2026-03-14T09:30:00+05:30", "2026-03-14T09:30Z"] {
            let mut env = Env::new(&t);
            let o = call(&mut env, "log_case", json!({
                "customer_id": target, "note": "n", "timestamp": bad
            }));
            assert!(o.is_error, "{bad}");
            assert_eq!(o.code.as_deref(), Some("invalid_timestamp"), "{bad}");
            assert_eq!(o.rule, Some("R5"), "{bad}");
            assert_eq!(env.rule_failures(), vec![("R5", 1)]);
        }

        // Well-formed but un-converted: the tool accepts it, scoring does not.
        let mut env = Env::new(&t);
        let o = call(&mut env, "log_case", json!({
            "customer_id": target, "note": "n", "timestamp": "2026-03-14T09:30:00Z"
        }));
        assert!(!o.is_error);
        let (ok, reason) = env.score("done");
        assert!(!ok);
        assert_eq!(reason, "wrong_timestamp");

        // Correctly converted to UTC: success.
        let mut env = Env::new(&t);
        let o = call(&mut env, "log_case", json!({
            "customer_id": target, "note": "n", "timestamp": "2026-03-14T04:00:00Z"
        }));
        assert!(!o.is_error);
        assert!(body(&o)["case_id"].is_string());
        let (ok, reason) = env.score("logged");
        assert!(ok, "{reason}");
    }

    #[test]
    fn r6_limiter_opens_only_after_adequate_wait() {
        let t = task("refund_small", false, true, 50 * 100);
        let mut env = Env::new(&t);
        let target = t.pool[0].id.clone();

        // Unlimited tools work while the limiter is closed.
        assert!(!call(&mut env, "search_customers", json!({"query": "q"})).is_error);
        assert!(!call(&mut env, "get_customer", json!({"id": target})).is_error);

        let o = call(&mut env, "refund", json!({"customer_id": target, "amount": 50}));
        assert!(o.is_error);
        assert_eq!(o.code.as_deref(), Some("rate_limited"));
        assert_eq!(o.rule, Some("R6"));
        assert_eq!(body(&o)["error"]["retry_after_s"], json!(2));

        // Immediate retry: still closed. Inadequate wait: still closed.
        assert!(call(&mut env, "refund", json!({"customer_id": target, "amount": 50})).is_error);
        assert!(!call(&mut env, "wait", json!({"seconds": 1})).is_error);
        assert!(call(&mut env, "refund", json!({"customer_id": target, "amount": 50})).is_error);

        // Adequate wait opens it; the next attempt goes through.
        let o = call(&mut env, "wait", json!({"seconds": 2}));
        assert_eq!(body(&o)["ok"], json!(true));
        let o = call(&mut env, "refund", json!({"customer_id": target, "amount": 50}));
        assert!(!o.is_error);
        assert!(env.score("done").0);
        assert_eq!(env.rule_failures(), vec![("R6", 3)]);

        // On a log task the limited endpoint is log_case, and wait is
        // otherwise a no-op.
        let t = task("log_case", false, true, 0);
        let mut env = Env::new(&t);
        let o = call(&mut env, "log_case", json!({
            "customer_id": t.pool[0].id, "note": "n", "timestamp": "2026-03-14T04:00:00Z"
        }));
        assert_eq!(o.code.as_deref(), Some("rate_limited"));
        assert!(!call(&mut env, "wait", json!({"seconds": 3})).is_error);
        let o = call(&mut env, "log_case", json!({
            "customer_id": t.pool[0].id, "note": "n", "timestamp": "2026-03-14T04:00:00Z"
        }));
        assert!(!o.is_error);
        assert!(env.score("done").0);
    }

    #[test]
    fn utc_conversion_hand_checked() {
        assert_eq!(utc_from_local(3, 14, 9, 30, 330), "2026-03-14T04:00:00Z");
        assert_eq!(utc_from_local(1, 2, 0, 15, 330), "2026-01-01T18:45:00Z");
        assert_eq!(utc_from_local(2, 27, 23, 45, -480), "2026-02-28T07:45:00Z");
        assert_eq!(utc_from_local(6, 10, 12, 0, -210), "2026-06-10T15:30:00Z");
    }

    #[test]
    fn timestamp_validator_shape() {
        assert!(is_utc_timestamp("2026-03-14T04:00:00Z"));
        for bad in [
            "2026-03-14 04:00:00Z",
            "2026-03-14T04:00:00",
            "2026-13-14T04:00:00Z",
            "2026-03-14T24:00:00Z",
            "2026-03-14T04:00:00z",
            "26-03-14T04:00:00Z",
            "",
        ] {
            assert!(!is_utc_timestamp(bad), "{bad}");
        }
    }

    #[test]
    fn scoring_negative_shapes() {
        // Wrong amount.
        let t = task("refund_small", false, false, 50 * 100);
        let mut env = Env::new(&t);
        assert!(!call(&mut env, "refund", json!({"customer_id": t.pool[0].id, "amount": 49})).is_error);
        assert_eq!(env.score("done").1, "wrong_amount");

        // Duplicate refund.
        let mut env = Env::new(&t);
        for _ in 0..2 {
            assert!(!call(&mut env, "refund", json!({"customer_id": t.pool[0].id, "amount": 50})).is_error);
        }
        assert_eq!(env.score("done").1, "duplicate_refund");

        // Unrequested cancellation on a refund-only task.
        let mut env = Env::new(&t);
        assert!(!call(&mut env, "refund", json!({"customer_id": t.pool[0].id, "amount": 50})).is_error);
        assert!(!call(&mut env, "cancel_subscription", json!({"customer_id": t.pool[0].id})).is_error);
        assert_eq!(env.score("done").1, "unrequested_cancellation");

        // Turn-cap shape: an empty final answer never scores.
        let env = Env::new(&t);
        assert_eq!(env.score("  ").1, "no_final_answer");
    }

    #[test]
    fn bad_requests_are_not_rule_failures() {
        let t = task("refund_small", false, false, 50 * 100);
        let mut env = Env::new(&t);
        for (tool, args) in [
            ("no_such_tool", "{}"),
            ("refund", "not json"),
            ("refund", "[1,2]"),
            ("search_customers", "{}"),
            ("wait", "{\"seconds\": -1}"),
        ] {
            let o = env.execute(tool, args);
            assert!(o.is_error, "{tool}");
            assert_eq!(o.code.as_deref(), Some("bad_request"), "{tool}");
            assert_eq!(o.rule, None, "{tool}");
        }
        assert!(env.rule_failures().is_empty());
    }

    #[test]
    fn replay_is_deterministic() {
        let t = gen_tasks(9, Split::Eval, 8).remove(1); // a refund_large task
        assert_eq!(t.template, "refund_large");
        let script = [
            ("search_customers", json!({"query": "x"})),
            ("request_approval", json!({"reason": "r"})),
            ("refund", json!({"customer_id": t.pool[t.target].id, "amount": t.amount_cents / 100})),
        ];
        let mut a = Env::new(&t);
        let mut b = Env::new(&t);
        for (tool, args) in &script {
            let oa = call(&mut a, tool, args.clone());
            let ob = call(&mut b, tool, args.clone());
            assert_eq!(oa.body, ob.body);
            assert_eq!(oa.is_error, ob.is_error);
        }
    }

    #[test]
    fn tool_schemas_cover_the_contract() {
        let v = tool_schemas();
        let arr = v.as_array().unwrap();
        let names: Vec<&str> =
            arr.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "search_customers",
                "get_customer",
                "request_approval",
                "refund",
                "cancel_subscription",
                "log_case",
                "wait"
            ]
        );
        for t in arr {
            assert_eq!(t["type"], json!("function"));
            assert!(t["function"]["parameters"]["required"].is_array());
            // Schemas are given; rules are hidden — no description may leak
            // a threshold or ordering.
            let desc = t["function"]["description"].as_str().unwrap();
            assert!(!desc.contains("$100") && !desc.to_lowercase().contains("before"), "{desc}");
        }
    }
}
