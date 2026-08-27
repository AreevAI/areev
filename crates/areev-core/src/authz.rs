//! Authorization primitives — principals, verbs, grants, and the host-side
//! credential map.
//!
//! The model (design of record: `docs/cal-all-you-need-proposal.md`, D2–D4):
//! *policy* (who may do what) lives **in the memory file** as grant grains,
//! scoped per namespace; *credentials* (who is this caller) live host-side in
//! a credential map that holds **no policy and no raw secrets** — tokens are
//! referenced by SHA-256 or by env-var name. A memory with no grant grains
//! grants nothing to anyone but the owner session (fail closed); the owner
//! session — a local open with no principal asserted — is the implicit
//! superuser, so the single-user path never meets any of this.
//!
//! This module is the shared vocabulary only. Building an [`AuthzSet`] from a
//! file's grant grains is the store's job; enforcing it at dispatch is the
//! facade's and the surfaces'.

use crate::error::{AreevError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// The reserved namespace grant grains live in. The OMS 1.6 spec draft
/// (§12.6) is the source of truth for this name — it follows the spec's
/// `agent:identity` / `agent:recommendations` reserved-namespace precedent.
pub const AUTHZ_NS: &str = "agent:authz";
/// Reserved namespace for reproducible run and assembly manifests.
pub const HARNESS_NS: &str = "agent:harness";
/// Relation carried by a grant grain (OMS `PERMISSION` category).
pub const REL_PERMITS: &str = "mg:permits";

/// One operation class — the unit of granting, `GRANT SELECT`-style.
///
/// The string forms (`read`, `loop.review`, …) are the wire/CAL vocabulary;
/// they appear in grant-grain objects and eventually in `GRANT` statements,
/// so they are frozen once the spec ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    Read,
    Write,
    Supersede,
    Delete,
    Erase,
    LoopRun,
    LoopReview,
    LoopApply,
    Admin,
    /// D5 (governed-agents §6.8): start/resume/fork a workflow run —
    /// Control-tier: it spends budgets and executes effects, so it is not
    /// plain `write`.
    RunExecute,
    /// D5: answer a `requires_action` Client ask. This IS the approval
    /// boundary — Control-tier, and the driver additionally refuses
    /// responder == triggering principal on approval asks (separation of
    /// duties, mirroring the loop's self-approval block).
    RunRespond,
    /// D5: cancel a run. Deliberately LOW-tier and broadly grantable — the
    /// brake must never be blocked by missing privilege (the kill-switch
    /// SLA depends on it).
    RunCancel,
}

impl Verb {
    /// Every verb, in canonical display order. Append-only, like error
    /// codes: the string forms live in grant grains that replicate.
    pub const ALL: [Verb; 12] = [
        Verb::Read,
        Verb::Write,
        Verb::Supersede,
        Verb::Delete,
        Verb::Erase,
        Verb::LoopRun,
        Verb::LoopReview,
        Verb::LoopApply,
        Verb::Admin,
        Verb::RunExecute,
        Verb::RunRespond,
        Verb::RunCancel,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Write => "write",
            Verb::Supersede => "supersede",
            Verb::Delete => "delete",
            Verb::Erase => "erase",
            Verb::LoopRun => "loop.run",
            Verb::LoopReview => "loop.review",
            Verb::LoopApply => "loop.apply",
            Verb::Admin => "admin",
            Verb::RunExecute => "run.execute",
            Verb::RunRespond => "run.respond",
            Verb::RunCancel => "run.cancel",
        }
    }

    pub fn parse(s: &str) -> Result<Verb> {
        match s {
            "read" => Ok(Verb::Read),
            "write" => Ok(Verb::Write),
            "supersede" => Ok(Verb::Supersede),
            "delete" => Ok(Verb::Delete),
            "erase" => Ok(Verb::Erase),
            "loop.run" => Ok(Verb::LoopRun),
            "loop.review" => Ok(Verb::LoopReview),
            "loop.apply" => Ok(Verb::LoopApply),
            "admin" => Ok(Verb::Admin),
            "run.execute" => Ok(Verb::RunExecute),
            "run.respond" => Ok(Verb::RunRespond),
            "run.cancel" => Ok(Verb::RunCancel),
            other => Err(AreevError::Validation(format!(
                "unknown verb {other:?} — one of: read, write, supersede, delete, erase, \
                 loop.run, loop.review, loop.apply, admin, run.execute, run.respond, \
                 run.cancel"
            ))),
        }
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of verbs allowed on a set of namespaces, inside one memory.
/// The memory axis is implicit — a grant lives in the file it governs (D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub verbs: Vec<Verb>,
    /// Governed namespaces. `["*"]` (or empty) = every namespace.
    pub namespaces: Vec<String>,
}

impl Grant {
    pub fn covers(&self, verb: Verb, ns: &str) -> bool {
        self.verbs.contains(&verb)
            && (self.namespaces.is_empty()
                || self.namespaces.iter().any(|n| n == "*" || n == ns))
    }

