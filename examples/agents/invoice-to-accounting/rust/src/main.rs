//! invoice -> accounting: the whole agent, one binary, embedded Areev.
//!
//! The Rust twin of ../../python/agent.py -- same subcommands, same
//! fixtures, same assertions (../../smoke.sh and ../../improve.sh drive all
//! three languages). Rust is the native surface: `AreevFacade` is exactly
//! what the Python and Node bindings wrap, so this file is also a map of
//! what those bindings do underneath.
//!
//! `agent tools` and `agent connector` are the two subprocess seams the
//! runtime spawns (JSON on stdin, JSON on stdout, one process per
//! invocation); they never open the memory. Everything else is the driver.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig, CalStoreFacade};
use areev_core::error::Hash;
use areev_loop::{AnalyzerConfigUpdate, Decision, Engine, ObserverType, RecStatus, ScopeSet};
use areev_loop_adapter::{now_ms, BorrowedSubstrate};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const NS: &str = "org.ops"; // triggers, plan, tool definitions, journals, raw mail
const DESK: &str = "agent:ap-desk"; // the agent's own principal -- it can never approve
const DESK_FROM: &str = "ap-desk@desk.example";

// One mailbox per client; the client's knowledge lives under org.<client>.
const MAILBOXES: [(&str, &str); 2] = [
    ("acme", "ap-acme@desk.example"),
    ("brightco", "ap-brightco@desk.example"),
];
const APPROVER: [(&str, &str); 2] = [
    ("acme", "dana@acme.example"),
    ("brightco", "priya@brightco.example"),
];

/// Pinned so every language's seeder mints the SAME content addresses --
/// created_at is part of a grain's bytes, and a grain is its bytes.
const EPOCH_MS: i64 = 1_756_000_000_000;

const CONFIDENCE_FLOOR: f64 = 0.75;
const DEFAULT_THRESHOLD: f64 = 2500.0;

fn env(name: &str, default: String) -> String {
    std::env::var(name).unwrap_or(default)
}

fn here() -> std::path::PathBuf {
    // rust/src/main.rs -> the example root is two levels above the crate.
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn out_dir() -> String {
    env("AGENT_OUT", here().join("out").display().to_string())
}

fn db_path() -> String {
    env("AGENT_DB", format!("{}/agent.db", out_dir()))
}

fn fixtures() -> String {
    env(
        "MAIL_FIXTURES",
        here().parent().unwrap().join("fixtures/mail").display().to_string(),
    )
}

fn emit(v: &Value) {
    println!("{v}");
}

fn append(path: &str, v: &Value) {
    use std::io::Write;
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(f, "{v}").unwrap();
}

fn marker(message_id: &str) -> String {
    hex::encode(Sha256::digest(message_id.as_bytes()))[..12].to_string()
}

fn read_stdin() -> Value {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).unwrap();
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

fn lookup<'a>(table: &[(&'a str, &'a str)], key: &str) -> &'a str {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or("unknown")
}

// ── the tools seam ─────────────────────────────────────────────────────────
// stdin is the run's merged state. On the trigger path the email is under
// "item" and the trigger's declared context under "context".

/// Collect every {subject, relation, object} object out of assembled context.
fn walk_grains<'a>(node: &'a Value, out: &mut Vec<&'a Map<String, Value>>) {
    match node {
        Value::Object(m) => {
            if m.contains_key("relation") && m.contains_key("subject") {
                out.push(m);
            }
            m.values().for_each(|v| walk_grains(v, out));
        }
        Value::Array(a) => a.iter().for_each(|v| walk_grains(v, out)),
        _ => {}
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("?").to_string()
}

fn f(v: &Value, key: &str, default: f64) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(default)
}

