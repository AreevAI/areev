//! Text pseudonymization: the Tier-0 detection chain, the placeholder codec,
//! and the keyed round-trip derivations (docs/anonymization-proposal.md, P0).
//!
//! Everything here is pure — no store access, no clock, no host config — so
//! every surface (store boundary, facade, CLI, bindings, LLM decorator)
//! shares one implementation. Offsets are UTF-8 byte positions over
//! NFC-normalized text: the same normalization canonical serialization
//! applies, so a span always slices exactly what would be stored or hashed
//! (proposal §5).
//!
//! Vocabulary is deliberate: this module *pseudonymizes* (D10). Only
//! `pseudonym` spans enter the mapping; `mask` and `redact` are one-way.

mod detect;

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::error::{AreevError, Result};

pub use detect::KNOWN_CATEGORIES;

/// A pluggable detector (proposal §5.2–5.3): Tier-1 NER over a command
/// seam, Tier-2 LLM — installed by the host, never shipped as a dependency.
/// Object-safe; spans use the same UTF-8-bytes-over-NFC contract as Tier 0.
pub trait DetectorBackend: Send + Sync {
    /// Which policy `detectors` entry this backend serves: "ner" or "llm".
    fn kind(&self) -> &str;
    /// Provenance id stamped on detections (e.g. "presidio/2.2").
    fn id(&self) -> &str;
    fn detect(&self, text: &str) -> Result<Vec<Detection>>;
}

/// One detected sensitive span. `start`/`end` are UTF-8 byte offsets into the
/// NFC-normalized text returned alongside the detections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    pub start: usize,
    pub end: usize,
    pub category: String,
    #[serde(default = "confidence_one")]
    pub confidence: f32,
    #[serde(default)]
    pub detector: String,
}

fn confidence_one() -> f32 {
    1.0
}

/// What the policy does with a detected span. Ranked by severity for overlap
/// resolution (proposal §5): a long low-severity span must never swallow a
/// high-severity one into a weaker treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    Pseudonym,
    Generalize(GenBucket),
    Mask,
    Redact,
}

/// Generalization buckets (proposal §6, quasi-identifier damping): coarsen
/// a value instead of replacing it. Irreversible by design; a value the
/// bucket cannot parse degrades to `[GENERALIZED:<CATEGORY>]` — coarsening
/// must never fall back to leaking the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenBucket {
    /// Dates → `YYYY-MM`.
    Month,
    /// Dates → `YYYY`.
    Year,
    /// Integers (ages, years) → `N0s`.
    Decade,
}

impl Action {
    fn severity(self) -> u8 {
        match self {
            Action::Allow => 0,
            Action::Pseudonym => 1,
            Action::Generalize(_) => 2,
            Action::Mask => 3,
            Action::Redact => 4,
        }
    }

    fn parse(s: &str) -> Result<Action> {
        match s {
            "allow" => Ok(Action::Allow),
            "pseudonym" => Ok(Action::Pseudonym),
            "mask" => Ok(Action::Mask),
            "redact" => Ok(Action::Redact),
            "generalize:month" => Ok(Action::Generalize(GenBucket::Month)),
            "generalize:year" => Ok(Action::Generalize(GenBucket::Year)),
            "generalize:decade" => Ok(Action::Generalize(GenBucket::Decade)),
            other if other == "generalize" || other.starts_with("generalize:") => {
                Err(AreevError::Validation(format!(
                    "unknown generalization '{other}' (expected generalize:month, \
                     generalize:year, or generalize:decade)"
                )))
            }
            other => Err(AreevError::Validation(format!(
                "unknown anonymization action '{other}' (expected pseudonym, mask, \
                 redact, generalize:<bucket>, or allow)"
            ))),
        }
    }
}

/// Coarsen one value into its bucket; unparseable values degrade to the
/// redaction form rather than leaking.
fn generalize_value(bucket: GenBucket, category: &str, value: &str) -> String {
    let fallback = || format!("[GENERALIZED:{}]", category_upper(category));
    match bucket {
        GenBucket::Month | GenBucket::Year => {
            // ISO first (YYYY-MM-DD), then D/M/Y-with-4-digit-year shapes.
            let (year, month) = if value.len() >= 7
                && value.as_bytes()[4] == b'-'
                && value[..4].chars().all(|c| c.is_ascii_digit())
            {
                (value[..4].to_string(), value.get(5..7).unwrap_or("").to_string())
            } else {
                let parts: Vec<&str> = value.split(['/', '.']).collect();
                match parts.as_slice() {
                    [_, m, y] if y.len() == 4 => ((*y).to_string(), format!("{:0>2}", m)),
                    _ => return fallback(),
                }
            };
            if year.len() != 4 || !year.chars().all(|c| c.is_ascii_digit()) {
                return fallback();
            }
            match bucket {
                GenBucket::Year => year,
                _ => {
                    if month.len() == 2 && month.chars().all(|c| c.is_ascii_digit()) {
                        format!("{year}-{month}")
                    } else {
                        fallback()
                    }
                }
            }
        }
        GenBucket::Decade => match value.trim().parse::<i64>() {
            Ok(n) if (0..=9999).contains(&n) => format!("{}0s", n / 10),
            _ => fallback(),
        },
    }
}