    /// The canonical object string a grant grain carries:
    /// `read,write ON caller,shared` / `delete ON *`. Normative per OMS 1.6
    /// §12.6: lowercase, comma-separated, **lexicographically sorted** verbs
    /// and namespaces, duplicates dropped — so two implementations writing
    /// the same grant produce the same content address.
    pub fn to_object_string(&self) -> String {
        let mut verbs: Vec<&str> = self.verbs.iter().map(Verb::as_str).collect();
        verbs.sort_unstable();
        verbs.dedup();
        let ns = if self.namespaces.is_empty() {
            "*".to_string()
        } else {
            let mut ns: Vec<&str> = self.namespaces.iter().map(String::as_str).collect();
            ns.sort_unstable();
            ns.dedup();
            ns.join(",")
        };
        format!("{} ON {}", verbs.join(","), ns)
    }

    pub fn from_object_string(s: &str) -> Result<Grant> {
        let (verbs_part, ns_part) = s.split_once(" ON ").ok_or_else(|| {
            AreevError::Validation(format!(
                "malformed grant object {s:?} — expected \"<verbs> ON <namespaces>\""
            ))
        })?;
        let mut verbs = Vec::new();
        for v in verbs_part.split(',') {
            let v = Verb::parse(v.trim())?;
            if !verbs.contains(&v) {
                verbs.push(v);
            }
        }
        if verbs.is_empty() {
            return Err(AreevError::Validation(format!(
                "grant object {s:?} names no verbs"
            )));
        }
        let mut namespaces = Vec::new();
        for n in ns_part.split(',') {
            let n = n.trim();
            if n.is_empty() {
                return Err(AreevError::Validation(format!(
                    "grant object {s:?} has an empty namespace"
                )));
            }
            if !namespaces.iter().any(|x| x == n) {
                namespaces.push(n.to_string());
            }
        }
        Ok(Grant { verbs, namespaces })
    }
}

/// The resolved rights of one session: a principal plus the grants that
/// cover it. Every dispatch layer asks the same question:
/// [`AuthzSet::check`].
#[derive(Debug, Clone)]
pub struct AuthzSet {
    principal: String,
    owner: bool,
    grants: Vec<Grant>,
}

impl AuthzSet {
    /// The implicit-superuser session: a local open with no principal
    /// asserted (`root@localhost`). Every verb on every namespace.
    pub fn owner(principal: impl Into<String>) -> Self {
        AuthzSet {
            principal: principal.into(),
            owner: true,
            grants: Vec::new(),
        }
    }

    /// A restricted session: only what the grants cover. Zero grants =
    /// nothing (fail closed).
    pub fn restricted(principal: impl Into<String>, grants: Vec<Grant>) -> Self {
        AuthzSet {
            principal: principal.into(),
            owner: false,
            grants,
        }
    }

    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn is_owner(&self) -> bool {
        self.owner
    }

    pub fn allows(&self, verb: Verb, ns: &str) -> bool {
        self.owner || self.grants.iter().any(|g| g.covers(verb, ns))
    }

    /// The one enforcement question. The refusal names the verb, the
    /// resource, and the principal — the pieces a granting admin needs.
    /// (Once `GRANT` parses, the message will also spell the statement that
    /// fixes it — not before, to avoid pointing at unshipped syntax.)
    pub fn check(&self, verb: Verb, ns: &str) -> Result<()> {
        if self.allows(verb, ns) {
            return Ok(());
        }
        Err(AreevError::AuthzDenied(format!(
            "principal {} lacks {verb} on namespace {ns:?}",
            self.principal
        )))
    }
}

/// The observer kind a principal label implies (`"agent"` / `"human"`),
/// used for audit stamping wherever no credential record declares one. The
/// answer always derives from the host-held label — never from statement or
/// request text, which must not be able to claim humanity.
pub fn observer_kind(principal: &str) -> &'static str {
    for prefix in ["agent:", "bot:", "job:", "svc:", "engine:"] {
        if principal.starts_with(prefix) {
            return "agent";
        }
    }
    "human"
}

/// A verifiable, non-disclosing reference to an erased identity, for audit
/// targets: the first 16 hex of SHA-256 over the identity string.
///
/// **Why not the identity itself.** An audit grain is immutable, replicates,
/// and lands in archives. Writing the raw identifier into it re-introduces
/// exactly the reference the erasure just removed — the erased subject stays
/// recallable from `agent:authz` forever, un-erasable by the subject
/// selector (which never matches `subject:<id> ns:<ns>` as a partition key),
/// and travels into every bundle and segment. That is a right-to-erasure
/// failure hiding inside the accountability record.
///
/// A fingerprint keeps both properties: given a candidate identity anyone
/// can recompute the digest and **verify** that a specific audit record is
/// about that person (answering "prove you erased me"), but the log cannot
/// be mined to enumerate who was erased. The human-readable reference — the
/// ticket or request number — belongs in BECAUSE, which the operator
/// controls and which names a *request*, not a data subject.
///
/// Truncated to 64 bits of digest: this is a correlation handle, not a
/// security boundary (identity strings are low-entropy, so a determined
/// attacker with a candidate list can always confirm guesses — which is the
/// same property that makes verification work).
pub fn subject_fingerprint(identity: &str) -> String {
    let digest = Sha256::digest(identity.as_bytes());
    hex_lower(&digest[..8])
}

