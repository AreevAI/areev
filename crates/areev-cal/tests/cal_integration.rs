//! M2 exit test: CAL text → executor → AreevFacade → embedded Turso store.
//!
//! Covers the read tier (RECALL, EXISTS, HISTORY, pipeline COUNT) and the
//! ADD tier (ADD, SUPERSEDE) end-to-end against a real memory file.

use areev_cal::executor::CalResultPayload;
use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_store::Areev;
use tempfile::TempDir;

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn added_hash(payload: &CalResultPayload) -> String {
    match payload {
        CalResultPayload::Added { hash, .. } => hash.clone(),
        CalResultPayload::Superseded { new_hash, .. } => new_hash.clone(),
        other => panic!("expected Added/Superseded payload, got: {other:?}"),
    }
}

#[test]
fn cal_add_then_recall() {
    let (ex, facade, _d) = setup();

    let add = ex
        .execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "integration""#,
            &facade,
        )
        .unwrap();
    let hash = added_hash(&add.result);
    assert_eq!(hash.len(), 64);

    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "john""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "rust");
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_recall_recent_experience_without_subject() {
    // The "reflect over recent experience" path: RECALL by grain type with no
    // subject/free-text anchor now does a bounded recent-scan (it used to
    // error). Observations carry observer_id, filtered as a post-condition.
    let (ex, facade, _d) = setup();
    for (obs_id, body) in [
        ("executor", "attempt 1 failed"),
        ("executor", "attempt 2 passed"),
        ("planner", "unrelated note"),
    ] {
        ex.execute(
            &format!(
                r#"ADD observation SET observer_id = "{obs_id}" SET observer_type = "llm" SET content = "{body}" SET namespace = "caller" REASON "log""#
            ),
            &facade,
        )
        .unwrap();
    }

    // Bare recent-by-type scan returns all three, newest first.
    let all = ex.execute("RECALL observations RECENT 10", &facade).unwrap();
    match all.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 3),
        other => panic!("expected Grains, got {other:?}"),
    }

    // observer_id post-filter narrows to the two executor observations.
    let filtered = ex
        .execute(
            r#"RECALL observations WHERE observer_id = "executor" RECENT 10"#,
            &facade,
        )
        .unwrap();
    match filtered.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 2),
        other => panic!("expected Grains, got {other:?}"),
    }

    // A wildcard recall with no anchor at all is still rejected as too broad.
    assert!(ex.execute("RECALL * RECENT 10", &facade).is_err());
}

#[test]
fn cal_exists_after_add() {
    let (ex, facade, _d) = setup();
    let add = ex
        .execute(
            r#"ADD fact SET subject = "a" SET relation = "r" SET object = "o" SET namespace = "caller" REASON "t""#,
            &facade,
        )
        .unwrap();
    let hash = added_hash(&add.result);

    let q = format!("EXISTS sha256:{hash}");
    let res = ex.execute(&q, &facade).unwrap();
    match res.result {
        CalResultPayload::Exists { exists, .. } => assert!(exists),
        other => panic!("expected Exists, got {other:?}"),
    }
}

#[test]
fn cal_supersede_and_history() {
    let (ex, facade, _d) = setup();
    let add = ex
        .execute(
            r#"ADD fact SET subject = "acct" SET relation = "balance" SET object = "100" SET namespace = "caller" REASON "init""#,
            &facade,
        )
        .unwrap();
    let h1 = added_hash(&add.result);

    let sup = ex
        .execute(
            &format!(r#"SUPERSEDE sha256:{h1} SET object = "80" REASON "withdrawal""#),
            &facade,
        )
        .unwrap();
    let h2 = added_hash(&sup.result);
    assert_ne!(h1, h2);

    // Current recall sees only the new version.
    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "acct""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "80");
        }
        other => panic!("expected Grains, got {other:?}"),
    }

    // HISTORY walks the chain, newest first.
    let hist = ex
        .execute(
            r#"HISTORY WHERE subject = "acct" AND relation = "balance""#,
            &facade,
        )
        .unwrap();
    match hist.result {
        CalResultPayload::History { versions } => {
            assert_eq!(versions.len(), 2);
            let v = serde_json::to_value(&versions).unwrap();
            assert_eq!(v[0]["object"], "80");
            assert_eq!(v[1]["object"], "100");
        }
        other => panic!("expected History, got {other:?}"),
    }
}

#[test]
fn cal_pipeline_count() {
    let (ex, facade, _d) = setup();
    for i in 0..3 {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "kid" SET relation = "likes" SET object = "toy{i}" SET namespace = "caller" REASON "t""#
            ),
            &facade,
        )
        .unwrap();
    }
    let res = ex
        .execute(r#"RECALL facts WHERE subject = "kid" | COUNT"#, &facade)
        .unwrap();
    match res.result {
        CalResultPayload::Count { count } => assert_eq!(count, 3),
        other => panic!("expected Count, got {other:?}"),
    }
}

#[test]
fn destructive_tokens_are_parse_errors() {
    let (ex, facade, _d) = setup();
    for q in ["DELETE sha256:abc", r#"DROP TABLE grains"#] {
        assert!(ex.execute(q, &facade).is_err(), "{q} must not execute");
    }
}

#[test]
fn cal_add_inherits_session_namespace() {
    // ADD without `SET namespace` must land in the session namespace so the
    // same session's RECALL can see it (RECALL already scoped to the session).
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "zoe" SET relation = "team" SET object = "core" REASON "session ns""#,
        &facade,
    )
    .unwrap();
    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "zoe""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1, "session RECALL must see the session ADD");
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["namespace"], "caller");
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_add_explicit_namespace_still_wins() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "zoe" SET relation = "team" SET object = "core" SET namespace = "other" REASON "explicit ns""#,
        &facade,
    )
    .unwrap();
    // Not visible in the session namespace…
    let in_session = ex
        .execute(r#"RECALL facts WHERE subject = "zoe""#, &facade)
        .unwrap();
    match in_session.result {
        CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty()),
        other => panic!("expected Grains, got {other:?}"),
    }
    // …but present where the user explicitly put it.
    let in_other = ex
        .execute(
            r#"RECALL facts WHERE namespace = "other" AND subject = "zoe""#,
            &facade,
        )
        .unwrap();
    match in_other.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 1),
        other => panic!("expected Grains, got {other:?}"),
    }
}

/// The CAL reference documents `FORMAT [json AS data, markdown AS readable]`,
/// but DATA/READABLE/COMPACT are keyword tokens — parsing the alias as a bare
/// identifier rejected the manual's own example. End-to-end: the aliases must
/// survive to the rendered output and key the format map.
#[test]
fn cal_format_aliases_may_be_reserved_words() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();

    let res = ex
        .execute(
            r#"RECALL facts WHERE subject = "john" FORMAT [json AS data, markdown AS readable]"#,
            &facade,
        )
        .unwrap();
    match res.result {
        CalResultPayload::MultiFormatted { formats, .. } => {
            let mut keys: Vec<_> = formats.keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, vec!["data".to_string(), "readable".to_string()]);
            assert!(!formats["data"].is_empty(), "json rendering is empty");
            assert!(!formats["readable"].is_empty(), "markdown rendering is empty");
        }
        other => panic!("expected MultiFormatted, got: {other:?}"),
    }
}

