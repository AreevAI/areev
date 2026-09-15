//! Grain attestation — a detached Ed25519 signature over a grain's content
//! hash, stored as an ordinary Observation in the reserved `agent:attest`
//! namespace (`docs/grain-attestation-plan.md`).
//!
//! Nothing here touches the attested grain: its bytes and its address are
//! unchanged, which is what keeps the `.mg` format, the bundle format, and
//! every existing file exactly as they were. The signing key and the trusted
//! author keys are host configuration installed on an open handle
//! ([`crate::Areev::set_signing_key`], [`crate::Areev::set_trusted_authors`])
//! and are never persisted in the memory.

use std::collections::BTreeMap;

use areev_core::authz::{ATTEST_NS, REL_ATTESTS};
use areev_core::error::{AreevError, Hash, Result};
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::types::{Observation, RelatedTo};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// The only algorithm this version signs or verifies with.
pub const ALG: &str = "ed25519";
/// Domain separation for the signed message: a signature made here can never
/// be mistaken for one made by another protocol over the same 32 bytes.
pub const DOMAIN: &str = "areev-attest-v1";
/// The `observer_type` every attestation Observation carries.
pub const OBSERVER_TYPE: &str = "attestation";

/// The bytes an author key signs for a grain: `DOMAIN || 0x00 || hash`.
pub fn signed_message(hash: &Hash) -> Vec<u8> {
    let mut m = Vec::with_capacity(DOMAIN.len() + 1 + 32);
    m.extend_from_slice(DOMAIN.as_bytes());
    m.push(0);
    m.extend_from_slice(hash.as_bytes());
    m
}

/// A key id is the first 16 hex characters of SHA-256 over the raw public key.
pub fn key_id_of(public: &VerifyingKey) -> String {
    hex::encode(&Sha256::digest(public.as_bytes())[..8])
}

fn parse_hex32(what: &str, hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str.trim())
        .map_err(|e| AreevError::SigningKeyInvalid(format!("{what}: not hex: {e}")))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        AreevError::SigningKeyInvalid(format!(
            "{what}: expected 32 bytes (64 hex characters), got {}",
            bytes.len()
        ))
    })
}

/// A host-held author key. The seed is zeroized on drop.
pub struct Signer {
    key: SigningKey,
    key_id: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer").field("key_id", &self.key_id).finish_non_exhaustive()
    }
}

impl Signer {
    /// From a 32-byte Ed25519 seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let seed = Zeroizing::new(seed);
        let key = SigningKey::from_bytes(&seed);
        let key_id = key_id_of(&key.verifying_key());
        Signer { key, key_id }
    }

    /// From the seed as 64 hex characters (the `--signing-key-env` form).
    pub fn from_seed_hex(hex_str: &str) -> Result<Self> {
        Ok(Self::from_seed(parse_hex32("signing seed", hex_str)?))
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The public half, hex — what goes into a trusted-authors document.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.key.verifying_key().as_bytes())
    }

    /// Sign a content hash. Ed25519 is deterministic: the same key and hash
    /// always produce the same 64 bytes, so an attestation grain is
    /// reproducible and re-attesting is a no-op.
    pub fn sign(&self, hash: &Hash) -> [u8; 64] {
        self.key.sign(&signed_message(hash)).to_bytes()
    }

    /// Build the attestation Observation for `hash`. `subject_created_at` is
    /// the attested grain's own timestamp so the attestation's address is a
    /// pure function of (key, hash) and two replicas mint one grain.
    pub fn attestation_for(&self, hash: &Hash, subject_created_at: i64) -> Observation {
        let attests = format!("sha256:{}", hash.to_hex());
        let mut obs = Observation::new(&self.key_id, OBSERVER_TYPE).subject(&attests);
        obs.common.namespace = Some(ATTEST_NS.to_string());
        obs.common.created_at = Some(subject_created_at);
        obs.common.context = Some(serde_json::json!({
            "attestation": true,
            "alg": ALG,
            "key_id": self.key_id,
            "attests": attests,
            "sig": hex::encode(self.sign(hash)),
            "domain": DOMAIN,
        }));
        obs.common.related_to = vec![RelatedTo {
            hash: hash.to_hex(),
            relation_type: REL_ATTESTS.to_string(),
            weight: None,
        }];
        obs
    }
}

