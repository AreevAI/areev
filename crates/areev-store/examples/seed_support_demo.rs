//! Seeds the "Northwind Support — billing desk" console demo.
//!
//! A support org whose agent memory holds the billing knowledge graph: who
//! owns each queue, who backs them up, who each account is assigned to, and
//! which system bills them. The corpus is planted so a single `areev loop
//! run` fires most of the built-in analyzers on data a support lead can read:
//! a live ownership contradiction, an exact-duplicate contact, a dead legacy
//! cluster, two stalled skills, an expired promo, and a tool that keeps 403ing.
//!
//! Usage: cargo run --release -p areev-store --example seed_support_demo -- <path.db>

use areev_core::types::{Event, Fact, Goal, Grain, Skill, Tool};
use areev_store::{Areev, TelemetryMode};

const NS: &str = "support";
const DAY: i64 = 86_400_000;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seed_support_demo <path.db>");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let d = |days_ago: i64| now - days_ago * DAY;

    let mut m = Areev::open_with_telemetry(&path, TelemetryMode::Aggregate).unwrap();

    // ── the knowledge graph ────────────────────────────────────────────────
    // (subject, relation, object, days_ago, confidence)
    let facts: &[(&str, &str, &str, i64, f64)] = &[
        // ── routing: who handles which billing queue ──
        // Two live owners for invoice_disputes: Priya went on leave and the
        // handover was recorded as a new fact instead of a supersession.
        ("invoice_disputes", "owner", "priya_raman", 274, 0.95),
        ("invoice_disputes", "owner", "marcus_webb", 140, 0.80),
        ("invoice_disputes", "backup_owner", "tom_reyes", 200, 0.9),
        ("invoice_disputes", "escalates_to", "nora_bright", 200, 1.0),
        ("invoice_disputes", "sla", "1_business_day", 210, 1.0),
        ("invoice_disputes", "runbook", "rb-billing-014", 205, 1.0),
        ("invoice_disputes", "slack_channel", "#billing-disputes", 205, 1.0),
        ("invoice_disputes", "weekly_volume", "42_tickets", 60, 0.7),
        ("refund_requests", "owner", "lena_fischer", 190, 0.95),
        ("refund_requests", "escalates_to", "priya_raman", 190, 0.9),
        ("refund_requests", "sla", "4_hours", 190, 1.0),
        ("refund_requests", "auto_approve_limit", "500_usd", 185, 1.0),
        ("refund_requests", "runbook", "rb-billing-022", 185, 1.0),
        ("failed_payments", "owner", "marcus_webb", 175, 0.95),
        ("failed_payments", "backup_owner", "tom_reyes", 175, 0.9),
        ("failed_payments", "sla", "2_hours", 175, 1.0),
        ("failed_payments", "runbook", "rb-billing-031", 170, 1.0),
        ("dunning", "owner", "marcus_webb", 160, 0.95),
        ("dunning", "tooling", "stripe_smart_retries", 160, 1.0),
        ("dunning", "sla", "3_business_days", 158, 1.0),
        ("chargebacks", "owner", "lena_fischer", 150, 0.95),
        ("chargebacks", "escalates_to", "dana_okafor", 150, 0.9),
        ("chargebacks", "response_window", "20_calendar_days", 148, 1.0),
        ("plan_upgrades", "owner", "sanjay_iyer", 145, 0.95),
        ("plan_upgrades", "escalates_to", "nora_bright", 145, 0.9),
        ("tax_exemption", "owner", "dana_okafor", 140, 0.95),
        ("tax_exemption", "tooling", "avalara", 140, 1.0),
        ("sales_tax_eu", "owner", "dana_okafor", 138, 0.9),
        ("annual_renewals", "owner", "sanjay_iyer", 130, 0.95),
        ("credit_notes", "owner", "priya_raman", 128, 0.9),
        ("proration_queries", "owner", "marcus_webb", 120, 0.9),
        // ── the people ──
        ("priya_raman", "role", "billing_operations_lead", 300, 1.0),
        ("priya_raman", "reports_to", "nora_bright", 300, 1.0),
        ("priya_raman", "region", "apac", 300, 1.0),
        ("priya_raman", "timezone", "asia/kolkata", 300, 1.0),
        ("priya_raman", "expertise", "revenue_recognition", 280, 0.9),
        ("marcus_webb", "role", "billing_support_specialist", 260, 1.0),
        ("marcus_webb", "reports_to", "priya_raman", 260, 1.0),
        ("marcus_webb", "region", "emea", 260, 1.0),
        ("marcus_webb", "timezone", "europe/berlin", 260, 1.0),
        ("marcus_webb", "expertise", "dunning_and_retries", 240, 0.9),
        ("marcus_webb", "status", "active", 260, 1.0),
        ("lena_fischer", "role", "disputes_and_chargebacks_specialist", 220, 1.0),
        ("lena_fischer", "reports_to", "nora_bright", 220, 1.0),
        ("lena_fischer", "region", "emea", 220, 1.0),
        ("lena_fischer", "expertise", "chargeback_representment", 215, 0.9),
        ("dana_okafor", "role", "revenue_operations_analyst", 250, 1.0),
        ("dana_okafor", "reports_to", "nora_bright", 250, 1.0),
        ("dana_okafor", "region", "amer", 250, 1.0),
        ("dana_okafor", "expertise", "indirect_tax", 245, 0.9),
        ("tom_reyes", "role", "tier1_support_agent", 120, 1.0),
        ("tom_reyes", "reports_to", "marcus_webb", 120, 1.0),
        ("tom_reyes", "region", "amer", 120, 1.0),
        ("sanjay_iyer", "role", "enterprise_account_manager", 230, 1.0),
        ("sanjay_iyer", "reports_to", "nora_bright", 230, 1.0),
        ("sanjay_iyer", "region", "apac", 230, 1.0),
        ("nora_bright", "role", "support_manager", 320, 1.0),
        ("nora_bright", "expertise", "escalation_policy", 320, 1.0),
        // ── the accounts ──
        ("acme_corp", "plan", "enterprise_annual", 300, 1.0),
        ("acme_corp", "currency", "usd", 300, 1.0),
        ("acme_corp", "billing_system", "stripe", 300, 1.0),
        ("acme_corp", "payment_method", "ach", 290, 1.0),
        ("acme_corp", "assigned_to", "sanjay_iyer", 290, 1.0),
        ("acme_corp", "renewal_date", "2026-11-01", 200, 1.0),
        ("acme_corp", "contract_value", "480000_usd", 200, 0.9),
        ("acme_corp", "industry", "manufacturing", 300, 1.0),
        // the same contact recorded twice, months apart, by two agents
        ("acme_corp", "billing_contact", "ap@acme.example", 260, 1.0),
        ("acme_corp", "billing_contact", "ap@acme.example", 95, 0.9),
        ("globex", "tier", "silver", 280, 1.0),
        ("globex", "plan", "business_monthly", 280, 1.0),
        ("globex", "currency", "eur", 280, 1.0),
        ("globex", "billing_system", "stripe", 280, 1.0),
        ("globex", "payment_method", "sepa_direct_debit", 275, 1.0),
        ("globex", "vat_id", "DE811907980", 275, 1.0),
        ("globex", "assigned_to", "marcus_webb", 270, 1.0),
        ("globex", "status", "dunning_stage_2", 20, 0.95),
        ("globex", "industry", "logistics", 280, 1.0),
        ("initech", "tier", "bronze", 210, 1.0),
        ("initech", "plan", "starter_monthly", 210, 1.0),
        ("initech", "currency", "usd", 210, 1.0),
        ("initech", "payment_method", "card", 210, 1.0),
        ("initech", "billing_system", "stripe", 210, 1.0),
        ("initech", "assigned_to", "tom_reyes", 200, 1.0),
        ("initech", "status", "past_due", 15, 0.95),
        ("umbrella_health", "tier", "gold", 190, 1.0),
        ("umbrella_health", "plan", "enterprise_annual", 190, 1.0),
        ("umbrella_health", "currency", "usd", 190, 1.0),
        ("umbrella_health", "billing_system", "netsuite", 190, 1.0),
        ("umbrella_health", "payment_method", "wire", 190, 1.0),
        ("umbrella_health", "requires", "signed_baa", 185, 1.0),
        ("umbrella_health", "assigned_to", "sanjay_iyer", 185, 1.0),
        ("umbrella_health", "industry", "healthcare", 190, 1.0),
        ("soylent_labs", "tier", "silver", 170, 1.0),
        ("soylent_labs", "plan", "business_annual", 170, 1.0),
        ("soylent_labs", "currency", "gbp", 170, 1.0),
        ("soylent_labs", "assigned_to", "dana_okafor", 165, 1.0),
        ("soylent_labs", "industry", "food_science", 170, 1.0),
        // ── the systems ──
        ("stripe", "purpose", "card_and_ach_processing", 300, 1.0),
        ("stripe", "owner", "dana_okafor", 300, 1.0),
        ("stripe", "key_scope", "restricted", 60, 0.9),
        ("netsuite", "purpose", "invoicing_and_general_ledger", 300, 1.0),
        ("netsuite", "owner", "dana_okafor", 300, 1.0),
        ("avalara", "purpose", "sales_tax_calculation", 250, 1.0),
        ("avalara", "owner", "dana_okafor", 250, 1.0),
        ("zendesk", "purpose", "ticketing", 310, 1.0),
        ("zendesk", "owner", "nora_bright", 310, 1.0),
        // ── the dead Zuora era: true, ancient, and never asked about ──
        ("zuora_legacy", "status", "decommissioned", 520, 1.0),
        ("zuora_legacy", "owner", "ex_contractor_rk", 540, 0.8),
        ("zuora_legacy", "runbook", "rb-legacy-002", 540, 0.8),
        ("chargeback_portal_v1", "status", "retired", 500, 1.0),
        ("chargeback_portal_v1", "owner", "ex_contractor_rk", 500, 0.8),
        ("soylent_labs", "legacy_account_id", "ZU-88213", 480, 0.8),
    ];

    for (s, r, o, days, conf) in facts {
        let f = Fact::new(s, r, o)
            .namespace(NS)
            .created_at(d(*days))
            .confidence(*conf)
            .source_type("agent");
        m.add(&f).unwrap();
    }

    // ── history: two facts that were properly superseded ───────────────────
    // Acme grew into platinum; Priya went on leave. Both keep their old value
    // as history, which is what a supersession is *for* — unlike the
    // invoice_disputes handover above, which never got one.
    let tier_v1 = m
        .add(
            &Fact::new("acme_corp", "tier", "gold")
                .namespace(NS)
                .created_at(d(300))
                .source_type("agent"),
        )
        .unwrap();
    let mut tier_v2 = Fact::new("acme_corp", "tier", "platinum")
        .namespace(NS)
        .created_at(d(90))
        .source_type("agent");
    m.supersede(&tier_v1, &mut tier_v2).unwrap();

    let status_v1 = m
        .add(
            &Fact::new("priya_raman", "status", "active")
                .namespace(NS)
                .created_at(d(300))
                .source_type("agent"),
        )
        .unwrap();
    let mut status_v2 = Fact::new("priya_raman", "status", "on_parental_leave")
        .namespace(NS)
        .created_at(d(45))
        .source_type("agent");
    m.supersede(&status_v1, &mut status_v2).unwrap();

    // ── an expired fact: the Q1 promo Acme is still being quoted ───────────
    let promo = Fact::new("acme_corp", "promo_discount", "20_percent_q1")
        .namespace(NS)
        .created_at(d(240))
        .valid_to(d(126))
        .source_type("agent");
    m.add(&promo).unwrap();
    let promo2 = Fact::new("globex", "promo_discount", "10_percent_winback")
        .namespace(NS)
        .created_at(d(200))
        .valid_to(d(80))
        .source_type("agent");
    m.add(&promo2).unwrap();

    // ── the fork seed ──────────────────────────────────────────────────────
    // Left as a single head here; the build script clones the file, supersedes
    // it two different ways, and merges the branches back so the console shows
    // a real open fork (two channels edited the same fact while offline).
    let fork_parent = m
        .add(
            &Fact::new("acme_corp", "escalation_contact", "priya_raman")
                .namespace(NS)
                .created_at(d(150))
                .source_type("agent"),
        )
        .unwrap();
    println!("fork_parent={}", fork_parent.to_hex());

    // ── skills the agent is practising ─────────────────────────────────────
    // (name, description, proficiency, practice_count, last_practiced_days_ago)
    let skills: &[(&str, &str, f64, u32, i64)] = &[
        (
            "eu_vat_exemption_handling",
            "Validate an EU VAT exemption claim and apply it to the open invoice",
            0.24,
            19,
            6,
        ),
        (
            "chargeback_representment",
            "Assemble and file representment evidence before the card network deadline",
            0.19,
            11,
            9,
        ),
        (
            "stripe_refund_issue",
            "Issue a full or partial refund against a Stripe charge and notify the customer",
            0.88,
            41,
            2,
        ),
        (
            "dunning_escalation",
            "Move a past-due account through the dunning ladder and pause it on payment",
            0.72,
            33,
            3,
        ),
        (
            "netsuite_credit_note",
            "Raise a NetSuite credit note against a disputed invoice line",
            0.55,
            7,
            11,
        ),
        (
            "invoice_dispute_triage",
            "Read a disputed invoice and route it to the queue that owns the root cause",
            0.81,
            57,
            1,
        ),
    ];
    for (name, desc, prof, practice, last) in skills {
        let mut sk = Skill::new(name, desc)
            .namespace(NS)
            .confidence(*prof) // proficiency aliases confidence (OMS D3)
            .created_at(d(*last + 90))
            .source_type("agent");
        sk.practice_count = Some(*practice);
        sk.last_practiced_at = Some(d(*last));
        sk.domain = Some("billing_support".to_string());
        m.add(&sk).unwrap();
    }

    // ── what the desk actually did: the conversation layer ─────────────────
    // (content, days_ago, session)
    let events: &[(&str, i64, &str)] = &[
        ("Acme called about INV-20411: they say the March overage was already credited and are disputing the line.", 25, "sess-3301"),
        ("Tried to escalate the Acme overage dispute to Priya — she is on parental leave, so it went to Marcus instead.", 24, "sess-3301"),
        ("Marcus confirmed the Acme overage credit was issued but never applied to the invoice; raising a NetSuite credit note.", 22, "sess-3308"),
        ("Globex asked whether their VAT exemption certificate can be applied retroactively to three closed invoices.", 40, "sess-3190"),
        ("Dana said EU VAT exemptions cannot be applied retroactively past the filing period; Globex escalated to their AM.", 39, "sess-3190"),
        ("Initech card declined for the third month running — moved the account to dunning stage 2 and paused provisioning.", 15, "sess-3352"),
        ("Umbrella Health will not accept emailed invoices until the signed BAA is on file; invoices are going out by post.", 60, "sess-3044"),
        ("Customer asked who approves a goodwill credit above the auto-approve limit. Nobody on shift knew; asked in #billing-disputes and got no answer.", 12, "sess-3371"),
        ("Second time this week we could not say who signs off on large goodwill credits — this needs to be written down.", 5, "sess-3402"),
        ("Soylent Labs invoice still shows a Zuora account id; nobody could say whether that system is still authoritative.", 33, "sess-3221"),
        ("Chargeback from Globex arrived 18 days into the 20-day representment window; Lena filed with two days to spare.", 48, "sess-3126"),
        ("Stripe refund lookups have been failing all morning with a 403; Tom worked the refund queue manually.", 4, "sess-3410"),
        ("Acme's renewal is in November and their tier changed to platinum in May — Sanjay wants the disputes SLA revisited.", 18, "sess-3340"),
        ("Tom asked who owns invoice disputes now. Memory gave two different answers, so he asked the manager.", 7, "sess-3395"),
    ];
    for (content, days, sess) in events {
        let mut e = Event::new(content)
            .namespace(NS)
            .created_at(d(*days))
            .source_type("agent");
        e.session_id = Some(sess.to_string());
        m.add(&e).unwrap();
    }

    // ── open goals ─────────────────────────────────────────────────────────
    for (desc, days) in [
        ("Cut billing-dispute first response to under four hours", 90i64),
        ("Publish the goodwill-credit approval matrix and store it in memory", 30),
        ("Migrate the last Zuora-era accounts off the legacy billing system", 120),
    ] {
        let g = Goal::new(desc)
            .namespace(NS)
            .created_at(d(days))
            .source_type("agent");
        m.add(&g).unwrap();
    }

    // ── the tool-call history the flagship analyzer clusters ───────────────
    // Numeric-only ids: the signature normalizer collapses digit runs to '#',
    // so distinct calls cluster while their content addresses stay unique.
    let mut minutes_ago = 5_i64;
    let call = |m: &mut Areev, name: &str, content: String, err: bool, ago: &mut i64| {
        let t = Tool::new(name)
            .is_error(err)
            .content(&content)
            .namespace(NS)
            .created_at(now - *ago * 60_000)
            .source_type("agent");
        m.add(&t).unwrap();
        *ago += 37;
    };

    // stripe_refund_lookup: 9 of 12 calls 403 on the same missing scope.
    for i in 0..9 {
        call(
            &mut m,
            "stripe_refund_lookup",
            format!("403 Forbidden: refund scope missing on restricted key (request {})", 40110 + i),
            true,
            &mut minutes_ago,
        );
    }
    for i in 0..3 {
        call(
            &mut m,
            "stripe_refund_lookup",
            format!("{{\"refund\":\"re_{}\",\"status\":\"succeeded\"}}", 88200 + i),
            false,
            &mut minutes_ago,
        );
    }
    // netsuite_invoice_fetch: 6 of 10 time out.
    for i in 0..6 {
        call(
            &mut m,
            "netsuite_invoice_fetch",
            format!("Timeout after 30000ms waiting for NetSuite SOAP endpoint (request {})", 51040 + i),
            true,
            &mut minutes_ago,
        );
    }
    for i in 0..4 {
        call(
            &mut m,
            "netsuite_invoice_fetch",
            format!("{{\"invoice\":\"INV-{}\",\"status\":\"open\"}}", 20410 + i),
            false,
            &mut minutes_ago,
        );
    }
    // avalara_tax_quote: 2 failures in 14 calls — below the firing threshold,
    // so the queue shows the analyzer being selective rather than noisy.
    for i in 0..2 {
        call(
            &mut m,
            "avalara_tax_quote",
            format!("Avalara returned 500 for jurisdiction lookup (request {})", 60010 + i),
            true,
            &mut minutes_ago,
        );
    }
    for i in 0..12 {
        call(
            &mut m,
            "avalara_tax_quote",
            format!("{{\"rate\":0.0825,\"jurisdiction\":\"TX-{}\"}}", 4800 + i),
            false,
            &mut minutes_ago,
        );
    }
    for i in 0..8 {
        call(
            &mut m,
            "zendesk_ticket_update",
            format!("{{\"ticket\":{},\"status\":\"pending\"}}", 77100 + i),
            false,
            &mut minutes_ago,
        );
    }

    let st = m.stats().unwrap();
    println!("seeded {} grains ({} ops) into {}", st.grains, st.ops, path);
}
