//! Seeds the "Northwind Trading — accounts payable" demo memory.
//!
//! One story, told once: an invoice-triage agent watches an AP mailbox,
//! extracts expense rows from invoice emails, parks anything unusual for a
//! human, and writes the approved rows to the expense sheet. What it learned
//! along the way — vendor spellings, category rules, corrected amounts —
//! lives in the same file as what it did.
//!
//! The corpus is planted so the console has something real on every page and
//! so a single `areev loop run` fires several built-in analyzers on data an
//! AP lead can read without a legend: a live ownership contradiction, an
//! exact-duplicate contact, a dead QuickBooks-era cluster, two stalled
//! skills, an expired discount, and an attachment parser that keeps timing
//! out on scanned PDFs.
//!
//! Everything downstream of the grains — saved queries, the polling trigger,
//! the governed runs, the loop recommendations — is driven by
//! `scripts/build_demo.sh`, which calls this seeder first.
//!
//! Usage:
//!   cargo run --release -p areev-store --example seed_accounting_demo -- <path.db>
//!   ... -- <path.db> --plan-only     just the tools + the plan, on a fixed
//!                                    clock, so the committed example bundle
//!                                    keeps its content address

use areev_core::types::{
    Event, ExecutorKind, Fact, Goal, Grain, Skill, Tool, ToolKind, Workflow,
};
use areev_store::{Areev, TelemetryMode};

const NS: &str = "accounting";
const NS_VENDORS: &str = "accounting.vendors";
const NS_RULES: &str = "accounting.rules";
const DAY: i64 = 86_400_000;

/// The triage instructions the agent actually follows. A Skill *grain*, not a
/// prompt file, so the loop's memory-class proposals can supersede it through
/// the four gates — which is the whole point of keeping it here.
const SKILL_V1: &str = "Turn one accounting email (body + thread history + attachments) into zero or more expense rows.\n\nRow schema: invoice_date, payment_date, vendor, description, category, amount, currency, attachment link.\n\nCategory is a CLOSED set: Software, Office Supply, Compliance, Equipments / Machinery, Meals / Entertainment, Event. Never invent a category.\n\nVendor canonicalization: consult the alias facts in accounting.vendors (relation mg:alias_of) before emitting a vendor.\n\nEmit a confidence per field. Anything below threshold, or any row at or above the review amount, goes to the human gate rather than the sheet.";

const SKILL_V2: &str = "Turn one accounting email (body + thread history + attachments) into zero or more expense rows.\n\nRow schema: invoice_date, payment_date (often equals invoice_date, not always), vendor, description, category, amount, currency, attachment link.\n\nCategory is a CLOSED set: Software, Office Supply, Compliance, Equipments / Machinery, Meals / Entertainment, Event. Never invent a category. The corpus is heavily imbalanced toward Software — do NOT default to it; justify the category from the vendor and the line description.\n\nVendor canonicalization: consult the alias facts in accounting.vendors (relation mg:alias_of) before emitting a vendor. An unseen spelling that closely resembles a known vendor keeps its extracted form and is flagged low-confidence, so the loop can learn a new alias — never hardcode a mapping.\n\nOne email may carry several invoices (emit several rows), and one invoice may legitimately split across categories. A later reply in the thread corrects or cancels an earlier message — thread history wins over the first message.\n\nEmit a confidence per field. Anything below threshold, or any row at or above the review amount, goes to the human gate rather than the sheet.";

