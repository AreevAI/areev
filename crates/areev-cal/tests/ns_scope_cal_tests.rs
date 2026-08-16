//! Namespace scoping end to end through CAL: `WHERE namespace = "org.*"`
//! prefix scopes, the `namespace IN (…)` set (issue #19 — every member must
//! be queried, not just the first), mount routing with patterns, the
//! fail-closed authorization sweep over an expansion, the pinned-session
//! escape hatch, and the exact-namespace refusals on writes / destruction /
//! grants.
//!
//! Filter tests assert what a scope EXCLUDES — a dropped scope fails open
//! into an over-broad answer straight into a model's context.

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

fn add_in(ex: &CalExecutor, f: &AreevFacade, ns: &str, subject: &str, object: &str) {
    let cal = format!(
        r#"ADD fact SET subject = "{subject}" SET relation = "prefers" SET object = "{object}" SET namespace = "{ns}" REASON "seed""#
    );
    let out = ex.execute(&cal, f).unwrap();
    assert!(
        matches!(out.result, CalResultPayload::Added { .. }),
        "seed add in {ns} failed: {:?}",
        out.result
    );
}

/// Seed the standard tree: an org hierarchy, the lookalike traps, and an
/// unrelated namespace.
fn seed(ex: &CalExecutor, f: &AreevFacade) {
    for (ns, o) in [
        ("org", "root-value"),
        ("org.sales", "sales-value"),
        ("org.sales.emea", "emea-value"),
        ("org.ops", "ops-value"),
        ("organization", "lookalike-value"),
        ("personal", "personal-value"),
    ] {
        add_in(ex, f, ns, "john", o);
    }
}