impl Drop for Signer {
    fn drop(&mut self) {
        // `SigningKey` zeroizes its own bytes on drop under the `zeroize`
        // feature; without it we cannot reach the seed, so the Zeroizing wrap
        // in `from_seed` is what covers the copy we made.
    }
}

/// What the importer and `verify` do with attestations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttestPolicy {
    /// Attestations are stored and ignored (the default).
    #[default]
    Off,
    /// Every attestation from a trusted key must verify; unsigned grains pass.
    Verify,
    /// Every grain outside the reserved namespaces must carry a valid
    /// attestation from a trusted key.
    Require,
}

impl AttestPolicy {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "off" => Ok(Self::Off),
            "verify" => Ok(Self::Verify),
            "require" => Ok(Self::Require),
            other => Err(AreevError::SigningKeyInvalid(format!(
                "policy must be off | verify | require, got {other:?}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Verify => "verify",
            Self::Require => "require",
        }
    }
}

/// The host's trusted-authors document:
///
/// ```json
/// { "version": 1,
///   "keys": { "<key_id>": "<32-byte public key, hex>" },
///   "policy": "verify" }
/// ```
///
/// `policy` is optional and defaults to `verify` — supplying a document at
/// all says the host wants attestations checked. Rotation is adding a key;
/// revocation is removing one, after which grains it attested read as
/// `unknown_key`.
#[derive(Debug, Clone)]
pub struct TrustedAuthors {
    keys: BTreeMap<String, VerifyingKey>,
    policy: AttestPolicy,
}

impl TrustedAuthors {
    pub fn from_json(json: &str) -> Result<Self> {
        let doc: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| AreevError::SigningKeyInvalid(format!("trusted authors: {e}")))?;
        let version = doc.get("version").and_then(|v| v.as_i64()).unwrap_or(1);
        if version != 1 {
            return Err(AreevError::SigningKeyInvalid(format!(
                "trusted authors: unsupported version {version} (this build reads version 1)"
            )));
        }
        let mut keys = BTreeMap::new();
        if let Some(map) = doc.get("keys") {
            let map = map.as_object().ok_or_else(|| {
                AreevError::SigningKeyInvalid("trusted authors: \"keys\" must be an object".into())
            })?;
            for (id, pk) in map {
                let pk_hex = pk.as_str().ok_or_else(|| {
                    AreevError::SigningKeyInvalid(format!(
                        "trusted authors: key {id:?} must be a hex string"
                    ))
                })?;
                let bytes = parse_hex32(&format!("trusted authors key {id:?}"), pk_hex)?;
                let vk = VerifyingKey::from_bytes(&bytes).map_err(|e| {
                    AreevError::SigningKeyInvalid(format!(
                        "trusted authors key {id:?}: not a valid Ed25519 public key: {e}"
                    ))
                })?;
                let expected = key_id_of(&vk);
                if id != &expected {
                    return Err(AreevError::SigningKeyInvalid(format!(
                        "trusted authors: key {id:?} is listed under the wrong id — its \
                         public key derives id {expected:?}"
                    )));
                }
                keys.insert(id.clone(), vk);
            }
        }
        let policy = match doc.get("policy").and_then(|p| p.as_str()) {
            Some(p) => AttestPolicy::parse(p)?,
            None => AttestPolicy::Verify,
        };
        Ok(TrustedAuthors { keys, policy })
    }