/// A fixed instant, so `--plan-only` produces the SAME content addresses on
/// every machine and every rebuild. The plan bundle that ships with
/// `examples/agents/invoice-to-accounting/` is committed, and a plan whose
/// hash moved with the clock would invalidate it on every regeneration.
const PLAN_EPOCH: i64 = 1_760_000_000_000;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seed_accounting_demo <path.db> [--plan-only]");
    let plan_only = std::env::args().any(|a| a == "--plan-only");
    let now = if plan_only {
        PLAN_EPOCH
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    let d = |days_ago: i64| now - days_ago * DAY;

    let mut m = if plan_only {
        Areev::open(&path).unwrap()
    } else {
        Areev::open_with_telemetry(&path, TelemetryMode::Aggregate).unwrap()
    };

    if plan_only {
        // Just the executable part: the tool definitions and the plan that
        // binds them. No corpus, no history — this is what a fresh install
        // imports before its first run.
        let (wf_h, _) = seed_plan(&mut m, now);
        println!("workflow={}", wf_h.to_hex());
        let st = m.stats().unwrap();
        println!("seeded {} plan grains ({} ops) into {}", st.grains, st.ops, path);
        return;
    }



    // ── the desk: who works it, and what each of them owns ─────────────────
    // (subject, relation, object, days_ago, confidence)
    let facts: &[(&str, &str, &str, i64, f64)] = &[
        // ── people ──
        ("maya_iyer", "role", "accounts_payable_lead", 400, 1.0),
        ("maya_iyer", "works_at", "Northwind Trading", 400, 1.0),
        ("maya_iyer", "reports_to", "dev_rao", 400, 1.0),
        ("maya_iyer", "assigned_to", "invoice_intake", 400, 0.95),
        ("maya_iyer", "email", "maya@northwind.example", 400, 1.0),
        ("maya_iyer", "expertise", "vendor_onboarding", 380, 0.9),
        ("dev_rao", "role", "financial_controller", 420, 1.0),
        ("dev_rao", "works_at", "Northwind Trading", 420, 1.0),
        ("dev_rao", "assigned_to", "payment_approvals", 420, 0.95),
        ("dev_rao", "email", "dev@northwind.example", 420, 1.0),
        ("dev_rao", "approval_limit", "25000_usd", 300, 1.0),
        ("tom_okafor", "role", "accounts_payable_clerk", 210, 1.0),
        ("tom_okafor", "works_at", "Northwind Trading", 210, 1.0),
        ("tom_okafor", "reports_to", "maya_iyer", 210, 1.0),
        ("tom_okafor", "assigned_to", "invoice_intake", 90, 0.9),
        ("lena_fischer", "role", "tax_analyst", 260, 1.0),
        ("lena_fischer", "works_at", "Northwind Trading", 260, 1.0),
        ("lena_fischer", "reports_to", "dev_rao", 260, 1.0),
        ("lena_fischer", "expertise", "vat_and_withholding", 250, 0.9),
        ("Northwind Trading", "headquartered_in", "Rotterdam", 420, 1.0),
        ("Northwind Trading", "fiscal_year_end", "2026-03-31", 420, 1.0),
        // ── the queues this desk runs ──
        // Two live owners for invoice_intake: Maya handed the queue to Tom
        // during a leave and the handover was recorded as a NEW fact instead
        // of a supersession. Both are current. That is the contradiction the
        // loop finds — planted the way it actually happens.
        ("invoice_intake", "owner", "maya_iyer", 400, 0.95),
        ("invoice_intake", "owner", "tom_okafor", 88, 0.80),
        ("invoice_intake", "sla", "1_business_day", 390, 1.0),
        ("invoice_intake", "review_threshold", "2500_usd", 300, 1.0),
        ("invoice_intake", "mailbox", "ap@northwind.example", 400, 1.0),
        ("invoice_intake", "weekly_volume", "63_invoices", 40, 0.7),
        ("payment_approvals", "owner", "dev_rao", 420, 0.95),
        ("payment_approvals", "escalates_to", "board_finance_committee", 300, 0.9),
        ("payment_approvals", "sla", "2_business_days", 300, 1.0),
        ("vendor_onboarding", "owner", "maya_iyer", 380, 0.95),
        ("vendor_onboarding", "requires", "w9_or_w8ben", 380, 1.0),
        ("tax_filing", "owner", "lena_fischer", 250, 0.95),
        ("tax_filing", "escalates_to", "dev_rao", 250, 0.9),
        // ── the vendors ──
        ("cobalt_cloud", "legal_name", "Cobalt Cloud, Inc.", 380, 1.0),
        ("cobalt_cloud", "default_category", "Software", 380, 0.95),
        ("cobalt_cloud", "payment_terms", "net_30", 380, 1.0),
        ("cobalt_cloud", "currency", "USD", 380, 1.0),
        ("cobalt_cloud", "headquartered_in", "Seattle", 380, 1.0),
        ("cobalt_cloud", "billing_email", "ar@cobaltcloud.example", 380, 1.0),
        // The same contact recorded twice, ten months apart, by two channels.
        // Byte-identical values collapse to one grain; these differ only in
        // confidence, so they are the duplicate the sweep is meant to find.
        ("cobalt_cloud", "billing_email", "ar@cobaltcloud.example", 55, 0.9),
        ("cobalt_cloud", "invoices_monthly", "true", 300, 0.9),
        ("ironwood_furniture", "legal_name", "Ironwood Furniture BV", 340, 1.0),
        ("ironwood_furniture", "default_category", "Office Supply", 340, 0.95),
        ("ironwood_furniture", "payment_terms", "net_45", 340, 1.0),
        ("ironwood_furniture", "currency", "EUR", 340, 1.0),
        ("ironwood_furniture", "headquartered_in", "Rotterdam", 340, 1.0),
        ("ironwood_furniture", "vat_id", "NL811907980B01", 335, 1.0),
        ("kestrel_legal", "legal_name", "Kestrel Legal LLP", 320, 1.0),
        ("kestrel_legal", "default_category", "Compliance", 320, 0.95),
        ("kestrel_legal", "payment_terms", "net_15", 320, 1.0),
        ("kestrel_legal", "currency", "USD", 320, 1.0),
        ("kestrel_legal", "requires_po", "true", 200, 0.9),
        ("meridian_freight", "legal_name", "Meridian Freight Lines", 300, 1.0),
        ("meridian_freight", "default_category", "Equipments / Machinery", 300, 0.85),
        ("meridian_freight", "payment_terms", "net_30", 300, 1.0),
        ("meridian_freight", "currency", "USD", 300, 1.0),
        ("meridian_freight", "headquartered_in", "Chicago", 300, 1.0),
        ("vantage_analytics", "legal_name", "Vantage Analytics Ltd", 280, 1.0),
        ("vantage_analytics", "default_category", "Software", 280, 0.95),
        ("vantage_analytics", "payment_terms", "net_30", 280, 1.0),
        ("vantage_analytics", "currency", "GBP", 280, 1.0),
        ("vantage_analytics", "headquartered_in", "Bristol", 280, 1.0),
        ("blue_harbor_catering", "legal_name", "Blue Harbor Catering", 240, 1.0),
        ("blue_harbor_catering", "default_category", "Meals / Entertainment", 240, 0.95),
        ("blue_harbor_catering", "payment_terms", "due_on_receipt", 240, 1.0),
        ("blue_harbor_catering", "currency", "EUR", 240, 1.0),
        ("pinnacle_machining", "legal_name", "Pinnacle Machining GmbH", 220, 1.0),
        ("pinnacle_machining", "default_category", "Equipments / Machinery", 220, 0.95),
        ("pinnacle_machining", "payment_terms", "net_60", 220, 1.0),
        ("pinnacle_machining", "currency", "EUR", 220, 1.0),
        ("pinnacle_machining", "headquartered_in", "Stuttgart", 220, 1.0),
        // ── the systems on either end ──
        ("expense_sheet", "purpose", "system_of_record_for_expense_rows", 400, 1.0),
        ("expense_sheet", "owner", "dev_rao", 400, 1.0),
        ("gmail_ap_mailbox", "purpose", "inbound_invoice_intake", 400, 1.0),
        ("gmail_ap_mailbox", "owner", "maya_iyer", 400, 1.0),
        // ── the dead QuickBooks era: true, ancient, and never asked about ──
        ("quickbooks_desktop", "status", "decommissioned", 700, 1.0),
        ("quickbooks_desktop", "owner", "ex_contractor_pv", 720, 0.8),
        ("quickbooks_desktop", "runbook", "rb-legacy-004", 720, 0.8),
        ("paper_voucher_process", "status", "retired", 680, 1.0),
        ("paper_voucher_process", "owner", "ex_contractor_pv", 680, 0.8),
        ("meridian_freight", "legacy_vendor_id", "QB-40221", 660, 0.8),
    ];

    for (s, r, o, days, conf) in facts {
        let f = Fact::new(s, r, o)
            .namespace(NS)
            .created_at(d(*days))
            .confidence(*conf)
            .source_type("agent");
        m.add(&f).unwrap();
    }

    // ── vendor aliases: learned from misspellings, never hardcoded ─────────
    // A child namespace, so a read scopes with `accounting.*` and a write
    // still names it exactly. Each one entered because a human corrected the
    // agent once.
    let aliases: &[(&str, &str, i64, f64)] = &[
        ("Cobalt Cloud Inc.", "cobalt_cloud", 370, 1.0),
        ("COBALT CLOUD, INC", "cobalt_cloud", 300, 0.95),
        ("Cobbalt Cloud", "cobalt_cloud", 120, 0.75),
        ("Ironwood Furniture B.V.", "ironwood_furniture", 330, 1.0),
        ("Ironwood Furn. BV", "ironwood_furniture", 150, 0.85),
        ("Kestrel Legal L.L.P.", "kestrel_legal", 310, 1.0),
        ("Meridian Freight Lines Inc", "meridian_freight", 290, 1.0),
        ("Vantage Analytics Limited", "vantage_analytics", 270, 1.0),
        ("Blue Harbour Catering", "blue_harbor_catering", 100, 0.8),
        ("Pinnacle Machining G.m.b.H.", "pinnacle_machining", 210, 0.95),
    ];
    for (spelling, canonical, days, conf) in aliases {
        let f = Fact::new(spelling, "mg:alias_of", canonical)
            .namespace(NS_VENDORS)
            .created_at(d(*days))
            .confidence(*conf)
            .source_type("agent");
        m.add(&f).unwrap();
    }

    // ── category rules: the other half of what the agent learned ───────────
    let rules: &[(&str, &str, &str, i64, f64)] = &[
        ("vendor:cobalt_cloud", "categorize_as", "Software", 360, 0.95),
        ("vendor:vantage_analytics", "categorize_as", "Software", 270, 0.95),
        ("vendor:ironwood_furniture", "categorize_as", "Office Supply", 330, 0.95),
        ("vendor:kestrel_legal", "categorize_as", "Compliance", 310, 0.95),
        ("vendor:blue_harbor_catering", "categorize_as", "Meals / Entertainment", 230, 0.95),
        ("vendor:pinnacle_machining", "categorize_as", "Equipments / Machinery", 210, 0.95),
        ("line_contains:desk chair", "categorize_as", "Office Supply", 180, 0.9),
        ("line_contains:standing desk", "categorize_as", "Office Supply", 180, 0.9),
        ("line_contains:seat license", "categorize_as", "Software", 170, 0.9),
        ("line_contains:annual audit", "categorize_as", "Compliance", 160, 0.9),
        ("line_contains:offsite dinner", "categorize_as", "Meals / Entertainment", 140, 0.9),
        ("line_contains:conference booth", "categorize_as", "Event", 120, 0.85),
        ("line_contains:CNC tooling", "categorize_as", "Equipments / Machinery", 110, 0.9),
        ("amount_at_or_above:2500_usd", "route_to", "human_review", 300, 1.0),
        ("confidence_below:0.75", "route_to", "human_review", 300, 1.0),
    ];
    for (s, r, o, days, conf) in rules {
        let f = Fact::new(s, r, o)
            .namespace(NS_RULES)
            .created_at(d(*days))
            .confidence(*conf)
            .source_type("agent");
        m.add(&f).unwrap();
    }

    // ── the inbound mail: what the desk actually received ──────────────────
    // Thread-indexed, because a correction arrives as a reply and thread
    // history has to win over the first message.
    // (content, days_ago, session)
    let events: &[(&str, i64, &str)] = &[
        ("Invoice INV-CC-88431 from Cobalt Cloud, Inc. — 240 seat licenses, billing period Jul 1–31. Amount 4,800.00 USD, net 30, PDF attached.", 34, "thr-ap-4401"),
        ("Reply from dev_rao on INV-CC-88431: approved, but the seat count is 220 not 240 — Cobalt credited the difference last month. Post 4,400.00.", 33, "thr-ap-4401"),
        ("Invoice IRN-2291 from Ironwood Furniture B.V. — 12 ergonomic desk chairs, 3,180.00 EUR, net 45. Delivery note attached separately.", 29, "thr-ap-4388"),
        ("Kestrel Legal L.L.P. invoice KL-7742 for the annual audit engagement, 12,500.00 USD, net 15. PO number missing from the invoice.", 26, "thr-ap-4372"),
        ("maya_iyer replied to Kestrel: we cannot post without a PO. Kestrel resent with PO-2026-118 attached.", 25, "thr-ap-4372"),
        ("Two invoices in one email from Meridian Freight — MF-5510 (2,140.00 USD freight) and MF-5511 (860.00 USD CNC tooling surcharge).", 21, "thr-ap-4359"),
        ("Vantage Analytics VA-1180, 1,950.00 GBP, monthly platform fee. Vendor spelled 'Vantage Analytics Limited' on the invoice header.", 18, "thr-ap-4341"),
        ("Blue Harbour Catering invoice BH-330 for the Q2 offsite dinner, 2,780.00 EUR. Note the vendor spells it 'Harbour' — our records say 'Harbor'.", 16, "thr-ap-4330"),
        ("Pinnacle Machining G.m.b.H. PM-9021, 18,400.00 EUR for the replacement spindle. Above the review threshold — went to dev_rao.", 12, "thr-ap-4318"),
        ("dev_rao approved PM-9021 but asked that the warranty term be recorded against the vendor before payment is released.", 11, "thr-ap-4318"),
        ("Scanned invoice from Cobbalt Cloud (sic) — attachment is a photographed page, the parser returned no text. Routed to a human.", 9, "thr-ap-4306"),
        ("tom_okafor asked who owns invoice intake now that Maya is back. Memory gave two different answers, so he asked the controller.", 7, "thr-ap-4299"),
        ("Duplicate of INV-CC-88431 arrived from a different Cobalt Cloud sender address. The run recorded one skip, not a second payment.", 6, "thr-ap-4401"),
        ("Ironwood asked whether the Q1 15% annual discount still applies. Nobody could say — the discount fact has no end date on the invoice copy.", 5, "thr-ap-4290"),
        ("Second time this month the sheet writer hit a rate limit mid-batch and half the rows landed. Tom re-ran it by hand.", 3, "thr-ap-4281"),
    ];
    for (content, days, sess) in events {
        let mut e = Event::new(content)
            .namespace(NS)
            .created_at(d(*days))
            .source_type("agent");
        e.session_id = Some(sess.to_string());
        m.add(&e).unwrap();
    }

    // ── the extracted rows, each pointing back at the mail it came from ────
    // `derived_from` is what makes `areev provenance` answer "which email
    // taught us this?" — the reverse leg of the same edge.
    let posted: &[(&str, &str, &str, &str, &str, i64)] = &[
        ("invoice:IRN-2291", "ironwood_furniture", "3180.00", "EUR", "Office Supply", 29),
        ("invoice:KL-7742", "kestrel_legal", "12500.00", "USD", "Compliance", 25),
        ("invoice:MF-5510", "meridian_freight", "2140.00", "USD", "Equipments / Machinery", 21),
        ("invoice:MF-5511", "meridian_freight", "860.00", "USD", "Equipments / Machinery", 21),
        ("invoice:VA-1180", "vantage_analytics", "1950.00", "GBP", "Software", 18),
        ("invoice:BH-330", "blue_harbor_catering", "2780.00", "EUR", "Meals / Entertainment", 16),
        ("invoice:PM-9021", "pinnacle_machining", "18400.00", "EUR", "Equipments / Machinery", 12),
    ];
    for (inv, vendor, amount, currency, category, days) in posted {
        for (rel, obj) in [
            ("vendor", *vendor),
            ("amount", *amount),
            ("currency", *currency),
            ("category", *category),
            ("status", "posted"),
        ] {
            let f = Fact::new(inv, rel, obj)
                .namespace(NS)
                .created_at(d(*days))
                .confidence(0.95)
                .source_type("agent");
            m.add(&f).unwrap();
        }
    }

    // ── history: what a supersession is actually for ───────────────────────
    // The Cobalt invoice was extracted at 4,800 and corrected to 4,400 by the
    // controller's reply. The old value stays in history; recall returns one
    // current value. This is the edit the agent got RIGHT — unlike the
    // invoice_intake handover above, which never got one.
    let amount_v1 = m
        .add(
            &Fact::new("invoice:INV-CC-88431", "amount", "4800.00")
                .namespace(NS)
                .created_at(d(34))
                .confidence(0.82)
                .source_type("agent"),
        )
        .unwrap();
    let mut amount_v2 = Fact::new("invoice:INV-CC-88431", "amount", "4400.00")
        .namespace(NS)
        .created_at(d(33))
        .confidence(1.0)
        .source_type("human");
    m.supersede(&amount_v1, &mut amount_v2).unwrap();
    for (rel, obj) in [
        ("vendor", "cobalt_cloud"),
        ("currency", "USD"),
        ("category", "Software"),
        ("status", "posted"),
        ("corrected_by", "dev_rao"),
    ] {
        m.add(
            &Fact::new("invoice:INV-CC-88431", rel, obj)
                .namespace(NS)
                .created_at(d(33))
                .confidence(0.95)
                .source_type("agent"),
        )
        .unwrap();
    }

    // A category the agent got wrong once and had corrected: freight booked
    // to Software because the vendor was unknown and Software is the modal
    // category. Superseded — and the reason it stopped happening is the
    // Skill revision below.
    let cat_v1 = m
        .add(
            &Fact::new("invoice:MF-5511", "category_first_pass", "Software")
                .namespace(NS)
                .created_at(d(21))
                .confidence(0.61)
                .source_type("agent"),
        )
        .unwrap();
    let mut cat_v2 = Fact::new("invoice:MF-5511", "category_first_pass", "Equipments / Machinery")
        .namespace(NS)
        .created_at(d(20))
        .confidence(1.0)
        .source_type("human");
    m.supersede(&cat_v1, &mut cat_v2).unwrap();

    // Maya went on leave and came back; the status fact was superseded both
    // times, which is why it is unambiguous while the queue ownership is not.
    let status_v1 = m
        .add(
            &Fact::new("maya_iyer", "status", "active")
                .namespace(NS)
                .created_at(d(400))
                .source_type("agent"),
        )
        .unwrap();
    let mut status_v2 = Fact::new("maya_iyer", "status", "on_leave")
        .namespace(NS)
        .created_at(d(90))
        .source_type("agent");
    let status_v2h = m.supersede(&status_v1, &mut status_v2).unwrap();
    let mut status_v3 = Fact::new("maya_iyer", "status", "active")
        .namespace(NS)
        .created_at(d(10))
        .source_type("agent");
    m.supersede(&status_v2h, &mut status_v3).unwrap();

    // ── an expired fact the desk is still quoting ──────────────────────────
    let discount = Fact::new("ironwood_furniture", "annual_discount", "15_percent")
        .namespace(NS)
        .created_at(d(330))
        .valid_to(d(64))
        .source_type("agent");
    m.add(&discount).unwrap();
    let discount2 = Fact::new("cobalt_cloud", "annual_discount", "8_percent_commit")
        .namespace(NS)
        .created_at(d(300))
        .valid_to(d(30))
        .source_type("agent");
    m.add(&discount2).unwrap();

    // ── the fork seed ──────────────────────────────────────────────────────
    // Left as a single head; `scripts/build_demo.sh` edits it two ways so the
    // console shows a real open fork (the mailbox agent and a human console
    // session changed the same fact while one of them was offline).
    let fork_parent = m
        .add(
            &Fact::new("kestrel_legal", "payment_terms_review", "net_15")
                .namespace(NS)
                .created_at(d(60))
                .source_type("agent"),
        )
        .unwrap();
    println!("fork_parent={}", fork_parent.to_hex());

    // ── the triage instructions, and the revision that fixed them ──────────
    let skill_v1 = Skill::new("invoice-triage", "How to turn an accounting email into expense-sheet rows")
        .instructions(SKILL_V1)
        .when_to_use("Assembled into every extraction context for the accounting namespaces")
        .version("1")
        .domain("accounts_payable")
        .namespace(NS)
        .created_at(d(400))
        .source_type("agent");
    let skill_v1h = m.add(&skill_v1).unwrap();
    let mut skill_v2 = Skill::new("invoice-triage", "How to turn an accounting email into expense-sheet rows")
        .instructions(SKILL_V2)
        .when_to_use("Assembled into every extraction context for the accounting namespaces")
        .version("2")
        .domain("accounts_payable")
        .namespace(NS)
        .created_at(d(19))
        .source_type("human");
    let skill_v2h = m.supersede(&skill_v1h, &mut skill_v2).unwrap();
    println!("skill_head={}", skill_v2h.to_hex());

    // ── skills the agent is practising, and two that stalled ───────────────
    // (name, description, proficiency, practice_count, last_practiced_days_ago)
    let skills: &[(&str, &str, f64, u32, i64)] = &[
        ("vendor_alias_learning", "Recognize a new spelling of a known vendor and propose the alias rather than guessing", 0.86, 44, 2),
        ("multi_invoice_email", "Split one email carrying several invoices into one row per invoice with per-file attribution", 0.79, 26, 3),
        ("threshold_routing", "Decide whether a row clears the auto-post threshold or needs a named approver", 0.91, 61, 1),
        ("scanned_pdf_extraction", "Read a photographed or scanned invoice where the PDF carries no text layer", 0.21, 14, 8),
        ("vat_reverse_charge", "Apply the EU reverse-charge treatment to a cross-border services invoice", 0.18, 9, 12),
        ("credit_note_matching", "Match an incoming credit note to the invoice it offsets before posting either", 0.58, 12, 6),
    ];
    for (name, desc, prof, practice, last) in skills {
        let mut sk = Skill::new(name, desc)
            .namespace(NS)
            .confidence(*prof) // proficiency aliases confidence (OMS D3)
            .created_at(d(*last + 120))
            .source_type("agent");
        sk.practice_count = Some(*practice);
        sk.last_practiced_at = Some(d(*last));
        sk.domain = Some("accounts_payable".to_string());
        m.add(&sk).unwrap();
    }

    // ── what the desk is trying to get better at ───────────────────────────
    for (desc, days) in [
        ("Category accuracy at or above 95% on the ground-truth invoice set", 120i64),
        ("Median invoice cycle time under one business day, mailbox to sheet", 90),
        ("Zero duplicate postings — every payment traceable to exactly one invoice", 150),
        ("Retire the last QuickBooks-era vendor ids from the active vendor list", 200),
    ] {
        let g = Goal::new(desc)
            .namespace(NS)
            .created_at(d(days))
            .source_type("agent");
        m.add(&g).unwrap();
    }


    let (wf_h, _reply_h) = seed_plan(&mut m, now);
    println!("workflow={}", wf_h.to_hex());

    // ── a second, smaller plan: the monthly vendor statement reconcile ─────
    let def = tool_def;
    let fetch_stmt_h = m.add(&def("fetch_statement", "Pull the vendor's monthly statement PDF from the shared drive.", false, d(180))).unwrap();
    let recon_h = m.add(&def("reconcile_lines", "Match every statement line against posted invoices and list the gaps.", false, d(180))).unwrap();
    let escalate_h = m.add(&def("escalate_gap", "A human decides what to do about an unmatched statement line.", true, d(180))).unwrap();
    let recon_wf = Workflow::new(vec![
        "fetch_statement".into(),
        "reconcile_lines".into(),
        "clean".into(),
        "escalate_gap".into(),
    ])
    .edge("fetch_statement", "reconcile_lines")
    .cond_edge("reconcile_lines", "clean", "gaps == 0")
    .cond_edge("reconcile_lines", "escalate_gap", "gaps > 0")
    .edge_with_cycles("escalate_gap", "reconcile_lines", 2)
    .bind("fetch_statement", &fetch_stmt_h.to_hex())
    .bind("reconcile_lines", &recon_h.to_hex())
    .bind("escalate_gap", &escalate_h.to_hex())
    .created_at(d(180))
    .namespace(NS);
    let recon_h2 = m.add(&recon_wf).unwrap();
    println!("workflow_reconcile={}", recon_h2.to_hex());

    // ── the tool-call history the flagship analyzer clusters ───────────────
    // Numeric-only ids: the signature normalizer collapses digit runs to '#',
    // so distinct calls cluster while their content addresses stay unique.
    let mut minutes_ago = 6_i64;
    let call = |m: &mut Areev, name: &str, content: String, err: bool, ago: &mut i64| {
        let t = Tool::new(name)
            .is_error(err)
            .content(&content)
            .namespace(NS)
            .created_at(now - *ago * 60_000)
            .source_type("agent");
        m.add(&t).unwrap();
        *ago += 41;
    };

    // parse_attachments: 8 of 11 fail on scanned pages with no text layer.
    // This is the recommendation an AP lead can act on without a legend.
    for i in 0..8 {
        call(
            &mut m,
            "parse_attachments",
            format!("pdftotext produced 0 characters — attachment appears to be a scanned image (attachment {})", 30140 + i),
            true,
            &mut minutes_ago,
        );
    }
    for i in 0..3 {
        call(
            &mut m,
            "parse_attachments",
            format!("{{\"texts\":[{{\"filename\":\"invoice-{}.pdf\",\"chars\":4180}}]}}", 8820 + i),
            false,
            &mut minutes_ago,
        );
    }
    // append_sheet: 5 of 14 hit the API rate limit mid-batch.
    for i in 0..5 {
        call(
            &mut m,
            "append_sheet",
            format!("429 Too Many Requests: write quota exceeded for spreadsheet (batch {})", 71020 + i),
            true,
            &mut minutes_ago,
        );
    }
    for i in 0..9 {
        call(
            &mut m,
            "append_sheet",
            format!("{{\"appended\":1,\"row\":{}}}", 1840 + i),
            false,
            &mut minutes_ago,
        );
    }
    // extract_rows: 2 failures in 16 — below the firing threshold, so the
    // queue shows the analyzer being selective rather than noisy.
    for i in 0..2 {
        call(
            &mut m,
            "extract_rows",
            format!("model returned malformed row JSON at index {i}"),
            true,
            &mut minutes_ago,
        );
    }
    for i in 0..14 {
        call(
            &mut m,
            "extract_rows",
            format!("{{\"rows\":1,\"confidence\":0.9{}}}", i % 10),
            false,
            &mut minutes_ago,
        );
    }
    for i in 0..11 {
        call(
            &mut m,
            "validate_rows",
            format!("{{\"needs_review\":false,\"key\":\"nw-{}\"}}", 5510 + i),
            false,
            &mut minutes_ago,
        );
    }
    for i in 0..7 {
        call(
            &mut m,
            "reply_email",
            format!("{{\"sent\":true,\"thread\":\"thr-ap-{}\"}}", 4280 + i),
            false,
            &mut minutes_ago,
        );
    }

    let st = m.stats().unwrap();
    println!("seeded {} grains ({} ops) into {}", st.grains, st.ops, path);
}