fn tool_main() -> i32 {
    let state = read_stdin();
    let item = state.get("item").unwrap_or(&state).clone();
    let ctx = state.get("context").cloned().unwrap_or(Value::Null);
    let mut grains = Vec::new();
    walk_grains(&ctx, &mut grains);
    let tool = env("AREEV_TOOL_NAME", String::new());

    match tool.as_str() {
        "parse_attachments" => {
            // A photographed invoice has no text layer. Failing loudly is
            // the correct behaviour: a silent empty extraction posts a
            // blank row.
            if item.get("scanned").and_then(Value::as_bool).unwrap_or(false) {
                eprintln!("pdftotext produced 0 characters - attachment is a scanned image");
                return 1;
            }
            emit(&json!({"texts": [{"filename": s(&item, "attachment"), "chars": 4180}]}));
        }
        "extract_rows" => {
            // The real one sends the PDF text to a model. This one reads the
            // fixture's own fields -- deterministic -- but applies the same
            // memory the real one would: an alias fact from a past
            // correction canonicalizes the vendor and settles the
            // confidence question it used to raise.
            let mut vendor = s(&item, "vendor");
            let mut confidence = f(&item, "confidence", 0.95);
            for g in &grains {
                if g.get("relation").and_then(Value::as_str) == Some("mg:alias_of")
                    && g.get("subject").and_then(Value::as_str) == Some(vendor.as_str())
                {
                    vendor = g.get("object").and_then(Value::as_str).unwrap_or("?").into();
                    confidence = confidence.max(0.95);
                }
            }
            emit(&json!({
                "rows": 1, "vendor": vendor, "amount": f(&item, "amount", 0.0),
                "currency": s(&item, "currency"), "category": s(&item, "category"),
                "field_confidence": confidence, "client": s(&item, "client"),
                "message_id": s(&item, "message_id"), "thread": s(&item, "thread"),
                "sender": s(&item, "sender"),
            }));
        }
        "validate_rows" => {
            // The threshold is a fact in org.<client>, delivered through the
            // trigger's declared context -- not a constant in this file.
            let client = s(&state, "client");
            let mut threshold = DEFAULT_THRESHOLD;
            for g in &grains {
                if g.get("relation").and_then(Value::as_str) == Some("review_threshold_usd")
                    && g.get("subject").and_then(Value::as_str) == Some(client.as_str())
                {
                    threshold = g
                        .get("object")
                        .and_then(Value::as_str)
                        .and_then(|o| o.parse().ok())
                        .unwrap_or(DEFAULT_THRESHOLD);
                }
            }
            let amount = f(&state, "amount", 0.0);
            let confidence = f(&state, "field_confidence", 1.0);
            let reason = if amount >= threshold {
                "amount at or above threshold"
            } else if confidence < CONFIDENCE_FLOOR {
                "field confidence below floor"
            } else {
                "clear"
            };
            emit(&json!({
                "needs_review": amount >= threshold || confidence < CONFIDENCE_FLOOR,
                "row_key": format!("{}#0", s(&state, "message_id")),
                "review_reason": reason,
            }));
        }
        "send_ask" => {
            // Always the client's approver, never the external sender. The
            // marker in the subject is how a reply finds its run again.
            let client = s(&state, "client");
            append(&format!("{}/outbox.jsonl", out_dir()), &json!({
                "to": lookup(&APPROVER, &client),
                "subject": format!("Approve this expense: {} {} {} [areev:ap/{}]",
                    s(&state, "vendor"), f(&state, "amount", 0.0), s(&state, "currency"),
                    marker(&s(&state, "message_id"))),
                "vendor": s(&state, "vendor"), "amount": f(&state, "amount", 0.0),
                "reason": s(&state, "review_reason"),
                "reply_with": "approve | reject | revise + `Field: value` lines",
            }));
            emit(&json!({"ask_sent": true}));
        }
        "apply_corrections" => {
            // Merge the approver's Field: value lines, mark them settled,
            // and go back around to re-ask -- the plan bounds this cycle
            // with max_cycles.
            let mut merged = json!({"field_confidence": 1.0, "revised": true});
            if let Some(c) = state.get("corrections").and_then(Value::as_object) {
                for (field, value) in c {
                    match field.as_str() {
                        "vendor" | "currency" | "category" => {
                            merged[field] = json!(value.as_str().unwrap_or_default());
                        }
                        "amount" => {
                            let n: f64 = value.as_str().and_then(|x| x.parse().ok()).unwrap_or(0.0);
                            merged[field] = json!(n);
                        }
                        _ => {}
                    }
                }
            }
            emit(&merged);
        }
        "append_sheet" => {
            let row = json!({
                "row_key": s(&state, "row_key"), "client": s(&state, "client"),
                "vendor": s(&state, "vendor"), "amount": f(&state, "amount", 0.0),
                "currency": s(&state, "currency"), "category": s(&state, "category"),
                "approved_by": state.get("responder").and_then(Value::as_str).unwrap_or("auto"),
            });
            append(&format!("{}/sheet.jsonl", out_dir()), &row);
            emit(&json!({"appended": 1, "row_key": row["row_key"]}));
        }
        "reply_email" => {
            let outcome = if state.get("decision").and_then(Value::as_str) == Some("reject") {
                "rejected"
            } else {
                "posted"
            };
            append(&format!("{}/outbox.jsonl", out_dir()), &json!({
                "to": s(&state, "sender"),
                "subject": format!("Re: {}", s(&state, "message_id")),
                "outcome": outcome,
            }));
            emit(&json!({"sent": true}));
        }
        other => {
            eprintln!("unknown tool: {other:?}");
            return 1;
        }
    }
    0
}