/// `RECALL *` is the documented explicit spelling of "any grain type"; it must
/// parse, execute, and mean exactly what omitting the grain type means.
///
/// Note what this does *not* assert. On this build `RECALL *` cannot actually
/// return a non-fact grain: the hexastore subject index and the text index
/// cover fact triples only, and the unanchored forms (`RECALL * LIMIT n`,
/// `RECALL * RECENT n`) are refused because they need a *specific* type. So
/// `*` is currently indistinguishable from `facts` in its results — the
/// syntax carries the right "no type filter" semantics and will widen on its
/// own if the store ever indexes other types.
#[test]
fn cal_recall_star_means_no_grain_type_filter() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        r#"ADD observation SET subject = "john" SET content = "asked about pricing" REASON "seed""#,
        &facade,
    )
    .unwrap();

    let starred = ex
        .execute(r#"RECALL * WHERE subject = "john""#, &facade)
        .expect("RECALL * must execute");
    let omitted = ex
        .execute(r#"RECALL WHERE subject = "john""#, &facade)
        .expect("typeless RECALL must execute");
    match (starred.result, omitted.result) {
        (
            CalResultPayload::Grains { grains: a, .. },
            CalResultPayload::Grains { grains: b, .. },
        ) => {
            assert_eq!(
                a.len(),
                b.len(),
                "RECALL * must mean the same as omitting the grain type"
            );
            assert!(!a.is_empty(), "expected the seeded fact");
        }
        other => panic!("expected two Grains payloads, got: {other:?}"),
    }
}

/// The anti-full-scan guard stays: with neither a subject, a free-text query,
/// nor a *specific* grain type there is nothing to anchor the scan. The CAL
/// reference's `RECALL * WHERE namespace = ...` example cannot execute, and
/// that is deliberate — do not relax this to make a snippet true.
#[test]
fn cal_recall_star_still_needs_an_anchor() {
    let (ex, facade, _d) = setup();
    let err = ex
        .execute(r#"RECALL * WHERE namespace = "caller""#, &facade)
        .expect_err("unanchored RECALL * must be refused");
    assert_eq!(err.code(), "CAL-E092", "unexpected error: {err:?}");
}

// ── Saved queries & custom templates ────────────────────────────────────
//
// These are host metadata carried by the file, not memories. The point of
// persisting them is that they survive the process — so the tests reopen.

fn facade_at(path: &std::path::Path) -> AreevFacade {
    let m = Areev::open(path.to_str().unwrap()).unwrap();
    AreevFacade::with_session(m, Some("caller".to_string()), None)
}

#[test]
fn saved_query_round_trips_through_the_file() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        ex.execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
            &facade,
        )
        .unwrap();
        ex.execute(
            r#"DEFINE QUERY "john brief" DESCRIPTION "what we know" AS { RECALL facts WHERE subject = "john" LIMIT 20 }"#,
            &facade,
        )
        .expect("DEFINE QUERY must succeed");
        assert!(
            facade.list_queries().iter().any(|q| q.name == "john brief"),
            "the query should be listed in the defining process"
        );
    }

    // A new process over the same file must still see it, and be able to run
    // it. Each reopen is scoped: one memory is one handle, so the previous one
    // has to be dropped first — which is also what "a new process" means.
    {
        let facade = facade_at(&path);
        let saved = facade
            .get_query("john brief")
            .expect("saved query must survive reopen");
        assert_eq!(saved.description, "what we know");
        assert!(!saved.builtin);

        let run = ex.execute(r#"RUN "john brief""#, &facade).expect("RUN must work");
        match run.result {
            CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 1),
            other => panic!("expected Grains from RUN, got: {other:?}"),
        }
    }

    // RUN records last_run_at, and that too is persisted.
    assert!(
        facade_at(&path).get_query("john brief").unwrap().last_run_at.is_some(),
        "RUN should have recorded last_run_at on disk"
    );

    // DROP removes it from the file, not just from memory.
    {
        let facade = facade_at(&path);
        ex.execute(r#"DROP QUERY "john brief""#, &facade)
            .expect("DROP QUERY must succeed");
    }
    assert!(facade_at(&path).get_query("john brief").is_none());
}

/// A file can carry host metadata this build cannot load — a template written
/// before the §10.8 body limit was enforced, a row from a newer version.
/// Skipping it is right (one bad row must not make the memory unusable), but
/// skipping it *silently* is not: the operator would find out by noticing a
/// template had gone missing.
#[test]
fn a_template_the_file_carries_but_this_build_cannot_load_is_reported() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");

    // Write the row the way an older build would have: straight into `meta`,
    // with a body past the limit this build enforces.
    {
        let m = Areev::open(path.to_str().unwrap()).unwrap();
        let oversized = "x".repeat(areev_cal::templates::MAX_TEMPLATE_SIZE + 1);
        let row = serde_json::json!({
            "source": oversized,
            "description": "written by an older build",
            "parent": null,
            "grain_types": [],
        });
        m.meta_put("tpl:legacy", &row.to_string()).unwrap();
    }

    let facade = facade_at(&path);
    assert!(
        facade.get_template("legacy").is_none(),
        "an unloadable template must not be served as if it were fine"
    );
    let warnings = facade.meta_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("legacy")),
        "the drop must be reported, got: {warnings:?}"
    );
}

/// `DEFINE TEMPLATE` names go through the label parser, so plenty of ordinary
/// words that happen to be CAL keywords are valid names. `FORMAT TEMPLATE` used
/// to accept only a bare identifier, which made those templates definable but
/// unreferenceable — you could write one and never use it.
#[test]
fn a_template_named_after_a_keyword_can_still_be_referenced() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "seed""#,
        &facade,
    )
    .unwrap();

    // Keyword tokens, but not names the built-in presets already hold.
    for name in ["recent", "scope", "priority"] {
        ex.execute(
            &format!(r#"DEFINE TEMPLATE {name} AS "[{name}] {{{{grain.content}}}}""#),
            &facade,
        )
        .unwrap_or_else(|e| panic!("DEFINE TEMPLATE {name} should parse: {e}"));

        let out = ex
            .execute(
                &format!(r#"RECALL facts WHERE subject = "john" FORMAT TEMPLATE {name}"#),
                &facade,
            )
            .unwrap_or_else(|e| panic!("FORMAT TEMPLATE {name} should resolve: {e}"));
        match out.result {
            CalResultPayload::Formatted { text, .. } => {
                assert!(text.contains(&format!("[{name}]")), "wrong template ran: {text}")
            }
            other => panic!("expected Formatted, got: {other:?}"),
        }
    }
}

/// The `FOR` clause rides the statement, not the template body, so it only
/// survives a reopen if it is put back explicitly — it used to be written to
/// the file and then dropped on the way back in.
#[test]
fn a_templates_for_clause_survives_a_reopen() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        ex.execute(
            r#"DEFINE TEMPLATE facts_only FOR facts, goals AS "{{grain.content}}""#,
            &facade,
        )
        .expect("DEFINE TEMPLATE ... FOR must succeed");
        assert_eq!(
            facade.get_template("facts_only").unwrap().grain_types,
            vec!["facts".to_string(), "goals".to_string()],
            "the FOR clause must be visible in the defining process too"
        );
    }

    let reopened = facade_at(&path).get_template("facts_only").unwrap();
    assert_eq!(
        reopened.grain_types,
        vec!["facts".to_string(), "goals".to_string()],
        "the FOR clause must survive the reopen"
    );
}

#[test]
fn custom_template_round_trips_and_renders() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        ex.execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
            &facade,
        )
        .unwrap();
        ex.execute(
            r#"DEFINE TEMPLATE "brief" DESCRIPTION "one line each" AS "{{#each grains}}- {{subject}} {{relation}} {{object}}
{{/each}}""#,
            &facade,
        )
        .expect("DEFINE TEMPLATE must succeed");
    }

    let facade = facade_at(&path);
    assert!(
        facade.list_templates().iter().any(|t| t.name == "brief" && !t.builtin),
        "custom template must survive reopen"
    );
    let rendered = ex
        .execute(
            r#"RECALL facts WHERE subject = "john" FORMAT preset "brief""#,
            &facade,
        )
        .expect("preset render must work");
    match rendered.result {
        CalResultPayload::Formatted { text, .. } => {
            assert!(text.contains("john likes rust"), "unexpected render: {text:?}");
        }
        other => panic!("expected Formatted, got: {other:?}"),
    }
}

/// The registry owns the rules; a rejected definition must not reach the file.
#[test]
fn an_invalid_saved_query_name_is_not_persisted() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    {
        let facade = facade_at(&path);
        // Leading digit violates the name rule.
        assert!(facade.define_query("9bad", "RECALL facts", None, &[]).is_err());
    }
    assert!(facade_at(&path).get_query("9bad").is_none());
}

// ── Sectioned templates (OMS CAL §10.6) ─────────────────────────────────────