/// One Tool Definition, the shape every node in the plan binds to.
fn tool_def(name: &str, desc: &str, client: bool, created: i64) -> Tool {
    let mut t = Tool::new(name)
        .kind(ToolKind::Definition)
        .tool_description(desc)
        .created_at(created)
        .namespace(NS);
    if client {
        t = t.executor_kind(ExecutorKind::Client);
    }
    t
}

/// The executable half of the demo: the seven tool definitions and the plan
/// that binds them. Split out so `--plan-only` emits exactly this and nothing
/// else — the bundle shipped with `examples/agents/invoice-to-accounting/` and
/// the plan inside `demo.db` are built from one source rather than two that
/// drift. Returns (workflow hash, the reply tool's hash).
fn seed_plan(m: &mut Areev, now: i64) -> (areev_core::Hash, areev_core::Hash) {
    let d = |days_ago: i64| now - days_ago * DAY;

    // ── the tool definitions the workflow binds to ─────────────────────────
    let def = tool_def;
    let parse_h = m.add(&def("parse_attachments", "Fetch each attachment by its cas:// address and extract text — pdftotext for PDFs, UTF-8 heuristic otherwise.", false, d(400))).unwrap();
    let extract_h = m.add(&def("extract_rows", "Extract candidate expense rows (vendor, dates, amount, currency, category, description) with a per-field confidence from the email body, the thread, and the attachment text.", false, d(400))).unwrap();
    let validate_h = m.add(&def("validate_rows", "Normalize rows, apply vendor aliases, compute the idempotency key from (message-id, invoice index, amount), and decide whether the row needs a human.", false, d(400))).unwrap();
    let ask_h = m.add(&def("send_ask", "Email the approver on the original thread — never the external sender — with the extracted rows and the run marker.", false, d(400))).unwrap();
    let review_h = m.add(&def("human_review", "Human approval gate. The responder structurally cannot be the principal that started the run.", true, d(400))).unwrap();
    let sheet_h = m.add(&def("append_sheet", "Append approved rows to the expense sheet, idempotent on the row key.", false, d(400))).unwrap();
    let reply_h = m.add(&def("reply_email", "Reply on the thread with the outcome, carrying the run marker used for dedupe and correction routing.", false, d(400))).unwrap();

    // ── the governed plan ──────────────────────────────────────────────────
    // Branch on needs_review, park on the human gate, and never reach the
    // sheet from a rejection. `retry` on extraction only — posting a row
    // twice is not a retryable mistake.
    let wf = Workflow::new(vec![
        "parse_attachments".into(),
        "extract_rows".into(),
        "validate_rows".into(),
        "send_ask".into(),
        "human_review".into(),
        "append_sheet".into(),
        "reply_email".into(),
        "reply_rejected".into(),
    ])
    .edge("parse_attachments", "extract_rows")
    .edge("extract_rows", "validate_rows")
    .cond_edge("validate_rows", "send_ask", "needs_review == true")
    .cond_edge("validate_rows", "append_sheet", "needs_review == false")
    .edge("send_ask", "human_review")
    .cond_edge("human_review", "append_sheet", "approved == true")
    .cond_edge("human_review", "reply_rejected", "approved == false")
    .edge("append_sheet", "reply_email")
    .retry("extract_rows", 1)
    .bind("parse_attachments", &parse_h.to_hex())
    .bind("extract_rows", &extract_h.to_hex())
    .bind("validate_rows", &validate_h.to_hex())
    .bind("send_ask", &ask_h.to_hex())
    .bind("human_review", &review_h.to_hex())
    .bind("append_sheet", &sheet_h.to_hex())
    .bind("reply_email", &reply_h.to_hex())
    .bind("reply_rejected", &reply_h.to_hex())
    .created_at(d(400))
    .namespace(NS);
    let wf_h = m.add(&wf).unwrap();
    (wf_h, reply_h)
}
