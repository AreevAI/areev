//! CAL 1.3 Wave-2 reads, end to end: the as-of read, the run↔memory join,
//! reverse provenance, the fork listing, and the store introspection reads.

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::{Capture, Areev};
use tempfile::TempDir;

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn payload(ex: &CalExecutor, f: &AreevFacade, q: &str) -> serde_json::Value {
    serde_json::to_value(ex.execute(q, f).unwrap().payload_json().unwrap()).unwrap()
}

#[test]
fn entity_at_walks_the_knowledge_axis() {
    let (ex, f, _dir) = setup();
    let h1 = f.with_store(|m| {
        let h1 = m
            .add(&Fact::new("alice", "employer", "acme").namespace("caller").created_at(1_000))
            .unwrap();
        let mut v2 = Fact::new("alice", "employer", "globex").namespace("caller").created_at(5_000);
        m.supersede(&h1, &mut v2).unwrap();
        h1
    });

    // What did we know at T=2000? The original value.
    let v = payload(&ex, &f, r#"ENTITY "alice" RELATION "employer" AT 2000 AXIS knowledge"#);
    assert_eq!(v["type"], "entity_at", "{v}");
    assert_eq!(v["grain"]["fields"]["object"], "acme", "{v}");
    assert_eq!(v["grain"]["hash"], h1.to_hex());

    // And at T=6000 — the superseding value.
    let v = payload(&ex, &f, r#"ENTITY "alice" RELATION "employer" AT 6000 AXIS knowledge"#);
    assert_eq!(v["grain"]["fields"]["object"], "globex", "{v}");

    // Before anything was known: null, stated.
    let v = payload(&ex, &f, r#"ENTITY "alice" RELATION "employer" AT 500 AXIS knowledge"#);
    assert!(v["grain"].is_null(), "{v}");
}

#[test]
fn the_run_join_crosses_execution_into_memory() {
    let (ex, f, _dir) = setup();
    // A run records an Event; a Fact is distilled from it (derived_from).
    let (event, fact) = f.with_store(|m| {
        let event = m
            .capture(
                "caller",
                "caller asked to switch to the aisle seat",
                &Capture { run_id: Some("run-42"), ..Default::default() },
            )
            .unwrap();
        let mut fact = Fact::new("caller:john", "prefers", "aisle seat")
            .namespace("caller")
            .created_at(2_000);
        fact.common.derived_from = Some(event.to_hex());
        let fact = m.add(&fact).unwrap();
        (event, fact)
    });

    // RUN TRACE: recorded + produced, one statement.
    let v = payload(&ex, &f, r#"RUN TRACE "run-42""#);
    assert_eq!(v["type"], "run_trace", "{v}");
    let recorded = v["trace"]["recorded"].as_array().unwrap();
    assert_eq!(recorded.len(), 1, "{v}");
    assert_eq!(recorded[0]["hash"], event.to_hex());
    let produced = v["trace"]["produced"].as_array().unwrap();
    assert_eq!(produced.len(), 1, "the distilled fact is the yield: {v}");
    assert_eq!(produced[0]["hash"], fact.to_hex());

    // RUNS TOUCHING: from the fact back to the run.
    let v = payload(&ex, &f, &format!("RUNS TOUCHING sha256:{}", fact.to_hex()));
    assert_eq!(v["type"], "runs_touching", "{v}");
    assert_eq!(v["runs"].as_array().unwrap(), &vec![serde_json::json!("run-42")]);

    // DERIVED FROM: the reverse provenance of the event.
    let v = payload(&ex, &f, &format!("DERIVED FROM sha256:{}", event.to_hex()));
    assert_eq!(v["type"], "derived_from", "{v}");
    assert_eq!(v["grains"][0]["hash"], fact.to_hex());
}

#[test]
fn show_forks_and_the_introspection_reads_answer() {
    let (ex, f, _dir) = setup();
    f.with_store(|m| {
        m.add(&Fact::new("a", "b", "c").namespace("caller").created_at(1_000)).unwrap();
    });

    let v = payload(&ex, &f, "SHOW FORKS");
    assert_eq!(v["type"], "forks", "{v}");
    assert_eq!(v["forks"].as_array().unwrap().len(), 0, "no forks on a linear store");

    let v = payload(&ex, &f, "DESCRIBE STATS");
    assert!(v["info"]["grains"].as_u64().unwrap() >= 1, "{v}");

    let v = payload(&ex, &f, "DESCRIBE INTEGRITY");
    assert_eq!(v["info"]["hash_mismatches"], 0, "{v}");
    assert_eq!(v["info"]["undecodable"], 0, "{v}");
}

#[test]
fn wave2_reads_respect_the_read_grants() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    // read on caller only — the cross-namespace reads need `*`.
    m.add(
        &Fact::new("user:narrow", REL_PERMITS, "read ON caller")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    let h = m.add(&Fact::new("a", "b", "c").namespace("caller").created_at(2_000)).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None)
        .with_principal("user:narrow")
        .unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // Namespace-scoped reads work…
    let v = payload(&ex, &f, r#"ENTITY "a" RELATION "b" AT 3000 AXIS knowledge"#);
    assert_eq!(v["grain"]["fields"]["object"], "c", "{v}");
    // …the provenance walk (namespace-spanning) is refused…
    let v = payload(&ex, &f, &format!("DERIVED FROM sha256:{}", h.to_hex()));
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");
    // …and so is DESCRIBE STATS (store-wide).
    let err = ex.execute("DESCRIBE STATS", &f).unwrap_err();
    assert!(err.to_string().contains("CAL-E121"), "{err}");
}
