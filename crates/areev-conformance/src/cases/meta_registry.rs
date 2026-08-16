//! The file-carried registry (saved queries `qry:`, templates `tpl:`,
//! retention policies) must survive bundle replication — "travels with the
//! file" includes backup/restore and sync, not just a raw file copy.
//! These cases pin the v2
//! bundle meta segment's contract on every backend: full imports carry the
//! registry, merges converge latest-wins without ping-ponging usage stats,
//! point-in-time restores leave it alone, and live local policy is never
//! silently swapped.

use crate::{fact, Backend};
use areev_store::RetentionPolicy;

fn qry_row(body: &str, updated_at: u64, last_run_at: Option<u64>) -> String {
    let mut v = serde_json::json!({
        "body": body,
        "description": "conformance",
        "params": [],
        "updated_at": updated_at,
    });
    if let Some(lr) = last_run_at {
        v["last_run_at"] = serde_json::json!(lr);
    }
    v.to_string()
}

pub fn registry_rides_a_full_bundle(b: &dyn Backend) {
    let mut src = b.open_named("reg_src");
    src.add(&fact("ns", "john", "prefers", "tea")).unwrap();
    src.meta_put("qry:brief", &qry_row("RECALL facts", 100, Some(200))).unwrap();
    src.meta_put("tpl:mine", r#"{"source":"ELEMENT { x }","updated_at":100}"#).unwrap();
    src.set_retention_policy(
        "calls",
        &RetentionPolicy { days: 30.0, grain_type: None, because: Some("policy".into()) },
    )
    .unwrap();

    let bundle = b.scratch().join("registry.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    let mut dst = b.open_named("reg_dst");
    let stats = dst.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert_eq!(stats.applied, 1, "the grain op applies");
    assert_eq!(stats.meta_applied, 3, "query + template + retention ride along");

    let q = dst.meta_get("qry:brief").unwrap().expect("saved query replicated");
    let qv: serde_json::Value = serde_json::from_str(&q).unwrap();
    assert_eq!(qv["body"], "RECALL facts");
    assert!(
        qv.get("last_run_at").is_none(),
        "usage stats are stripped at export — definitions replicate, run \
         counters do not: {q}"
    );
    assert!(dst.meta_get("tpl:mine").unwrap().is_some(), "template replicated");
    let policies = dst.retention_policies().unwrap();
    assert_eq!(policies.len(), 1, "retention policy replicated");
    assert_eq!(policies[0].0, "calls");
}

pub fn registry_merge_is_latest_wins_and_keeps_local_last_run(b: &dyn Backend) {
    let mut src = b.open_named("merge_src");
    src.add(&fact("ns", "john", "prefers", "tea")).unwrap();
    src.meta_put("qry:brief", &qry_row("RECALL facts LIMIT 5", 200, None)).unwrap();
    let bundle = b.scratch().join("merge.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    // The replica has an OLDER definition it has actually run.
    let mut dst = b.open_named("merge_dst");
    dst.meta_put("qry:brief", &qry_row("RECALL facts", 100, Some(555))).unwrap();

    let stats = dst.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert_eq!(stats.meta_applied, 1, "newer incoming definition wins");
    let merged: serde_json::Value =
        serde_json::from_str(&dst.meta_get("qry:brief").unwrap().unwrap()).unwrap();
    assert_eq!(merged["body"], "RECALL facts LIMIT 5", "definition updated");
    assert_eq!(
        merged["last_run_at"], 555,
        "local usage survives an incoming definition update"
    );

    // Replay converges: equal updated_at keeps local.
    let again = dst.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert_eq!(again.applied, 0, "op replay is a no-op");
    assert_eq!(again.meta_applied, 0, "registry replay is a no-op");

    // An older incoming definition never rolls a newer local one back.
    let mut src2 = b.open_named("merge_src2");
    src2.add(&fact("ns", "john", "prefers", "tea")).unwrap();
    src2.meta_put("qry:brief", &qry_row("RECALL facts LIMIT 1", 50, None)).unwrap();
    let stale = b.scratch().join("merge_stale.mgb");
    src2.bundle_since(0, stale.to_str().unwrap()).unwrap();
    let s = dst.import_bundle(stale.to_str().unwrap()).unwrap();
    assert_eq!(s.meta_applied, 0);
    let kept: serde_json::Value =
        serde_json::from_str(&dst.meta_get("qry:brief").unwrap().unwrap()).unwrap();
    assert_eq!(kept["body"], "RECALL facts LIMIT 5", "newer local kept");
}

pub fn pitr_import_skips_the_registry(b: &dyn Backend) {
    let mut src = b.open_named("pitr_src");
    src.add(&fact("ns", "john", "prefers", "tea")).unwrap();
    src.meta_put("qry:brief", &qry_row("RECALL facts", 100, None)).unwrap();
    let ops = src.changes_since(0, 10).unwrap();
    let bundle = b.scratch().join("pitr.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    // Meta rows have no HLC, so a point-in-time restore cannot place them on
    // the timeline — it must not resurrect definitions from after the cutoff.
    let mut dst = b.open_named("pitr_dst");
    let stats = dst
        .import_bundle_until(bundle.to_str().unwrap(), Some(ops.last().unwrap().hlc))
        .unwrap();
    assert_eq!(stats.applied, 1, "the grain within the cutoff applies");
    assert_eq!(stats.meta_applied, 0);
    assert_eq!(stats.meta_skipped, 1, "the registry entry is skipped, visibly");
    assert!(dst.meta_get("qry:brief").unwrap().is_none());
}

pub fn retention_row_never_clobbers_local_policy(b: &dyn Backend) {
    let mut src = b.open_named("ret_src");
    src.add(&fact("ns", "john", "prefers", "tea")).unwrap();
    src.set_retention_policy(
        "calls",
        &RetentionPolicy { days: 7.0, grain_type: None, because: Some("source".into()) },
    )
    .unwrap();
    let bundle = b.scratch().join("ret.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    let mut dst = b.open_named("ret_dst");
    dst.set_retention_policy(
        "calls",
        &RetentionPolicy { days: 90.0, grain_type: None, because: Some("local".into()) },
    )
    .unwrap();
    let stats = dst.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert_eq!(stats.meta_applied, 0, "a live local policy is never swapped by sync");
    let policies = dst.retention_policies().unwrap();
    assert_eq!(policies[0].1.days, 90.0, "local retention policy kept");
}

/// `anon:<ns>` policy rows replicate write-if-absent like retention rows
/// (docs/anonymization-proposal.md P1): a full import carries the policy to
/// a replica that has none — and takes effect on the live handle — while a
/// replica's own declared policy is never silently swapped by sync.
pub fn anon_policy_replicates_write_if_absent(b: &dyn Backend) {
    let mut src = b.open_named("anon_src");
    src.add(&fact("ns", "caller:john", "prefers", "tea")).unwrap();
    src.set_anon_policy("ns", r#"{"mode": "egress"}"#).unwrap();
    let bundle = b.scratch().join("anon.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    let mut dst = b.open_named("anon_dst");
    let stats = dst.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert!(stats.meta_applied >= 1, "the anon policy rides the bundle: {stats:?}");
    assert_eq!(
        dst.anon_active_mode("ns").unwrap().as_deref(),
        Some("egress"),
        "a replicated policy takes effect on the live handle"
    );
    let got = dst.recall("ns", "caller:john", None, 4).unwrap();
    assert_eq!(
        got[0].fields["subject"], "[PERSON_1]",
        "the replica's egress boundary engages without a reopen"
    );

    let mut third = b.open_named("anon_third");
    third.set_anon_policy("ns", r#"{"mode": "audit"}"#).unwrap();
    third.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert_eq!(
        third.anon_active_mode("ns").unwrap().as_deref(),
        Some("audit"),
        "a live local anon policy is never swapped by sync"
    );
}

/// REQ-ANON-2: the sealed vault (the re-identification table) never rides a
/// bundle — export omits `vault:` rows, and the importer's allowlist refuses
/// them even from a crafted bundle.
pub fn vault_rows_never_replicate(b: &dyn Backend) {
    // The vault needs a page cipher, so the fixture source is always an
    // embedded encrypted file; what's backend-parameterized is the property
    // under test — the IMPORTER refusing vault rows.
    let src_path = b.scratch().join("vault_src.db");
    let mut src =
        areev_store::Areev::open_encrypted(src_path.to_str().unwrap(), [5u8; 32]).unwrap();
    src.set_anon_policy("ns", r#"{"mode": "egress", "scope": "session", "vault": true}"#)
        .unwrap();
    src.add(&fact("ns", "caller:john", "prefers", "tea")).unwrap();
    let _ = src.recall("ns", "caller:john", None, 4).unwrap(); // mints + persists a vault row
    assert!(!src.meta_scan("vault:ns:").unwrap().is_empty(), "precondition: vault row exists");

    let bundle = b.scratch().join("vault.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();
    let bytes = std::fs::read(&bundle).unwrap();
    assert!(
        !bytes.windows(6).any(|w| w == b"vault:"),
        "the bundle must not carry vault rows"
    );

    let mut dst = b.open_named("vault_dst");
    dst.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert!(
        dst.meta_scan("vault:").unwrap().is_empty(),
        "no vault row may exist on the replica"
    );
}

/// Value-derived pseudonym features (ingress modes, memory scope, the vault)
/// are keyed from the page cipher. A memory with no page key — an
/// unencrypted file, or ANY Postgres schema (the page cipher is
/// file-backend-only) — must refuse those declarations loudly at `set`,
/// never degrade to unkeyed derivation. Plain egress stays available.
pub fn value_derived_anon_refuses_without_page_key(b: &dyn Backend) {
    let mut m = b.open_named("anon_nokey");
    for policy in [
        r#"{"mode": "ingress"}"#,
        r#"{"mode": "both"}"#,
        r#"{"mode": "egress", "scope": "memory"}"#,
        r#"{"mode": "egress", "scope": "session", "vault": true}"#,
    ] {
        let err = m.set_anon_policy("ns", policy).unwrap_err().to_string();
        assert!(
            err.contains("encrypted"),
            "expected an encrypted-memory refusal for {policy}, got: {err}"
        );
    }
    // The keyless-safe modes still work end to end.
    m.set_anon_policy("ns", r#"{"mode": "egress", "scope": "session"}"#).unwrap();
    m.add(&fact("ns", "caller:john", "prefers", "tea")).unwrap();
    let got = m.recall("ns", "caller:john", None, 4).unwrap();
    assert_eq!(got[0].fields["subject"], "[PERSON_1]", "egress must work keyless");
}