/// Constant-time byte comparison — avoids leaking a bearer token through
/// response timing. A length mismatch fails fast (token length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Build the Tier-2 audit Observation for one destructive execution — the
/// accountability record (GDPR Art. 5(2)/30) that `areev audit export`
/// emits.
///
/// **One builder, every surface.** CAL destruction, the CLI's
/// `forget-subject`/`purge-older-than`, and anything else that destroys
/// must produce byte-identical audit shapes, or the evidence export becomes
/// a union of dialects. Note the deliberate asymmetry with REQ-ERASE-5: the
/// *engine* (`Areev::forget_subject`) still writes no audit grain of its
/// own — a library caller owns its own logging — but the surfaces a human
/// or agent actually invokes are hosts, and hosts audit.
///
/// `target` describes what was destroyed in the surface's own vocabulary:
/// `hash:<hex>` (a content address of already-deleted content — not identity
/// material), `subject:<fp> ns:<ns>` where `<fp>` is a
/// [`subject_fingerprint`] (**never** the raw identity — see that function
/// for why), or `older_than:<n>d ns:<ns>` (an age, no identity at all).
pub fn audit_observation(
    principal: &str,
    verb: &str,
    target: &str,
    because: Option<&str>,
    count: usize,
    now_ms: i64,
) -> crate::types::Observation {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Process-static: two identical erasures in the same millisecond must
    // stay two records, not collapse into one content address.
    static AUDIT_SEQ: AtomicU64 = AtomicU64::new(0);
    let mut obs = crate::types::Observation {
        observer_id: principal.to_string(),
        observer_type: observer_kind(principal).to_string(),
        subject: Some(target.to_string()),
        object: Some(verb.to_string()),
        observer_model: None,
        frame_id: Some(format!(
            "tier2:{now_ms}:{}",
            AUDIT_SEQ.fetch_add(1, Ordering::Relaxed)
        )),
        sync_group: None,
        observation_mode: None,
        observation_scope: None,
        compression_ratio: None,
        common: Default::default(),
    };
    obs.common.namespace = Some(AUTHZ_NS.to_string());
    obs.common.created_at = Some(now_ms);
    obs.common.context = Some(serde_json::json!({
        "audit": "tier2",
        "verb": verb,
        "target": target,
        "because": because.unwrap_or(""),
        "grains_erased": count,
        // Names the scheme so a verifier knows how to recompute a subject
        // fingerprint years later, without reading our source.
        "subject_ref": "sha256-64/hex",
    }));
    obs
}

/// The prefix every Areev-minted bearer token carries.
///
/// Three jobs, all of which a bare random string cannot do: secret scanners
/// (GitHub's, GitLab's, and every commercial one) can be taught a single
/// regex for it; a human who finds one in a paste knows what it opens and
/// who to tell; and the server can reject an obviously-malformed credential
/// before it reaches the constant-time scan. Modelled on GitHub's `ghp_`.
///
/// It is deliberately NOT required — an operator's existing token keeps
/// working — but [`token_is_minted`] is what the startup banner uses to warn
/// that a credential's entropy is unknown.
pub const TOKEN_PREFIX: &str = "areev_pat_";

/// Whether `token` has the shape [`encode_token_body`] produces: the prefix plus at
/// least 32 base32 characters (160 bits — the floor below which a digest in
/// a shared file starts being worth grinding).
///
/// A `true` here means the entropy is known-good *because Areev generated
/// it*. It is not a security check — an attacker can spell the prefix — it
/// is an operator-hygiene signal, which is why nothing refuses on a `false`.
pub fn token_is_minted(token: &str) -> bool {
    match token.strip_prefix(TOKEN_PREFIX) {
        Some(body) => {
            body.len() >= 32 && body.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        }
        None => false,
    }
}

/// Render 32 random bytes as the token body. Split out from generation so
/// the CSPRNG stays in the host that mints (the CLI) while the *format* —
/// the thing every reader must agree on — lives here with its validator.
///
/// Base32-ish over `[a-z0-9]`: case-insensitive to transcribe, safe in a URL,
/// a shell word, and a JSON string without escaping.
pub fn encode_token_body(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    // 32 bytes = 256 bits → 51 full 5-bit groups (255 bits); the last bit is
    // dropped rather than padded, leaving 255 bits of entropy in 51 chars.
    let mut out = String::with_capacity(51);
    let mut acc: u16 = 0;
    let mut bits = 0u8;
    for &b in bytes {
        acc = (acc << 8) | b as u16;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((acc >> bits) & 0x1f) as usize;
            out.push(ALPHABET[idx] as char);
        }
    }
    out
}

