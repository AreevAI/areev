//! CAL end-to-end over each backend: text → executor → AreevFacade →
//! store. The facade and executor are backend-blind by design (the Db seam
//! sits below `Areev`), so one smoke per backend pins the whole stack —
//! including the destructive-op gate, which must behave identically
//! regardless of where the bytes live.

use areev_cal::executor::CalResultPayload;
use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_conformance::{Backend, TursoBackend};

fn drive(facade: &AreevFacade) {
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let add = ex
        .execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "smoke""#,
            facade,
        )
        .unwrap();
    let hash = match &add.result {
        CalResultPayload::Added { hash, .. } => hash.clone(),
        other => panic!("expected Added, got {other:?}"),
    };
    assert_eq!(hash.len(), 64);

    let recall = ex.execute(r#"RECALL facts WHERE subject = "john""#, facade).unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "rust");
        }
        other => panic!("expected Grains, got {other:?}"),
    }

    drive_validity_window(&ex, facade);
    drive_where_fails_closed(&ex, facade);

    // The destructive gate is identical on every backend: FORGET works when
    // allowed, and a no-destructive executor refuses it.
    let gated = CalExecutor::new(CalExecutorConfig {
        allow_destructive_ops: false,
        ..CalExecutorConfig::default()
    });
    let refused = gated.execute(&format!("FORGET sha256:{hash}"), facade).unwrap();
    assert!(
        matches!(refused.result, CalResultPayload::Unsupported { .. }),
        "FORGET must come back Unsupported with allow_destructive_ops=false, got {:?}",
        refused.result
    );
    let forgotten = ex.execute(&format!("FORGET sha256:{hash}"), facade).unwrap();
    assert!(
        matches!(forgotten.result, CalResultPayload::Forgotten { .. }),
        "expected Forgotten, got {:?}",
        forgotten.result
    );
    let after = ex.execute(r#"RECALL facts WHERE subject = "john""#, facade).unwrap();
    match after.result {
        CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty()),
        other => panic!("expected Grains, got {other:?}"),
    }
}

/// #206 — "what is currently valid" has to be expressible as a query.
///
/// `valid_to` is a `GrainCommon` field on every grain type, so this is a
/// backend-blind property: the value rides inside the immutable blob and is
/// post-filtered by the executor, which means it must answer identically
/// wherever the bytes live.
fn drive_validity_window(ex: &CalExecutor, facade: &AreevFacade) {
    let count = |src: &str| -> usize {
        match ex.execute(src, facade).unwrap().result {
            CalResultPayload::Grains { grains, .. } => grains.len(),
            other => panic!("expected Grains, got {other:?}"),
        }
    };

    // A waiver that lapsed, and a policy with no expiry — the two shapes any
    // memory modelling a temporary exception has.
    ex.execute(
        r#"ADD fact SET subject = "waiver" SET relation = "covers" SET object = "acme"
           SET namespace = "vt" SET valid_to = 1000 REASON "expires""#,
        facade,
    )
    .unwrap();
    ex.execute(
        r#"ADD fact SET subject = "policy" SET relation = "covers" SET object = "everyone"
           SET namespace = "vt" REASON "no expiry""#,
        facade,
    )
    .unwrap();

    assert_eq!(count(r#"RECALL facts WHERE namespace = "vt""#), 2);

    // Past its valid_to: excluded by the predicate…
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "vt" AND (valid_to IS NULL OR valid_to > 5000)"#),
        1,
        "an expired fact must not survive the currently-valid predicate"
    );
    // …and included without it.
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "vt" AND valid_to > 500"#),
        1,
        "the expired fact is still readable when the query asks for it"
    );
    // Sortable, not only filterable.
    assert_eq!(
        count(r#"RECALL facts WHERE namespace = "vt" ORDER BY valid_to DESC"#),
        2
    );
}

/// #207 — a `WHERE` predicate must never widen. A grain that does not carry
/// the field matches neither `= x` nor `!= x`, and the same holds however the
/// negation is spelled.
fn drive_where_fails_closed(ex: &CalExecutor, facade: &AreevFacade) {
    let count = |src: &str| -> usize {
        match ex.execute(src, facade).unwrap().result {
            CalResultPayload::Grains { grains, .. } => grains.len(),
            other => panic!("expected Grains, got {other:?}"),
        }
    };

    ex.execute(
        r#"ADD skill SET name = "alpha" SET description = "active" SET namespace = "fc" REASON "seed""#,
        facade,
    )
    .unwrap();
    ex.execute(
        r#"ADD skill SET name = "beta" SET description = "retired" SET namespace = "fc" REASON "seed""#,
        facade,
    )
    .unwrap();
    assert_eq!(count(r#"RECALL skills WHERE namespace = "fc""#), 2);

    // `object` is a real field name (Fact/Observation/Goal declare it) that
    // means nothing for a Skill. Every negation of it must match nothing —
    // this is the exact query that shipped returning both skills, including
    // the one whose description IS "retired".
    for src in [
        r#"RECALL skills WHERE namespace = "fc" AND object != "retired""#,
        r#"RECALL skills WHERE namespace = "fc" AND NOT object = "retired""#,
        r#"RECALL skills WHERE namespace = "fc" AND object NOT IN ("retired")"#,
    ] {
        assert_eq!(count(src), 0, "a predicate the grain cannot answer widened: {src}");
    }

    // A field the type DOES carry still filters — `description` is required
    // on every Skill and was refused as unqueryable until #207.
    assert_eq!(
        count(r#"RECALL skills WHERE namespace = "fc" AND description != "retired""#),
        1
    );
}

#[test]
fn cal_over_turso() {
    let b = TursoBackend::new();
    let facade = AreevFacade::with_session(b.open(), Some("caller".to_string()), None);
    drive(&facade);
}

#[cfg(feature = "postgres")]
#[test]
fn cal_over_postgres() {
    let url = match std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
    {
        Some(u) => u,
        None => {
            if std::env::var("CI").as_deref() == Ok("true") {
                panic!("CI=true but no DATABASE_URL — the postgres job must not silently skip");
            }
            eprintln!("skipping: no DATABASE_URL/AREEV_PG_URL");
            return;
        }
    };
    let b = areev_conformance::PgBackend::new(&url);
    let facade = AreevFacade::with_session(b.open(), Some("caller".to_string()), None);
    drive(&facade);
}