// ── the connector seam ─────────────────────────────────────────────────────
// The contract from docs/triggers.md: an ABSENT cursor means "seed and fire
// nothing", so declaring a trigger never replays mailbox history.

fn connector_main() -> i32 {
    let req = read_stdin();
    let upto = env("MAIL_UPTO", "03".into());
    let mailbox = s(&req, "scope").trim_start_matches("mailbox:").to_string();
    let client = MAILBOXES.iter().find(|(_, m)| *m == mailbox).map(|(c, _)| *c);
    let mut names: Vec<String> = client
        .and_then(|c| std::fs::read_dir(format!("{}/{c}", fixtures())).ok())
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                .filter(|n| n.ends_with(".json") && n[..2] <= *upto)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let Some(cursor) = req.get("cursor").and_then(Value::as_str) else {
        emit(&json!({"items": [], "cursor": "0", "more": false}));
        return 0;
    };
    let consumed: usize = cursor.parse().unwrap_or(0);
    let max = req.get("max_items").and_then(Value::as_u64).unwrap_or(100) as usize;
    let mut items = Vec::new();
    for name in names.iter().skip(consumed).take(max) {
        let raw = std::fs::read_to_string(format!("{}/{}/{name}", fixtures(), client.unwrap()))
            .unwrap();
        let payload: Value = serde_json::from_str(&raw).unwrap();
        items.push(json!({"id": payload["message_id"], "payload": payload}));
    }
    let end = consumed + items.len();
    emit(&json!({"items": items, "cursor": end.to_string(), "more": end < names.len()}));
    0
}

// ── the driver ─────────────────────────────────────────────────────────────

/// The embedded stack the bindings wrap: store -> facade (+ a CAL executor
/// whose plan cache lives as long as the handle).
struct Db {
    facade: Arc<AreevFacade>,
    cal: CalExecutor,
    actor: String,
}

impl Db {
    fn open(actor: &str) -> Db {
        std::fs::create_dir_all(out_dir()).ok();
        let store = areev_store::Areev::open_with_telemetry(
            &db_path(),
            areev_store::TelemetryMode::Aggregate,
        )
        .expect("open memory");
        let facade = AreevFacade::with_session(store, Some(NS.to_string()), None);
        Db {
            facade: Arc::new(facade),
            cal: CalExecutor::new(CalExecutorConfig::default()),
            actor: actor.to_string(),
        }
    }

    fn cal(&self, query: &str) -> Value {
        let res = self.cal.execute(query, &*self.facade).expect("cal");
        res.payload_json().expect("cal payload")
    }

    fn add(&self, grain_type: &str, mut fields: Map<String, Value>) -> String {
        fields.entry("namespace".to_string()).or_insert(json!(NS));
        self.facade.cal_add(grain_type, &fields).expect("add").to_hex()
    }

    /// The same runner the bindings build: your tool command behind the
    /// one-process-per-call executor, acting as this handle's principal.
    fn runner(&self, tool_cmd: Option<&str>) -> areev_run::Runner {
        let executor: Arc<dyn areev_run::HostToolExecutor> = match tool_cmd {
            Some(cmd) => Arc::new(areev_run::CommandExecutor::new(cmd)),
            None => Arc::new(NoExec),
        };
        areev_run::Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(areev_run::SystemClock),
            executor,
            llm: None,
            observer: None,
            ns: NS.to_string(),
            principal: self.actor.clone(),
        }
    }

    fn run_opts() -> areev_run::RunOptions {
        areev_run::RunOptions {
            budgets: areev_run::BudgetsSpec {
                max_usd_micros: Some(2_000_000),
                max_wall_ms: Some(300_000),
                ..Default::default()
            },
            ask_ttl_sec: Some(3600),
            workers: 4,
            ..Default::default()
        }
    }
}

