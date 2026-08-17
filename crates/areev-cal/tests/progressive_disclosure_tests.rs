//! Issue #25 — `WITH progressive_disclosure(level)` executes, and a Skill's
//! definition body is reachable through a rendered path.
//!
//! Two failures shared one shape: the option was documented in
//! `docs/cal-reference.md` but parsed-and-discarded (the executor even said so
//! in a comment), and `instructions` — the field that IS the skill — could not
//! be projected or rendered at all, so the only way to read it was raw JSON
//! recall, which defeats budgeted assembly.

use areev_cal::executor::CalResultPayload;
use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use tempfile::TempDir;

const INSTRUCTIONS: &str = "STEP 1: read the email. STEP 2: classify the vendor.";
const WHEN: &str = "when an invoice email arrives";

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = areev_store::Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    ex.execute(
        &format!(
            r#"ADD skill SET name = "triage" SET description = "Triage invoices"
               SET instructions = "{INSTRUCTIONS}" SET when_to_use = "{WHEN}"
               SET namespace = "caller" REASON "seed""#
        ),
        &facade,
    )
    .unwrap();
    (ex, facade, dir)
}

fn text_of(ex: &CalExecutor, facade: &AreevFacade, cal: &str) -> String {
    match ex.execute(cal, facade).unwrap().result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got {other:?}"),
    }
}

#[test]
fn full_disclosure_surfaces_a_skills_definition_body() {
    let (ex, facade, _d) = setup();
    let full = text_of(
        &ex,
        &facade,
        "RECALL skills FORMAT sml WITH progressive_disclosure(full)",
    );
    assert!(full.contains(INSTRUCTIONS), "instructions must render at full: {full}");
    assert!(full.contains(WHEN), "when_to_use must render at full: {full}");
    assert!(full.contains("<instructions>"), "as its own element: {full}");
}

#[test]
fn the_default_render_is_unchanged() {
    // The whole point of gating the body behind `full`: a recall of many skills
    // must stay a listing, not become many playbooks. This is also the
    // backward-compatibility pin — an unannotated query renders as it always
    // has, which is what keeps areev-context's render parity honest.
    let (ex, facade, _d) = setup();
    let default = text_of(&ex, &facade, "RECALL skills FORMAT sml");
    assert!(!default.contains(INSTRUCTIONS), "default must NOT carry instructions: {default}");
    assert!(!default.contains(WHEN), "default must NOT carry when_to_use: {default}");
    assert!(default.contains("Triage invoices"), "description still renders: {default}");

    // Below full, too — the tier has to actually gate, not just exist.
    for lvl in ["summary", "headlines"] {
        let t = text_of(
            &ex,
            &facade,
            &format!("RECALL skills FORMAT sml WITH progressive_disclosure({lvl})"),
        );
        assert!(!t.contains(INSTRUCTIONS), "{lvl} must not carry instructions: {t}");
    }
}

#[test]
fn the_option_no_longer_warns_that_it_is_a_no_op() {
    // It used to emit CAL-W004 "parsed but executor no-op" while the reference
    // documented it as supported. Whichever way that contradiction resolved,
    // it must not survive.
    let (ex, facade, _d) = setup();
    let r = ex
        .execute("RECALL skills FORMAT sml WITH progressive_disclosure(full)", &facade)
        .unwrap();
    assert!(
        !r.warnings.iter().any(|w| w.contains("progressive_disclosure")),
        "no no-op warning expected, got {:?}",
        r.warnings
    );
}

#[test]
fn lower_tiers_clip_free_text_bodies() {
    let dir = TempDir::new().unwrap();
    let m = areev_store::Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let body = "This is a deliberately long observation body that should be clipped at the lower tiers.";
    ex.execute(
        &format!(
            r#"ADD observation SET subject = "vendor" SET content = "{body}"
               SET namespace = "caller" REASON "seed""#
        ),
        &facade,
    )
    .unwrap();

    let summary = text_of(
        &ex,
        &facade,
        "RECALL observations FORMAT sml WITH progressive_disclosure(summary)",
    );
    let headlines = text_of(
        &ex,
        &facade,
        "RECALL observations FORMAT sml WITH progressive_disclosure(headlines)",
    );
    let default = text_of(&ex, &facade, "RECALL observations FORMAT sml");

    assert!(default.contains("clipped at the lower tiers."), "default keeps the body: {default}");
    assert!(!summary.contains("clipped at the lower tiers."), "summary clips: {summary}");
    assert!(!headlines.contains("clipped at the lower tiers."), "headlines clips: {headlines}");
    // The ladder must be ordered, not merely "shorter than default".
    assert!(
        summary.len() < headlines.len() && headlines.len() < default.len(),
        "summary < headlines < default; got {} / {} / {}",
        summary.len(),
        headlines.len(),
        default.len()
    );
}

#[test]
fn instructions_is_projectable() {
    // `PROJECT name, instructions` used to be CAL-E060 "not available on grain
    // type skills", because the field was missing from the type's registry row.
    let (ex, facade, _d) = setup();
    let r = ex
        .execute("RECALL skills | PROJECT name, instructions", &facade)
        .unwrap();
    let CalResultPayload::Grains { grains, .. } = r.result else {
        panic!("expected Grains");
    };
    let v = serde_json::to_value(&grains[0]).unwrap();
    assert_eq!(v["fields"]["instructions"].as_str(), Some(INSTRUCTIONS));
    assert_eq!(v["fields"]["name"].as_str(), Some("triage"));
}

#[test]
fn full_disclosure_reaches_a_budgeted_assemble() {
    // The reported workaround was fetching instructions via raw JSON recall
    // before `run start` and injecting them outside the assembly budget. This
    // is the path that makes that unnecessary.
    let (ex, facade, _d) = setup();
    let out = text_of(
        &ex,
        &facade,
        r#"ASSEMBLE "agent boot" FROM skills: (RECALL skills) BUDGET 4000 tokens WITH progressive_disclosure(full) FORMAT sml"#,
    );
    assert!(out.contains(INSTRUCTIONS), "instructions must reach an ASSEMBLE: {out}");
}
