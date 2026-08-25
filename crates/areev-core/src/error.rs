use std::fmt;

/// Content-addressed SHA-256 hash (32 bytes, displayed as lowercase hex).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Create a hash from a fixed-size 32-byte array (compile-time safe).
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Hash(*bytes)
    }

    /// Create a hash from a variable-length byte slice (fallible).
    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 32 {
            return Err(AreevError::Format(format!(
                "hash requires 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes[..32]);
        Ok(Hash(arr))
    }

    pub fn from_hex(hex_str: &str) -> Result<Self> {
        let bytes = hex::decode(hex_str)
            .map_err(|e| AreevError::Format(format!("invalid hex hash: {}", e)))?;
        if bytes.len() != 32 {
            return Err(AreevError::Format(format!(
                "hash must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        Ok(Self::from_bytes(&bytes.try_into().unwrap()))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", &self.to_hex()[..16])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl serde::Serialize for Hash {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Hash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// All errors in areev-core.
#[derive(Debug)]
pub enum AreevError {
    NotFound(Hash),
    Format(String),
    Validation(String),
    Serialization(String),
    ToolRenderUnsupported(String),
    Storage(String),
    /// Another writer holds this memory (single-writer-per-memory is
    /// enforced, not advisory, on backends that can arbitrate it).
    StoreBusy(String),
    /// The connection to a backing store asks for transport encryption this
    /// build cannot provide. Its own code because the alternative — reporting
    /// a generic validation failure — reads as a typo in the DSN, when what
    /// actually happened is that a *refusal to downgrade to plaintext* saved
    /// the operator from an unencrypted connection they did not ask for.
    TlsUnavailable(String),
    /// A write was attempted through a handle opened with `read_only: true`
    /// (`AreevOptions::read_only`). Refused at the store layer on BOTH
    /// backends — on postgres this is what stands between a least-privilege
    /// SELECT-only role and a raw `42501 permission denied`; on the embedded
    /// backend there is no privilege system to fail against, so the store
    /// enforces the same contract itself, which is what lets one conformance
    /// case cover both.
    ReadOnly(String),
    /// A read-only open could not verify the schema/tables it expected to
    /// find (postgres only — the embedded backend bootstraps its own file
    /// regardless of `read_only`). Distinct from [`Storage`](Self::Storage)
    /// because the fix differs: "schema absent" needs someone to create and
    /// migrate it; "schema present but not initialized" needs the owning
    /// role to open it read-write once to finish bootstrap. A read-only role
    /// can do neither itself — that is the whole point of the least-privilege
    /// grant — so the message says which one it is rather than surfacing the
    /// raw permission-denied Postgres gives for `CREATE SCHEMA`/DDL.
    ReadOnlyOpenFailed(String),
    SupersessionConflict(Hash),
    /// A supersession-chain walk (`Areev::supersession_chain`) did not reach
    /// a root within the bounded hop count. Real edit histories terminate in
    /// a handful of hops; exceeding the bound means the `supersedes` links
    /// are corrupt (e.g. cyclic) rather than merely long, so the walk fails
    /// loudly instead of looping the process forever.
    SupersessionChainTooDeep(Hash),
    CryptoError(String),
    AccumulateRetryExhausted,
    AccumulateInternal(String),
    AccumulateBackpressureRejected,
    Internal(String),
    /// A verb the session's grants don't cover (`authz::AuthzSet::check`).
    AuthzDenied(String),
    /// A principal name no credential authenticates.
    AuthzUnknownPrincipal(String),
    /// The credential map failed to load or validate (fail closed).
    AuthzConfigInvalid(String),
    /// A presented bearer token matched no credential. Deliberately carries
    /// no payload: a refused secret must never reach a log line.
    AuthzTokenUnrecognized,
}

impl AreevError {
    /// Stable machine-readable error code in `DOMAIN-Ennn` form (see the
    /// repo-root `ERROR_CODES.md` registry). Every `Display` string begins
    /// with this code, so a user who reports the leading token points us at
    /// the exact variant and subsystem. **Codes are append-only debugging
    /// handles — never renumber or reuse an existing one.**
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "MEM-E001",
            Self::SupersessionConflict(_) => "MEM-E002",
            Self::SupersessionChainTooDeep(_) => "STO-E006",
            Self::ToolRenderUnsupported(_) => "MEM-E110",
            Self::Format(_) => "FMT-E001",
            Self::Serialization(_) => "FMT-E002",
            Self::Validation(_) => "VAL-E001",
            Self::Storage(_) => "STO-E001",
            Self::StoreBusy(_) => "STO-E002",
            Self::TlsUnavailable(_) => "STO-E003",
            Self::ReadOnly(_) => "STO-E004",
            Self::ReadOnlyOpenFailed(_) => "STO-E005",
            Self::CryptoError(_) => "CRY-E001",
            // These originate in CAL ACCUMULATE semantics and bubble up
            // through the store, so they keep their CAL-domain codes.
            Self::AccumulateRetryExhausted => "CAL-E083",
            Self::AccumulateInternal(_) => "CAL-E084",
            Self::AccumulateBackpressureRejected => "CAL-E085",
            Self::Internal(_) => "SYS-E001",
            Self::AuthzDenied(_) => "AUT-E001",
            Self::AuthzUnknownPrincipal(_) => "AUT-E002",
            Self::AuthzConfigInvalid(_) => "AUT-E003",
            Self::AuthzTokenUnrecognized => "AUT-E004",
        }
    }
}

impl std::fmt::Display for AreevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Invariant: every arm's message starts with `self.code()` — pinned by
        // `code_prefixes_every_display` in the tests below.
        match self {
            Self::NotFound(h) => write!(f, "MEM-E001: grain not found: {h}"),
            Self::SupersessionConflict(h) => write!(f, "MEM-E002: already superseded: {h}"),
            Self::SupersessionChainTooDeep(h) => write!(
                f,
                "STO-E006: supersession chain from {h} did not terminate within the bounded walk — the supersedes links may be cyclic or corrupt"
            ),
            Self::ToolRenderUnsupported(m) => write!(f, "MEM-E110: tool render unsupported: {m}"),
            Self::Format(m) => write!(f, "FMT-E001: format error: {m}"),
            Self::Serialization(m) => write!(f, "FMT-E002: serialization error: {m}"),
            Self::Validation(m) => write!(f, "VAL-E001: validation error: {m}"),
            Self::Storage(m) => write!(f, "STO-E001: storage error: {m}"),
            Self::StoreBusy(m) => write!(f, "STO-E002: store busy: {m}"),
            Self::TlsUnavailable(m) => write!(f, "STO-E003: {m}"),
            Self::ReadOnly(m) => write!(f, "STO-E004: refusing write on a read-only memory: {m}"),
            Self::ReadOnlyOpenFailed(m) => write!(f, "STO-E005: {m}"),
            Self::CryptoError(m) => write!(f, "CRY-E001: crypto error: {m}"),
            Self::AccumulateRetryExhausted => write!(f, "CAL-E083: ACCUMULATE retry budget exhausted"),
            Self::AccumulateInternal(m) => write!(f, "CAL-E084: ACCUMULATE internal failure: {m}"),
            Self::AccumulateBackpressureRejected => write!(f, "CAL-E085: ACCUMULATE backpressure: inflight cap exceeded"),
            Self::Internal(m) => write!(f, "SYS-E001: internal error: {m}"),
            Self::AuthzDenied(m) => write!(f, "AUT-E001: authorization denied: {m}"),
            Self::AuthzUnknownPrincipal(p) => write!(f, "AUT-E002: unknown principal: {p}"),
            Self::AuthzConfigInvalid(m) => write!(f, "AUT-E003: {m}"),
            Self::AuthzTokenUnrecognized => write!(f, "AUT-E004: token not recognized"),
        }
    }
}

impl std::error::Error for AreevError {}

pub type Result<T> = std::result::Result<T, AreevError>;

#[cfg(test)]
mod error_code_tests {
    use super::*;

    /// One representative instance of every variant — extend when adding one.
    fn all_variants() -> Vec<AreevError> {
        let h = Hash::from_bytes(&[0u8; 32]);
        vec![
            AreevError::NotFound(h),
            AreevError::SupersessionConflict(h),
            AreevError::SupersessionChainTooDeep(h),
            AreevError::ToolRenderUnsupported("x".into()),
            AreevError::Format("x".into()),
            AreevError::Serialization("x".into()),
            AreevError::Validation("x".into()),
            AreevError::Storage("x".into()),
            AreevError::StoreBusy("x".into()),
            AreevError::TlsUnavailable("x".into()),
            AreevError::ReadOnly("x".into()),
            AreevError::ReadOnlyOpenFailed("x".into()),
            AreevError::CryptoError("x".into()),
            AreevError::AccumulateRetryExhausted,
            AreevError::AccumulateInternal("x".into()),
            AreevError::AccumulateBackpressureRejected,
            AreevError::Internal("x".into()),
            AreevError::AuthzDenied("x".into()),
            AreevError::AuthzUnknownPrincipal("x".into()),
            AreevError::AuthzConfigInvalid("x".into()),
            AreevError::AuthzTokenUnrecognized,
        ]
    }

    /// The reported code must be the leading token of the message, so a user
    /// pasting either gives us the same handle.
    #[test]
    fn code_prefixes_every_display() {
        for e in all_variants() {
            let msg = e.to_string();
            let code = e.code();
            assert!(
                msg.starts_with(&format!("{code}: ")),
                "`{msg}` must start with its code `{code}`"
            );
        }
    }

    /// Every code matches the `DOMAIN-Ennn` standard (see ERROR_CODES.md):
    /// a 3-letter uppercase domain, `-E`, then digits.
    #[test]
    fn codes_follow_the_repo_standard() {
        for e in all_variants() {
            let c = e.code();
            let (domain, num) = c.split_once("-E").unwrap_or_else(|| panic!("bad code: {c}"));
            assert_eq!(domain.len(), 3, "{c}: domain must be 3 letters");
            assert!(domain.chars().all(|ch| ch.is_ascii_uppercase()), "{c}: domain uppercase");
            assert!(!num.is_empty() && num.chars().all(|ch| ch.is_ascii_digit()), "{c}: numeric suffix");
        }
    }
}
