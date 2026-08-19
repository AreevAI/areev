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
pub fn value_derived_anon_refuses_without_key_material(b: &dyn Backend) {
    let mut m = b.open_named("anon_nokey");
    for policy in [
        r#"{"mode": "ingress"}"#,
        r#"{"mode": "both"}"#,
        r#"{"mode": "egress", "scope": "memory"}"#,
        r#"{"mode": "egress", "scope": "session", "vault": true}"#,
    ] {
        let err = m.set_anon_policy("ns", policy).unwrap_err().to_string();
        assert!(
            err.contains("anon_key"),
            "the refusal must name the fix that works on EVERY backend, got: {err}"
        );
    }
    // The keyless-safe modes still work end to end.
    m.set_anon_policy("ns", r#"{"mode": "egress", "scope": "session"}"#).unwrap();
    m.add(&fact("ns", "caller:john", "prefers", "tea")).unwrap();
    let got = m.recall("ns", "caller:john", None, 4).unwrap();
    assert_eq!(got[0].fields["subject"], "[PERSON_1]", "egress must work keyless");
}

/// A host-supplied `anon_key` unlocks value-derived tokens and the mapping
/// vault on EVERY backend — the gap that made reversible pseudonymisation
/// unusable exactly where it was most wanted.
///
/// The Postgres backend refuses `encryption_key` (it is a page-cipher
/// capability), and the vault key used to be HKDF-derived from that page key
/// alone. So on a stateless host — Cloud Run, ECS, Kubernetes, i.e. the
/// deployments the Postgres backend exists to serve — mappings could not
/// persist and deterministic tokens were unavailable entirely. The key is
/// host-supplied and never persisted, so this case also pins that the vault
/// rows survive a REOPEN with the same key: that is what makes de-anonymisation
/// work across processes and instances rather than within one lifetime.
pub fn host_anon_key_unlocks_the_vault(b: &dyn Backend) {
    let key = [7u8; 32];
    let opts = || areev_store::AreevOptions {
        anon_key: Some(key),
        ..Default::default()
    };

    let mut m = b.open_named_with("anon_hostkey", opts());
    // A vault-enabled policy is accepted with no page cipher in sight.
    m.set_anon_policy("ns", r#"{"mode": "egress", "scope": "memory", "vault": true}"#)
        .expect("a host-supplied anon_key must satisfy a vault-enabled policy");
    m.add(&fact("ns", "caller:john", "prefers", "tea")).unwrap();
    let first = m.recall("ns", "caller:john", None, 4).unwrap();
    let token = first[0].fields["subject"].as_str().unwrap().to_string();
    assert_ne!(token, "caller:john", "the identity must not reach the caller");
    drop(m);

    // Reopened with the SAME key: the token is reproducible, which is the
    // property counter tokens cannot give and the whole reason for HMAC-derived
    // ones. Vault rows written before the drop are still readable.
    let mut m2 = b.open_named_with("anon_hostkey", opts());
    m2.add(&fact("ns", "caller:john", "drinks", "coffee")).unwrap();
    let again = m2.recall("ns", "caller:john", None, 8).unwrap();
    assert!(
        again.iter().all(|g| g.fields["subject"] == serde_json::json!(token)),
        "the same identity must map to the same pseudonym across processes"
    );
}

// ---- trigger evaluation state (`trg:`) -----------------------------------
//
// The declaration is a grain and replicates; the evaluation state must not.
// Both backends have to agree on that and on the CAS that arbitrates claims —
// the embedded tier satisfies it through the single writer, Postgres through a
// real conditional update, and the trigger evaluator runs the same code path on
// each rather than branching per backend.

pub fn trigger_state_never_replicates(b: &dyn Backend) {
    let mut src = b.open_named("trg_src");
    src.add(&fact("ns", "john", "prefers", "tea")).unwrap();
    // A cursor pointing at a third-party system's position, and a registry row
    // that is supposed to travel, so the case can tell "nothing replicated"
    // apart from "trg: specifically did not".
    src.put_trigger_state(
        "abc123",
        None,
        &areev_store::TriggerState {
            cursor: Some("prod-1802611".into()),
            fence: 3,
            ..Default::default()
        },
    )
    .unwrap();
    src.meta_put("qry:brief", &qry_row("RECALL facts", 100, None)).unwrap();

    let bundle = b.scratch().join("trigger_state.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    let mut dst = b.open_named("trg_dst");
    dst.import_bundle(bundle.to_str().unwrap()).unwrap();

    assert!(
        dst.trigger_state("abc123").unwrap().is_none(),
        "a restored memory must not inherit the source's cursor — it would skip \
         real work while reporting success"
    );
    assert!(
        dst.meta_get("qry:brief").unwrap().is_some(),
        "…and the rows that are supposed to replicate still did"
    );
}

pub fn meta_cas_admits_one_claimer_and_fences_the_loser(b: &dyn Backend) {
    let m = b.open_named("trg_cas");

    // Claim-if-absent: first wins, second loses, value untouched.
    assert!(m.meta_cas("trg:t", None, "A1").unwrap());
    assert!(!m.meta_cas("trg:t", None, "B1").unwrap());
    assert_eq!(m.meta_get("trg:t").unwrap().as_deref(), Some("A1"));

    // Swap against a stale expectation loses and changes nothing — this is the
    // zombie release, refused because the fence rides inside the value.
    assert!(!m.meta_cas("trg:t", Some("stale"), "Z").unwrap());
    assert_eq!(m.meta_get("trg:t").unwrap().as_deref(), Some("A1"));

    // Swap against the exact prior value wins.
    assert!(m.meta_cas("trg:t", Some("A1"), "A2").unwrap());
    assert_eq!(m.meta_get("trg:t").unwrap().as_deref(), Some("A2"));
}
