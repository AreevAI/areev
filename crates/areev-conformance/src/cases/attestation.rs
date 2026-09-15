//! Grain attestation contract (`docs/grain-attestation-plan.md`): a detached
//! Ed25519 signature per grain, stored as an ordinary grain, verified only at
//! bundle import — identical on both backends because nothing about the
//! format, the schema, or the bundle layout changes.

use crate::{fact, fact_at, Backend};
use areev_core::authz::ATTEST_NS;
use areev_core::types::Observation;
use areev_store::{AreevOptions, AttestPolicy, Signer};

const SEED_A: [u8; 32] = [0x11; 32];
const SEED_B: [u8; 32] = [0x22; 32];

fn trusted_json(seed: [u8; 32], policy: &str) -> String {
    let s = Signer::from_seed(seed);
    format!(
        r#"{{"version":1,"keys":{{"{}":"{}"}},"policy":"{policy}"}}"#,
        s.key_id(),
        s.public_key_hex()
    )
}

/// Lowercase hex, so the case needs no dependency the crate lacks.
fn hx(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ro_opts() -> AreevOptions {
    AreevOptions { read_only: true, ..AreevOptions::default() }
}

/// Every write through a keyed handle is followed by its attestation; the
/// attestation is a pure function of (key, hash), so re-attesting is a no-op
/// and the attested grain's own address never moves.
pub fn attestation_follows_every_write_and_is_deterministic(b: &dyn Backend) {
    // One grain value for both stores: an unpinned `created_at` is stamped
    // at serialize time (ms), and two stamps are two grains.
    let berlin = fact_at("ns", "alice", "lives_in", "Berlin", 1_700_000_000_000);
    let mut plain = b.open_named("att_plain");
    let h_plain = plain.add(&berlin).unwrap();

    let mut m = b.open_named("att_keyed");
    let key_id = m.set_signing_key(SEED_A);
    let h = m.add(&berlin).unwrap();
    assert_eq!(h, h_plain, "attesting must not change the attested grain's address");
    let atts = m.recall(ATTEST_NS, &format!("sha256:{}", h.to_hex()), None, 8).unwrap();
    assert_eq!(atts.len(), 1, "one attestation per written grain");
    assert_eq!(atts[0].get_str("observer_id"), Some(key_id.as_str()));

    // Re-attesting is idempotent: same grain, no second attestation.
    let a1 = m.attest(&h).unwrap();
    let a2 = m.attest(&h).unwrap();
    assert_eq!(a1, a2);
    assert_eq!(m.recall(ATTEST_NS, &format!("sha256:{}", h.to_hex()), None, 8).unwrap().len(), 1);
    let stats = m.attest_all(None).unwrap();
    assert_eq!((stats.attested, stats.skipped), (0, 1));

    // Supersede is attested too; the attestation namespace itself is not.
    let mut v2 = fact("ns", "alice", "lives_in", "Munich");
    let h2 = m.supersede(&h, &mut v2).unwrap();
    assert_eq!(m.recall(ATTEST_NS, &format!("sha256:{}", h2.to_hex()), None, 8).unwrap().len(), 1);
    m.set_trusted_authors(&trusted_json(SEED_A, "verify")).unwrap();
    let rep = m.verify_attestations().unwrap();
    assert_eq!((rep.attested, rep.unattested, rep.attest_invalid), (2, 0, 0), "{rep:?}");
    assert_eq!(rep.attestations, 2, "attestations are never attested themselves");

    // A memory that predates its key is retro-filled by attest_all.
    plain.set_signing_key(SEED_A);
    let stats = plain.attest_all(Some("ns")).unwrap();
    assert_eq!((stats.attested, stats.skipped), (1, 0));
}

/// Only the store mints attestations: the public write API refuses the
/// reserved namespace, whatever the caller puts in the grain.
pub fn user_writes_to_the_attest_namespace_are_refused(b: &dyn Backend) {
    let mut m = b.open_named("att_forge");
    let mut forged = Observation::new("deadbeefdeadbeef", "attestation");
    forged.common.namespace = Some(ATTEST_NS.to_string());
    let err = m.add(&forged).unwrap_err().to_string();
    assert!(err.starts_with("VAL-"), "forged attestation must be a validation refusal: {err}");
    let mut also = fact(ATTEST_NS, "x", "y", "z");
    assert!(m.add(&also).is_err());
    also.common.namespace = Some("ns".into());
    m.add(&also).unwrap();
}

/// A tampered attestation from a trusted key is tampering: the whole bundle
/// is refused before any write, under `verify` as well as `require`.
pub fn tampered_attestation_refuses_the_bundle_before_any_write(b: &dyn Backend) {
    let mut a = b.open_named("att_tamper_a");
    a.set_signing_key(SEED_A);
    let h = a.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    let path = b.scratch().join("att_tamper.mgb");
    a.bundle_since(0, path.to_str().unwrap()).unwrap();

    // Re-sign the same hash with a different key but keep key A's id: the
    // attestation still parses, and A's public key rejects the signature.
    let forged = Signer::from_seed(SEED_B).sign(&h);
    let mut bytes = std::fs::read(&path).unwrap();
    let real = Signer::from_seed(SEED_A).sign(&h);
    let real_hex = hx(&real);
    let pos = bytes
        .windows(real_hex.len())
        .position(|w| w == real_hex.as_bytes())
        .expect("attestation signature must be in the bundle");
    bytes[pos..pos + real_hex.len()].copy_from_slice(hx(&forged).as_bytes());
    // The attestation record's own address changes with its bytes, so
    // relabel it — otherwise Phase 0's hash check fires first.
    let mut i = 4;
    if &bytes[..4] == b"MGB2" {
        i = 8 + u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    }
    while i < bytes.len() {
        let hash_at = i + 1 + 8;
        let len_at = hash_at + 32;
        let len = u32::from_le_bytes(bytes[len_at..len_at + 4].try_into().unwrap()) as usize;
        let blob_at = len_at + 4;
        let blob = &bytes[blob_at..blob_at + len];
        if blob_at <= pos && pos < blob_at + len {
            let fixed = areev_core::format::content_address(blob);
            bytes[hash_at..hash_at + 32].copy_from_slice(fixed.as_bytes());
            break;
        }
        i = blob_at + len;
    }
    std::fs::write(&path, &bytes).unwrap();

    let mut peer = b.open_named("att_tamper_peer");
    peer.set_trusted_authors(&trusted_json(SEED_A, "verify")).unwrap();
    let err = peer.import_bundle(path.to_str().unwrap()).unwrap_err().to_string();
    assert!(err.starts_with("CRY-E002"), "{err}");
    assert!(err.contains("nothing was imported"), "{err}");
    assert_eq!(peer.changes_since(0, 100).unwrap().len(), 0, "nothing may be written");
    assert!(peer.recall("ns", "alice", None, 8).unwrap().is_empty());
}

/// `require` refuses an unsigned bundle whole; `verify` counts and admits
/// it; `off` (the default) imports it exactly as before.
pub fn require_policy_refuses_unsigned_bundles_and_off_admits_them(b: &dyn Backend) {
    let mut a = b.open_named("att_unsigned_a");
    a.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    a.add(&fact("ns", "bob", "lives_in", "Paris")).unwrap();
    let path = b.scratch().join("att_unsigned.mgb");
    a.bundle_since(0, path.to_str().unwrap()).unwrap();

    let mut strict = b.open_named("att_unsigned_strict");
    strict.set_trusted_authors(&trusted_json(SEED_A, "require")).unwrap();
    let err = strict.import_bundle(path.to_str().unwrap()).unwrap_err().to_string();
    assert!(err.starts_with("CRY-E003"), "{err}");
    assert!(err.contains("2 unsigned"), "{err}");
    assert_eq!(strict.changes_since(0, 100).unwrap().len(), 0);

    let mut lenient = b.open_named("att_unsigned_verify");
    lenient.set_trusted_authors(&trusted_json(SEED_A, "verify")).unwrap();
    let st = lenient.import_bundle(path.to_str().unwrap()).unwrap();
    assert_eq!((st.applied, st.attested, st.unattested, st.unknown_key), (2, 0, 2, 0));

    let mut plain = b.open_named("att_unsigned_off");
    assert_eq!(plain.attest_policy(), AttestPolicy::Off);
    let st = plain.import_bundle(path.to_str().unwrap()).unwrap();
    assert_eq!((st.applied, st.attested, st.unattested), (2, 0, 0), "off counts nothing");
}

/// A bundle signed by a trusted key passes `require`; the same bundle read
/// by a host that trusts a different key is `unknown_key` — admitted under
/// `verify`, refused under `require`. Attestations replicate as plain grains
/// through MGB1/MGB2 with no format change, and `--require-attested`'s
/// policy override applies to an installed set.
pub fn attested_bundle_passes_require_and_unknown_keys_are_named(b: &dyn Backend) {
    let mut a = b.open_named("att_signed_a");
    a.set_signing_key(SEED_A);
    let h = a.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    let path = b.scratch().join("att_signed.mgb");
    a.bundle_since(0, path.to_str().unwrap()).unwrap();

    let mut trusts_a = b.open_named("att_signed_trusts_a");
    trusts_a.set_trusted_authors(&trusted_json(SEED_A, "require")).unwrap();
    let st = trusts_a.import_bundle(path.to_str().unwrap()).unwrap();
    assert_eq!((st.applied, st.attested, st.unattested, st.unknown_key), (2, 1, 0, 0), "{st:?}");
    assert_eq!(trusts_a.recall(ATTEST_NS, &format!("sha256:{}", h.to_hex()), None, 8).unwrap().len(), 1);
    let rep = trusts_a.verify_attestations().unwrap();
    assert_eq!((rep.attested, rep.unattested, rep.attest_unknown_key), (1, 0, 0), "{rep:?}");

    let mut trusts_b = b.open_named("att_signed_trusts_b");
    trusts_b.set_trusted_authors(&trusted_json(SEED_B, "verify")).unwrap();
    let st = trusts_b.import_bundle(path.to_str().unwrap()).unwrap();
    assert_eq!((st.applied, st.attested, st.unknown_key), (2, 0, 1), "{st:?}");
    let rep = trusts_b.verify_attestations().unwrap();
    assert_eq!((rep.attested, rep.unattested, rep.attest_unknown_key), (0, 1, 1), "{rep:?}");

    let mut strict_b = b.open_named("att_signed_strict_b");
    strict_b.set_trusted_authors(&trusted_json(SEED_B, "verify")).unwrap();
    strict_b.set_attest_policy(AttestPolicy::Require);
    let err = strict_b.import_bundle(path.to_str().unwrap()).unwrap_err().to_string();
    assert!(err.starts_with("CRY-E003") && err.contains("1 signed by unknown keys"), "{err}");
    assert_eq!(strict_b.changes_since(0, 100).unwrap().len(), 0);
}

/// Verification is read-only work: it runs on a read-only handle (the
/// least-privilege Postgres role), and `forget` of the subject leaves the
/// attestation an orphan that verify reports rather than hides.
pub fn attestation_verification_is_read_only_and_reports_orphans(b: &dyn Backend) {
    {
        let mut m = b.open_named("att_ro");
        m.set_signing_key(SEED_A);
        let h = m.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
        m.add(&fact("ns", "bob", "lives_in", "Paris")).unwrap();
        m.forget(&h).unwrap();
    }
    let mut ro = b.open_named_with("att_ro", ro_opts());
    ro.set_trusted_authors(&trusted_json(SEED_A, "verify")).unwrap();
    let rep = ro.verify_attestations().unwrap();
    assert_eq!(
        (rep.attestations, rep.attested, rep.attest_orphaned, rep.unattested),
        (2, 1, 1, 0),
        "{rep:?}"
    );
    ro.set_signing_key(SEED_B);
    let err = ro.attest_all(None).unwrap_err().to_string();
    assert!(err.starts_with("STO-E004"), "a read-only handle must refuse to attest: {err}");
}