    /// A document trusting exactly this signer, with the given policy.
    pub fn single(signer: &Signer, policy: AttestPolicy) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(signer.key_id().to_string(), signer.key.verifying_key());
        TrustedAuthors { keys, policy }
    }

    pub fn policy(&self) -> AttestPolicy {
        self.policy
    }

    pub fn with_policy(mut self, policy: AttestPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(String::as_str)
    }

    /// Check one parsed attestation.
    pub fn check(&self, att: &Attestation) -> Verdict {
        if att.alg != ALG {
            return Verdict::Invalid(format!("unsupported alg {:?}", att.alg));
        }
        let Some(vk) = self.keys.get(&att.key_id) else {
            return Verdict::UnknownKey;
        };
        let sig = Signature::from_bytes(&att.sig);
        match vk.verify(&signed_message(&att.attests), &sig) {
            Ok(()) => Verdict::Valid,
            Err(_) => Verdict::Invalid(format!(
                "signature by key {} does not verify over {}",
                att.key_id,
                att.attests.to_hex()
            )),
        }
    }
}

/// The outcome of checking one attestation against the trusted authors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    /// The key id is not in the trusted set; nothing can be said either way.
    UnknownKey,
    /// A trusted key's signature fails — tampering, or a forged attestation.
    Invalid(String),
}

/// One attestation as read back from a stored grain.
#[derive(Debug, Clone)]
pub struct Attestation {
    /// The attestation grain's own address.
    pub hash: Hash,
    pub key_id: String,
    pub alg: String,
    /// The attested grain's address.
    pub attests: Hash,
    pub sig: [u8; 64],
}

/// Parse a deserialized grain as an attestation. `None` when it is not one
/// (wrong namespace or type). A grain that *claims* to be one but is
/// malformed is `Some(Err(..))`, because a broken attestation in the reserved
/// namespace is worth reporting, not silently skipping.
pub fn parse_attestation(view: &DeserializedGrain) -> Option<Result<Attestation>> {
    let ns = view.fields.get("namespace").and_then(|v| v.as_str())?;
    if ns != ATTEST_NS {
        return None;
    }
    let otype = view.fields.get("observer_type").and_then(|v| v.as_str())?;
    if otype != OBSERVER_TYPE {
        return None;
    }
    let ctx = match view.fields.get("context") {
        Some(c) => c,
        None => {
            return Some(Err(AreevError::AttestationInvalid(format!(
                "{}: attestation has no context",
                view.hash.to_hex()
            ))))
        }
    };
    let get = |k: &str| ctx.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let parsed = (|| -> Result<Attestation> {
        let bad = |m: String| AreevError::AttestationInvalid(format!("{}: {m}", view.hash.to_hex()));
        let key_id = get("key_id").ok_or_else(|| bad("missing key_id".into()))?;
        let alg = get("alg").ok_or_else(|| bad("missing alg".into()))?;
        let attests = get("attests").ok_or_else(|| bad("missing attests".into()))?;
        let attests_hex = attests
            .strip_prefix("sha256:")
            .ok_or_else(|| bad(format!("attests {attests:?} is not sha256:<hex>")))?;
        let attests = Hash::from_hex(attests_hex).map_err(|e| bad(format!("attests: {e}")))?;
        let sig_hex = get("sig").ok_or_else(|| bad("missing sig".into()))?;
        let sig_bytes = hex::decode(&sig_hex).map_err(|e| bad(format!("sig: not hex: {e}")))?;
        let sig = <[u8; 64]>::try_from(sig_bytes.as_slice())
            .map_err(|_| bad(format!("sig: expected 64 bytes, got {}", sig_bytes.len())))?;
        // The `related_to` edge must agree with the context, or a reader
        // walking edges and a reader parsing context would disagree.
        let edge_ok = view
            .fields
            .get("related_to")
            .and_then(|r| r.as_array())
            .is_some_and(|arr| {
                arr.iter().any(|e| {
                    e.get("relation_type").and_then(|v| v.as_str()) == Some(REL_ATTESTS)
                        && e.get("hash").and_then(|v| v.as_str()) == Some(attests_hex)
                })
            });
        if !edge_ok {
            return Err(bad(format!(
                "related_to carries no {REL_ATTESTS} edge to {attests_hex}"
            )));
        }
        Ok(Attestation { hash: view.hash, key_id, alg, attests, sig })
    })();
    Some(parsed)
}

/// Namespaces that are never attested: the attestations themselves, and the
/// other reserved host records.
pub fn is_attestable_ns(ns: &str) -> bool {
    ns != ATTEST_NS && ns != areev_core::authz::AUTHZ_NS && ns != areev_core::authz::HARNESS_NS
}