/// The host-side credential map (`areev-auth.json`): tokens → principal
/// names, nothing else. No verbs, no namespaces, no raw secrets — a token is
/// referenced by its SHA-256 or by the env var that holds it, so the file is
/// inert if stolen or synced.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialMap {
    pub version: u32,
    #[serde(default)]
    pub tokens: Vec<CredentialEntry>,
    /// IdP group → principal (A2). The same job the `tokens` list already
    /// does — map an external identifier onto a principal the FILE grants
    /// rights to — for the axis SSO actually scales on: without it every
    /// person behind the proxy needs their own grant grain, which is the
    /// administrative burden SSO exists to remove.
    ///
    /// A group-derived principal is a ROLE, not a person. That is why it can
    /// never answer a HITL approval, even under `--sso-approvals allow`:
    /// "someone in engineering approved this" is not an audit record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialEntry {
    /// Stable, operator-chosen name for THIS credential — not the principal.
    ///
    /// Two tokens for one principal (a laptop and a CI runner, or an old and
    /// a new one mid-rotation) are otherwise indistinguishable: revoking one
    /// means identifying a line by its digest, and no log line can ever name
    /// which credential acted. The id appears on successful auth and in
    /// `whoami`; it is **never** echoed on a failure, because a refused
    /// secret must not confirm which credential it nearly matched.
    ///
    /// **Optional, with a derived fallback** ([`CredentialEntry::id`]) — an
    /// `areev-auth.json` written before ids existed must keep working across
    /// an upgrade. A console that refuses to start because a new field is
    /// missing is a worse security outcome than an ugly default: it gets
    /// fixed by rolling back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Free-text note for humans (`"CI runner, rotates quarterly"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Lowercase hex SHA-256 of the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Name of the environment variable holding the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// The principal this credential authenticates as.
    pub principal: String,
    /// Per-memory scope (the enterprise plane's rule): when set, this
    /// credential authenticates ONLY on services whose memory label is in
    /// the list — one auth file shared across server instances, each token
    /// reaching only its memories. `None` = every memory (the common
    /// single-memory deployment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memories: Option<Vec<String>>,
    /// Optional expiry (ISO-8601; `"2026-12-31T23:59:59Z"`). Past it, the
    /// credential authenticates nobody — indistinguishably from an unknown
    /// token, so an expired credential cannot be used to probe which ids
    /// exist.
    ///
    /// Deliberately OPTIONAL. A mandatory lifetime would break a homelab
    /// console at 3am for a threat model it does not have; `areev auth mint
    /// --expires 90d` is the documented path for the deployments that want
    /// one, and the startup banner names credentials expiring within 14 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl CredentialEntry {
    /// This credential's effective id: the operator's `id` when set, else a
    /// stable one derived from what identifies the credential anyway.
    ///
    /// Derivation is deliberately *not* positional (`token-0`, `token-1`):
    /// an index shifts when an unrelated line is added, so `areev auth
    /// revoke --id token-1` would eventually revoke the wrong credential —
    /// the exact failure an id exists to prevent. A digest prefix and an
    /// env-var name are both properties of the credential itself, so they
    /// survive reordering.
    pub fn id(&self) -> String {
        match (self.id.as_deref(), &self.sha256, &self.env) {
            (Some(id), _, _) => id.to_string(),
            // Not a secret: it is a prefix of a digest the file already
            // publishes in full, one line up.
            (None, Some(h), _) => h.chars().take(8).collect::<String>().to_ascii_lowercase(),
            (None, None, Some(var)) => format!("env:{var}"),
            (None, None, None) => "unnamed".to_string(),
        }
    }

    /// Expiry as epoch ms, or `None` when the entry never expires.
    ///
    /// An `expires_at` that does not parse is treated as **already expired**
    /// (`Some(i64::MIN)`), not as "no expiry". `from_json` refuses such a map
    /// outright so this should be unreachable — but if it ever is reached,
    /// failing closed is the only safe reading of a lifetime nobody can
    /// interpret.
    fn expires_at_ms(&self) -> Option<i64> {
        self.expires_at
            .as_deref()
            .map(|s| crate::time::iso8601_to_ms(s).unwrap_or(i64::MIN))
    }

    /// Whether this credential is expired at `now_ms`.
    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        self.expires_at_ms().is_some_and(|exp| now_ms >= exp)
    }
}

