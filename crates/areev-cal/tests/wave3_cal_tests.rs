//! CAL 1.3 Wave-3, end to end: MERGE (fork resolution), RELATED (the
//! graph walk), NOVELTY (the paraphrase check), and the honest inert
//! warning for `WITH occurrence`.

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::{Areev, EmbedBackend};
use tempfile::TempDir;

fn payload(ex: &CalExecutor, f: &AreevFacade, q: &str) -> serde_json::Value {
    serde_json::to_value(ex.execute(q, f).unwrap().payload_json().unwrap()).unwrap()
}

/// Build a real fork the way forks actually happen: two replicas supersede
/// the same head divergently, then sync (import UNIONs heads).
fn forked_store(dir: &TempDir) -> Areev {
    let a_path = dir.path().join("a.db");
    let b_path = dir.path().join("b.db");
    let mut a = Areev::open(a_path.to_str().unwrap()).unwrap();
    let base = a
        .add(&Fact::new("acme", "tier", "starter").namespace("caller").created_at(1_000))
        .unwrap();
    let seed = dir.path().join("seed.mgb");
    let st = a.bundle_since(0, seed.to_str().unwrap()).unwrap();
    {
        let mut b = Areev::open(b_path.to_str().unwrap()).unwrap();
        b.import_bundle(seed.to_str().unwrap()).unwrap();
        let mut vb = Fact::new("acme", "tier", "growth").namespace("caller").created_at(2_000);
        b.supersede(&base, &mut vb).unwrap();
        let bb = dir.path().join("b.mgb");
        b.bundle_since(0, bb.to_str().unwrap()).unwrap();
        let mut va = Fact::new("acme", "tier", "enterprise").namespace("caller").created_at(2_000);
        a.supersede(&base, &mut va).unwrap();
        a.import_bundle(bb.to_str().unwrap()).unwrap();
        let _ = st;
    }
    a
}

#[test]
fn merge_closes_the_fork_show_forks_confirms() {
    let dir = TempDir::new().unwrap();
    let m = forked_store(&dir);
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // The fork is visible…
    let v = payload(&ex, &f, "SHOW FORKS");
    assert_eq!(v["forks"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["forks"][0]["subject"], "acme");

    // …MERGE resolves it with a written reason…
    let v = payload(
        &ex,
        &f,
        r#"MERGE "acme" RELATION "tier" TO "enterprise" CONFIDENCE 0.95 BECAUSE "confirmed on the renewal call""#,
    );
    assert_eq!(v["type"], "merged", "{v}");
    assert_eq!(v["object"], "enterprise");

    // …and afterwards there is one live value and no fork.
    let v = payload(&ex, &f, "SHOW FORKS");
    assert_eq!(v["forks"].as_array().unwrap().len(), 0, "{v}");
    let facts = payload(&ex, &f, r#"RECALL facts WHERE subject = "acme""#);
    let objects: Vec<&str> = facts["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["fields"]["object"].as_str().unwrap())
        .collect();
    assert!(objects.iter().all(|o| *o == "enterprise"), "{objects:?}");
}

#[test]
fn merge_requires_because_a_fork_and_the_supersede_grant() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // BECAUSE is syntax.
    {
        let m = Areev::open(dir.path().join("plain.db").to_str().unwrap()).unwrap();
        let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
        let err = ex
            .execute(r#"MERGE "acme" RELATION "tier" TO "x""#, &f)
            .expect_err("merge without BECAUSE must not parse");
        assert!(err.to_string().contains("CAL-E018"), "{err}");

        // No fork → refused, not a silent supersede.
        f.with_store(|m| {
            m.add(&Fact::new("acme", "tier", "starter").namespace("caller").created_at(1_000))
                .unwrap();
        });
        let v = payload(&ex, &f, r#"MERGE "acme" RELATION "tier" TO "x" BECAUSE "t""#);
        assert_eq!(v["type"], "unsupported", "a merge needs >=2 tips: {v}");
    }

    // The supersede grant is the authorization.
    let m = forked_store(&dir);
    f_with_grants(m, &ex);
}

fn f_with_grants(mut m: Areev, ex: &CalExecutor) {
    m.add(
        &Fact::new("agent:writer", REL_PERMITS, "read,write ON caller")
            .namespace(AUTHZ_NS)
            .created_at(3_000),
    )
    .unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None)
        .with_principal("agent:writer")
        .unwrap();
    let v = payload(ex, &f, r#"MERGE "acme" RELATION "tier" TO "x" BECAUSE "t""#);
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");
}

#[test]
fn related_walks_the_graph_bounded() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("g.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    f.with_store(|m| {
        m.add(&Fact::new("alice", "reports_to", "bob").namespace("caller").created_at(1_000)).unwrap();
        m.add(&Fact::new("bob", "reports_to", "carol").namespace("caller").created_at(2_000)).unwrap();
        m.add(&Fact::new("carol", "reports_to", "dana").namespace("caller").created_at(3_000)).unwrap();
    });

    let v = payload(&ex, &f, r#"RELATED "alice" VIA "reports_to" DEPTH 2"#);
    assert_eq!(v["type"], "related_entities", "{v}");
    let ents = v["entities"].as_array().unwrap();
    assert!(ents.contains(&serde_json::json!("bob")), "{v}");
    assert!(ents.contains(&serde_json::json!("carol")), "{v}");
    // DEPTH bounds the walk — dana is three hops out.
    assert!(!ents.contains(&serde_json::json!("dana")), "{v}");
}

#[test]
fn novelty_needs_an_embedder_and_ranks_with_one() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("n.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // Honest refusal without a host embedder.
    let v = payload(&ex, &f, r#"NOVELTY "prefers a window seat""#);
    assert_eq!(v["type"], "unsupported", "{v}");

    // A trivial deterministic embedder makes the leg live.
    struct ByteEmbed;
    impl EmbedBackend for ByteEmbed {
        fn dim(&self) -> usize {
            4
        }
        fn embed(&self, text: &str) -> areev_core::error::Result<Vec<f32>> {
            let mut v = [0f32; 4];
            for (i, b) in text.bytes().enumerate() {
                v[i % 4] += b as f32;
            }
            Ok(v.to_vec())
        }
        fn model(&self) -> &str {
            "byte-test"
        }
    }
    f.with_store(|m| {
        m.set_embedder(Box::new(ByteEmbed));
        m.add(&Fact::new("john", "prefers", "window seat").namespace("caller").created_at(1_000))
            .unwrap();
    });
    let v = payload(&ex, &f, r#"NOVELTY "prefers a window seat" LIMIT 3"#);
    assert_eq!(v["type"], "novelty_matches", "{v}");
    let rows = v["matches"].as_array().unwrap();
    assert!(!rows.is_empty(), "{v}");
    assert!(rows[0]["similarity"].as_f64().unwrap() > 0.0);
}

#[test]
fn with_occurrence_warns_honestly() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let res = ex
        .execute(
            r#"ADD fact SET subject = "x" SET relation = "r" SET object = "o" WITH occurrence REASON "t""#,
            &f,
        )
        .unwrap();
    assert!(
        res.warnings.iter().any(|w| w.contains("record_tool_call")),
        "the warning must point at the right tool: {:?}",
        res.warnings
    );
}