fn default_mode() -> String {
    "egress".into()
}
fn default_action() -> String {
    "pseudonym".into()
}
fn default_scope() -> String {
    "context".into()
}
fn default_placeholder() -> String {
    "[{CATEGORY}_{ID}]".into()
}
fn default_min_confidence() -> f32 {
    0.5
}

/// The declarative anonymization policy (proposal §8.1). Serialized as the
/// JSON value of an `anon:<ns>` meta row from P1 on; in P0 it is supplied
/// explicitly to the text APIs.
///
/// `deny_unknown_fields` is the fail-closed half of D3: a policy field this
/// build does not understand could be the field that *strengthens* the
/// policy, so refusing it loudly beats silently ignoring it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnonPolicy {
    #[serde(default = "default_mode")]
    pub mode: String,
    /// category → action ("pseudonym" | "mask" | "redact" | "allow").
    #[serde(default)]
    pub categories: BTreeMap<String, String>,
    /// Action for categories a detector emits but the map omits — the chain
    /// fails closed on categories it didn't anticipate, not open.
    #[serde(default = "default_action")]
    pub default_action: String,
    /// User dictionary; matches become category `custom`.
    #[serde(default)]
    pub custom_terms: Vec<String>,
    /// Pseudonym stability scope. P0 supports `context` only (per-call
    /// numbering); `session`/`memory` arrive with the store boundary.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Placeholder template; must contain `{CATEGORY}` and `{ID}`.
    #[serde(default = "default_placeholder")]
    pub placeholder: String,
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f32,
    /// Which chain links this policy demands (proposal §5.4): "tier0" runs
    /// in-tree; "ner"/"llm" require a host-installed [`DetectorBackend`] of
    /// that kind and FAIL CLOSED without one (D6).
    #[serde(default = "default_detectors")]
    pub detectors: Vec<String>,
    /// Persist the pseudonym mapping to the file's sealed vault (proposal
    /// §7). Requires scope `session`/`memory` and an encrypted memory.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vault: bool,
    /// Storage limitation for vault rows (REQ-ANON-6), in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_ttl_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub because: Option<String>,
}

fn default_detectors() -> Vec<String> {
    vec!["tier0".to_string()]
}

impl Default for AnonPolicy {
    fn default() -> Self {
        AnonPolicy {
            mode: default_mode(),
            categories: BTreeMap::new(),
            default_action: default_action(),
            custom_terms: Vec::new(),
            scope: default_scope(),
            placeholder: default_placeholder(),
            min_confidence: default_min_confidence(),
            detectors: default_detectors(),
            vault: false,
            vault_ttl_days: None,
            because: None,
        }
    }
}