struct NoExec;
impl areev_run::HostToolExecutor for NoExec {
    fn execute(&self, tool: &str, _h: &str, _i: &Value, _k: &str) -> areev_run::ExecResult {
        areev_run::ExecResult::Err {
            cause: areev_run::FailCause::ExecutorError,
            detail: format!("no tool_cmd given; cannot execute host tool '{tool}'"),
        }
    }
}

/// The trigger evaluator's bridge into the runtime, verbatim from the
/// bindings: a firing starts a real run through the same Runner.
struct RunnerStarter {
    runner: areev_run::Runner,
    opts: areev_run::RunOptions,
}

impl areev_trigger::RunStarter for RunnerStarter {
    fn start(&self, workflow: &str, run_id: &str, input: Value) -> areev_trigger::StartResult {
        let hash = match Hash::from_hex(workflow) {
            Ok(h) => h,
            Err(e) => return areev_trigger::StartResult::Failed(format!("workflow: {e}")),
        };
        match self.runner.start(&hash, run_id, input, &self.opts) {
            Ok(_) => areev_trigger::StartResult::Started,
            Err(areev_run::CoreRunError::Tainted { why }) if why.contains("already exists") => {
                areev_trigger::StartResult::Duplicate
            }
            Err(e) => areev_trigger::StartResult::Failed(e.to_string()),
        }
    }
}