/// A sectioned definition must survive a real process reopen and render
/// through the engine-driven pipeline: HEADER once, ELEMENT per grain,
/// FOOTER once.
#[test]
fn sectioned_template_round_trips_and_renders() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        for (rel, obj) in [("likes", "rust"), ("prefers", "dark mode")] {
            ex.execute(
                &format!(
                    r#"ADD fact SET subject = "john" SET relation = "{rel}" SET object = "{obj}" REASON "seed""#
                ),
                &facade,
            )
            .unwrap();
        }
        ex.execute(
            "DEFINE TEMPLATE roster\n\
             HEADER {\n\
             <context>\n\
             }\n\
             ELEMENT {\n\
             <{{grain.type}}>{{grain.content}}</{{grain.type}}>\n\
             }\n\
             FOOTER {\n\
             </context>\n\
             }",
            &facade,
        )
        .expect("sectioned DEFINE TEMPLATE must succeed");
    }

    // A new process over the same file recovers the sectioned form.
    let facade = facade_at(&path);
    let info = facade
        .get_template("roster")
        .expect("sectioned template must survive reopen");
    assert!(info.source.contains("ELEMENT {"), "source: {:?}", info.source);

    let rendered = ex
        .execute(
            r#"RECALL facts WHERE subject = "john" ORDER BY created_at ASC FORMAT preset "roster""#,
            &facade,
        )
        .expect("sectioned render must work");
    match rendered.result {
        CalResultPayload::Formatted { text, .. } => {
            assert_eq!(
                text,
                "<context>\n\
                 <fact>likes rust</fact>\n\
                 <fact>prefers dark mode</fact>\n\
                 </context>",
                "sectioned render shape"
            );
        }
        other => panic!("expected Formatted, got: {other:?}"),
    }
}

/// §10.6.1 defines the shorthand by equivalence, so the two spellings must
/// produce byte-identical output.
#[test]
fn element_shorthand_equals_an_element_section() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();

    ex.execute(
        r#"DEFINE TEMPLATE shorthand AS "{{grain.subject}}: {{grain.content}}""#,
        &facade,
    )
    .expect("shorthand definition");
    ex.execute(
        "DEFINE TEMPLATE sectioned\nELEMENT {\n{{grain.subject}}: {{grain.content}}\n}",
        &facade,
    )
    .expect("sectioned definition");

    let render = |name: &str| match ex
        .execute(
            &format!(r#"RECALL facts WHERE subject = "john" FORMAT preset "{name}""#),
            &facade,
        )
        .unwrap()
        .result
    {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    assert_eq!(render("shorthand"), render("sectioned"));
    assert_eq!(render("shorthand"), "john: likes rust");
}

/// §7 spells template names as bare identifiers; the quoted form stays
/// accepted for names that need a space.
#[test]
fn template_names_may_be_bare_or_quoted() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(r#"DEFINE TEMPLATE bare AS "{{grain.content}}""#, &facade)
        .expect("bare identifier name");
    ex.execute(r#"DEFINE TEMPLATE "two words" AS "{{grain.content}}""#, &facade)
        .expect("quoted name");

    let names: Vec<String> = facade.list_templates().into_iter().map(|t| t.name).collect();
    assert!(names.contains(&"bare".to_string()), "{names:?}");
    assert!(names.contains(&"two words".to_string()), "{names:?}");
}

/// The reason section bodies are lexed as raw text: template prose is not
/// CAL, and before the raw capture each of these failed the whole query.
#[test]
fn a_section_body_may_contain_text_that_is_not_cal() {
    use areev_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        "DEFINE TEMPLATE prose\nELEMENT {\ndon't DELETE this — 50% \"kept\"\n}",
        &facade,
    )
    .expect("a body that is not CAL must still define");

    let info = facade.get_template("prose").unwrap();
    assert!(
        info.source.contains("don't DELETE this — 50% \"kept\""),
        "body was mangled: {:?}",
        info.source
    );
}

/// The three §10.6 `FORMAT TEMPLATE` forms are told apart by token class:
/// bare = registered name, quoted = ELEMENT shorthand, braced = sections.
#[test]
fn format_template_has_three_forms() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        "DEFINE TEMPLATE named\nELEMENT {\n[{{grain.subject}}] {{grain.content}}\n}",
        &facade,
    )
    .unwrap();

    let render = |q: &str| match ex.execute(q, &facade).expect(q).result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted from {q:?}, got: {other:?}"),
    };

    // Bare identifier — the registered template.
    assert_eq!(
        render(r#"RECALL facts WHERE subject = "john" FORMAT TEMPLATE named"#),
        "[john] likes rust"
    );
    // Quoted string — inline ELEMENT shorthand.
    assert_eq!(
        render(r#"RECALL facts WHERE subject = "john" FORMAT TEMPLATE "[{{grain.subject}}] {{grain.content}}""#),
        "[john] likes rust"
    );
    // Braced — inline sections.
    assert_eq!(
        render(
            "RECALL facts WHERE subject = \"john\" \
             FORMAT TEMPLATE { ELEMENT {\n[{{grain.subject}}] {{grain.content}}\n} }"
        ),
        "[john] likes rust"
    );
}

/// The §10.1.1 example the spec grammar previously forbade, now that
/// `format_spec` admits `"TEMPLATE" , string_literal`.
#[test]
fn spec_10_1_1_aliased_inline_template_example_runs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "alice" SET relation = "prefers" SET object = "dark mode" REASON "seed""#,
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"RECALL facts FORMAT [json AS structured, TEMPLATE "{{grain.subject}}: {{grain.object}}" AS oneliner]"#,
            &facade,
        )
        .expect("the spec's own example must run");
    match out.result {
        CalResultPayload::MultiFormatted { formats, .. } => {
            assert_eq!(
                formats.get("oneliner").map(String::as_str),
                Some("alice: dark mode"),
                "formats: {formats:?}"
            );
            assert!(formats.contains_key("structured"), "formats: {formats:?}");
        }
        other => panic!("expected MultiFormatted, got: {other:?}"),
    }
}

// ── §10.5 assembly/source/budget variables + §10.7 inheritance ──────────────

/// The spec's own `semantic_sml` template (§10.6), over a two-source
/// ASSEMBLE: HEADER once with the assembly intent, ELEMENT per grain,
/// SOURCE_BREAK between sources, FOOTER once.
#[test]
fn spec_semantic_sml_renders_over_a_multi_source_assemble() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for (rel, obj) in [("prefers", "dark mode"), ("works at", "acme")] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "{rel}" SET object = "{obj}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }
    ex.execute(
        r#"ADD goal SET subject = "alice" SET object = "ship Q1 review" REASON "seed""#,
        &facade,
    )
    .unwrap();

    ex.execute(
        "DEFINE TEMPLATE semantic_sml EXTENDS structured\n\
         HEADER {\n\
         <context intent=\"{{assembly.intent}}\" sources=\"{{assembly.source_count}}\">\n\
         }\n\
         ELEMENT {\n\
         <{{grain.type}} subject=\"{{grain.subject}}\">{{grain.content}}</{{grain.type}}>\n\
         }\n\
         SOURCE_BREAK {\n\
         \n\
         }\n\
         FOOTER {\n\
         </context>\n\
         }",
        &facade,
    )
    .expect("EXTENDS structured must resolve");

    let out = ex
        .execute(
            r#"ASSEMBLE brief FOR "helping alice" FROM f: (RECALL facts WHERE subject = "alice"), g: (RECALL goals RECENT 5) BUDGET 3000 tokens FORMAT TEMPLATE semantic_sml"#,
            &facade,
        )
        .expect("assemble render");

    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    assert!(
        text.starts_with("<context intent=\"helping alice\" sources=\"2\">"),
        "HEADER must carry assembly.* — got:\n{text}"
    );
    assert!(text.ends_with("</context>"), "FOOTER missing:\n{text}");
    assert!(text.contains("<goal subject=\"alice\">ship Q1 review</goal>"), "{text}");
    assert!(
        text.contains("<fact subject=\"alice\">prefers dark mode</fact>"),
        "{text}"
    );
    // SOURCE_BREAK is an empty body between the two sources, so exactly one
    // blank line separates the fact group from the goal group.
    assert!(text.contains("</fact>\n\n<goal"), "no SOURCE_BREAK between sources:\n{text}");
}