impl AnonPolicy {
    /// Parse and validate a policy from JSON. Parse or validation failure is a
    /// hard `VAL` error (D3): a policy this build cannot read must not
    /// silently mean "no policy".
    pub fn from_json(json: &str) -> Result<AnonPolicy> {
        let policy: AnonPolicy = serde_json::from_str(json).map_err(|e| {
            AreevError::Validation(format!("invalid anonymization policy: {e}"))
        })?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        match self.mode.as_str() {
            "off" | "egress" | "ingress" | "both" | "audit" => {}
            other => {
                return Err(AreevError::Validation(format!(
                    "invalid anonymization policy: unknown mode '{other}' (expected \
                     off, egress, ingress, both, or audit)"
                )));
            }
        }
        match self.scope.as_str() {
            "context" | "session" | "memory" => {}
            other => {
                return Err(AreevError::Validation(format!(
                    "invalid anonymization policy: unknown scope '{other}' (expected \
                     context, session, or memory)"
                )));
            }
        }
        if !(self.placeholder.contains("{CATEGORY}") && self.placeholder.contains("{ID}")) {
            return Err(AreevError::Validation(
                "invalid anonymization policy: placeholder template must contain \
                 {CATEGORY} and {ID}"
                    .into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.min_confidence) || !self.min_confidence.is_finite() {
            return Err(AreevError::Validation(
                "invalid anonymization policy: min_confidence must be within 0.0..=1.0"
                    .into(),
            ));
        }
        Action::parse(&self.default_action).map_err(|e| {
            AreevError::Validation(format!("invalid anonymization policy default_action: {e}"))
        })?;
        for (cat, act) in &self.categories {
            if cat.is_empty() {
                return Err(AreevError::Validation(
                    "invalid anonymization policy: empty category name".into(),
                ));
            }
            Action::parse(act).map_err(|e| {
                AreevError::Validation(format!(
                    "invalid anonymization policy for category '{cat}': {e}"
                ))
            })?;
        }
        if self.detectors.is_empty() {
            return Err(AreevError::Validation(
                "invalid anonymization policy: detectors must not be empty (use \
                 [\"tier0\"])"
                    .into(),
            ));
        }
        for d in &self.detectors {
            if !matches!(d.as_str(), "tier0" | "ner" | "llm") {
                return Err(AreevError::Validation(format!(
                    "invalid anonymization policy: unknown detector '{d}' (expected \
                     tier0, ner, or llm)"
                )));
            }
        }
        if self.vault && self.scope == "context" {
            return Err(AreevError::Validation(
                "invalid anonymization policy: vault persistence needs scope \
                 \"session\" or \"memory\" — context-scope mappings are ephemeral \
                 by definition"
                    .into(),
            ));
        }
        if let Some(ttl) = self.vault_ttl_days {
            if !self.vault {
                return Err(AreevError::Validation(
                    "invalid anonymization policy: vault_ttl_days needs vault: true"
                        .into(),
                ));
            }
            if ttl < 0.0 || !ttl.is_finite() {
                return Err(AreevError::Validation(
                    "invalid anonymization policy: vault_ttl_days must be a \
                     non-negative, finite number"
                        .into(),
                ));
            }
        }
        for term in &self.custom_terms {
            if term.trim().is_empty() {
                return Err(AreevError::Validation(
                    "invalid anonymization policy: empty custom_terms entry".into(),
                ));
            }
        }
        Ok(())
    }

    fn action_for(&self, category: &str) -> Action {
        match self.categories.get(category) {
            // Validated in `validate()`; unreachable fallback keeps this total.
            Some(act) => Action::parse(act).unwrap_or(Action::Redact),
            None => Action::parse(&self.default_action).unwrap_or(Action::Redact),
        }
    }
}

/// Scan outcome: the NFC-normalized text the offsets refer to, plus the
/// surviving detections after overlap resolution (sorted by start).
#[derive(Debug, Clone, Serialize)]
pub struct ScanOutcome {
    pub text: String,
    pub detections: Vec<Detection>,
}

/// Anonymize outcome. `mapping` holds pseudonym spans only — `mask` and
/// `redact` are one-way by definition (proposal §6) — keyed by placeholder
/// token. `mapping_id` is the keyed round-trip handle (D11).
#[derive(Debug, Clone, Serialize)]
pub struct AnonOutcome {
    pub text: String,
    pub mapping: BTreeMap<String, String>,
    pub mapping_id: String,
    /// Spans replaced (all actions except allow).
    pub replaced: usize,
}

/// Rehydrate outcome. `unmatched` lists placeholder-shaped tokens left in the
/// text that the mapping did not cover — reported, never guessed.
#[derive(Debug, Clone, Serialize)]
pub struct RehydrateOutcome {
    pub text: String,
    pub replaced: usize,
    pub unmatched: Vec<String>,
}

/// NFC-normalize without allocating when the input already is.
pub fn nfc(text: &str) -> Cow<'_, str> {
    if unicode_normalization::is_nfc(text) {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text.nfc().collect())
    }
}

/// Run the Tier-0 detector chain and resolve overlaps (proposal §5).
///
/// `known_identities` are identities the caller already holds (e.g. interned
/// subjects like `caller:john`); each matches as category `person` — the
/// schema-aware detector a text-only proxy cannot have. Detections below
/// `min_confidence` are dropped; overlapping spans coalesce by action
/// severity first, span length second, then (start, category) as the
/// deterministic tiebreak; `allow` spans are resolved and then discarded.
pub fn scan(text: &str, policy: &AnonPolicy, known_identities: &[String]) -> Result<ScanOutcome> {
    scan_with(text, policy, known_identities, &[])
}

/// [`scan`] with host-installed detector backends. Fail-closed (D6): a
/// policy demanding "ner"/"llm" with no matching backend installed errors —
/// it never silently degrades to Tier 0 alone. Backend spans are validated
/// (in-bounds, on char boundaries) and garbage is an error, not a skip.
pub fn scan_with(
    text: &str,
    policy: &AnonPolicy,
    known_identities: &[String],
    backends: &[&dyn DetectorBackend],
) -> Result<ScanOutcome> {
    policy.validate()?;
    let text = nfc(text).into_owned();
    let mut detections = if policy.detectors.iter().any(|d| d == "tier0") {
        detect::run_tier0(&text, &policy.custom_terms, known_identities)?
    } else {
        Vec::new()
    };
    for kind in policy.detectors.iter().filter(|d| *d != "tier0") {
        let backend = backends
            .iter()
            .find(|b| b.kind() == kind)
            .ok_or_else(|| {
                AreevError::Validation(format!(
                    "anonymization policy demands detector \"{kind}\" but no such \
                     backend is installed on this host — egress fails closed (D6); \
                     install one (e.g. --anonymize-cmd) or drop it from the policy"
                ))
            })?;
        for d in backend.detect(&text)? {
            let in_bounds = d.start < d.end
                && d.end <= text.len()
                && text.is_char_boundary(d.start)
                && text.is_char_boundary(d.end);
            if !in_bounds || d.category.is_empty() {
                return Err(AreevError::Validation(format!(
                    "detector {} returned an invalid span {}..{} — refusing the \
                     whole result (D6): a mis-sliced span is a silent leak",
                    backend.id(),
                    d.start,
                    d.end
                )));
            }
            detections.push(d);
        }
    }
    detections.retain(|d| d.confidence >= policy.min_confidence);
    let mut survivors = resolve_overlaps(detections, policy);
    survivors.retain(|d| policy.action_for(&d.category) != Action::Allow);
    Ok(ScanOutcome { text, detections: survivors })
}