fn self_cmd(sub: &str) -> String {
    format!("{} {sub}", std::env::current_exe().unwrap().display())
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

fn seed() -> i32 {
    let db = Db::open(DESK);

    let tool_def = |name: &str, description: &str, executor_kind: Option<&str>| {
        let mut fields = obj(json!({
            "tool_name": name, "kind": "definition",
            "tool_description": description, "created_at": EPOCH_MS,
        }));
        if let Some(k) = executor_kind {
            fields.insert("executor_kind".into(), json!(k));
        }
        db.add("tool", fields)
    };

    let parse = tool_def("parse_attachments", "pull the text layer out of each attachment", None);
    let extract = tool_def("extract_rows", "turn invoice text into expense rows", None);
    let validate = tool_def("validate_rows", "decide whether a person has to look", None);
    let ask = tool_def("send_ask", "email the client's approver, with a marker", None);
    let review = tool_def(
        "human_review",
        "a person decides: approve, revise, or reject",
        Some("client"),
    );
    let corrections = tool_def("apply_corrections", "merge the approver's Field: value lines", None);
    let sheet = tool_def("append_sheet", "append the approved row to the expense sheet", None);
    let reply = tool_def("reply_email", "tell the sender what happened", None);

    let wf = db.add("workflow", obj(json!({
        "name": "invoice-to-accounting",
        "nodes": ["parse_attachments", "extract_rows", "validate_rows", "send_ask",
                  "human_review", "apply_corrections", "append_sheet",
                  "reply_done", "reply_rejected"],
        "edges": [
            {"src": "parse_attachments", "dst": "extract_rows"},
            {"src": "extract_rows", "dst": "validate_rows"},
            {"src": "validate_rows", "dst": "append_sheet", "cond": "needs_review == false"},
            {"src": "validate_rows", "dst": "send_ask", "cond": "needs_review == true"},
            {"src": "send_ask", "dst": "human_review"},
            {"src": "human_review", "dst": "append_sheet", "cond": "decision == \"approve\""},
            {"src": "human_review", "dst": "apply_corrections", "cond": "decision == \"revise\""},
            {"src": "human_review", "dst": "reply_rejected", "cond": "decision == \"reject\""},
            // The correction cycle: revise -> merge -> re-ask, at most 3 times.
            {"src": "apply_corrections", "dst": "send_ask", "max_cycles": 3},
            {"src": "append_sheet", "dst": "reply_done"},
        ],
        "bindings": {"parse_attachments": parse, "extract_rows": extract,
                     "validate_rows": validate, "send_ask": ask,
                     "human_review": review, "apply_corrections": corrections,
                     "append_sheet": sheet,
                     // Two nodes, one tool: both replies are the same effect.
                     "reply_done": reply, "reply_rejected": reply},
        "retries": {"extract_rows": 1},
        "created_at": EPOCH_MS,
    })));

    db.add("skill", obj(json!({
        "name": "invoice-triage",
        "description": "how this desk reads an invoice",
        "instructions": "Extract one row per invoice. Prefer the canonical vendor \
                         name from the alias facts. Never guess an amount: a \
                         low-confidence field goes to review, not to the sheet.",
        "created_at": EPOCH_MS,
    })));

    // Client knowledge lives under org.<client> -- exact namespaces to
    // write, a "org.*" prefix to read the whole desk in one query.
    for (subject, relation, object, ns) in [
        ("acme", "review_threshold_usd", "2500", "org.acme"),
        ("brightco", "review_threshold_usd", "2500", "org.brightco"),
        ("Meridian Freight", "payment_terms", "net_30", "org.acme.vendors"),
        ("Cobalt Cloud", "payment_terms", "net_45", "org.brightco.vendors"),
    ] {
        db.add("fact", obj(json!({
            "subject": subject, "relation": relation, "object": object, "namespace": ns,
        })));
    }

    // Retrieval + presentation ship IN the file as saved queries/templates
    // (qry:/tpl: meta rows) -- they replicate with the memory and are what
    // the triggers below name as declared context.
    db.cal("DEFINE TEMPLATE vendor_line AS \
            \"- {{subject}} {{relation}} {{object}} ({{confidence}})\"");
    db.cal("DEFINE QUERY \"extract_ctx\"($session) \
            DESCRIPTION \"what extraction should know before reading an invoice\" \
            AS { ASSEMBLE \"extract_ctx\" FROM \
            instructions: (RECALL skills LIMIT 2), \
            desk: (RECALL facts WHERE namespace = \"org.*\" LIMIT 120), \
            thread: (RECALL events WHERE session_id = $session RECENT 10) \
            BUDGET 4000 tokens FORMAT json }");
    db.cal("DEFINE QUERY \"desk_pulse\"() \
            DESCRIPTION \"the desk briefing itself: plan, tools, lessons, outcomes\" \
            AS { ASSEMBLE \"desk_pulse\" FROM \
            plan: (RECALL workflows LIMIT 3), \
            tools: (RECALL tools WHERE kind = \"definition\" LIMIT 12), \
            activity: (RECALL tools WHERE kind != \"definition\" RECENT 40), \
            lessons: (RECALL facts WHERE namespace = \"org.*\" LIMIT 40) \
            BUDGET 2500 tokens FORMAT markdown }");

    // Egress anonymization starts in audit mode on the client subtrees --
    // measure before you rewrite. NEVER on org.ops: the rewriter would
    // mangle the operational JSON (dates, 64-char hashes) that lives there.
    for ns in ["org.acme", "org.brightco"] {
        db.facade
            .with_store(|m| m.set_anon_policy(ns, "{\"mode\": \"audit\"}"))
            .expect("anon policy");
    }

    let mut triggers = Map::new();
    for (client, mailbox) in MAILBOXES {
        let hash = db.add("trigger", obj(json!({
            "kind": "polling", "connector": "mock",
            "scope": format!("mailbox:{mailbox}"), "interval_secs": 1,
            "workflow": wf, "dedup_key": ["/message_id"],
            "context_query": "extract_ctx($session = /thread)",
            "because": format!("poll the {client} AP mailbox for invoices"),
        })));
        triggers.insert(client.to_string(), json!(hash));
    }

    emit(&json!({"workflow": wf, "triggers": triggers}));
    0
}

fn ingest() -> i32 {
    let db = Db::open(DESK);
    let ev = areev_trigger::Evaluator {
        facade: Arc::clone(&db.facade),
        clock: Arc::new(areev_trigger::SystemClock),
        // A connector IS a tool: same JSON-on-stdio contract, same spawn
        // hardening, one subprocess shape to learn.
        connector: Some(Arc::new(areev_run::CommandExecutor::new(&self_cmd("connector")))),
        starter: Some(Arc::new(RunnerStarter {
            runner: db.runner(Some(&self_cmd("tools"))),
            opts: Db::run_opts(),
        })),
        credentials: BTreeMap::new(),
        ns: NS.to_string(),
        principal: DESK.to_string(),
    };
    let report = ev.run(&areev_trigger::EvalOptions::default()).expect("trigger run");
    emit(&serde_json::to_value(&report).unwrap());
    0
}

/// (run_id, ask_id, merged_state) for every parked run.
fn pending_asks(db: &Db) -> Vec<(String, String, Value)> {
    let runner = db.runner(None);
    let mut out = Vec::new();
    for run_id in runner.recent_runs(100).unwrap_or_default() {
        let Ok(report) = runner.inspect(&run_id) else { continue };
        let inspect = serde_json::to_value(&report).unwrap();
        if inspect.get("phase").and_then(Value::as_str) != Some("open") {
            continue;
        }
        if let Some(asks) = inspect.get("pending_asks").and_then(Value::as_object) {
            for (ask_id, entry) in asks {
                let state = entry.pointer("/ask/input").cloned().unwrap_or(json!({}));
                out.push((run_id.clone(), ask_id.clone(), state));
            }
        }
    }
    out
}

fn asks() -> i32 {
    let db = Db::open(DESK);
    let rows: Vec<Value> = pending_asks(&db)
        .into_iter()
        .map(|(run_id, ask, state)| {
            let item = state.get("item").unwrap_or(&state).clone();
            json!({
                "run_id": run_id, "ask": ask, "marker": marker(&s(&item, "message_id")),
                "vendor": state.get("vendor"), "amount": state.get("amount"),
                "reason": state.get("review_reason"),
            })
        })
        .collect();
    emit(&json!(rows));
    0
}

/// Deterministic reply reading: verb first, then Field: value lines. Quoted
/// history is cut, so a reply that quotes the ask does not re-approve
/// itself.
fn classify(body: &str) -> Option<Value> {
    let mut verb: Option<String> = None;
    let mut corrections = Map::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.starts_with('>')
            || (line.starts_with("On ") && line.ends_with(" wrote:"))
            || line.starts_with("-- Original Message")
            || line.starts_with("From: ")
        {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let field = ["Vendor", "Amount", "Currency", "Category"]
            .iter()
            .find(|k| line.len() > k.len() && line[..k.len()].eq_ignore_ascii_case(k)
                  && line[k.len()..].starts_with(':'));
        if let Some(k) = field {
            corrections.insert(k.to_lowercase(), json!(line[k.len() + 1..].trim()));
        } else if verb.is_none() {
            verb = line.split_whitespace().next().map(str::to_lowercase);
        }
    }
    match verb.as_deref() {
        Some("reject") => Some(json!({"decision": "reject"})),
        Some("revise") => Some(json!({"decision": "revise", "corrections": corrections})),
        _ if !corrections.is_empty() => {
            Some(json!({"decision": "revise", "corrections": corrections}))
        }
        Some("approve") => Some(json!({"decision": "approve"})),
        _ => None,
    }
}

fn reply(path: &str) -> i32 {
    let mail: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let sender = s(&mail, "from");
    let principal = if sender == DESK_FROM {
        DESK.to_string()
    } else {
        format!("user:{}", sender.split('@').next().unwrap_or("?"))
    };
    let subject = s(&mail, "subject");
    let Some(ref_start) = subject.find("[areev:ap/") else {
        eprintln!("unclassified reply -- left unactioned, a person reads it");
        return 3;
    };
    let reference = &subject[ref_start + 10..ref_start + 22];
    let Some(verdict) = classify(&s(&mail, "body")) else {
        eprintln!("unclassified reply -- left unactioned, a person reads it");
        return 3;
    };

    let db = Db::open(DESK);
    for (run_id, ask_id, state) in pending_asks(&db) {
        let item = state.get("item").unwrap_or(&state).clone();
        if marker(&s(&item, "message_id")) != reference {
            continue;
        }
        let mut result = verdict.clone();
        result["responder"] = json!(principal);
        let runner = db.runner(None);
        if let Err(e) = runner.respond(&run_id, &ask_id, result, false, &principal) {
            eprintln!("respond refused: {e}");
            return 4;
        }
        // A correction the approver then approved is a lesson worth keeping:
        // record the alias where the client's knowledge lives, and record
        // the correction itself as a tool outcome the loop can cluster.
        if verdict["decision"] == "approve"
            && state.get("revised").and_then(Value::as_bool).unwrap_or(false)
            && state.get("vendor") != item.get("vendor")
        {
            db.add("fact", obj(json!({
                "subject": s(&item, "vendor"), "relation": "mg:alias_of",
                "object": s(&state, "vendor"),
                "namespace": format!("org.{}.vendors", s(&state, "client")),
            })));
        }
        if let Some(c) = verdict.get("corrections").and_then(Value::as_object) {
            // The result string IS the loop's cluster key (normalized,
            // truncated at 80 chars) -- keep it short and stable.
            for field in c.keys() {
                db.facade
                    .record_tool_call(
                        NS, "extract_rows", None,
                        &format!("corr:{field}:{}", s(&state, "client")),
                        true, Some(&s(&item, "thread")), None, Some(&run_id),
                        None, None, None, None, None, None,
                    )
                    .expect("record correction");
            }
        }
        let session = db
            .runner(Some(&self_cmd("tools")))
            .resume(&run_id, &Db::run_opts())
            .expect("resume");
        let outcome = match session {
            areev_run::RunSession::Finished { outcome, .. } => {
                json!({"finished": format!("{outcome:?}")})
            }
            areev_run::RunSession::Parked { envelope, .. } => json!({"parked": envelope}),
        };
        emit(&json!({
            "run_id": run_id, "decision": verdict["decision"],
            "responder": principal, "outcome": outcome,
        }));
        return 0;
    }
    eprintln!("no parked run matches marker {reference}");
    5
}

fn rec_rows(db: &Db, status: Option<RecStatus>) -> Vec<Value> {
    let sub = BorrowedSubstrate::new(&db.facade);
    Engine::with_builtins()
        .recommendations(&sub, status)
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "hash": r.hash, "severity": r.severity.as_str(),
                "summary": r.summary.render(), "analyzer": r.analyzer,
                "target": r.target_ref,
            })
        })
        .collect()
}

