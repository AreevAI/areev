//! `tags INCLUDE […]` / `EXCLUDE […]` actually filter (#318).
//!
//! Both forms parsed, `DESCRIBE FIELDS` advertised `tags` as filterable, the
//! executor marked them consumed by push-down — and nothing read them. So
//! `tags EXCLUDE ["label:restricted"]` returned exactly the grains it was
//! asked to exclude, and `areev corpus --select` wrote them into the export
//! under a manifest recording the exclusion. This is the failure class #91
//! closed for every other field.

use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::Event;
use areev_store::Areev;
use tempfile::TempDir;

fn rig(dir: &TempDir) -> AreevFacade {
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".into()), None);
    // Three events: tagged `a`, tagged `a` + `b`, and untagged.
    let mut only_a = Event::new("filed a report");
    only_a.common.namespace = Some("caller".into());
    only_a.common.tags = vec!["a".into()];
    only_a.common.created_at = Some(1_000);
    let mut a_and_b = Event::new("filed b report");
    a_and_b.common.namespace = Some("caller".into());
    a_and_b.common.tags = vec!["a".into(), "b".into()];
    a_and_b.common.created_at = Some(2_000);
    let mut untagged = Event::new("filed c report");
    untagged.common.namespace = Some("caller".into());
    untagged.common.created_at = Some(3_000);
    f.with_store(|m| {
        m.add(&only_a)?;
        m.add(&a_and_b)?;
        m.add(&untagged)
    })
    .unwrap();
    f
}

fn count(f: &AreevFacade, q: &str) -> usize {
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let res = ex.execute(q, f).unwrap();
    let v = serde_json::to_value(res.payload_json().unwrap()).unwrap();
    v["grains"].as_array().map(Vec::len).unwrap_or(0)
}

#[test]
fn include_requires_every_named_tag() {
    let d = TempDir::new().unwrap();
    let f = rig(&d);
    assert_eq!(count(&f, r#"RECALL events WHERE tags INCLUDE ["a"] LIMIT 10"#), 2);
    assert_eq!(count(&f, r#"RECALL events WHERE tags INCLUDE ["b"] LIMIT 10"#), 1);
    assert_eq!(
        count(&f, r#"RECALL events WHERE tags INCLUDE ["a", "b"] LIMIT 10"#),
        1,
        "INCLUDE is conjunctive"
    );
    // A tag nothing carries returns nothing — before this it returned
    // everything.
    assert_eq!(count(&f, r#"RECALL events WHERE tags INCLUDE ["zzz"] LIMIT 10"#), 0);
}

#[test]
fn exclude_removes_the_tagged_grains() {
    let d = TempDir::new().unwrap();
    let f = rig(&d);
    // The `a`-only grain and the untagged one survive; the `a`+`b` one goes.
    assert_eq!(count(&f, r#"RECALL events WHERE tags EXCLUDE ["b"] LIMIT 10"#), 2);
    assert_eq!(count(&f, r#"RECALL events WHERE tags EXCLUDE ["a"] LIMIT 10"#), 1);
    assert_eq!(count(&f, r#"RECALL events WHERE tags EXCLUDE ["zzz"] LIMIT 10"#), 3);
}

#[test]
fn an_untagged_grain_matches_no_include_and_every_exclude() {
    let d = TempDir::new().unwrap();
    let f = rig(&d);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let v = serde_json::to_value(
        ex.execute(r#"RECALL events WHERE tags EXCLUDE ["a"] LIMIT 10"#, &f)
            .unwrap()
            .payload_json()
            .unwrap(),
    )
    .unwrap();
    let texts: Vec<String> = v["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.to_string())
        .collect();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("filed c report"), "{texts:?}");
}

#[test]
fn in_and_not_in_agree_with_the_include_and_exclude_spellings() {
    let d = TempDir::new().unwrap();
    let f = rig(&d);
    assert_eq!(
        count(&f, r#"RECALL events WHERE tags IN ("a") LIMIT 10"#),
        count(&f, r#"RECALL events WHERE tags INCLUDE ["a"] LIMIT 10"#)
    );
    assert_eq!(
        count(&f, r#"RECALL events WHERE tags NOT IN ("b") LIMIT 10"#),
        count(&f, r#"RECALL events WHERE tags EXCLUDE ["b"] LIMIT 10"#)
    );
}

#[test]
fn an_export_selector_that_excludes_a_label_excludes_it() {
    // The reason this matters: `areev corpus --select` takes read-only CAL,
    // so the selector IS the control, and the export manifest records it as
    // a claim about what was excluded.
    let d = TempDir::new().unwrap();
    let m = Areev::open(d.path().join("x.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".into()), None);
    let mut restricted = Event::new("MNPI: pending acquisition");
    restricted.common.namespace = Some("caller".into());
    restricted.common.tags = vec!["label:restricted".into()];
    restricted.common.created_at = Some(1_000);
    let mut ordinary = Event::new("quarterly summary");
    ordinary.common.namespace = Some("caller".into());
    ordinary.common.created_at = Some(2_000);
    f.with_store(|m| {
        m.add(&restricted)?;
        m.add(&ordinary)
    })
    .unwrap();

    let ex = CalExecutor::new(CalExecutorConfig::default());
    let v = serde_json::to_value(
        ex.execute(
            r#"RECALL events WHERE tags EXCLUDE ["label:restricted"] LIMIT 50"#,
            &f,
        )
        .unwrap()
        .payload_json()
        .unwrap(),
    )
    .unwrap();
    let dump = v["grains"].to_string();
    assert!(
        !dump.contains("MNPI"),
        "a restricted-class grain must not survive its own exclusion: {dump}"
    );
    assert!(dump.contains("quarterly summary"));
}