/// Detect, then apply the policy's actions, producing prompt-safe text plus
/// the mapping for the pseudonym spans. One-shot (`context`-scope) form of
/// [`SessionAnonymizer`]: numbering and mapping start fresh per call.
///
/// `key` keys the `mapping_id` derivation (D11). Callers on trusted
/// in-process surfaces may pass `None` (the id is then derived under an
/// all-zero key and MUST NOT be shipped to untrusted surfaces without the
/// mapping — from P1 on, the store boundary always supplies a real key).
pub fn anonymize(
    text: &str,
    policy: &AnonPolicy,
    known_identities: &[String],
    key: Option<&[u8]>,
) -> Result<AnonOutcome> {
    let mut session = if policy.scope == "memory" {
        let Some(k) = key else {
            return Err(AreevError::Validation(
                "anonymization scope \"memory\" needs a key (pass key_hex; \
                 value-derived tokens are keyed by design)"
                    .into(),
            ));
        };
        SessionAnonymizer::new_keyed(policy.clone(), hmac_sha256(k, b"areev.anon.tokenkey.v1"))?
    } else {
        SessionAnonymizer::new(policy.clone())?
    };
    let (out, replaced) = session.transform_text(text, known_identities)?;
    let mapping_id = session.mapping_id(key)?;
    Ok(AnonOutcome { text: out, mapping: session.into_mapping(), mapping_id, replaced })
}

/// Stateful pseudonym assignment shared across texts: the same
/// (category, value) pair yields the same token for the lifetime of the
/// session, which is what keeps tokens consistent across the grains of one
/// recall and across the calls of one process session (`session` scope).
/// Long-lived holders bound it with [`SessionAnonymizer::evict_to`] — an
/// unbounded in-process re-identification table is exactly what D5 forbids.
#[derive(Debug, Clone)]
pub struct SessionAnonymizer {
    policy: AnonPolicy,
    by_value: BTreeMap<(String, String), String>,
    order: std::collections::VecDeque<(String, String)>,
    counters: BTreeMap<String, u64>,
    mapping: BTreeMap<String, String>,
    reserved: std::collections::BTreeSet<String>,
    /// (token, value) pairs minted since the last [`Self::take_pending`]
    /// drain — the vault write-behind queue.
    pending: Vec<(String, String)>,
    /// When set, pseudonym token ids are value-derived HMAC fragments —
    /// stable across handles and processes — instead of appearance-order
    /// counters. Required for `memory` scope and for every ingress
    /// transform (D8: the same raw text must always transform identically).
    token_key: Option<[u8; 32]>,
}

impl SessionAnonymizer {
    pub fn new(policy: AnonPolicy) -> Result<Self> {
        policy.validate()?;
        if policy.scope == "memory" {
            return Err(AreevError::Validation(
                "anonymization scope \"memory\" needs a token key — use \
                 SessionAnonymizer::new_keyed (the store derives it from the \
                 file's encryption key)"
                    .into(),
            ));
        }
        Ok(Self::build(policy, None))
    }

    /// A session whose pseudonym ids derive from `key` — the `memory`-scope
    /// and ingress form. The same (key, category, value) always yields the
    /// same token, on any handle.
    pub fn new_keyed(policy: AnonPolicy, key: [u8; 32]) -> Result<Self> {
        policy.validate()?;
        Ok(Self::build(policy, Some(key)))
    }

    fn build(policy: AnonPolicy, token_key: Option<[u8; 32]>) -> Self {
        SessionAnonymizer {
            policy,
            by_value: BTreeMap::new(),
            order: std::collections::VecDeque::new(),
            counters: BTreeMap::new(),
            mapping: BTreeMap::new(),
            reserved: std::collections::BTreeSet::new(),
            pending: Vec::new(),
            token_key,
        }
    }

    pub fn policy(&self) -> &AnonPolicy {
        &self.policy
    }

    /// The accumulated placeholder → value map (pseudonym spans only).
    pub fn mapping(&self) -> &BTreeMap<String, String> {
        &self.mapping
    }

    pub fn into_mapping(self) -> BTreeMap<String, String> {
        self.mapping
    }

    pub fn len(&self) -> usize {
        self.by_value.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_value.is_empty()
    }