/// Regression: a *single*-source `ASSEMBLE` rendered with an empty plan, so
/// `assembly.*`, `budget.*` and `source.*` silently came back blank — bound on
/// the multi-source path and empty here, for no reason a user could see.
///
/// The single-source path budgets by grain count rather than tokens, so
/// `budget.unit` says `grains`; mislabelling it `tokens` would be a lie about
/// what the number means.
#[test]
fn assembly_variables_are_bound_for_a_single_source_assemble() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for obj in ["dark mode", "window seats", "oat milk"] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "prefers" SET object = "{obj}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }

    ex.execute(
        "DEFINE TEMPLATE one_source\n\
         HEADER {\n\
         [{{assembly.name}}|{{assembly.intent}}] {{budget.used}}/{{budget.total}} {{budget.unit}}\n\
         }\n\
         ELEMENT {\n\
         - [{{source.index}}] {{grain.content}}\n\
         }\n\
         ELEMENT_OMIT {\n\
         - (dropped: {{grain.content}})\n\
         }",
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"ASSEMBLE brief FOR "helping alice" FROM (RECALL facts WHERE subject = "alice") BUDGET 2 tokens FORMAT TEMPLATE one_source"#,
            &facade,
        )
        .expect("single-source assemble render");

    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    assert!(
        text.starts_with("[brief|helping alice] 2/2 grains"),
        "assembly.*/budget.* must be bound for one source too — got:\n{text}"
    );
    assert!(text.contains("- [0] "), "source.index unbound:\n{text}");
    assert!(
        text.contains("(dropped: "),
        "the budget cut a grain, so ELEMENT_OMIT must account for it:\n{text}"
    );
}

/// `source.*` is scoped to the element run: bound inside ELEMENT, unbound in
/// HEADER, where §10.5 offers only the assembly-level namespace.
#[test]
fn source_variables_are_scoped_to_the_element_run() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "alice" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        "DEFINE TEMPLATE labelled\n\
         HEADER {\n\
         head:[{{source.label}}] budget:{{budget.total}}\n\
         }\n\
         ELEMENT {\n\
         [{{source.label}}#{{source.index}}] {{grain.content}}\n\
         }",
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"ASSEMBLE b FOR "x" FROM f: (RECALL facts WHERE subject = "alice") BUDGET 500 tokens FORMAT TEMPLATE labelled"#,
            &facade,
        )
        .unwrap();
    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };
    // HEADER: source.label unbound (empty), budget.total bound.
    assert!(text.starts_with("head:[] budget:500"), "{text}");
    // ELEMENT: source.label bound.
    assert!(text.contains("[f#0] likes rust"), "{text}");
}

/// The same template must still render for a bare RECALL, with the
/// assembly-only variables resolving to empty rather than erroring (§10.8).
#[test]
fn assembly_variables_are_empty_for_a_bare_recall() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "alice" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        r#"DEFINE TEMPLATE mixed AS "[{{assembly.intent}}|{{source.label}}] {{grain.content}}""#,
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"RECALL facts WHERE subject = "alice" FORMAT TEMPLATE mixed"#,
            &facade,
        )
        .unwrap();
    match out.result {
        CalResultPayload::Formatted { text, .. } => assert_eq!(text, "[|] likes rust"),
        other => panic!("expected Formatted, got: {other:?}"),
    }
}

/// `ELEMENT_OMIT` renders the grains the budget dropped. Previously
/// unreachable: the assembler trimmed them before the renderer ran, so the
/// section parsed and inherited but could never fire.
#[test]
fn element_omit_renders_grains_the_budget_dropped() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for i in 0..8 {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "note{i}" SET object = "a reasonably long value so the token estimate is not trivial {i}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }

    ex.execute(
        "DEFINE TEMPLATE with_omit\n\
         ELEMENT {\n\
         + {{grain.content}}\n\
         }\n\
         ELEMENT_OMIT {\n\
         - omitted {{grain.type}}\n\
         }",
        &facade,
    )
    .unwrap();

    // A budget far too small for 8 grains forces most of them out.
    let out = ex
        .execute(
            r#"ASSEMBLE b FOR "x" FROM f: (RECALL facts WHERE subject = "alice") BUDGET 20 tokens FORMAT TEMPLATE with_omit"#,
            &facade,
        )
        .unwrap();
    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    let kept = text.lines().filter(|l| l.starts_with('+')).count();
    let omitted = text.lines().filter(|l| l.starts_with("- omitted")).count();
    assert!(omitted > 0, "budget should have dropped grains:\n{text}");
    assert_eq!(kept + omitted, 8, "every grain accounted for:\n{text}");
}

/// §10.8 caps `{{#each}}` at 200. The cap is correct, but truncating quietly
/// would read as "these are all the grains", so it must be announced.
#[test]
fn each_iteration_cap_emits_cal_w011() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for i in 0..205 {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "note{i}" SET object = "v{i}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }
    // A whole-result template — the only form that can trip the cap.
    ex.execute(
        r#"DEFINE TEMPLATE legacy AS "{{#each grains}}{{object}}
{{/each}}""#,
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 205 FORMAT preset "legacy""#,
            &facade,
        )
        .unwrap();

    let text = match &out.result {
        CalResultPayload::Formatted { text, .. } => text.clone(),
        other => panic!("expected Formatted, got: {other:?}"),
    };
    assert_eq!(text.lines().count(), 200, "cap should hold");
    assert!(
        out.warnings.iter().any(|w| w.starts_with("CAL-W011")),
        "truncation must be announced, got warnings: {:?}",
        out.warnings
    );

    // A sectioned template has the engine drive iteration, so it renders
    // everything and must NOT warn.
    ex.execute(
        "DEFINE TEMPLATE sectioned\nELEMENT {\n{{grain.object}}\n}",
        &facade,
    )
    .unwrap();
    let out2 = ex
        .execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 205 FORMAT TEMPLATE sectioned"#,
            &facade,
        )
        .unwrap();
    let text2 = match &out2.result {
        CalResultPayload::Formatted { text, .. } => text.clone(),
        other => panic!("expected Formatted, got: {other:?}"),
    };
    assert_eq!(text2.lines().count(), 205, "sectioned render is not capped");
    assert!(
        !out2.warnings.iter().any(|w| w.starts_with("CAL-W011")),
        "sectioned render must not warn: {:?}",
        out2.warnings
    );
}

// ---------------------------------------------------------------------------
// CONTRADICTIONS — fork-aware recall
// ---------------------------------------------------------------------------

