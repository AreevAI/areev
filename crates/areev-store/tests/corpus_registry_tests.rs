use areev_core::authz::{subject_fingerprint, HARNESS_NS};
use areev_core::types::{Event, Grain, GrainType, Role};
use areev_store::Areev;

#[test]
fn export_registry_is_grain_backed_linked_and_replicates() {
    let dir = tempfile::tempdir().unwrap();
    let a_path = dir.path().join("a.db");
    let b_path = dir.path().join("b.db");
    let bundle = dir.path().join("exports.mgb");
    let mut a = Areev::open(a_path.to_str().unwrap()).unwrap();
    let source = a
        .add(
            &Event::new("hello")
                .role(Role::User)
                .session("s1".into())
                .subject("person:7")
                .namespace("caller")
                .created_at(10),
        )
        .unwrap();
    let manifest = a
        .record_corpus_export(
            "RECALL events WHERE session_id = \"s1\"",
            "train.jsonl",
            Some("trainer@example"),
            20,
            &[subject_fingerprint("person:7")],
            &[source.to_hex()],
        )
        .unwrap();

    let registry = a.corpus_exports().unwrap();
    assert_eq!(registry.len(), 1);
    assert_eq!(registry[0].manifest_hash, manifest.to_hex());
    assert_eq!(registry[0].recipient.as_deref(), Some("trainer@example"));
    assert_eq!(registry[0].source_hashes, vec![source.to_hex()]);
    assert_eq!(registry[0].subject_fingerprints, vec![subject_fingerprint("person:7")]);
    let manifest_grain = a.get(&manifest).unwrap();
    let context = &manifest_grain.fields["context"];
    assert!(context.get("selector").is_none(), "raw selector must not replicate");
    assert_eq!(context["selector_sha256"].as_str().map(str::len), Some(64));
    assert!(
        !serde_json::to_string(context).unwrap().contains("person:7"),
        "registry context must not resurrect the exported identity"
    );
    let links = a
        .recall(HARNESS_NS, &manifest.to_hex(), Some("mg:corpus_source"), 10)
        .unwrap();
    assert_eq!(links.len(), 1, "the manifest source is a traversable related_to edge");
    a.rebuild_link_indexes().unwrap();
    assert_eq!(a.corpus_exports().unwrap().len(), 1, "maintenance rebuilds the sparse index");

    a.bundle_since(0, bundle.to_str().unwrap()).unwrap();
    let mut b = Areev::open(b_path.to_str().unwrap()).unwrap();
    b.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert_eq!(b.corpus_exports().unwrap().len(), 1, "registry must travel in MGB1");
}

#[test]
fn identity_and_retention_erasure_identify_stale_exports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let mut db = Areev::open(path.to_str().unwrap()).unwrap();
    let first = db
        .add(
            &Event::new("first")
                .role(Role::User)
                .session("person:7:thread".into())
                .subject("person:7")
                .namespace("caller")
                .created_at(10),
        )
        .unwrap();
    let first_manifest = db.record_corpus_export(
        "RECALL events WHERE subject = \"person:7\"",
        "first.jsonl",
        None,
        100,
        &[subject_fingerprint("person:7")],
        &[first.to_hex()],
    )
    .unwrap();
    assert_eq!(db.corpus_exports_touching_subject("person:7").unwrap().len(), 1);
    let erased = db.forget_subject("caller", "person:7").unwrap();
    assert!(erased.erased_hashes.contains(&first.to_hex()));
    let retained = db.get(&first_manifest).unwrap();
    assert!(
        !serde_json::to_string(&retained.fields).unwrap().contains("person:7"),
        "the immutable registry must not retain an erased identity"
    );

    let second = db
        .add(
            &Event::new("old")
                .role(Role::Assistant)
                .session("s2".into())
                .namespace("caller")
                .created_at(20),
        )
        .unwrap();
    db.record_corpus_export(
        "RECALL events",
        "second.jsonl",
        None,
        200,
        &[],
        &[second.to_hex()],
    )
    .unwrap();
    let report = db
        .forget_older_than(Some("caller"), 50, Some(GrainType::Event))
        .unwrap();
    assert_eq!(report.erased_hashes, vec![second.to_hex()]);
    assert_eq!(
        db.corpus_exports_touching_hashes(&report.erased_hashes)
            .unwrap()
            .len(),
        1
    );
}