    /// The keyed round-trip handle over the current mapping state (D11).
    pub fn mapping_id(&self, key: Option<&[u8]>) -> Result<String> {
        derive_mapping_id(key, &self.policy, &self.mapping)
    }

    /// Detect + apply actions over one text; returns (transformed, spans
    /// replaced). Tokens already literally present in the text are reserved
    /// so minted tokens renumber around them.
    pub fn transform_text(
        &mut self,
        text: &str,
        known_identities: &[String],
    ) -> Result<(String, usize)> {
        self.transform_text_with(text, known_identities, &[])
    }

    /// [`Self::transform_text`] with host detector backends (fail-closed on
    /// a demanded-but-missing kind — see [`scan_with`]).
    pub fn transform_text_with(
        &mut self,
        text: &str,
        known_identities: &[String],
        backends: &[&dyn DetectorBackend],
    ) -> Result<(String, usize)> {
        let ScanOutcome { text, detections } =
            scan_with(text, &self.policy, known_identities, backends)?;
        for t in template_shaped_tokens(&text, &self.policy.placeholder) {
            self.reserved.insert(t);
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        let mut replaced = 0usize;
        for d in &detections {
            out.push_str(&text[cursor..d.start]);
            let value = &text[d.start..d.end];
            match self.policy.action_for(&d.category) {
                Action::Allow => unreachable!("allow spans dropped in scan()"),
                Action::Redact => {
                    out.push_str(&format!("[REDACTED:{}]", category_upper(&d.category)));
                    replaced += 1;
                }
                Action::Mask => {
                    out.push_str(&mask_value(value));
                    replaced += 1;
                }
                Action::Generalize(bucket) => {
                    out.push_str(&generalize_value(bucket, &d.category, value));
                    replaced += 1;
                }
                Action::Pseudonym => {
                    let token = self.token_for(&d.category, value);
                    out.push_str(&token);
                    replaced += 1;
                }
            }
            cursor = d.end;
        }
        out.push_str(&text[cursor..]);
        Ok((out, replaced))
    }

    /// Structural single-value transform: a whole field value whose category
    /// the schema already knows (a `subject` is a `person` by construction).
    /// Applies the category's action to the entire value.
    pub fn transform_value(&mut self, category: &str, value: &str) -> String {
        match self.policy.action_for(category) {
            Action::Allow => value.to_string(),
            Action::Redact => format!("[REDACTED:{}]", category_upper(category)),
            Action::Mask => mask_value(value),
            Action::Generalize(bucket) => generalize_value(bucket, category, value),
            Action::Pseudonym => self.token_for(category, value),
        }
    }

    /// The token already assigned to `(category, value)`, if any — exact
    /// lookup, no detection. Lets callers keep bare entity-term lists
    /// (graph reads) consistent with values pseudonymized elsewhere.
    pub fn token_if_known(&self, category: &str, value: &str) -> Option<&str> {
        self.by_value
            .get(&(category.to_string(), value.to_string()))
            .map(String::as_str)
    }

    /// Bound the session table, evicting oldest-first. An evicted value
    /// loses its stable token (and its mapping entry — old responses citing
    /// it stop rehydrating); the next sighting mints a fresh one. That is
    /// the deliberate cost of bounding a long-lived re-identification table.
    pub fn evict_to(&mut self, max_entries: usize) {
        while self.by_value.len() > max_entries {
            let Some(oldest) = self.order.pop_front() else { break };
            if let Some(token) = self.by_value.remove(&oldest) {
                self.mapping.remove(&token);
            }
        }
    }

    /// New (token, value) pairs minted since the last drain — the vault
    /// write-behind hook (proposal §7). Seeded entries never appear here.
    pub fn take_pending(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.pending)
    }

    /// Seed the session from persisted vault rows so tokens continue across
    /// process restarts instead of colliding. Counter-based sessions bump
    /// their counters past every seeded numeric id.
    pub fn seed(&mut self, entries: Vec<(String, String)>) {
        for (token, value) in entries {
            for (cat_upper, id_part) in parse_token_parts(&self.policy.placeholder, &token) {
                if let Ok(n) = id_part.parse::<u64>() {
                    let cat_key = cat_upper.to_ascii_lowercase();
                    let c = self.counters.entry(cat_key).or_insert(0);
                    if *c < n {
                        *c = n;
                    }
                }
            }
            // Category is recoverable from the token for bookkeeping; use
            // the uppercase form lowercased as the by_value key's category.
            let cat = parse_token_parts(&self.policy.placeholder, &token)
                .first()
                .map(|(c, _)| c.to_ascii_lowercase())
                .unwrap_or_else(|| "custom".into());
            self.reserved.insert(token.clone());
            self.by_value.insert((cat.clone(), value.clone()), token.clone());
            self.order.push_back((cat, value.clone()));
            self.mapping.insert(token, value);
        }
    }