fn objects(payload: &CalResultPayload) -> Vec<String> {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains
            .iter()
            .map(|g| {
                serde_json::to_value(g).unwrap()["fields"]["object"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
            .collect(),
        other => panic!("expected Grains, got {other:?}"),
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// WHERE namespace = "org.*"
// ---------------------------------------------------------------------------

#[test]
fn where_namespace_prefix_selects_the_tree_and_nothing_else() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);

    let out = ex
        .execute(r#"RECALL facts WHERE namespace = "org.*" AND subject = "john""#, &f)
        .unwrap();
    let objs = sorted(objects(&out.result));
    assert_eq!(
        objs,
        vec!["emea-value", "ops-value", "root-value", "sales-value"],
        "org.* = org + descendants, exactly"
    );
}

#[test]
fn where_namespace_prefix_scopes_free_text_recall() {
    let (ex, f, _d) = setup();
    for (ns, o) in [
        ("org.sales", "zebra forecast q3"),
        ("org.ops", "zebra fleet maintenance"),
        ("personal", "zebra photo album"),
    ] {
        add_in(&ex, &f, ns, &format!("note-{ns}"), o);
    }
    let out = ex
        .execute(r#"RECALL facts LIKE "zebra" WHERE namespace = "org.*""#, &f)
        .unwrap();
    let objs = objects(&out.result);
    assert_eq!(objs.len(), 2, "{objs:?}");
    assert!(!objs.iter().any(|o| o.contains("photo")), "personal grain leaked: {objs:?}");
}

#[test]
fn typed_recent_scan_honors_the_prefix_scope() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    // Unanchored typed recall (the recent-scan path) — must stay scoped.
    let out = ex
        .execute(r#"RECALL facts WHERE namespace = "org.sales.*" RECENT 10"#, &f)
        .unwrap();
    let objs = sorted(objects(&out.result));
    assert_eq!(objs, vec!["emea-value", "sales-value"]);
}

#[test]
fn count_pipeline_over_a_prefix_scope() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    let out = ex
        .execute(r#"RECALL facts WHERE namespace = "org.*" | COUNT"#, &f)
        .unwrap();
    let v = serde_json::to_value(out.payload_json().unwrap()).unwrap();
    assert_eq!(v["count"], 4, "{v}");
}

#[test]
fn malformed_pattern_is_an_error_not_an_empty_result() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    for bad in ["org*", "*", "*.org"] {
        let q = format!(r#"RECALL facts WHERE namespace = "{bad}" AND subject = "john""#);
        let out = ex.execute(&q, &f);
        let failed = match out {
            Err(_) => true,
            Ok(r) => matches!(r.result, CalResultPayload::Unsupported { .. }),
        };
        assert!(failed, "{bad} must refuse, not silently return nothing/everything");
    }
}

// ---------------------------------------------------------------------------
// namespace IN (…) — the #19 fix
// ---------------------------------------------------------------------------

#[test]
fn namespace_in_queries_every_member_not_just_the_first() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    let out = ex
        .execute(
            r#"RECALL facts WHERE namespace IN ("org.sales", "personal") AND subject = "john""#,
            &f,
        )
        .unwrap();
    let objs = sorted(objects(&out.result));
    // Before the fix this returned only the first member's grains.
    assert_eq!(objs, vec!["personal-value", "sales-value"]);
}

#[test]
fn namespace_in_accepts_pattern_members() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    let out = ex
        .execute(
            r#"RECALL facts WHERE namespace IN ("personal", "org.sales.*") AND subject = "john""#,
            &f,
        )
        .unwrap();
    let objs = sorted(objects(&out.result));
    assert_eq!(objs, vec!["emea-value", "personal-value", "sales-value"]);
    assert!(!objs.contains(&"ops-value".to_string()), "org.ops is outside org.sales.*");
}

#[test]
fn empty_namespace_in_selects_nothing() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    // An empty LET binding expanding into IN must scope to NOTHING.
    let out = ex
        .execute(
            r#"LET $none = SUBJECTS OF (RECALL facts WHERE subject = "nobody")
               RECALL facts WHERE namespace IN $none"#,
            &f,
        )
        .map(|r| r.result);
    match out {
        Ok(CalResultPayload::Grains { grains, .. }) => {
            assert!(grains.is_empty(), "empty IN must select nothing")
        }
        Ok(CalResultPayload::Unsupported { .. }) | Err(_) => {}
        Ok(other) => panic!("unexpected payload {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Mounts
// ---------------------------------------------------------------------------

fn setup_with_mount() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let mut facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    // Seed the org replica BEFORE mounting (mounts are read-only through CAL).
    let mut org = Areev::open(dir.path().join("org.db").to_str().unwrap()).unwrap();
    for (ns, o) in [("policies", "pto-policy"), ("policies.hr", "hr-policy"), ("sales", "quota")] {
        let mut fact = areev_core::types::Fact::new("doc", "states", o);
        fact.common.namespace = Some(ns.to_string());
        org.add(&fact).unwrap();
    }
    facade.mount("orgrep", org);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

#[test]
fn prefix_scope_inside_a_mount_expands_against_the_mounted_store() {
    let (ex, f, _d) = setup_with_mount();
    // A same-named namespace in the session store, which must NOT answer.
    add_in(&ex, &f, "policies", "doc", "session-local-policy");

    let out = ex
        .execute(r#"RECALL facts WHERE namespace = "orgrep.policies.*" AND subject = "doc""#, &f)
        .unwrap();
    let objs = sorted(objects(&out.result));
    assert_eq!(objs, vec!["hr-policy", "pto-policy"]);
    assert!(!objs.contains(&"session-local-policy".to_string()));
    assert!(!objs.contains(&"quota".to_string()));
}

#[test]
fn a_namespace_set_spanning_stores_refuses_with_a_pointer_at_assemble() {
    let (ex, f, _d) = setup_with_mount();
    seed(&ex, &f);
    let out = ex.execute(
        r#"RECALL facts WHERE namespace IN ("personal", "orgrep.policies") AND subject = "john""#,
        &f,
    );
    let msg = match out {
        Err(e) => e.to_string(),
        Ok(r) => match r.result {
            CalResultPayload::Unsupported { message, .. } => message,
            other => panic!("a cross-store set must refuse, got {other:?}"),
        },
    };
    assert!(msg.contains("ASSEMBLE"), "the refusal should point at ASSEMBLE: {msg}");
}

// ---------------------------------------------------------------------------
// ASSEMBLE with prefix sources
// ---------------------------------------------------------------------------

#[test]
fn assemble_sources_take_prefix_scopes_and_dedup_overlap() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    // Two sources whose scopes OVERLAP on org.sales.*: dedup must keep one
    // copy of each grain.
    let out = ex
        .execute(
            r#"ASSEMBLE FROM
                 all_org: (RECALL facts WHERE namespace = "org.*" AND subject = "john"),
                 sales:   (RECALL facts WHERE namespace = "org.sales.*" AND subject = "john")
               FORMAT json"#,
            &f,
        )
        .unwrap();
    let v = serde_json::to_value(out.payload_json().unwrap()).unwrap();
    let grains = v["grains"].as_array().expect("json grains");
    let mut objs: Vec<String> = grains
        .iter()
        .map(|g| g["fields"]["object"].as_str().unwrap_or("").to_string())
        .collect();
    objs.sort();
    assert_eq!(
        objs,
        vec!["emea-value", "ops-value", "root-value", "sales-value"],
        "union of the two scopes, deduped — and nothing outside org.*"
    );
}

// ---------------------------------------------------------------------------
// Authorization: fail closed over the expansion
// ---------------------------------------------------------------------------

fn rebind(f: AreevFacade, principal: Option<&str>) -> AreevFacade {
    let store = f.into_inner();
    let fresh = AreevFacade::with_session(store, Some("caller".to_string()), None);
    match principal {
        Some(p) => fresh.with_principal(p).unwrap(),
        None => fresh,
    }
}

#[test]
fn prefix_scope_fails_closed_when_a_covered_namespace_is_missing_a_grant() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    // Grant the bot two of the four org namespaces only.
    for ns in ["org", "org.sales"] {
        // Dotted namespaces are quoted in DCL (the identifier grammar has no `.`).
        let g = format!(r#"GRANT read ON "{ns}" TO "agent:bot" WITH because("test")"#);
        ex.execute(&g, &f).unwrap();
    }
    let f = rebind(f, Some("agent:bot"));
    let out = ex.execute(r#"RECALL facts WHERE namespace = "org.*" AND subject = "john""#, &f);
    let msg = match out {
        Err(e) => e.to_string(),
        Ok(r) => match r.result {
            CalResultPayload::Unsupported { message, .. } => message,
            other => panic!("must refuse, got {other:?}"),
        },
    };
    // Fail closed — and the refusal names the PATTERN the caller typed,
    // never the discovered namespace (multi-tenant disclosure).
    assert!(msg.contains("org.*"), "{msg}");
    assert!(!msg.contains("org.ops"), "refusal must not name the uncovered namespace: {msg}");
    assert!(!msg.contains("org.sales.emea"), "{msg}");

    // The exact namespaces the bot DOES hold still answer.
    let ok = ex
        .execute(r#"RECALL facts WHERE namespace = "org.sales" AND subject = "john""#, &f)
        .unwrap();
    assert_eq!(objects(&ok.result), vec!["sales-value"]);
}

#[test]
fn prefix_scope_passes_when_every_covered_namespace_is_granted() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    for ns in ["org", "org.sales", "org.sales.emea", "org.ops"] {
        let g = format!(r#"GRANT read ON "{ns}" TO "agent:bot" WITH because("test")"#);
        ex.execute(&g, &f).unwrap();
    }
    let f = rebind(f, Some("agent:bot"));
    let out = ex
        .execute(r#"RECALL facts WHERE namespace = "org.*" AND subject = "john""#, &f)
        .unwrap();
    assert_eq!(sorted(objects(&out.result)).len(), 4);
}

#[test]
fn a_star_grant_covers_any_prefix_scope() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    ex.execute(r#"GRANT read ON * TO "agent:bot" WITH because("test")"#, &f).unwrap();
    let f = rebind(f, Some("agent:bot"));
    let out = ex
        .execute(r#"RECALL facts WHERE namespace = "org.*" AND subject = "john""#, &f)
        .unwrap();
    assert_eq!(objects(&out.result).len(), 4);
}

#[test]
fn grants_refuse_prefix_patterns() {
    let (ex, f, _d) = setup();
    let out = ex.execute(r#"GRANT read ON "org.*" TO "agent:bot" WITH because("t")"#, &f);
    let refused = match out {
        Err(_) => true,
        Ok(r) => matches!(r.result, CalResultPayload::Unsupported { .. }),
    };
    assert!(refused, "GRANT ON org.* must refuse — grants are exact or *");
}

// ---------------------------------------------------------------------------
// The pinned session cannot escape via IN or patterns
// ---------------------------------------------------------------------------

#[test]
fn namespace_override_pin_defeats_in_sets_and_patterns() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("tenant_a".to_string()), None);
    let ex_owner = CalExecutor::new(CalExecutorConfig::default());
    add_in(&ex_owner, &f, "tenant_a", "john", "a-value");
    add_in(&ex_owner, &f, "tenant_b", "john", "b-value");

    let pinned = CalExecutor::new(CalExecutorConfig {
        namespace_override: Some("tenant_a".to_string()),
        ..Default::default()
    });
    for escape in [
        r#"RECALL facts WHERE namespace = "tenant_b" AND subject = "john""#,
        r#"RECALL facts WHERE namespace = "tenant_*" AND subject = "john""#,
        r#"RECALL facts WHERE namespace IN ("tenant_a", "tenant_b") AND subject = "john""#,
    ] {
        let out = pinned.execute(escape, &f);
        match out.map(|r| r.result) {
            Ok(CalResultPayload::Grains { grains, .. }) => {
                let objs: Vec<String> = grains
                    .iter()
                    .map(|g| {
                        serde_json::to_value(g).unwrap()["fields"]["object"]
                            .as_str()
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect();
                assert!(
                    !objs.contains(&"b-value".to_string()),
                    "pinned session escaped its namespace via {escape:?}: {objs:?}"
                );
            }
            // A refusal is equally safe.
            Ok(CalResultPayload::Unsupported { .. }) | Err(_) => {}
            Ok(other) => panic!("unexpected payload {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Writes and destruction refuse patterns through CAL
// ---------------------------------------------------------------------------

#[test]
fn add_with_a_pattern_namespace_refuses() {
    let (ex, f, _d) = setup();
    let out = ex.execute(
        r#"ADD fact SET subject = "s" SET relation = "r" SET object = "o" SET namespace = "org.*" REASON "t""#,
        &f,
    );
    let refused = match out {
        Err(_) => true,
        Ok(r) => matches!(r.result, CalResultPayload::Unsupported { .. }),
    };
    assert!(refused, "ADD into a pattern namespace must refuse");
    // And nothing was written anywhere.
    let all = ex.execute(r#"RECALL facts WHERE subject = "s""#, &f).unwrap();
    assert!(objects(&all.result).is_empty());
}

#[test]
fn purge_with_a_pattern_namespace_refuses() {
    let (ex, f, _d) = setup();
    seed(&ex, &f);
    let out = ex.execute(r#"PURGE OLDER THAN 0d IN "org.*" BECAUSE "cleanup""#, &f);
    let refused = match out {
        Err(_) => true,
        Ok(r) => matches!(r.result, CalResultPayload::Unsupported { .. }),
    };
    assert!(refused, "PURGE takes an exact namespace, never a pattern");
    // Nothing was destroyed: the whole tree still answers.
    let still = ex
        .execute(r#"RECALL facts WHERE namespace = "org.*" AND subject = "john""#, &f)
        .unwrap();
    assert_eq!(objects(&still.result).len(), 4);
}

#[test]
fn forget_subject_in_a_pattern_session_refuses() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    // A session deliberately bound to a scope: reads scope, writes/destruction refuse.
    let f = AreevFacade::with_session(m, Some("org.*".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let out = ex.execute(r#"FORGET SUBJECT "john" BECAUSE "erasure test""#, &f);
    let refused = match out {
        Err(_) => true,
        Ok(r) => matches!(r.result, CalResultPayload::Unsupported { .. }),
    };
    assert!(refused, "identity erasure needs an exact namespace");
}

#[test]
fn assemble_sources_cannot_escape_a_pinned_session() {
    // ASSEMBLE sources execute through the same recall path as RECALL, so
    // the namespace_override pin must hold there too — pinned forever here.
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("tenant_a".to_string()), None);
    let ex_owner = CalExecutor::new(CalExecutorConfig::default());
    add_in(&ex_owner, &f, "tenant_a", "john", "a-value");
    add_in(&ex_owner, &f, "tenant_b", "john", "b-value");

    let pinned = CalExecutor::new(CalExecutorConfig {
        namespace_override: Some("tenant_a".to_string()),
        ..Default::default()
    });
    let out = pinned.execute(
        r#"ASSEMBLE FROM
             a: (RECALL facts WHERE namespace = "tenant_b" AND subject = "john"),
             b: (RECALL facts WHERE namespace IN ("tenant_a", "tenant_b") AND subject = "john")
           FORMAT json"#,
        &f,
    );
    match out.map(|r| serde_json::to_value(r.payload_json().unwrap()).unwrap()) {
        Ok(v) => {
            let objs: Vec<String> = v["grains"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|g| g["fields"]["object"].as_str().unwrap_or("").to_string())
                .collect();
            assert!(
                !objs.contains(&"b-value".to_string()),
                "a pinned session escaped via an ASSEMBLE source: {objs:?}"
            );
        }
        Err(_) => {} // a refusal is equally safe
    }
}