fn improve() -> i32 {
    let db = Db::open(DESK);
    let engine = Engine::with_builtins();
    // Tune the analyzers to this desk's volume: at ~4 invoices a week the
    // stock "half of all runs failed" bar would stay silent for a quarter.
    {
        let mut sub = BorrowedSubstrate::new(&db.facade);
        engine
            .set_analyzer_config(
                &mut sub,
                "loop.run_outcome/1",
                AnalyzerConfigUpdate {
                    enabled: Some(true),
                    params: Some(obj(json!({"min_failure_ratio": 0.4}))),
                    ..Default::default()
                },
                &ScopeSet::all(),
            )
            .expect("analyzer config");
    }
    // Optional LLM reflection (DISCOVER->GROUND->VERIFY) on top of the
    // deterministic floor: LOOP_LLM_CMD names any --llm-cmd backend (see
    // examples/llm/); every model finding is grounded in grains first.
    let engine = match std::env::var("LOOP_LLM_CMD") {
        Ok(cmd) if !cmd.is_empty() => {
            engine.with_llm(Box::new(areev_loop::CommandLlm::new(&cmd, None).expect("llm cmd")))
        }
        _ => engine,
    };
    let report = {
        let mut sub = BorrowedSubstrate::new(&db.facade);
        let opts = areev_loop::RunOptions {
            triggering_actor: Some(db.actor.clone()),
            ..Default::default()
        };
        engine.run(&mut sub, &opts, now_ms()).expect("loop run")
    };
    emit(&json!({
        "loop": serde_json::to_value(&report).unwrap(),
        "pending": rec_rows(&db, Some(RecStatus::Pending)),
    }));
    0
}