    fn token_for(&mut self, category: &str, value: &str) -> String {
        let k = (category.to_string(), value.to_string());
        if let Some(t) = self.by_value.get(&k) {
            return t.clone();
        }
        let token = match self.token_key {
            Some(key) => derived_token(&self.policy.placeholder, &key, category, value),
            None => {
                let counter = self.counters.entry(category.to_string()).or_insert(0);
                mint_token(&self.policy.placeholder, category, counter, &self.reserved)
            }
        };
        self.by_value.insert(k.clone(), token.clone());
        self.order.push_back(k);
        self.mapping.insert(token.clone(), value.to_string());
        self.pending.push((token.clone(), value.to_string()));
        token
    }

    /// Drop every entry whose value matches one of `identities` — the
    /// in-memory half of REQ-ANON-1 (an erased subject must not survive in
    /// any live mapping).
    pub fn scrub_values(&mut self, identities: &[String]) -> usize {
        let doomed: Vec<(String, String)> = self
            .by_value
            .iter()
            .filter(|((_, v), _)| identities.iter().any(|i| i == v))
            .map(|(k, _)| k.clone())
            .collect();
        let n = doomed.len();
        for k in doomed {
            if let Some(token) = self.by_value.remove(&k) {
                self.mapping.remove(&token);
            }
            self.order.retain(|o| *o != k);
        }
        self.pending.retain(|(_, v)| !identities.iter().any(|i| i == v));
        n
    }
}

/// Parse `(CATEGORY, ID)` pairs a token exposes under `template` — used by
/// vault seeding to restore counters and category bookkeeping.
fn parse_token_parts(template: &str, token: &str) -> Vec<(String, String)> {
    let mut pattern = String::from("^");
    let mut rest = template;
    while let Some(idx) = rest.find('{') {
        pattern.push_str(&regex::escape(&rest[..idx]));
        if rest[idx..].starts_with("{CATEGORY}") {
            pattern.push_str("([A-Z][A-Z0-9_]*)");
            rest = &rest[idx + "{CATEGORY}".len()..];
        } else if rest[idx..].starts_with("{ID}") {
            pattern.push_str("([0-9A-Za-z]+)");
            rest = &rest[idx + "{ID}".len()..];
        } else {
            pattern.push_str(&regex::escape(&rest[idx..idx + 1]));
            rest = &rest[idx + 1..];
        }
    }
    pattern.push_str(&regex::escape(rest));
    pattern.push('$');
    let Ok(re) = regex::Regex::new(&pattern) else { return Vec::new() };
    let Some(c) = re.captures(token) else { return Vec::new() };
    match (c.get(1), c.get(2)) {
        (Some(cat), Some(id)) => vec![(cat.as_str().to_string(), id.as_str().to_string())],
        _ => Vec::new(),
    }
}

/// The value-derived pseudonym token: `{ID}` is an 8-hex HMAC fragment over
/// (category, value) under `key`. Pure — erasure and DSAR reads use this to
/// recompute an ingress-stored pseudonym from the real identity
/// (REQ-ANON-7) without any session state.
pub fn derived_token(template: &str, key: &[u8; 32], category: &str, value: &str) -> String {
    let mut msg = Vec::with_capacity(category.len() + value.len() + 1);
    msg.extend_from_slice(category.as_bytes());
    msg.push(0x1e);
    msg.extend_from_slice(value.as_bytes());
    let digest = hmac_sha256(key, &msg);
    template
        .replace("{CATEGORY}", &category_upper(category))
        .replace("{ID}", &hex::encode(&digest[..4]))
}

/// Replace exact placeholder tokens with their mapped originals. Tokens the
/// mapping does not cover are left intact and reported in `unmatched`; the
/// codec never guesses (proposal §6).
pub fn rehydrate(text: &str, mapping: &BTreeMap<String, String>) -> Result<RehydrateOutcome> {
    let text = nfc(text).into_owned();
    for k in mapping.keys() {
        if k.is_empty() {
            return Err(AreevError::Validation(
                "invalid anonymization mapping: empty placeholder key".into(),
            ));
        }
    }

    // One left-to-right pass over the original text: collect every occurrence
    // of every key, prefer the longest key at a position, and never rescan
    // spliced-in values (a mapped value that happens to contain a
    // placeholder-shaped string must come back verbatim, not recurse).
    let mut hits: Vec<(usize, &str)> = Vec::new();
    for key in mapping.keys() {
        for (pos, _) in text.match_indices(key.as_str()) {
            hits.push((pos, key.as_str()));
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.len().cmp(&a.1.len())));

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut replaced = 0usize;
    for (pos, key) in hits {
        if pos < cursor {
            continue; // overlapped by an earlier (longer) key
        }
        out.push_str(&text[cursor..pos]);
        out.push_str(&mapping[key]);
        cursor = pos + key.len();
        replaced += 1;
    }
    out.push_str(&text[cursor..]);

    // Best-effort report of leftover tokens in the *default* shape; a custom
    // template's leftovers are only recognized when they share the bracketed
    // CATEGORY_ID silhouette.
    let mut unmatched: Vec<String> = Vec::new();
    for token in template_shaped_tokens(&out, &default_placeholder()) {
        if !mapping.contains_key(&token) && !unmatched.contains(&token) {
            unmatched.push(token);
        }
    }
    unmatched.sort();
    Ok(RehydrateOutcome { text: out, replaced, unmatched })
}

