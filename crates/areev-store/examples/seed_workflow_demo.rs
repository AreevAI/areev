//! Seeds three Workflow grains into the "Northwind Support — billing desk"
//! corpus (see `seed_support_demo.rs`), so the console's workflow graph view
//! has real plans to render: a linear+branch plan, a plan with a human
//! approval gate, and a plan with a bounded retry cycle.
//!
//! Node kinds are deliberately varied so the graph view exercises all of
//! them: host-tool nodes (bound to a `Definition` Tool with the default
//! `ExecutorKind::Host`), a human-approval gate (`ExecutorKind::Client`),
//! and unbound "abstract" nodes (no binding — a journaled LLM tool-calling
//! step at run time). `escalation_gate` is bound from two different
//! workflows to the *same* Tool grain hash — `add` is idempotent on content
//! address, so this is one grain, reused, not a duplicate.
//!
//! Usage: cargo run --release -p areev-store --example seed_workflow_demo -- <path.db>

use areev_core::types::{ExecutorKind, Tool, ToolKind, Workflow};
use areev_core::Grain;
use areev_store::Areev;

const NS: &str = "support";
const DAY: i64 = 86_400_000;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seed_workflow_demo <path.db>");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let d = |days_ago: i64| now - days_ago * DAY;

    let mut m = Areev::open(&path).unwrap();

    let def = |name: &str, desc: &str, client: bool, created: i64| {
        let mut t = Tool::new(name).kind(ToolKind::Definition).tool_description(desc).created_at(created).namespace(NS);
        if client {
            t = t.executor_kind(ExecutorKind::Client);
        }
        t
    };

    // ── shared tool definitions ─────────────────────────────────────────
    let lookup_refund_h = m.add(&def("stripe_refund_lookup", "Look up a Stripe refund by charge id", false, d(20))).unwrap();
    let issue_refund_h = m.add(&def("stripe_refund_issue", "Issue a full or partial Stripe refund and notify the customer", false, d(20))).unwrap();
    let refund_gate_h = m.add(&def("refund_approval_gate", "A human approves a refund above the auto-approve limit", true, d(20))).unwrap();
    let fetch_invoice_h = m.add(&def("netsuite_invoice_fetch", "Fetch a disputed invoice from NetSuite", false, d(18))).unwrap();
    let credit_note_h = m.add(&def("netsuite_credit_note", "Raise a NetSuite credit note against a disputed invoice line", false, d(18))).unwrap();
    // Reused across two workflows below — same (name, desc, kind) means the
    // same content address, i.e. one grain.
    let escalation_gate_h = m.add(&def("escalation_gate", "A human reviews an escalated case before it proceeds", true, d(18))).unwrap();
    let smart_retries_h = m.add(&def("stripe_smart_retries", "Retry a failed payment via Stripe Smart Retries", false, d(15))).unwrap();

    // ── workflow 1: refund_approval — branch on the auto-approve limit ──
    let refund_approval = Workflow::new(vec![
        "lookup_refund".into(),
        "check_limit".into(),
        "auto_approve".into(),
        "approve".into(),
        "issue_refund".into(),
        "deny".into(),
    ])
    .trigger("refund_requests ticket opened")
    .edge("lookup_refund", "check_limit")
    .cond_edge("check_limit", "auto_approve", "amount.under_limit == true")
    .cond_edge("check_limit", "approve", "amount.under_limit == false")
    .edge("auto_approve", "issue_refund")
    .cond_edge("approve", "issue_refund", "approved == true")
    .cond_edge("approve", "deny", "approved == false")
    .bind("lookup_refund", &lookup_refund_h.to_hex())
    .bind("auto_approve", &issue_refund_h.to_hex())
    .bind("issue_refund", &issue_refund_h.to_hex())
    .bind("approve", &refund_gate_h.to_hex())
    // check_limit, deny: unbound — abstract nodes
    .created_at(d(20))
    .namespace(NS);
    let refund_wf_h = m.add(&refund_approval).unwrap();

    // ── workflow 2: invoice_dispute_triage — validate, credit or escalate ─
    let invoice_dispute = Workflow::new(vec![
        "fetch_invoice".into(),
        "validate_dispute".into(),
        "apply_credit".into(),
        "escalate".into(),
        "notify_customer".into(),
    ])
    .trigger("invoice_disputes ticket opened")
    .edge("fetch_invoice", "validate_dispute")
    .cond_edge("validate_dispute", "apply_credit", "dispute.valid == true")
    .cond_edge("validate_dispute", "escalate", "dispute.valid == false")
    .edge("apply_credit", "notify_customer")
    .edge("escalate", "notify_customer")
    .bind("fetch_invoice", &fetch_invoice_h.to_hex())
    .bind("apply_credit", &credit_note_h.to_hex())
    .bind("escalate", &escalation_gate_h.to_hex())
    // validate_dispute, notify_customer: unbound — abstract nodes
    .created_at(d(18))
    .namespace(NS);
    let dispute_wf_h = m.add(&invoice_dispute).unwrap();

    // ── workflow 3: dunning_escalation — bounded retry cycle ─────────────
    // check_payment -> send_reminder is the max_cycles-bounded back-edge
    // that makes the {send_reminder, check_payment} cycle legal (RUN-E002
    // refuses an unbounded one at load).
    let dunning = Workflow::new(vec![
        "send_reminder".into(),
        "check_payment".into(),
        "resolved".into(),
        "escalate_to_collections".into(),
    ])
    .trigger("payment failed 3 consecutive months")
    .edge("send_reminder", "check_payment")
    .edge_with_cycles("check_payment", "send_reminder", 3)
    .cond_edge("check_payment", "resolved", "paid == true")
    .cond_edge("check_payment", "escalate_to_collections", "paid == false")
    .bind("send_reminder", &smart_retries_h.to_hex())
    .bind("escalate_to_collections", &escalation_gate_h.to_hex())
    // check_payment, resolved: unbound — abstract nodes
    .created_at(d(15))
    .namespace(NS);
    let dunning_wf_h = m.add(&dunning).unwrap();

    println!("refund_approval        -> {refund_wf_h}");
    println!("invoice_dispute_triage -> {dispute_wf_h}");
    println!("dunning_escalation     -> {dunning_wf_h}");
    let st = m.stats().unwrap();
    println!("store now has {} grains ({} ops)", st.grains, st.ops);
}
