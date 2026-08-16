//! CommandEmbed — the shell-out embedding backend. Uses a tiny Python script
//! that maps text deterministically (sha256 → 8 floats), so identical text
//! embeds identically and an exact-text query must rank its grain first.
//! Skips (with a note) when no Python is on PATH — CI runners all have one.

use areev_core::types::{Fact, Grain};
use areev_store::{CommandEmbed, Areev, AreevOptions};

fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|c| {
        std::process::Command::new(c)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

const EMBED_PY: &str = r#"
import sys, json, hashlib
t = sys.stdin.read()
h = hashlib.sha256(t.encode()).digest()
print(json.dumps([b / 255.0 for b in h[:8]]))
"#;

fn fact(s: &str, r: &str, o: &str, ts: i64) -> Fact {
    let mut f = Fact::new(s, r, o).created_at(ts);
    f.common.namespace = Some("main".to_string());
    f
}

#[test]
fn command_embed_probes_dim_and_powers_vector_recall() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("embed.py");
    std::fs::write(&script, EMBED_PY).unwrap();
    let cmd = format!("{py} {}", script.display());

    let ce = CommandEmbed::new(&cmd, Some("sha-toy")).unwrap();
    let db = dir.path().join("v.db");
    let mut m = Areev::open_with(db.to_str().unwrap(), AreevOptions::default()).unwrap();
    m.set_embedder(Box::new(ce));
    assert_eq!(m.embedder_dim(), Some(8), "dim learned from the probe call");
    assert_eq!(m.declared_embedding(), Some(("sha-toy", 8)), "provenance stamped");

    let target = m.add(&fact("alice", "prefers", "tea", 1_700_000_000_000)).unwrap();
    m.add(&fact("bob", "prefers", "coffee", 1_700_000_000_001)).unwrap();
    m.add(&fact("carol", "works_at", "acme", 1_700_000_000_002)).unwrap();

    // exact projected text ⇒ identical vector ⇒ cosine distance 0 ⇒ rank 1
    let hits = m.search_vector("main", "alice prefers tea", 3).unwrap();
    assert!(!hits.is_empty(), "vector leg returns candidates");
    let grains = m
        .recall_hybrid("main", None, None, Some("alice prefers tea"), 3, None)
        .unwrap();
    assert_eq!(
        grains[0].fields["object"],
        serde_json::json!("tea"),
        "exact-text query ranks its grain first (got: {:?})",
        grains[0].fields
    );
    assert!(m.get(&target).is_ok());
}

#[test]
fn command_embed_rejects_broken_commands() {
    assert!(CommandEmbed::new("", None).is_err());
    assert!(CommandEmbed::new("definitely-not-a-real-binary-xyz", None).is_err());
}