/// Build a store holding a real open fork: two writers supersede the same head
/// and their histories are synced, so both tips stay live.
///
/// `created_at` is fixed so the provisional-head election (max `created_at`,
/// then hash) is deterministic — the same reason `fork_merge_tests` pins it.
fn setup_forked() -> (CalExecutor, AreevFacade, TempDir) {
    use areev_core::types::{Fact, Grain};

    let dir = TempDir::new().unwrap();
    let pa = dir.path().join("a.db");
    let pb = dir.path().join("b.db");
    let bundle_v1 = dir.path().join("v1.mgb");
    let bundle_b = dir.path().join("b.mgb");

    let fact = |s: &str, r: &str, o: &str| {
        let mut f = Fact::new(s, r, o).confidence(0.9);
        f.common.namespace = Some("caller".to_string());
        f
    };

    let mut a = Areev::open(pa.to_str().unwrap()).unwrap();
    // An uncontested fact, to prove CONTRADICTIONS filters rather than
    // returning everything.
    a.add(&fact("john", "city", "berlin")).unwrap();
    let v1 = a.add(&fact("john", "plan", "basic")).unwrap();
    let st = a.bundle_since(0, bundle_v1.to_str().unwrap()).unwrap();

    let mut b = Areev::open(pb.to_str().unwrap()).unwrap();
    b.import_bundle(bundle_v1.to_str().unwrap()).unwrap();

    let mut v2a = fact("john", "plan", "pro");
    v2a.common.created_at = Some(1_800_000_000_000);
    a.supersede(&v1, &mut v2a).unwrap();
    let mut v2b = fact("john", "plan", "enterprise");
    v2b.common.created_at = Some(1_800_000_000_500);
    b.supersede(&v1, &mut v2b).unwrap();

    b.bundle_since(st.last_op_seq, bundle_b.to_str().unwrap())
        .unwrap();
    a.import_bundle(bundle_b.to_str().unwrap()).unwrap();
    assert_eq!(a.heads("caller", "john", "plan").unwrap().len(), 2);

    let facade = AreevFacade::with_session(a, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn grains_of(payload: CalResultPayload) -> Vec<serde_json::Value> {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains
            .iter()
            .map(|g| serde_json::to_value(g).unwrap())
            .collect(),
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn contradictions_returns_only_contested_grains() {
    let (ex, facade, _d) = setup_forked();

    // Plain recall answers with the uncontested fact too, and says nothing
    // about the disputed one — this is the blind spot CONTRADICTIONS closes.
    let plain = grains_of(ex.execute(r#"RECALL facts"#, &facade).unwrap().result);
    let objects: Vec<&str> = plain
        .iter()
        .filter_map(|g| g["fields"]["object"].as_str())
        .collect();
    assert!(objects.contains(&"berlin"), "plain recall: {objects:?}");
    assert!(
        plain.iter().all(|g| g["contested_by"].is_null()),
        "plain recall must not stamp fork status"
    );

    let contested = grains_of(
        ex.execute(r#"RECALL facts CONTRADICTIONS"#, &facade)
            .unwrap()
            .result,
    );
    let objects: Vec<&str> = contested
        .iter()
        .filter_map(|g| g["fields"]["object"].as_str())
        .collect();
    assert_eq!(
        contested.len(),
        2,
        "both tips of the fork, nothing else: {objects:?}"
    );
    assert!(
        objects.contains(&"pro") && objects.contains(&"enterprise"),
        "both disputed values surface: {objects:?}"
    );
    assert!(
        !objects.contains(&"berlin"),
        "uncontested fact must be filtered out: {objects:?}"
    );

    // Each tip names the other, so an agent can see *what* it disagrees with
    // rather than just that two rows exist.
    for g in &contested {
        let peers = g["contested_by"].as_array().expect("contested_by present");
        assert_eq!(peers.len(), 1, "one peer tip each");
        let self_hash = g["hash"].as_str().unwrap();
        assert_ne!(peers[0].as_str().unwrap(), self_hash, "peer is the other tip");
    }
}

#[test]
fn contradictions_composes_with_other_clauses() {
    let (ex, facade, _d) = setup_forked();

    // WHERE narrows first; CONTRADICTIONS filters what survives.
    let hits = grains_of(
        ex.execute(
            r#"RECALL facts WHERE subject = "john" AND relation = "plan" CONTRADICTIONS"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert_eq!(hits.len(), 2);

    // A key with no fork yields nothing — the clause reports absence of
    // conflict, it does not fall back to returning the value.
    let hits = grains_of(
        ex.execute(
            r#"RECALL facts WHERE subject = "john" AND relation = "city" CONTRADICTIONS"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert!(hits.is_empty(), "uncontested key: {hits:?}");
}

#[test]
fn contradictions_of_subquery_scopes_to_its_keys() {
    let (ex, facade, _d) = setup_forked();

    // Sub-query selects the contested key → the fork is in scope.
    let hits = grains_of(
        ex.execute(
            r#"RECALL facts CONTRADICTIONS OF (RECALL facts WHERE subject = "john" AND relation = "plan")"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert_eq!(hits.len(), 2, "fork is inside the sub-query's keys");

    // Sub-query selects only an uncontested key → the fork is out of scope,
    // even though it is still open.
    let hits = grains_of(
        ex.execute(
            r#"RECALL facts CONTRADICTIONS OF (RECALL facts WHERE subject = "john" AND relation = "city")"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert!(hits.is_empty(), "fork scoped out: {hits:?}");
}

#[test]
fn with_contradiction_detection_annotates_without_filtering() {
    let (ex, facade, _d) = setup_forked();

    let hits = grains_of(
        ex.execute(r#"RECALL facts WITH contradiction_detection"#, &facade)
            .unwrap()
            .result,
    );

    let uncontested: Vec<_> = hits
        .iter()
        .filter(|g| g["fields"]["object"] == "berlin")
        .collect();
    assert_eq!(uncontested.len(), 1, "uncontested grain is still returned");
    assert!(
        uncontested[0]["contested_by"].is_null(),
        "settled grain carries no fork marker"
    );

    let disputed: Vec<_> = hits
        .iter()
        .filter(|g| !g["contested_by"].is_null())
        .collect();
    assert_eq!(disputed.len(), 2, "both tips marked: {hits:?}");
}

#[test]
fn contradictions_on_a_store_with_no_forks_is_empty_not_an_error() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "t""#,
        &facade,
    )
    .unwrap();

    let out = ex.execute(r#"RECALL facts CONTRADICTIONS"#, &facade).unwrap();
    assert!(grains_of(out.result).is_empty(), "no forks → no contradictions");
}

/// Regression: `CONTRADICTIONS` filters *after* recall, so whatever LIMIT
/// bounded the recall used to bound which forks could be seen. With the default
/// limit that made "nothing is contested" mean "nothing among the newest 50" —
/// a false all-clear from the one clause an agent is meant to trust.
///
/// Burying the fork under more than `default_limit` newer, uncontested facts
/// reproduces it: before the fix the scan stopped at 50 and the query answered
/// "nothing is contested" about a memory that plainly was.
#[test]
fn contradictions_are_not_hidden_by_the_default_limit() {
    use areev_core::types::{Fact, Grain};
    let (ex, facade, _d) = setup_forked();

    // Newer than the fork tips (created_at 1_800_000_000_xxx), so every one of
    // these sorts ahead of them and fills the default 50-grain window.
    facade
        .with_store(|m| {
            for i in 0..60u64 {
                let mut f = Fact::new(&format!("filler{i}"), "note", "irrelevant").confidence(0.9);
                f.common.namespace = Some("caller".to_string());
                f.common.created_at = Some(1_900_000_000_000 + i as i64);
                m.add(&f)?;
            }
            Ok::<_, areev_core::error::AreevError>(())
        })
        .unwrap();

    // The fillers really do bury the fork: an executor that cannot scan past
    // 50 finds nothing — which is exactly the old behaviour, and why the
    // assertion below is worth making.
    let narrow = CalExecutor::new(CalExecutorConfig {
        max_limit: 50,
        default_limit: 50,
        ..CalExecutorConfig::default()
    });
    let out = narrow.execute(r#"RECALL facts CONTRADICTIONS"#, &facade).unwrap();
    assert!(
        grains_of(out.result).is_empty() && out.warnings.iter().any(|w| w.starts_with("CAL-W012")),
        "the fixture must actually bury the fork, and say so when it cannot see past it"
    );

    let found = grains_of(ex.execute(r#"RECALL facts CONTRADICTIONS"#, &facade).unwrap().result);
    assert_eq!(
        found.len(),
        2,
        "both tips must be found however deep they sit: {found:?}"
    );
}

/// `LIMIT` bounds the answer, not the search for it: `CONTRADICTIONS LIMIT 1`
/// means "one contested grain", never "contested among the first grain".
#[test]
fn contradictions_limit_bounds_the_contested_grains_not_the_scan() {
    let (ex, facade, _d) = setup_forked();

    let one = grains_of(
        ex.execute(r#"RECALL facts CONTRADICTIONS LIMIT 1"#, &facade)
            .unwrap()
            .result,
    );
    assert_eq!(one.len(), 1, "LIMIT 1 must yield one contested grain: {one:?}");
    assert!(
        one[0]["contested_by"].as_array().is_some_and(|a| !a.is_empty()),
        "the returned grain must itself be contested: {one:?}"
    );
}

/// An answer that only covers part of the memory must say so. `CONTRADICTIONS`
/// is the one clause whose *empty* result an agent may act on, so a scan cut
/// short by `max_limit` is announced as `CAL-W012` rather than passing for a
/// complete all-clear.
#[test]
fn a_bounded_contradiction_scan_is_announced() {
    let (_ex, facade, _d) = setup_forked();
    let ex = CalExecutor::new(CalExecutorConfig {
        max_limit: 1,
        default_limit: 1,
        ..CalExecutorConfig::default()
    });

    let out = ex.execute(r#"RECALL facts CONTRADICTIONS"#, &facade).unwrap();
    assert!(
        out.warnings.iter().any(|w| w.starts_with("CAL-W012")),
        "a truncated scan must not read as a clean all-clear, got: {:?}",
        out.warnings
    );

    // The unbounded case stays quiet — the warning has to mean something.
    let (ex, facade, _d) = setup_forked();
    let out = ex.execute(r#"RECALL facts CONTRADICTIONS"#, &facade).unwrap();
    assert!(
        !out.warnings.iter().any(|w| w.starts_with("CAL-W012")),
        "a complete scan must not warn, got: {:?}",
        out.warnings
    );
}

/// A fork is keyed `(namespace, subject, relation)`. Scoping on
/// `(subject, relation)` alone let a fork in one namespace be reported as in
/// scope because an unrelated grain in another namespace shared the pair.
#[test]
fn contradictions_scope_does_not_leak_across_namespaces() {
    let (ex, facade, _d) = setup_forked();

    // Same subject and relation as the fork, but a different namespace, and
    // uncontested. Selecting it must not pull the `caller` fork into scope.
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "plan" SET object = "none" SET namespace = "other" REASON "t""#,
        &facade,
    )
    .unwrap();

    let scoped = grains_of(
        ex.execute(
            r#"RECALL facts CONTRADICTIONS OF (RECALL facts WHERE subject = "john" AND relation = "plan" AND namespace = "other")"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert!(
        scoped.is_empty(),
        "a fork in `caller` is not in scope for a sub-query that selected `other`: {scoped:?}"
    );
}

#[test]
fn merging_a_fork_clears_the_contradiction() {
    use areev_core::types::{Fact, Grain};
    let (ex, facade, _d) = setup_forked();

    assert_eq!(
        grains_of(ex.execute(r#"RECALL facts CONTRADICTIONS"#, &facade).unwrap().result).len(),
        2
    );

    // Resolve it the way an operator (or an agent with write approval) would.
    let mut merged = Fact::new("john", "plan", "enterprise (migrated from pro)").confidence(0.95);
    merged.common.namespace = Some("caller".to_string());
    facade
        .with_store(|m| m.merge_heads("caller", "john", "plan", &mut merged))
        .unwrap();

    let after = grains_of(ex.execute(r#"RECALL facts CONTRADICTIONS"#, &facade).unwrap().result);
    assert!(
        after.is_empty(),
        "merge closes the fork, so nothing is contested: {after:?}"
    );
}

/// A `PRIORITY` clause that names some sources gives the unnamed ones weight
/// 0.0 — a deliberate "this source gets nothing", not a missing entry. Reading
/// a 0 allocation as "absent" and substituting an even share inverts the clause.
///
/// The zero-weight source has to be the one processed *first*: budget is
/// consumed in source order, so if the funded source runs first it drains
/// `remaining_budget` and the wrong fallback happens to land on ~0 anyway.
#[test]
fn a_zero_priority_source_gets_no_budget() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for i in 0..8 {
        ex.execute(
            &format!(r#"ADD fact SET subject = "alice" SET relation = "r{i}" SET object = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa{i}" REASON "seed""#),
            &facade,
        ).unwrap();
        ex.execute(
            &format!(r#"ADD goal SET subject = "alice" SET object = "gggggggggggggggggggggggggggggg{i}" REASON "seed""#),
            &facade,
        ).unwrap();
    }

    // `f` is listed first but PRIORITY funds only `g`, so `f` is the 0-weight
    // source and is reached while the whole budget is still unspent.
    let out = ex
        .execute(
            r#"ASSEMBLE brief FROM f: (RECALL facts WHERE subject = "alice"), g: (RECALL goals RECENT 8) BUDGET 400 tokens PRIORITY g: 1.0"#,
            &facade,
        )
        .expect("assemble");

    let unfunded = match out.result {
        CalResultPayload::Assembled { ref sources, .. } => {
            sources.iter().find(|s| s.label == "f").expect("f source").grain_count
        }
        other => panic!("expected Assembled, got: {other:?}"),
    };
    // The budget trim always keeps at least one grain, so 1 is the floor an
    // unfunded source can contribute — more than that means it was funded.
    assert_eq!(
        unfunded, 1,
        "a 0-weight source must not be funded, got {unfunded} grains"
    );
}

/// A `WHERE` with no subject and no free-text query falls to a recent-by-type
/// scan, which takes no structural predicates — so `relation` was dropped on
/// the floor and the query answered with every grain of that type. Returning
/// more than was asked for is worse than returning nothing: the caller has no
/// way to tell the filter never ran.
///
/// The same scan reads the grains table straight through, and supersession is
/// index-layer state, so a stale version came back alongside the head that
/// replaced it — both presented as current.
#[test]
fn an_unanchored_where_applies_its_filters_and_serves_heads() {
    use areev_core::types::{Fact, Grain};
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    // Two relations, each a superseded root plus the head that replaced it.
    facade
        .with_store(|m| {
            for k in ["plan", "city"] {
                let mut root = Fact::new("john", k, "base").confidence(0.9);
                root.common.namespace = Some("caller".to_string());
                let root_hash = m.add(&root)?;
                let mut v2 = Fact::new("john", k, "current").confidence(0.9);
                v2.common.namespace = Some("caller".to_string());
                m.supersede(&root_hash, &mut v2)?;
            }
            Ok::<_, areev_core::error::AreevError>(())
        })
        .unwrap();

    let count = |q: &str| match ex.execute(q, &facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => grains.len(),
        other => panic!("expected Grains, got: {other:?}"),
    };

    assert_eq!(count(r#"RECALL facts WHERE relation = "plan""#), 1, "relation must filter");
    assert_eq!(count(r#"RECALL facts WHERE relation = "city""#), 1, "relation must filter");
    assert_eq!(count(r#"RECALL facts WHERE relation = "nope""#), 0, "no match is an answer");
    // No relation filter at all: still heads only, not every version ever written.
    assert_eq!(count(r#"RECALL facts RECENT 10"#), 2, "recall serves heads");
    // `WITH superseded` opts the history back in.
    assert_eq!(
        count(r#"RECALL facts WHERE relation = "plan" WITH superseded"#),
        2,
        "WITH superseded must widen the scan back to the whole chain"
    );
    // The anchored leg is unchanged.
    assert_eq!(count(r#"RECALL facts WHERE subject = "john" AND relation = "plan""#), 1);
}

/// `WITH superseded` used to be a silent no-op on every *anchored* recall: the
/// executor set the flag, but the structural leg filtered `cur=1`, BM25 dropped
/// non-live postings after scoring, and the vector leg joined on `svt IS NULL`.
/// So the same option meant two different things depending on whether the query
/// happened to carry a subject — the least discoverable failure shape there is.
#[test]
fn with_superseded_widens_the_anchored_legs_and_labels_the_history() {
    use areev_core::types::{Fact, Grain};
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    facade
        .with_store(|m| {
            let mut root = Fact::new("kim", "prefers", "tea leaves").confidence(0.9);
            root.common.namespace = Some("caller".to_string());
            let h1 = m.add(&root)?;
            let mut v2 = Fact::new("kim", "prefers", "coffee beans").confidence(0.9);
            v2.common.namespace = Some("caller".to_string());
            m.supersede(&h1, &mut v2)?;
            Ok::<_, areev_core::error::AreevError>(())
        })
        .unwrap();

    let grains = |q: &str| match ex.execute(q, &facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => grains,
        other => panic!("expected Grains, got: {other:?}"),
    };

    // Structural leg (WHERE subject).
    assert_eq!(grains(r#"RECALL facts WHERE subject = "kim""#).len(), 1, "default is heads-only");
    let widened = grains(r#"RECALL facts WHERE subject = "kim" WITH superseded"#);
    assert_eq!(widened.len(), 2, "the anchored leg must widen to the chain");

    // Every stale version comes back labeled. Handing a model an outdated value
    // that reads as current would be a worse answer than omitting it.
    let stale: Vec<_> = widened
        .iter()
        .filter(|g| g.fields.get("superseded_by").is_some_and(|v| v.is_string()))
        .collect();
    assert_eq!(stale.len(), 1, "exactly the superseded version carries a label");
    assert_eq!(stale[0].fields["object"].as_str(), Some("tea leaves"));

    // BM25 leg (ABOUT): "tea" survives only in history, so heads-only recall
    // cannot find that word at all.
    assert_eq!(grains(r#"RECALL facts ABOUT "tea""#).len(), 0);
    assert_eq!(grains(r#"RECALL facts ABOUT "tea" WITH superseded"#).len(), 1);
}
/// The exact path `record_tool_call` takes on both bindings (#40).
///
/// `created_at` has millisecond resolution, so two identical calls in the same
/// millisecond are byte-identical grains — one content address, and the second
/// insert used to raise `STO-E001: UNIQUE constraint failed: grains.hash`. It
/// is timing-dependent, so it passed on slow runs and failed on warm ones, and
/// an agent retrying a failing tool is exactly the workload that hits it.
#[test]
fn re_recording_an_identical_tool_call_is_idempotent() {
    use areev_cal::facade::CalStoreFacade;
    let (_ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "tool_name": "crm_lookup",
        "content": "timeout after 30s",
        "is_error": true,
        "namespace": "caller",
        "session_id": "ep-1",
        // Pinned, so the same-millisecond case is deterministic rather than a
        // race the test has to win.
        "created_at": 1_700_000_000_000i64,
    })
    .as_object()
    .unwrap()
    .clone();

    let first = facade.cal_add("tool", &fields).unwrap();
    for attempt in 0..5 {
        let again = facade
            .cal_add("tool", &fields)
            .unwrap_or_else(|e| panic!("attempt {attempt} raised {e}"));
        assert_eq!(first, again, "the same content is the same address");
    }
    assert_eq!(facade.count().unwrap(), 1, "stored exactly once");
}

/// docs/cal-reference §3.1 claimed *every* `RECALL` needs a subject filter or a
/// free-text query. It doesn't — the rule only binds untyped recalls, and the
/// doc's own `RECALL facts WHERE namespace = "caller" | COUNT` example relies
/// on that. #48 was someone hardening a deployment on the stronger reading.
/// Pin what the engine actually does so prose and behavior cannot drift again.
#[test]
fn the_anchoring_rule_binds_untyped_recalls_only() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "prefers" SET object = "tea" SET namespace = "caller" REASON "seed""#,
        &facade,
    )
    .unwrap();

    let count = |q: &str| -> usize {
        match ex.execute(q, &facade).unwrap().result {
            CalResultPayload::Grains { grains, .. } => grains.len(),
            CalResultPayload::Count { count, .. } => count,
            other => panic!("expected grains/count, got: {other:?}"),
        }
    };

    // Typed: unanchored is legal, bounded by default_limit rather than a filter.
    for q in [
        "RECALL facts",
        "RECALL facts RECENT 5",
        "RECALL facts LIMIT 10",
        r#"RECALL facts WHERE namespace = "caller""#,
    ] {
        assert_eq!(count(q), 1, "typed unanchored recall `{q}` should run");
    }
    // The doc's own copy-pasteable example.
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "caller" | COUNT"#),
        1
    );

    // Untyped: rejected, because `*` has no type to bound the scan with.
    for q in ["RECALL *", "RECALL grains", "RECALL all"] {
        assert!(
            ex.execute(q, &facade).is_err(),
            "untyped unanchored recall `{q}` must be rejected"
        );
    }
}

// ── Fields that were set and then silently dropped (#36) ────────────────────

/// `valid_to` (and its three siblings) are first-class common fields, but no
/// builder arm claimed them, so they were swept into `common.context` — which
/// compacts its keys on write, turning `valid_to` into `{"context": {"vt": …}}`.
/// Nothing reads that: `loop.staleness` looks for a top-level `valid_to`, and
/// so does the store's world-time projection. A bindings user setting expiry
/// the documented way got a fact that silently never expires.
#[test]
fn temporal_validity_bounds_land_in_the_typed_field_exactly_once() {
    use areev_cal::facade::CalStoreFacade;
    let (_ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "subject": "promo",
        "relation": "code",
        "object": "SAVE20",
        "namespace": "caller",
        "valid_from": 1_500_000_000_000i64,
        "valid_to": 1_600_000_000_000i64,
        "system_valid_from": 1_500_000_000_001i64,
    })
    .as_object()
    .unwrap()
    .clone();
    let hash = facade.cal_add("fact", &fields).unwrap();

    let g = facade.get(&hash).unwrap();
    assert_eq!(g.get_i64("valid_to"), Some(1_600_000_000_000));
    assert_eq!(g.get_i64("valid_from"), Some(1_500_000_000_000));
    assert_eq!(g.get_i64("system_valid_from"), Some(1_500_000_000_001));

    // Exactly once: not also inside `context` under a compacted key, which is
    // where it used to land and where nothing could read it.
    let json = serde_json::to_value(&g.fields).unwrap();
    let ctx = json.get("context").cloned().unwrap_or(serde_json::Value::Null);
    for compact in ["vt", "vf", "svf", "svt"] {
        assert!(
            ctx.get(compact).is_none(),
            "`{compact}` must not also be stored in context: {ctx}"
        );
    }
}

/// Every number in the CAL AST is an f64, and `serde_json::Number::from_f64`
/// always produces a JSON *float* — so `as_i64()` returned None for all of
/// them and every i64-typed field set from CAL text was discarded on the way
/// to the grain builder. Found while checking the above through the CAL
/// surface: `SET created_at = …` had no effect at all.
#[test]
fn integer_literals_set_from_cal_reach_the_grain() {
    let (ex, facade, _d) = setup();
    use areev_cal::facade::CalStoreFacade;

    let payload = ex
        .execute(
            r#"ADD fact SET subject = "promo" SET relation = "code" SET object = "SAVE20"
               SET namespace = "caller" SET created_at = 1700000000000
               SET valid_to = 1600000000000 SET confidence = 0.75 BECAUSE "seed""#,
            &facade,
        )
        .unwrap();
    let hash = areev_core::error::Hash::from_hex(&added_hash(&payload.result)).unwrap();

    let g = facade.get(&hash).unwrap();
    assert_eq!(g.get_i64("created_at"), Some(1_700_000_000_000));
    assert_eq!(g.get_i64("valid_to"), Some(1_600_000_000_000));
    // Fractional values keep working — this is about integral literals only.
    assert_eq!(g.get_f64("confidence"), Some(0.75));
}

// ── WHERE ... IN, and the WITH options that used to do nothing (#47, #53) ──
//
// Every assertion below checks that a clause **excludes** something. A test
// that only checks "the row I wanted is present" passes against a filter that
// is ignored entirely, which is exactly how `IN` shipped broken: the executor
// set `RecallParams::subject_in` and nothing downstream ever read it.

fn seeded() -> (CalExecutor, AreevFacade, TempDir) {
    let (ex, facade, d) = setup();
    let add = |s: &str, r: &str, o: &str| {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "{s}" SET relation = "{r}" SET object = "{o}" SET namespace = "caller" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    };
    add("john", "prefers", "tea");
    add("jane", "prefers", "coffee");
    add("bob", "prefers", "water");
    add("jane", "knows", "john");
    (ex, facade, d)
}

fn subjects(ex: &CalExecutor, facade: &AreevFacade, q: &str) -> Vec<String> {
    match ex.execute(q, facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => {
            let mut s: Vec<String> = grains
                .iter()
                .filter_map(|g| g.fields.get("subject")?.as_str().map(str::to_string))
                .collect();
            s.sort();
            s
        }
        other => panic!("expected grains, got: {other:?}"),
    }
}

#[test]
fn where_in_actually_filters() {
    let (ex, f, _d) = seeded();

    assert_eq!(
        subjects(&ex, &f, r#"RECALL facts WHERE subject IN ("jane") AND relation = "prefers""#),
        vec!["jane"],
        "a one-element IN must exclude john and bob"
    );
    assert_eq!(
        subjects(
            &ex,
            &f,
            r#"RECALL facts WHERE subject IN ("jane", "bob") AND relation = "prefers""#
        ),
        vec!["bob", "jane"],
    );
    // relation/object IN exclude too.
    assert_eq!(
        subjects(&ex, &f, r#"RECALL facts WHERE subject = "jane" AND relation IN ("knows")"#),
        vec!["jane"]
    );
    assert!(
        subjects(&ex, &f, r#"RECALL facts WHERE subject = "jane" AND object IN ("nothing")"#)
            .is_empty()
    );
}

/// The sharp edge: an empty binding is the *natural* "no friends yet" outcome,
/// and treating an empty filter as no filter returned the whole table into a
/// model's context.
#[test]
fn an_empty_in_selects_nothing_not_everything() {
    let (ex, f, _d) = seeded();
    assert!(
        subjects(
            &ex,
            &f,
            r#"LET $none = SUBJECTS OF (RECALL facts WHERE relation = "nobody_has_this");
               RECALL facts WHERE subject IN $none AND relation = "prefers""#
        )
        .is_empty(),
        "an empty LET binding must scope to nothing"
    );
}

/// The documented two-step pattern from §7. The LET scope was evaluated and
/// then dropped — the executor comment called WHERE resolution "Phase 3" — so
/// "friends-of-john's preferences" answered with everybody's.
#[test]
fn the_documented_let_pattern_scopes_the_recall() {
    let (ex, f, _d) = seeded();
    assert_eq!(
        subjects(
            &ex,
            &f,
            r#"LET $friends = SUBJECTS OF (RECALL facts WHERE relation = "knows" AND object = "john");
               RECALL facts WHERE subject IN $friends AND relation = "prefers""#
        ),
        vec!["jane"],
        "only john's friends, not the whole table"
    );
}

/// Silently scoping to nothing is as wrong as silently scoping to everything;
/// the caller can only tell the difference if we say so.
#[test]
fn an_undefined_variable_is_an_error() {
    let (ex, f, _d) = seeded();
    let err = ex
        .execute(r#"RECALL facts WHERE subject IN $never_defined"#, &f)
        .expect_err("an unbound parameter must not be silently dropped");
    assert!(err.to_string().contains("never_defined"), "got: {err}");
}

/// `hash` is on the envelope, not in `fields`. `WHERE hash = <real address>`
/// used to match nothing structurally and filter nothing afterwards, so it
/// returned every grain; `hash IN` took the other branch and returned none.
#[test]
fn where_hash_resolves_the_envelope_in_both_directions() {
    let (ex, f, _d) = seeded();
    let all = match ex
        .execute(r#"RECALL facts WHERE subject = "john""#, &f)
        .unwrap()
        .result
    {
        CalResultPayload::Grains { grains, .. } => grains,
        other => panic!("{other:?}"),
    };
    let h = all[0].hash.clone();

    for q in [
        format!(r#"RECALL facts WHERE hash = "{h}""#),
        format!(r#"RECALL facts WHERE hash IN ("{h}")"#),
        // `sha256:` is how the rest of CAL spells an address.
        format!(r#"RECALL facts WHERE hash = "sha256:{h}""#),
    ] {
        match ex.execute(&q, &f).unwrap().result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1, "`{q}` should select exactly one grain");
                assert_eq!(grains[0].hash, h);
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(
        subjects(&ex, &f, &format!(r#"RECALL facts WHERE hash = "{}""#, "0".repeat(64)))
            .is_empty(),
        "an address nothing has must select nothing"
    );
}

/// §5 promises `conflict_resolution` keeps only the newest grain per
/// (subject, relation). It was implemented on the ASSEMBLE post-merge path
/// only, which a RECALL payload never reaches.
#[test]
fn with_conflict_resolution_keeps_one_grain_per_key() {
    let (ex, facade, _d) = setup();
    for (rel, obj) in [("prefers", "window seat"), ("prefers", "tea"), ("allergic_to", "peanuts")] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "john" SET relation = "{rel}" SET object = "{obj}" SET namespace = "caller" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }
    let relations = |q: &str| match ex.execute(q, &facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => {
            let mut r: Vec<String> = grains
                .iter()
                .filter_map(|g| g.fields.get("relation")?.as_str().map(str::to_string))
                .collect();
            r.sort();
            r
        }
        other => panic!("{other:?}"),
    };

    assert_eq!(
        relations(r#"RECALL facts WHERE subject = "john""#),
        vec!["allergic_to", "prefers", "prefers"],
        "without the option both `prefers` values are served"
    );
    assert_eq!(
        relations(r#"RECALL facts WHERE subject = "john" WITH conflict_resolution"#),
        vec!["allergic_to", "prefers"],
        "with it, one grain per (subject, relation)"
    );
}

/// `dedup(<field>)` was implemented for ASSEMBLE and inert on RECALL, though
/// §5 introduces the whole table with "WITH options tune recall behavior".
#[test]
fn with_dedup_field_applies_to_recall_and_assemble_alike() {
    let (ex, facade, _d) = setup();
    for (rel, obj) in [("prefers", "tea"), ("also_likes", "tea"), ("hates", "coffee")] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "john" SET relation = "{rel}" SET object = "{obj}" SET namespace = "caller" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }
    let count = |q: &str| match ex.execute(q, &facade).unwrap().result {
        CalResultPayload::Grains { grains, .. } => grains.len(),
        CalResultPayload::Assembled { grains, .. } => grains.len(),
        other => panic!("{other:?}"),
    };

    assert_eq!(count(r#"RECALL facts WHERE subject = "john""#), 3);
    assert_eq!(
        count(r#"RECALL facts WHERE subject = "john" WITH dedup(object)"#),
        2,
        "two grains share the object \"tea\""
    );
    // Clause order used to decide whether the option applied at all: the
    // documented order routed it to the ASSEMBLE engine, the other order to the
    // post-merge pass, which ignored the field and did similarity dedup.
    for q in [
        r#"ASSEMBLE "x" FROM p: (RECALL facts WHERE subject = "john") FORMAT json WITH dedup(object)"#,
        r#"ASSEMBLE "x" FROM p: (RECALL facts WHERE subject = "john") WITH dedup(object) FORMAT json"#,
    ] {
        let payload = ex.execute(q, &facade).unwrap().result;
        let n = match payload {
            CalResultPayload::Formatted { grain_count, .. } => grain_count,
            CalResultPayload::Assembled { ref grains, .. } => grains.len(),
            CalResultPayload::Grains { ref grains, .. } => grains.len(),
            ref other => panic!("{other:?}"),
        };
        assert_eq!(n, 2, "`{q}` must dedup on `object` regardless of clause order");
    }
}

/// `annotate_relative_time` and `explanation` returned output byte-identical to
/// the same query without them.
#[test]
fn annotate_and_explain_change_the_payload() {
    let (ex, f, _d) = seeded();
    let grains = |q: &str| match ex.execute(q, &f).unwrap().result {
        CalResultPayload::Grains { grains, .. } => grains,
        other => panic!("{other:?}"),
    };

    let annotated = grains(r#"RECALL facts WHERE subject = "john" WITH annotate_relative_time"#);
    assert!(
        annotated.iter().all(|g| g.relative_time.is_some()),
        "every grain should carry a relative-time label"
    );
    assert!(grains(r#"RECALL facts WHERE subject = "john""#)
        .iter()
        .all(|g| g.relative_time.is_none()));

    let explained = grains(r#"RECALL facts WHERE subject = "john" WITH explanation"#);
    assert!(
        explained
            .iter()
            .all(|g| g.explanation.as_deref().is_some_and(|e| e.contains("john"))),
        "the explanation should name the predicate that matched: {explained:?}"
    );
}

/// §5's own rule: an option that cannot be honoured "returns an honest error
/// rather than silently degrading". `score_breakdown` has shipped for several
/// releases, so a warning is the non-breaking form of honest — the defect was
/// the silence, not the absence.
#[test]
fn an_inert_with_option_says_so() {
    let (ex, f, _d) = seeded();
    let res = ex
        .execute(r#"RECALL facts WHERE subject = "john" WITH score_breakdown"#, &f)
        .unwrap();
    assert!(
        res.warnings.iter().any(|w| w.contains("CAL-W014") && w.contains("score_breakdown")),
        "expected a CAL-W014 warning, got: {:?}",
        res.warnings
    );
}

/// …and the warning has to survive the trip to a caller (#68). Raising it into
/// `CalExecResult::warnings` was only half the channel: Python, Node and the
/// MCP `areev_cal` tool all serialized `result` alone, so every `CAL-Wnnn`
/// was dropped at the boundary and `score_breakdown` stayed exactly as silent
/// as before the warning existed. `payload_json` is what those three return.
#[test]
fn warnings_survive_into_the_wire_payload() {
    let (ex, f, _d) = seeded();
    let res = ex
        .execute(r#"RECALL facts WHERE subject = "john" WITH score_breakdown"#, &f)
        .unwrap();
    let payload = res.payload_json().unwrap();
    let warnings = payload["warnings"]
        .as_array()
        .expect("payload must carry a warnings array");
    assert!(
        warnings.iter().any(|w| w.as_str().is_some_and(|s| s.contains("CAL-W014"))),
        "expected CAL-W014 in the payload, got: {payload}"
    );
    // The payload a caller already parses is unchanged otherwise.
    assert_eq!(payload["type"], "grains");

    // A clean query keeps the exact shape it had before — `warnings` is added
    // only when there is something to say.
    let clean = ex
        .execute(r#"RECALL facts WHERE subject = "john""#, &f)
        .unwrap();
    assert!(
        clean.payload_json().unwrap().get("warnings").is_none(),
        "no warnings means no key, not an empty array"
    );
}