/// Parse a mapping serialized as a JSON object of placeholder → value.
pub fn mapping_from_json(json: &str) -> Result<BTreeMap<String, String>> {
    serde_json::from_str(json)
        .map_err(|e| AreevError::Validation(format!("invalid anonymization mapping: {e}")))
}

// ---- internals -------------------------------------------------------------

fn category_upper(category: &str) -> String {
    category
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

fn mint_token(
    template: &str,
    category: &str,
    counter: &mut u64,
    reserved: &std::collections::BTreeSet<String>,
) -> String {
    loop {
        *counter += 1;
        let token = template
            .replace("{CATEGORY}", &category_upper(category))
            .replace("{ID}", &counter.to_string());
        if !reserved.contains(&token) {
            return token;
        }
    }
}

/// Every literal in `text` that matches the template's token silhouette.
fn template_shaped_tokens(text: &str, template: &str) -> std::collections::BTreeSet<String> {
    let mut pattern = String::new();
    let mut rest = template;
    while let Some(idx) = rest.find('{') {
        pattern.push_str(&regex::escape(&rest[..idx]));
        if rest[idx..].starts_with("{CATEGORY}") {
            pattern.push_str("[A-Z][A-Z0-9_]*");
            rest = &rest[idx + "{CATEGORY}".len()..];
        } else if rest[idx..].starts_with("{ID}") {
            pattern.push_str("[0-9A-Za-z]+");
            rest = &rest[idx + "{ID}".len()..];
        } else {
            pattern.push_str(&regex::escape(&rest[idx..idx + 1]));
            rest = &rest[idx + 1..];
        }
    }
    pattern.push_str(&regex::escape(rest));
    let mut out = std::collections::BTreeSet::new();
    if let Ok(re) = regex::Regex::new(&pattern) {
        for m in re.find_iter(text) {
            out.insert(m.as_str().to_string());
        }
    }
    out
}

fn mask_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut run_started = false;
    for c in value.chars() {
        if c.is_alphanumeric() {
            if run_started {
                out.push('*');
            } else {
                out.push(c);
                run_started = true;
            }
        } else {
            out.push(c);
            run_started = false;
        }
    }
    out
}

/// Overlap resolution (proposal §5): repeatedly take the best remaining span
/// by (action severity, length, earliest start, category), evicting whatever
/// it overlaps. O(n²) on the per-text detection count, which is small.
fn resolve_overlaps(mut detections: Vec<Detection>, policy: &AnonPolicy) -> Vec<Detection> {
    let mut survivors: Vec<Detection> = Vec::new();
    while !detections.is_empty() {
        let best = detections
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                let sa = policy.action_for(&a.category).severity();
                let sb = policy.action_for(&b.category).severity();
                sa.cmp(&sb)
                    .then((a.end - a.start).cmp(&(b.end - b.start)))
                    .then(b.start.cmp(&a.start))
                    .then(b.category.cmp(&a.category))
            })
            .map(|(i, _)| i)
            .expect("non-empty");
        let winner = detections.swap_remove(best);
        detections.retain(|d| d.end <= winner.start || d.start >= winner.end);
        survivors.push(winner);
    }
    survivors.sort_by_key(|d| d.start);
    survivors
}

/// The keyed round-trip handle (D11): truncated HMAC-SHA256 over the
/// canonicalized policy, the scope, and the sorted placeholder→value pairs.
/// Keyed, never a bare digest — an unkeyed hash over the values would hand
/// the egress channel an offline-guessing oracle for low-entropy values.
fn derive_mapping_id(
    key: Option<&[u8]>,
    policy: &AnonPolicy,
    mapping: &BTreeMap<String, String>,
) -> Result<String> {
    let policy_json = serde_json::to_string(policy)
        .map_err(|e| AreevError::Validation(format!("anonymization policy serialize: {e}")))?;
    let mut msg = Vec::with_capacity(policy_json.len() + 64);
    msg.extend_from_slice(policy_json.as_bytes());
    msg.push(0x1f);
    msg.extend_from_slice(policy.scope.as_bytes());
    for (k, v) in mapping {
        msg.push(0x1f);
        msg.extend_from_slice(k.as_bytes());
        msg.push(0x1e);
        msg.extend_from_slice(v.as_bytes());
    }
    let zero_key = [0u8; 32];
    let digest = hmac_sha256(key.unwrap_or(&zero_key), &msg);
    Ok(hex::encode(&digest[..8]))
}

