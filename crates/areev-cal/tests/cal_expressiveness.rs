//! #209 / #210 / #211 — the three reads that could not be written in CAL:
//! summarise by frequency, extract part of a value, and navigate into a
//! structured payload.
//!
//! The spec decisions behind them are recorded in
//! `docs/oms-1.7-amendments-cal-expressiveness.md`.

use areev_cal::executor::CalResultPayload;
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn mem(d: &TempDir) -> Areev {
    Areev::open(d.path().join("x.db").to_str().unwrap()).unwrap()
}

fn add(m: &mut Areev, ns: &str, s: &str, r: &str, o: &str) {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    m.add(&f).unwrap();
}

fn text(ex: &CalExecutor, facade: &AreevFacade, src: &str) -> String {
    match ex.execute(src, facade).unwrap().result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// #209 — per-group counts
// ---------------------------------------------------------------------------

/// The fixture is deliberately uneven *and* has a tie, because the ordering is
/// part of the contract: frequency is the answer, and a deterministic
/// tiebreak is what makes it reproducible across backends and runs.
fn seeded_failures(d: &TempDir) -> Areev {
    let mut m = mem(d);
    for (tool, n) in [("search_notes", 3), ("search_contacts", 2), ("send_email", 2), ("ping", 1)] {
        for i in 0..n {
            add(&mut m, "e", tool, "failed_with", &format!("status 401 attempt {i}"));
        }
    }
    m
}

#[test]
fn group_by_then_count_projects_one_row_per_group_most_frequent_first() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_failures(&d), Some("e".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let res = ex
        .execute(
            r#"RECALL facts WHERE namespace = "e" LIMIT 400 GROUP BY subject COUNT"#,
            &facade,
        )
        .unwrap();
    let CalResultPayload::GroupCounts { field, groups, total_available } = res.result else {
        panic!("expected GroupCounts");
    };
    assert_eq!(field, "subject");
    assert_eq!(total_available, Some(4));

    let rows: Vec<(String, i64)> = groups
        .iter()
        .map(|g| {
            (
                g.fields["key"].as_str().unwrap().to_string(),
                g.fields["count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("search_notes".into(), 3),
            // The tie breaks by key ascending, not by insertion or scan order.
            ("search_contacts".into(), 2),
            ("send_email".into(), 2),
            ("ping".into(), 1),
        ]
    );
    // A group is computed, not stored: anything keying on a content address
    // (dedup above all) must be able to tell it from a grain.
    assert!(groups.iter().all(|g| g.hash.is_empty() && g.grain_type == "group"));
}

/// The behaviour this replaces answered the plain total — identical to
/// `COUNT` alone, so nothing could have wanted it. `COUNT` without a
/// `GROUP BY` must still answer exactly that.
#[test]
fn count_without_group_by_is_still_the_plain_total() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_failures(&d), Some("e".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    match ex
        .execute(r#"RECALL facts WHERE namespace = "e" LIMIT 400 COUNT"#, &facade)
        .unwrap()
        .result
    {
        CalResultPayload::Count { count } => assert_eq!(count, 8),
        other => panic!("expected Count, got {other:?}"),
    }
}

#[test]
fn group_variables_render_most_frequent_first() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_failures(&d), Some("e".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    ex.execute(
        r#"DEFINE TEMPLATE topfail ELEMENT {- ({{group.count}}x) {{group.key}}
}"#,
        &facade,
    )
    .unwrap();
    let out = text(
        &ex,
        &facade,
        r#"RECALL facts WHERE namespace = "e" LIMIT 400 GROUP BY subject COUNT FORMAT TEMPLATE topfail"#,
    );
    assert!(out.starts_with("- (3x) search_notes"), "{out}");
    assert!(out.contains("- (1x) ping"), "{out}");
}

/// `group.*` is bound on a group row and nowhere else — on an ordinary grain
/// it must not read a field that happens to be called `key`.
#[test]
fn group_variables_are_null_on_an_ordinary_grain() {
    let d = TempDir::new().unwrap();
    let mut m = mem(&d);
    add(&mut m, "g", "a", "note", "b");
    let facade = AreevFacade::with_session(m, Some("g".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    ex.execute(
        r#"DEFINE TEMPLATE gk ELEMENT {[{{group.key}}]}"#,
        &facade,
    )
    .unwrap();
    assert_eq!(
        text(&ex, &facade, r#"RECALL facts WHERE namespace = "g" FORMAT TEMPLATE gk"#),
        "[]"
    );
}

/// The `execute_source` discard the issue names: an assembly could not carry
/// a grouped summary, so "the errors this agent hits most" had to be a
/// separate read the host tallied and spliced in itself.
#[test]
fn an_assemble_source_can_be_a_grouped_count() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_failures(&d), Some("e".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let res = ex
        .execute(
            r#"ASSEMBLE "brief" FROM top: (RECALL facts WHERE namespace = "e" LIMIT 400 GROUP BY subject COUNT)"#,
            &facade,
        )
        .unwrap();
    match res.result {
        CalResultPayload::Assembled { grains, .. } => {
            assert_eq!(grains.len(), 4, "the four groups reached the assembly");
            assert_eq!(grains[0].fields["key"], "search_notes");
        }
        other => panic!("expected Assembled, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// #210 — extracting part of a value
// ---------------------------------------------------------------------------

fn seeded_text(d: &TempDir) -> Areev {
    let mut m = mem(d);
    add(
        &mut m,
        "s",
        "sess1",
        "note",
        "[Q3 close handoff] the ops team flagged three invoices\nsecond line",
    );
    m
}

#[test]
fn extractors_pull_a_title_out_of_free_text() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_text(&d), Some("s".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // Four routes to the same answer — the motivating case from #210.
    for (name, body, want) in [
        ("t1", r#"{{{grain.object | between("[", "]")}}}"#, "Q3 close handoff"),
        (
            "t2",
            r#"{{{grain.object | split("]", 0) | strip_prefix("[")}}}"#,
            "Q3 close handoff",
        ),
        (
            "t3",
            r#"{{{grain.object | match("\[([^\]]+)\]", 1)}}}"#,
            "Q3 close handoff",
        ),
        (
            "t4",
            r#"{{{grain.object | first_line | strip_suffix(" invoices")}}}"#,
            "[Q3 close handoff] the ops team flagged three",
        ),
    ] {
        ex.execute(&format!("DEFINE TEMPLATE {name} ELEMENT {body}"), &facade)
            .unwrap();
        assert_eq!(
            text(
                &ex,
                &facade,
                &format!(r#"RECALL facts WHERE namespace = "s" FORMAT TEMPLATE {name}"#)
            ),
            want,
            "filter chain {name}"
        );
    }
}

/// Totality: a filter that finds nothing yields empty. One grain that does not
/// match the shape must not fail the render of every other grain.
#[test]
fn an_extractor_that_finds_nothing_renders_empty_not_an_error() {
    let d = TempDir::new().unwrap();
    let mut m = mem(&d);
    add(&mut m, "s", "plain", "note", "no brackets here at all");
    let facade = AreevFacade::with_session(m, Some("s".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    ex.execute(
        r#"DEFINE TEMPLATE e1 ELEMENT {[{{grain.object | between("[", "]")}}]}"#,
        &facade,
    )
    .unwrap();
    assert_eq!(
        text(&ex, &facade, r#"RECALL facts WHERE namespace = "s" FORMAT TEMPLATE e1"#),
        "[]"
    );
}

/// …but a bad *argument* is an authoring mistake, and is refused when the
/// template is defined rather than rendering empty on every grain forever.
/// This is the auditability half: an unreadable saved query never gets stored.
#[test]
fn bad_filter_arguments_are_refused_at_define_time() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(mem(&d), Some("s".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    for (name, body) in [
        // Backreferences force backtracking, so the engine does not have them.
        ("b1", r#"{{{grain.object | match("(a)\1")}}}"#),
        // The POSIX `]`-first-in-class quirk is not this dialect.
        ("b2", r#"{{{grain.object | match("[[]([^]]+)[]]")}}}"#),
        ("b3", r#"{{{grain.object | split("]", notanumber)}}}"#),
        ("b4", r#"{{{grain.object | between("[")}}}"#),
        ("b5", r#"{{{grain.object | get("a.b.c.d.e.f.g.h.i")}}}"#),
    ] {
        let err = ex
            .execute(&format!("DEFINE TEMPLATE {name} ELEMENT {body}"), &facade)
            .unwrap_err();
        assert_eq!(err.code(), "CAL-E049", "{name} should refuse at define time");
    }

    // An unknown filter is still refused — the set stays closed.
    let err = ex
        .execute(
            r#"DEFINE TEMPLATE b6 ELEMENT {{{grain.object | eval("1+1")}}}"#,
            &facade,
        )
        .unwrap_err();
    assert!(format!("{err}").contains("CAL-E043"), "{err}");
}

// ---------------------------------------------------------------------------
// #211 — navigating a structured payload
// ---------------------------------------------------------------------------

fn seeded_json(d: &TempDir) -> Areev {
    let mut m = mem(d);
    add(
        &mut m,
        "j",
        "search_api",
        "fails_with",
        r#"{"error":{"code":"rate_limited"},"retry_after":30,"tags":["a","b"]}"#,
    );
    add(&mut m, "j", "other_api", "fails_with", r#"{"error":{"code":"timeout"}}"#);
    m
}

#[test]
fn get_reads_a_scalar_out_of_a_json_field() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_json(&d), Some("j".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    ex.execute(
        r#"DEFINE TEMPLATE sig ELEMENT {{{grain.subject}}={{grain.object | get("error.code")}}/{{grain.object | get("tags.0")}};}"#,
        &facade,
    )
    .unwrap();
    let out = text(
        &ex,
        &facade,
        r#"RECALL facts WHERE namespace = "j" ORDER BY subject ASC FORMAT TEMPLATE sig"#,
    );
    assert!(out.contains("search_api=rate_limited/a;"), "{out}");
    // A path that is absent on THIS grain renders empty, not an error.
    assert!(out.contains("other_api=timeout/;"), "{out}");
}

#[test]
fn a_missing_path_or_a_non_json_value_renders_empty() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_json(&d), Some("j".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    ex.execute(
        r#"DEFINE TEMPLATE m ELEMENT {[{{grain.object | get("nope.nothing")}}][{{grain.subject | get("a")}}]}"#,
        &facade,
    )
    .unwrap();
    let out = text(
        &ex,
        &facade,
        r#"RECALL facts WHERE namespace = "j" AND subject = "other_api" FORMAT TEMPLATE m"#,
    );
    assert_eq!(out, "[][]");
}

#[test]
fn where_filters_on_a_json_path() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_json(&d), Some("j".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let count = |src: &str| match ex.execute(src, &facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => grains.len(),
        other => panic!("expected Grains, got {other:?}"),
    };

    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "j" AND object.error.code = "rate_limited""#),
        1
    );
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "j" AND object.error.code = "nope""#),
        0
    );
    // Numbers compare as numbers, not as text.
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "j" AND object.retry_after > 10"#),
        1
    );
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "j" AND object.retry_after > 100"#),
        0
    );
    // IS NULL / IS NOT NULL see the path, not just the base field.
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "j" AND object.retry_after IS NOT NULL"#),
        1
    );
}

/// The composition that matters: a path that does not resolve is UNKNOWN
/// (#207), so navigation inherits the fails-closed rule rather than adding
/// one of its own. Both grains here carry `object`; only one carries the path.
#[test]
fn an_unresolvable_path_fails_closed_under_negation() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_json(&d), Some("j".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let count = |src: &str| match ex.execute(src, &facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => grains.len(),
        other => panic!("expected Grains, got {other:?}"),
    };

    assert_eq!(count(r#"RECALL facts WHERE namespace = "j""#), 2);
    // `other_api` has no `retry_after`. It must match neither the equality
    // nor its negation — otherwise `!=` is a filter that widens.
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "j" AND object.retry_after != 999"#),
        1
    );
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "j" AND NOT object.retry_after = 999"#),
        1
    );
}

#[test]
fn a_field_path_past_the_bound_is_refused_before_the_scan() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(seeded_json(&d), Some("j".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let err = ex
        .execute(
            r#"RECALL facts WHERE namespace = "j" AND object.a.b.c.d.e.f.g.h.i = "x""#,
            &facade,
        )
        .unwrap_err();
    assert_eq!(err.code(), "CAL-E002");
    assert!(format!("{err}").contains("8 segments"), "{err}");
}

/// A structured field and a JSON document stored as a string must navigate
/// identically, or the accessor depends on how the writer typed the field.
#[test]
fn a_parsed_object_and_a_json_string_navigate_the_same() {
    let d = TempDir::new().unwrap();
    let mut m = mem(&d);
    // `object` here is a JSON *string*; `input` on the tool grain below is a
    // parsed object the engine round-trips as JSON — which is exactly why a
    // Python or Node host could already see inside it while CAL could not.
    add(&mut m, "k", "as_string", "sig", r#"{"app":"phone"}"#);
    let mut tool = areev_core::types::Tool::new("phone.search_contacts")
        .input(serde_json::json!({"app": "phone"}));
    tool.common.namespace = Some("k".to_string());
    m.add(&tool).unwrap();
    let facade = AreevFacade::with_session(m, Some("k".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let count = |src: &str| match ex.execute(src, &facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => grains.len(),
        other => panic!("expected Grains, got {other:?}"),
    };
    assert_eq!(count(r#"RECALL facts WHERE namespace = "k" AND object.app = "phone""#), 1);
    assert_eq!(count(r#"RECALL tools WHERE namespace = "k" AND input.app = "phone""#), 1);
}