fn decide(args: &[String]) -> i32 {
    let (Some(rec_prefix), Some(action)) = (args.first(), args.get(1)) else {
        eprintln!("usage: decide <rec> approve|apply|dismiss --because ... --as user:X");
        return 2;
    };
    let mut because = None;
    let mut actor = "user:anonymous".to_string();
    let mut it = args[2..].iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--because" => because = it.next().cloned(),
            "--as" => actor = it.next().cloned().unwrap_or(actor),
            _ => {}
        }
    }
    let Some(because) = because else {
        eprintln!("a decision with no written reason is refused");
        return 2;
    };
    let db = Db::open(&actor);
    let engine = Engine::with_builtins();
    let hash = rec_rows(&db, None)
        .iter()
        .filter_map(|r| r["hash"].as_str().map(str::to_string))
        .find(|h| h.starts_with(rec_prefix.as_str()))
        .unwrap_or_else(|| rec_prefix.clone());
    let mut sub = BorrowedSubstrate::new(&db.facade);
    let scopes = ScopeSet::all();
    let now = now_ms();
    let outcome = match action.as_str() {
        "approve" => engine
            .review(&mut sub, &hash, Decision::Approve, &actor, ObserverType::Human,
                    &scopes, &because, now)
            .map(|_| json!({"hash": hash, "status": "approved"})),
        // The fused approve+apply, preflighted FIRST so an advisory finding
        // refuses before any state transition -- same order as the bindings.
        "apply" => engine
            .preflight_apply(&sub, &hash, &scopes, false, false)
            .and_then(|_| {
                engine.review(&mut sub, &hash, Decision::Approve, &actor,
                              ObserverType::Human, &scopes, &because, now)?;
                engine.apply(&mut sub, &hash, &actor, ObserverType::Human, &scopes,
                             &because, false, now)
            })
            .map(|a| json!({"hash": hash, "rollbackable": a.rollbackable})),
        "dismiss" => engine
            .review(&mut sub, &hash, Decision::Reject, &actor, ObserverType::Human,
                    &scopes, &because, now)
            .map(|_| json!({"hash": hash, "status": "rejected"})),
        other => {
            eprintln!("unknown action {other:?}");
            return 2;
        }
    };
    match outcome {
        Ok(v) => {
            emit(&v);
            0
        }
        Err(e) => {
            eprintln!("refused: {e}");
            4
        }
    }
}