impl CredentialMap {
    /// Parse and validate. Fail closed: unknown keys, a bad version, an
    /// entry with both or neither credential form, or a malformed digest all
    /// refuse the whole map.
    pub fn from_json(s: &str) -> Result<CredentialMap> {
        let map: CredentialMap = serde_json::from_str(s)
            .map_err(|e| AreevError::AuthzConfigInvalid(format!("credential map: {e}")))?;
        if map.version != 1 {
            return Err(AreevError::AuthzConfigInvalid(format!(
                "credential map: unsupported version {} (expected 1)",
                map.version
            )));
        }
        let mut seen_ids: Vec<String> = Vec::with_capacity(map.tokens.len());
        for (i, t) in map.tokens.iter().enumerate() {
            if t.principal.trim().is_empty() {
                return Err(AreevError::AuthzConfigInvalid(format!(
                    "credential map: entry {i} has an empty principal"
                )));
            }
            // The id is what makes one credential revocable and one log line
            // attributable. It may be derived, but it must be legible and
            // unique — a duplicate makes `areev auth revoke --id` ambiguous,
            // which is the one thing an id exists to prevent.
            if let Some(explicit) = t.id.as_deref() {
                if explicit.trim().is_empty() {
                    return Err(AreevError::AuthzConfigInvalid(format!(
                        "credential map: entry {i} ({}) has an empty \"id\" — omit the field \
                         to get a derived one, or give it a name",
                        t.principal
                    )));
                }
                if !explicit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
                {
                    return Err(AreevError::AuthzConfigInvalid(format!(
                        "credential map: entry {i} id {explicit:?} must be alphanumeric with \
                         -, _ or . (it is printed in logs and passed to `areev auth revoke`)"
                    )));
                }
            }
            let id = t.id();
            if seen_ids.contains(&id) {
                return Err(AreevError::AuthzConfigInvalid(format!(
                    "credential map: duplicate id {id:?} — revoking it would be ambiguous, \
                     which is the one thing an id exists to prevent"
                )));
            }
            seen_ids.push(id.clone());
            // An unparseable lifetime is refused at LOAD, not silently
            // treated as "never expires" — the failure mode of a typo'd
            // expiry must be a console that will not start, never a
            // credential that outlives its intended window.
            if let Some(raw) = &t.expires_at {
                if crate::time::iso8601_to_ms(raw).is_none() {
                    return Err(AreevError::AuthzConfigInvalid(format!(
                        "credential map: entry {i} ({id}) has an unparseable \"expires_at\" \
                         {raw:?} — expected ISO-8601, e.g. \"2026-12-31T23:59:59Z\""
                    )));
                }
            }
            match (&t.sha256, &t.env) {
                (Some(_), Some(_)) | (None, None) => {
                    return Err(AreevError::AuthzConfigInvalid(format!(
                        "credential map: entry {i} ({}) must have exactly one of \
                         \"sha256\" or \"env\"",
                        t.principal
                    )));
                }
                (Some(h), None) => {
                    if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(AreevError::AuthzConfigInvalid(format!(
                            "credential map: entry {i} ({}) sha256 must be 64 hex chars",
                            t.principal
                        )));
                    }
                }
                (None, Some(v)) => {
                    if v.trim().is_empty() {
                        return Err(AreevError::AuthzConfigInvalid(format!(
                            "credential map: entry {i} ({}) has an empty \"env\" variable name",
                            t.principal
                        )));
                    }
                }
            }
            if let Some(memories) = &t.memories {
                if memories.is_empty() || memories.iter().any(|m| m.trim().is_empty()) {
                    return Err(AreevError::AuthzConfigInvalid(format!(
                        "credential map: entry {i} ({}) has an empty \"memories\" \
                         scope — omit the field to grant every memory",
                        t.principal
                    )));
                }
            }
        }
        Ok(map)
    }

    /// Resolve a presented bearer token to its principal. The error carries
    /// no part of the token — a refused secret must not leak into logs.
    ///
    /// Expiry is evaluated against the system clock; [`resolve_at`] takes the
    /// instant explicitly for tests.
    ///
    /// [`resolve_at`]: Self::resolve_at
    pub fn resolve(&self, presented: &str) -> Result<&str> {
        self.resolve_at(presented, crate::time::now_ms())
    }

    /// [`resolve`](Self::resolve) at an explicit instant.
    pub fn resolve_at(&self, presented: &str, now_ms: i64) -> Result<&str> {
        self.entry_at(presented, now_ms).map(|t| t.principal.as_str())
    }

    /// The matching, unexpired entry for `presented` — the shared core of
    /// every resolve path, so expiry and the empty-token rule cannot drift
    /// between them.
    fn entry_at(&self, presented: &str, now_ms: i64) -> Result<&CredentialEntry> {
        // An empty presented token can never authenticate. Without this, an
        // env-referenced credential whose variable is exported *empty*
        // (`Environment=AREEV_BOT_TOKEN=` in a unit file, `export VAR=` in a
        // wrapper) matches the empty string an `Authorization: Bearer `
        // header parses to, and every caller becomes that principal.
        if presented.is_empty() {
            return Err(AreevError::AuthzTokenUnrecognized);
        }
        let digest = hex::encode(Sha256::digest(presented.as_bytes()));
        // Constant-shape scan: every entry is examined whether or not an
        // earlier one matched, so an EXPIRED credential is not even
        // timing-distinguishable from an unknown one. (This is why expiry is
        // filtered after the scan rather than short-circuiting inside it.)
        let mut found: Option<&CredentialEntry> = None;
        for t in &self.tokens {
            let matched = match (&t.sha256, &t.env) {
                (Some(h), None) => h.eq_ignore_ascii_case(&digest),
                // The env var must actually hold a secret — an unset or
                // empty variable authenticates nobody.
                (None, Some(var)) => std::env::var(var)
                    .is_ok_and(|v| !v.trim().is_empty() && ct_eq(v.as_bytes(), presented.as_bytes())),
                _ => false,
            };
            if matched && found.is_none() {
                found = Some(t);
            }
        }
        match found {
            Some(t) if !t.is_expired_at(now_ms) => Ok(t),
            // Expired resolves exactly like unknown: same error, no mention
            // of the id, so a stale credential cannot be used to enumerate
            // which ids the map holds.
            _ => Err(AreevError::AuthzTokenUnrecognized),
        }
    }

    /// The credential id that authenticated `presented`, for the audit/log
    /// line on a SUCCESSFUL auth. Never call this on a failure path.
    pub fn resolve_id_at(&self, presented: &str, now_ms: i64) -> Option<String> {
        self.entry_at(presented, now_ms).ok().map(|t| t.id())
    }

    /// Resolve a token FOR ONE MEMORY: like [`resolve`](Self::resolve), but
    /// a credential carrying a `memories` scope only authenticates when
    /// `memory` is listed. The refusal is indistinguishable from an unknown
    /// token — a scoped credential must not confirm which memories exist.
    pub fn resolve_for_memory(&self, presented: &str, memory: &str) -> Result<&str> {
        self.resolve_for_memory_at(presented, memory, crate::time::now_ms())
    }

    /// [`resolve_for_memory`](Self::resolve_for_memory) at an explicit
    /// instant.
    pub fn resolve_for_memory_at(
        &self,
        presented: &str,
        memory: &str,
        now_ms: i64,
    ) -> Result<&str> {
        // Shares `entry_at` so the empty-token rule, the constant-shape scan
        // and expiry cannot drift between the two resolve paths — the bug
        // class where one caller honors an expiry the other ignores.
        let t = self.entry_at(presented, now_ms)?;
        match &t.memories {
            Some(list) if !list.iter().any(|m| m == memory) => {
                Err(AreevError::AuthzTokenUnrecognized)
            }
            _ => Ok(&t.principal),
        }
    }

    /// The credential id that authenticated `presented` on `memory`, for the
    /// success log line. `None` on any refusal.
    pub fn resolve_id_for_memory_at(
        &self,
        presented: &str,
        memory: &str,
        now_ms: i64,
    ) -> Option<String> {
        let t = self.entry_at(presented, now_ms).ok()?;
        match &t.memories {
            Some(list) if !list.iter().any(|m| m == memory) => None,
            _ => Some(t.id()),
        }
    }

    /// Credentials expiring within `window_ms` of `now_ms` (and those already
    /// expired), for the startup banner. Returns `(id, expires_at)` pairs.
    ///
    /// A console that only reports an expiry at the moment it starts refusing
    /// is a console that reports it during an incident.
    pub fn expiring_within(&self, now_ms: i64, window_ms: i64) -> Vec<(String, String)> {
        self.tokens
            .iter()
            .filter_map(|t| {
                let raw = t.expires_at.as_deref()?;
                let exp = crate::time::iso8601_to_ms(raw)?;
                (exp <= now_ms + window_ms).then_some((t.id(), raw.to_string()))
            })
            .collect()
    }

    /// The principal an IdP group maps to, if any (A2).
    ///
    /// Group names are compared case-insensitively: directories are
    /// inconsistent about the case they emit (`Engineering` vs
    /// `engineering`), and a mapping that silently misses because of it
    /// fails *open* into whatever the identity alone was granted — which is
    /// the wrong direction for an authorization lookup to be sloppy in.
    pub fn principal_for_group(&self, group: &str) -> Option<&str> {
        let g = group.trim();
        self.groups.as_ref()?.iter().find_map(|(k, v)| {
            k.eq_ignore_ascii_case(g).then_some(v.as_str())
        })
    }

    /// Whether any credential authenticates as this principal — surfaces
    /// that require a *known* principal name use this to refuse typos early.
    pub fn knows_principal(&self, principal: &str) -> Result<()> {
        if self.tokens.iter().any(|t| t.principal == principal) {
            return Ok(());
        }
        Err(AreevError::AuthzUnknownPrincipal(principal.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_roundtrip_their_string_forms() {
        for v in Verb::ALL {
            assert_eq!(Verb::parse(v.as_str()).unwrap(), v);
        }
        assert!(Verb::parse("loop-run").is_err());
        assert!(Verb::parse("").is_err());
    }

    #[test]
    fn an_empty_env_credential_authenticates_nobody() {
        // The variable name itself must be non-empty…
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"env":"  ","principal":"agent:writer"}]}"#
        )
        .is_err());

        // …and a variable exported EMPTY must not match the empty string an
        // `Authorization: Bearer ` header parses to. Otherwise every caller
        // becomes `agent:writer`.
        std::env::set_var("AREEV_TEST_EMPTY_TOK", "");
        let map = CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"env":"AREEV_TEST_EMPTY_TOK","principal":"agent:writer"}]}"#,
        )
        .unwrap();
        assert!(map.resolve("").is_err(), "empty bearer must not authenticate");
        assert!(map.resolve("anything").is_err());

        // Unset behaves the same.
        std::env::remove_var("AREEV_TEST_EMPTY_TOK");
        assert!(map.resolve("").is_err());
    }

    #[test]
    fn grant_object_string_roundtrips() {
        let g = Grant {
            verbs: vec![Verb::Read, Verb::Write],
            namespaces: vec!["caller".into(), "shared".into()],
        };
        let s = g.to_object_string();
        assert_eq!(s, "read,write ON caller,shared");
        assert_eq!(Grant::from_object_string(&s).unwrap(), g);

        let all = Grant { verbs: vec![Verb::Erase], namespaces: vec!["*".into()] };
        assert_eq!(all.to_object_string(), "erase ON *");
        assert_eq!(
            Grant::from_object_string("erase ON *").unwrap().namespaces,
            vec!["*".to_string()]
        );

        assert!(Grant::from_object_string("read caller").is_err());
        assert!(Grant::from_object_string(" ON x").is_err());
        assert!(Grant::from_object_string("read ON ").is_err());
    }

    #[test]
    fn owner_allows_everything_restricted_fails_closed() {
        let owner = AuthzSet::owner("user:local");
        for v in Verb::ALL {
            assert!(owner.check(v, "any-ns").is_ok());
        }

        let none = AuthzSet::restricted("agent:bot", Vec::new());
        for v in Verb::ALL {
            assert!(none.check(v, "caller").is_err(), "{v} must be refused");
        }
    }

    #[test]
    fn grants_cover_exactly_what_they_say() {
        let set = AuthzSet::restricted(
            "agent:bot",
            vec![Grant {
                verbs: vec![Verb::Read, Verb::Write],
                namespaces: vec!["caller".into()],
            }],
        );
        assert!(set.check(Verb::Read, "caller").is_ok());
        assert!(set.check(Verb::Write, "caller").is_ok());
        assert!(set.check(Verb::Write, "shared").is_err());
        assert!(set.check(Verb::Delete, "caller").is_err());

        let star = AuthzSet::restricted(
            "job:sweep",
            vec![Grant { verbs: vec![Verb::Erase], namespaces: vec!["*".into()] }],
        );
        assert!(star.check(Verb::Erase, "anything").is_ok());
    }

    #[test]
    fn refusal_names_verb_namespace_and_principal_with_the_aut_code() {
        let set = AuthzSet::restricted("agent:bot", Vec::new());
        let err = set.check(Verb::Delete, "caller").unwrap_err();
        assert_eq!(err.code(), "AUT-E001");
        let msg = err.to_string();
        for needle in ["delete", "caller", "agent:bot"] {
            assert!(msg.contains(needle), "{msg:?} must name {needle}");
        }
    }

    const MAP: &str = r#"{
        "version": 1,
        "tokens": [
            { "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
              "principal": "user:anna" },
            { "env": "AREEV_TEST_BOT_TOKEN", "principal": "agent:bot" }
        ]
    }"#;

    #[test]
    fn credential_map_loads_and_resolves_by_sha256() {
        let map = CredentialMap::from_json(MAP).unwrap();
        // sha256("test") — the digest above.
        assert_eq!(map.resolve("test").unwrap(), "user:anna");
        assert!(map.knows_principal("user:anna").is_ok());
        assert_eq!(
            map.knows_principal("user:nobody").unwrap_err().code(),
            "AUT-E002"
        );
    }

    #[test]
    fn credential_map_resolves_by_env_var() {
        let map = CredentialMap::from_json(MAP).unwrap();
        // Unique var name per test binary run; set before resolve.
        std::env::set_var("AREEV_TEST_BOT_TOKEN", "s3cret");
        assert_eq!(map.resolve("s3cret").unwrap(), "agent:bot");
        std::env::remove_var("AREEV_TEST_BOT_TOKEN");
    }

    #[test]
    fn unrecognized_token_error_never_echoes_the_secret() {
        let map = CredentialMap::from_json(MAP).unwrap();
        let err = map.resolve("super-secret-value").unwrap_err();
        assert_eq!(err.code(), "AUT-E004");
        assert!(!err.to_string().contains("super-secret-value"));
    }

    #[test]
    fn credential_map_fails_closed() {
        // Unknown key.
        assert_eq!(
            CredentialMap::from_json(r#"{"version":1,"tokens":[],"roles":{}}"#)
                .unwrap_err()
                .code(),
            "AUT-E003"
        );
        // Wrong version.
        assert!(CredentialMap::from_json(r#"{"version":2,"tokens":[]}"#).is_err());
        // Both credential forms.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"sha256":"00","env":"X","principal":"p"}]}"#
        )
        .is_err());
        // Neither form.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"principal":"p"}]}"#
        )
        .is_err());
        // Malformed digest.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"sha256":"zz","principal":"p"}]}"#
        )
        .is_err());
        // Empty principal.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"env":"X","principal":"  "}]}"#
        )
        .is_err());
        // [A1] An id that cannot be printed or passed to `revoke`.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"env":"X","principal":"p","id":"has space"}]}"#
        )
        .is_err());
        // [A1] Duplicate ids make revocation ambiguous.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[
                {"env":"X","principal":"p","id":"dup"},
                {"env":"Y","principal":"q","id":"dup"}
            ]}"#
        )
        .is_err());
        // [A1] A typo'd lifetime must refuse the map, never read as "never
        // expires" — the failure mode of a bad expiry is a console that will
        // not start, not a credential that outlives its window.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"env":"X","principal":"p","expires_at":"soon"}]}"#
        )
        .is_err());
    }

    /// [A1] A map written before ids existed still loads — and every entry
    /// still gets a stable, non-positional id.
    #[test]
    fn pre_id_maps_load_and_derive_stable_ids() {
        let map = CredentialMap::from_json(
            r#"{"version":1,"tokens":[
                {"sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
                 "principal":"user:a"},
                {"env":"AREEV_LEGACY_TOK","principal":"user:b"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(map.tokens[0].id(), "9f86d081");
        assert_eq!(map.tokens[1].id(), "env:AREEV_LEGACY_TOK");

        // Non-positional: prepending an entry does not renumber the others,
        // which is what makes `revoke --id` safe.
        let grown = CredentialMap::from_json(
            r#"{"version":1,"tokens":[
                {"env":"AREEV_NEW_TOK","principal":"user:c"},
                {"sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
                 "principal":"user:a"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(grown.tokens[1].id(), "9f86d081");
    }

    /// [A1] Expiry refuses indistinguishably from an unknown token, and one
    /// credential expiring leaves its principal's others alone.
    #[test]
    fn expired_credentials_refuse_like_unknown_ones() {
        let map = CredentialMap::from_json(
            r#"{"version":1,"tokens":[
                {"sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
                 "principal":"user:a","id":"old","expires_at":"2026-01-01T00:00:00Z"},
                {"sha256":"fd61a03af4f77d870fc21e05e7e80678095c92d808cfb3b5c279ee04c74aca13",
                 "principal":"user:a","id":"new"}
            ]}"#,
        )
        .unwrap();
        let before = crate::time::iso8601_to_ms("2025-06-01T00:00:00Z").unwrap();
        let after = crate::time::iso8601_to_ms("2026-06-01T00:00:00Z").unwrap();

        // sha256("test") is the first digest; sha256("test3") the second.
        assert_eq!(map.resolve_at("test", before).unwrap(), "user:a");
        assert_eq!(map.resolve_id_at("test", before).as_deref(), Some("old"));

        // Past the expiry it is refused — with the SAME error an unknown
        // token gets, and without naming the id.
        let err = map.resolve_at("test", after).unwrap_err();
        assert_eq!(err.code(), map.resolve_at("never-issued", after).unwrap_err().code());
        assert!(!err.to_string().contains("old"), "must not name the id: {err}");
        assert!(map.resolve_id_at("test", after).is_none());

        // The principal's OTHER credential is untouched — expiring one token
        // must not lock the human out of their own account.
        assert_eq!(map.resolve_at("test3", after).unwrap(), "user:a");

        // And the same rule holds on the memory-scoped path.
        assert!(map.resolve_for_memory_at("test", "m", after).is_err());
        assert_eq!(map.resolve_for_memory_at("test3", "m", after).unwrap(), "user:a");
    }

    /// [A1] The banner input: what is about to expire, before it does.
    #[test]
    fn expiring_within_reports_the_window() {
        let map = CredentialMap::from_json(
            r#"{"version":1,"tokens":[
                {"env":"A","principal":"p","id":"soon","expires_at":"2026-01-10T00:00:00Z"},
                {"env":"B","principal":"p","id":"later","expires_at":"2027-01-01T00:00:00Z"},
                {"env":"C","principal":"p","id":"never"}
            ]}"#,
        )
        .unwrap();
        let now = crate::time::iso8601_to_ms("2026-01-01T00:00:00Z").unwrap();
        let two_weeks = 14 * 86_400_000;
        let due: Vec<String> = map
            .expiring_within(now, two_weeks)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(due, vec!["soon".to_string()]);
    }

    /// [A1] The token format: recognizable to a scanner, and honest about
    /// what it does and does not prove.
    #[test]
    fn minted_token_shape_is_recognizable() {
        let body = encode_token_body(&[0u8; 32]);
        let token = format!("{TOKEN_PREFIX}{body}");
        assert!(token_is_minted(&token));
        assert!(token.starts_with("areev_pat_"));
        assert_eq!(body.len(), 51, "51 base32 chars = 255 bits");

        // Distinct entropy → distinct tokens.
        let other = encode_token_body(&[1u8; 32]);
        assert_ne!(body, other);

        // An operator's own token is not "minted" — that is a hygiene
        // signal, not an auth check, so nothing here refuses it.
        assert!(!token_is_minted("hunter2"));
        assert!(!token_is_minted("areev_pat_short"));
    }
}
