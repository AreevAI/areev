//! #208 — an `ASSEMBLE` budget that drops grains has to say so.
//!
//! The failure this pins is not a wrong answer but an *undetectable* one. The
//! budget applies whether or not the caller wrote one (4000 tokens by
//! default), and when it bound it discarded the tail of each source in
//! silence while `total_available` reported the post-budget count — so
//! `grains.len() == total_available` held for a truncated assembly exactly as
//! it did for a complete one. A host composing a prompt has no other signal
//! that its rules, its policies or its recent turns were trimmed.

use areev_cal::executor::CalResultPayload;
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

/// Enough grains, each long enough, that the 4000-token default cannot hold
/// them. The bug hid behind fixtures too small to bind.
fn seeded(dir: &TempDir) -> Areev {
    let mut m = Areev::open(dir.path().join("budget.db").to_str().unwrap()).unwrap();
    for i in 0..200 {
        let subject = format!("s{i}");
        let object = format!(
            "this is a reasonably long object value number {i}, long enough that \
             two hundred of them cannot fit inside the four-thousand-token default"
        );
        let mut f = Fact::new(&subject, "note", &object).confidence(0.9);
        f.common.namespace = Some("b".to_string());
        m.add(&f).unwrap();
    }
    m
}

fn assembled(
    ex: &CalExecutor,
    facade: &AreevFacade,
    src: &str,
) -> (usize, Option<usize>, Vec<String>) {
    let res = ex.execute(src, facade).unwrap();
    let warnings = res.warnings.clone();
    match res.result {
        CalResultPayload::Assembled {
            grains,
            total_available,
            ..
        } => (grains.len(), total_available, warnings),
        other => panic!("expected Assembled, got {other:?}"),
    }
}

#[test]
fn an_unbudgeted_assemble_reports_what_its_default_dropped() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded(&d), Some("b".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let (kept, total_available, warnings) = assembled(
        &ex,
        &facade,
        r#"ASSEMBLE "e" FROM e: (RECALL facts WHERE namespace = "b" LIMIT 400)"#,
    );

    assert!(
        kept < 200,
        "fixture must be large enough for the default budget to bind (kept {kept})"
    );
    // The number that used to hide the cut: it reported `kept`, so the drop
    // was not computable from the payload at all.
    assert_eq!(
        total_available,
        Some(200),
        "total_available must be the PRE-budget count, or a truncated assembly \
         is arithmetically indistinguishable from a complete one"
    );

    let w = warnings
        .iter()
        .find(|w| w.starts_with("CAL-W017"))
        .unwrap_or_else(|| panic!("no CAL-W017 among {warnings:?}"));
    assert!(w.contains(&format!("dropped {} of 200 grains", 200 - kept)), "{w}");
    // The label matters: "130 grains went" is not actionable on a four-source
    // assembly without knowing which section lost them.
    assert!(w.contains("[e]"), "{w}");
    // …and so does *why* the budget was 4000, since the caller never wrote it.
    assert!(w.contains("No BUDGET clause was written"), "{w}");
}

#[test]
fn an_explicit_budget_that_binds_names_itself_not_the_default() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded(&d), Some("b".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let (_, _, warnings) = assembled(
        &ex,
        &facade,
        r#"ASSEMBLE "e" FROM e: (RECALL facts WHERE namespace = "b" LIMIT 400) BUDGET 4000 tokens"#,
    );
    let w = warnings.iter().find(|w| w.starts_with("CAL-W017")).unwrap();
    assert!(w.contains("BUDGET 4000 tokens dropped"), "{w}");
    assert!(
        !w.contains("No BUDGET clause was written"),
        "a budget the caller wrote must not be reported as the default: {w}"
    );
}

#[test]
fn a_budget_that_fits_stays_silent() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded(&d), Some("b".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let (kept, total_available, warnings) = assembled(
        &ex,
        &facade,
        r#"ASSEMBLE "e" FROM e: (RECALL facts WHERE namespace = "b" LIMIT 400) BUDGET 16000 tokens"#,
    );

    assert_eq!(kept, 200, "the ceiling holds the whole set");
    assert_eq!(total_available, Some(200));
    assert!(
        !warnings.iter().any(|w| w.starts_with("CAL-W017")),
        "a budget that dropped nothing must not warn — silence has to keep \
         meaning 'nothing was cut': {warnings:?}"
    );
}

/// The plain `RECALL` under the same predicate is the control: if it also
/// came back short the assembly's cut would not be the budget's doing.
#[test]
fn the_same_selection_without_assemble_is_whole() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded(&d), Some("b".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let res = ex
        .execute(
            r#"RECALL facts WHERE namespace = "b" LIMIT 400"#,
            &facade,
        )
        .unwrap();
    match res.result {
        CalResultPayload::Grains {
            grains,
            total_available,
        } => {
            assert_eq!(grains.len(), 200);
            assert_eq!(total_available, Some(200));
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}