fn teach(args: &[String]) -> i32 {
    let [ns, subject, relation, object] = args else {
        eprintln!("usage: teach NS SUBJECT RELATION OBJECT");
        return 2;
    };
    let db = Db::open(DESK);
    let hash = db.add("fact", obj(json!({
        "subject": subject, "relation": relation, "object": object, "namespace": ns,
    })));
    println!("{hash}");
    0
}

fn brief() -> i32 {
    let db = Db::open(DESK);
    println!("{}", db.cal("RUN \"desk_pulse\"()"));
    println!(
        "{}",
        db.cal("RECALL facts WHERE namespace = \"org.*\" LIMIT 20 FORMAT TEMPLATE vendor_line")
    );
    0
}

fn runs() -> i32 {
    // Outcome the same way `areev run list` derives it: the run-terminal
    // Observation the runtime writes in agent:harness.
    let db = Db::open(DESK);
    let obs = db.cal("RECALL observations WHERE namespace = \"agent:harness\" RECENT 200 FORMAT json");
    let mut outcome = BTreeMap::new();
    for g in obs.get("grains").and_then(Value::as_array).unwrap_or(&vec![]) {
        let fields = &g["fields"];
        if fields.get("observation_kind").and_then(Value::as_str) == Some("run_outcome") {
            outcome.insert(s(fields, "run_id"), s(fields, "object"));
        }
    }
    let rows: Vec<Value> = db
        .runner(None)
        .recent_runs(100)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let o = outcome.get(&r).cloned().unwrap_or_else(|| "open".into());
            json!({"run_id": r, "outcome": o})
        })
        .collect();
    emit(&json!(rows));
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("tools") => tool_main(),
        Some("connector") => connector_main(),
        Some("seed") => seed(),
        Some("ingest") => ingest(),
        Some("asks") => asks(),
        Some("reply") => reply(&args[1]),
        Some("improve") => improve(),
        Some("decide") => decide(&args[1..]),
        Some("teach") => teach(&args[1..]),
        Some("brief") => brief(),
        Some("runs") => runs(),
        _ => {
            eprintln!(
                "usage: agent tools|connector|seed|ingest|asks|reply|improve|decide|teach|brief|runs"
            );
            2
        }
    };
    std::process::exit(code);
}