#[cfg(test)]
mod tests {
    use super::*;
    use areev_core::format::deserialize::deserialize_blob;
    use areev_core::format::serialize::serialize_grain;

    fn signer() -> Signer {
        Signer::from_seed([7u8; 32])
    }

    #[test]
    fn sign_is_deterministic_and_verifies() {
        let s = signer();
        let h = Hash::from_bytes(&[1u8; 32]);
        assert_eq!(s.sign(&h), s.sign(&h));
        let obs = s.attestation_for(&h, 1_000);
        let (blob, _) = serialize_grain(&obs).unwrap();
        let view = deserialize_blob(&blob).unwrap();
        let att = parse_attestation(&view).unwrap().unwrap();
        assert_eq!(att.attests, h);
        assert_eq!(att.key_id, s.key_id());
        let trusted = TrustedAuthors::single(&s, AttestPolicy::Verify);
        assert_eq!(trusted.check(&att), Verdict::Valid);
    }

    #[test]
    fn attestation_grain_is_a_pure_function_of_key_and_hash() {
        let s = signer();
        let h = Hash::from_bytes(&[2u8; 32]);
        let (_, a) = serialize_grain(&s.attestation_for(&h, 5)).unwrap();
        let (_, b) = serialize_grain(&Signer::from_seed([7u8; 32]).attestation_for(&h, 5)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tampered_signature_is_invalid_and_unknown_key_is_not() {
        let s = signer();
        let h = Hash::from_bytes(&[3u8; 32]);
        let (blob, _) = serialize_grain(&s.attestation_for(&h, 1)).unwrap();
        let view = deserialize_blob(&blob).unwrap();
        let mut att = parse_attestation(&view).unwrap().unwrap();
        att.sig[0] ^= 0x80;
        let trusted = TrustedAuthors::single(&s, AttestPolicy::Verify);
        assert!(matches!(trusted.check(&att), Verdict::Invalid(_)));
        let other = TrustedAuthors::single(&Signer::from_seed([9u8; 32]), AttestPolicy::Verify);
        assert_eq!(other.check(&att), Verdict::UnknownKey);
    }

    #[test]
    fn trusted_authors_document_round_trips_and_checks_ids() {
        let s = signer();
        let json = format!(
            r#"{{"version":1,"keys":{{"{}":"{}"}},"policy":"require"}}"#,
            s.key_id(),
            s.public_key_hex()
        );
        let t = TrustedAuthors::from_json(&json).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.policy(), AttestPolicy::Require);
        let wrong_id = format!(r#"{{"keys":{{"deadbeefdeadbeef":"{}"}}}}"#, s.public_key_hex());
        let err = TrustedAuthors::from_json(&wrong_id).unwrap_err();
        assert!(err.to_string().starts_with("CRY-E004"), "{err}");
        let bad_policy = format!(
            r#"{{"keys":{{"{}":"{}"}},"policy":"maybe"}}"#,
            s.key_id(),
            s.public_key_hex()
        );
        assert!(TrustedAuthors::from_json(&bad_policy).is_err());
        assert!(Signer::from_seed_hex("zz").is_err());
        assert_eq!(TrustedAuthors::from_json("{}").unwrap().policy(), AttestPolicy::Verify);
    }

    #[test]
    fn non_attestation_grains_are_none_and_malformed_ones_are_errors() {
        let mut obs = Observation::new("k", "not-an-attestation");
        obs.common.namespace = Some("shared".into());
        let (blob, _) = serialize_grain(&obs).unwrap();
        assert!(parse_attestation(&deserialize_blob(&blob).unwrap()).is_none());

        let mut claimed = Observation::new("k", OBSERVER_TYPE);
        claimed.common.namespace = Some(ATTEST_NS.into());
        let (blob, _) = serialize_grain(&claimed).unwrap();
        let r = parse_attestation(&deserialize_blob(&blob).unwrap()).unwrap();
        assert!(r.unwrap_err().to_string().starts_with("CRY-E002"));
    }
}