/// RFC 2104 HMAC-SHA256, hand-rolled over the sha2 dependency the crate
/// already carries (dependency-light: no hmac crate for twenty lines).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    let ipad: Vec<u8> = key_block.iter().map(|b| b ^ 0x36).collect();
    inner.update(&ipad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    let opad: Vec<u8> = key_block.iter().map(|b| b ^ 0x5c).collect();
    outer.update(&opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc4231_case_2() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn policy_rejects_unknown_field_and_unknown_bucket_and_unbuilt_scope() {
        assert!(AnonPolicy::from_json(r#"{"surprise": 1}"#).is_err());
        assert!(AnonPolicy::from_json(r#"{"categories": {"date": "generalize:month"}}"#).is_ok());
        assert!(AnonPolicy::from_json(r#"{"categories": {"date": "generalize:eon"}}"#).is_err());
        assert!(AnonPolicy::from_json(r#"{"scope": "session"}"#).is_ok()); // built in P1
        assert!(AnonPolicy::from_json(r#"{"scope": "memory"}"#).is_ok()); // built in P2
        // ...but memory scope is keyed by design: the unkeyed paths refuse it.
        let memory = AnonPolicy::from_json(r#"{"scope": "memory"}"#).unwrap();
        assert!(SessionAnonymizer::new(memory.clone()).is_err());
        assert!(anonymize("x", &memory, &[], None).is_err());
        assert!(anonymize("mail a@b.co", &memory, &[], Some(b"k")).is_ok());
        assert!(AnonPolicy::from_json("{}").is_ok());
    }

    #[test]
    fn session_anonymizer_is_stable_across_texts_and_evicts_oldest() {
        let mut s = SessionAnonymizer::new(AnonPolicy::default()).unwrap();
        let (a, _) = s.transform_text("mail a@b.co", &[]).unwrap();
        let (b, _) = s.transform_text("again a@b.co and new c@d.io", &[]).unwrap();
        assert_eq!(a, "mail [EMAIL_1]");
        assert_eq!(b, "again [EMAIL_1] and new [EMAIL_2]"); // stable across texts
        assert_eq!(s.transform_value("person", "caller:john"), "[PERSON_1]");
        assert_eq!(s.transform_value("person", "caller:john"), "[PERSON_1]");
        assert_eq!(s.token_if_known("person", "caller:john"), Some("[PERSON_1]"));
        assert_eq!(s.len(), 3);

        s.evict_to(1);
        assert_eq!(s.len(), 1);
        assert_eq!(s.token_if_known("email", "a@b.co"), None); // oldest evicted
        assert_eq!(s.token_if_known("person", "caller:john"), Some("[PERSON_1]"));
    }

    #[test]
    fn generalization_coarsens_and_never_leaks() {
        assert_eq!(generalize_value(GenBucket::Month, "date", "2026-08-16"), "2026-08");
        assert_eq!(generalize_value(GenBucket::Month, "date", "16/08/2026"), "2026-08");
        assert_eq!(generalize_value(GenBucket::Year, "date", "2026-08-16"), "2026");
        assert_eq!(generalize_value(GenBucket::Decade, "age", "47"), "40s");
        // Unparseable values degrade to the redaction form, never the raw value.
        assert_eq!(generalize_value(GenBucket::Month, "date", "someday"), "[GENERALIZED:DATE]");
        assert_eq!(generalize_value(GenBucket::Decade, "age", "young"), "[GENERALIZED:AGE]");

        let mut policy = AnonPolicy::default();
        policy.categories.insert("date".into(), "generalize:month".into());
        let out = anonymize("met on 2026-08-16 at noon", &policy, &[], None).unwrap();
        assert_eq!(out.text, "met on 2026-08 at noon");
        assert!(out.mapping.is_empty(), "generalization is one-way");
    }

    #[test]
    fn mask_keeps_shape() {
        assert_eq!(mask_value("john.doe@example.com"), "j***.d**@e******.c**");
        assert_eq!(mask_value("+1-555-0142"), "+1-5**-0***");
    }

    #[test]
    fn collision_renumbers_around_literal_tokens() {
        let policy = AnonPolicy::default();
        let out = anonymize(
            "already has [EMAIL_1] and a real x@y.io address",
            &policy,
            &[],
            None,
        )
        .unwrap();
        assert!(out.text.contains("[EMAIL_1]")); // the literal survives
        assert!(out.mapping.contains_key("[EMAIL_2]")); // we minted around it
        assert_eq!(out.mapping["[EMAIL_2]"], "x@y.io");
    }

    #[test]
    fn mapping_id_is_keyed() {
        let policy = AnonPolicy::default();
        let a = anonymize("mail me at a@b.co", &policy, &[], None).unwrap();
        let b = anonymize("mail me at a@b.co", &policy, &[], Some(b"k1")).unwrap();
        let c = anonymize("mail me at a@b.co", &policy, &[], Some(b"k1")).unwrap();
        assert_ne!(a.mapping_id, b.mapping_id); // key changes the id
        assert_eq!(b.mapping_id, c.mapping_id); // same inputs + key → same id
    }
}
