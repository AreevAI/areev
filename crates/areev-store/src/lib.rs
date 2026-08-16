//! areev-store — the embedded Turso-backed store for Areev.
//!
//! Implements the store schema: dictionary-encoded 2½-permutation
//! triple indexes (SPO + POS mandatory, OSP selective for entity-valued
//! relations), `entity_latest` materialization, op-log + HLC + tombstones,
//! thread index, and the vaais operation profile (add / recall / batch /
//! supersede / forget) plus bounded graph ops and two-axis `entity_at`.
//!
//! `Areev` drives a sync storage seam (`db::Db`); the embedded Turso backend
//! hides its own current-thread runtime behind it, so point ops stay µs-class
//! with no executor hop at the call site, and a second backend can implement
//! the same seam without touching the store logic.

mod db;
#[cfg(feature = "postgres")]
pub mod pg;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use areev_core::authz;
use areev_core::error::{Hash, AreevError, Result};
use areev_core::format::deserialize::{deserialize_blob, DeserializedGrain};
use areev_core::format::serialize::serialize_grain;
use areev_core::types::Grain;
use areev_core::types::{
    step_action_node, step_action_relation, Observation, RelatedTo, STEP_ACTION_PREFIX,
};
use turso::Value;
use zeroize::Zeroize;

use db::{db_err, with_txn, Db, TursoDb};

/// Op-log operation kinds.
pub const OP_ADD: i64 = 1;
pub const OP_SUPERSEDE: i64 = 2;
pub const OP_FORGET: i64 = 3; // tombstone

/// Temporal axis for `entity_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// "What was true in the world at T" — `valid_from`/`valid_to`.
    World,
    /// "What did the agent know at T" — supersession chain walk.
    Knowledge,
}

/// One op-log record, the change-feed unit.
#[derive(Debug, Clone)]
pub struct OpRecord {
    pub op_seq: i64,
    pub hlc: i64,
    pub op: i64,
    pub hash: Hash,
}

/// Traversal direction for `related`. `In` uses the
/// selective OSP index, so it only sees entity-valued relations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

impl Direction {
    /// Parse the wire spelling used by every binding (`out`/`in`/`both`).
    /// Defaults to `Out` so a caller that omits it walks the graph forwards.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "out" | "" => Some(Self::Out),
            "in" => Some(Self::In),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

impl Axis {
    /// Parse the wire spelling used by every binding.
    ///
    /// `world` = what was true at T (`valid_from`/`valid_to`);
    /// `knowledge` = what the agent knew at T (walks the supersession chain).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "world" | "" => Some(Self::World),
            "knowledge" => Some(Self::Knowledge),
            _ => None,
        }
    }
}

/// Split the comma-separated relation list the bindings take.
///
/// Relations arrive as one scalar string (the FFI convention is scalars in,
/// JSON out), so `"mg:knows, reports_to"` becomes two relations. Empty entries
/// are dropped — a trailing comma is not an anonymous relation.
pub fn parse_relations(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect()
}

/// Result of `bundle_since` — the git-shaped incremental backup (§5.10).
#[derive(Debug, Clone)]
pub struct BundleStats {
    pub ops: usize,
    pub bytes: u64,
    pub last_op_seq: i64,
}

/// Result of `import_bundle`.
#[derive(Debug, Clone, Default)]
pub struct ImportStats {
    pub applied: usize,
    pub skipped: usize,
    /// Registry meta rows (saved queries, templates, retention policies)
    /// applied from the bundle's meta segment. Counted separately from ops:
    /// replaying the same bundle keeps `applied == 0` while the registry
    /// merge stays idempotent on its own terms.
    pub meta_applied: usize,
    /// Registry meta rows skipped (already present and not older, or
    /// unparseable against a present local row).
    pub meta_skipped: usize,
}

/// Escape `\`, `%` and `_` for a `LIKE ?N ESCAPE '\'` pattern — the one
/// escaping rule every prefix/contains scan in this crate must share.
pub(crate) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// The `cas://sha256:` hex digests a grain's `content_refs` name (compact
/// key `u` or expanded `uri`).
fn cas_hexes(view: &DeserializedGrain) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(refs) = view.fields.get("content_refs").and_then(|v| v.as_array()) {
        for r in refs {
            let uri = r
                .get("uri")
                .and_then(|u| u.as_str())
                .or_else(|| r.get("u").and_then(|u| u.as_str()));
            if let Some(hex) = uri.and_then(|u| u.strip_prefix("cas://sha256:")) {
                out.push(hex.to_string());
            }
        }
    }
    out
}

/// What the identity selector matched (see `Areev::select_identity`).
#[derive(Default)]
struct IdentitySelection {
    identity_ids: Vec<i64>,
    identity_names: Vec<String>,
    seqs: Vec<i64>,
}

/// What a bulk erasure targets (internal to the erasure core).
enum ErasureSelector {
    /// Every structured reference to one identity in one namespace. The
    /// identity's dictionary ids (exact + partition-prefixed keys) are
    /// resolved INSIDE the erasure transaction, after the serialization
    /// point.
    Identity { ns_id: i64, subject: String, text_mentions: bool },
    /// Everything older than the cutoff, optionally scoped. `limit` bounds
    /// the sweep to the oldest N grains — CAL's `PURGE … LIMIT n`, which
    /// exists so an operator can pilot a retention rule on a reviewable
    /// batch instead of committing to the whole namespace.
    OlderThan {
        ns_id: Option<i64>,
        cutoff_ms: i64,
        gtype: Option<i64>,
        limit: Option<usize>,
    },
}

/// Options for [`Areev::forget_subject_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ErasureOptions {
    /// Also erase grains whose INDEXED TEXT mentions the identity — search
    /// symmetry: whatever the BM25 leg can find by these tokens, erasure
    /// removes. Opt-in because token matching over-reaches for identifiers
    /// that are common words ("may", "mark"); safe and thorough for
    /// distinctive ids (contact codes, phone fragments). Requires text
    /// indexing to be on.
    pub text_mentions: bool,
}

/// A declared retention policy for one namespace (GDPR Art. 5(1)(e)).
/// One namespace's outcome from a retention sweep: swept, or skipped with
/// the refusal on record (a held namespace must not abort the cron pass,
/// and must not vanish from its output either).
#[derive(Debug, Clone)]
pub struct RetentionSweep {
    pub ns: String,
    pub days: f64,
    pub outcome: RetentionOutcome,
}

#[derive(Debug, Clone)]
pub enum RetentionOutcome {
    Swept(ErasureReport),
    Skipped { why: String },
}

/// Serialized as the JSON value of a `retention:<ns>` meta row, so it
/// travels with the file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RetentionPolicy {
    /// Maximum age in days. Grains with `created_at` older than this are
    /// erased by a sweep. Fractional days are allowed (tests, hourly data).
    pub days: f64,
    /// Restrict the sweep to one grain type (e.g. `"event"`). `None` sweeps
    /// every type in the namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grain_type: Option<String>,
    /// Free-text note: why this policy exists. Carried for the operator and
    /// for the BECAUSE of a governed purge — a retention rule with no
    /// recorded rationale is not auditable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub because: Option<String>,
}

/// Result of a bulk erasure ([`Areev::forget_subject`] /
/// [`Areev::forget_older_than`]) — the numbers a host's compliance audit
/// records. Deliberately contains no identity material.
#[derive(Debug, Clone, Default)]
pub struct ErasureReport {
    /// Grains tombstoned and physically deleted from the hot store.
    pub grains_erased: usize,
    /// Dictionary entries removed because nothing references them any more
    /// (the identity string itself is erasable data).
    pub terms_removed: usize,
    /// Vocabulary tokens removed because they occurred only in erased text.
    pub vocab_removed: usize,
    /// CAS blobs reclaimed because only erased grains referenced them.
    pub blobs_reclaimed: usize,
    /// Content addresses erased by this transaction. Hashes carry no identity
    /// material and let hosts mark derived corpora/checkpoints stale exactly.
    pub erased_hashes: Vec<String>,
}

/// One governed corpus export registry entry, reconstructed from its immutable
/// Observation grain and `related_to` source edges.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CorpusExportRegistry {
    pub manifest_hash: String,
    pub export_id: String,
    pub destination: String,
    pub recipient: Option<String>,
    pub exported_at_ms: i64,
    pub subject_fingerprints: Vec<String>,
    pub source_hashes: Vec<String>,
}

/// Pure registry predicate shared by the store and host surfaces that must
/// inspect a pre-erasure snapshot.
pub fn exports_touching_hashes(
    exports: &[CorpusExportRegistry],
    hashes: &[String],
) -> Vec<CorpusExportRegistry> {
    let hashes: std::collections::HashSet<&str> =
        hashes.iter().map(String::as_str).collect();
    exports
        .iter()
        .filter(|export| {
            export
                .source_hashes
                .iter()
                .any(|hash| hashes.contains(hash.as_str()))
        })
        .cloned()
        .collect()
}

/// Result of [`Areev::subject_report`] — the read-only DSAR mirror of
/// [`Areev::forget_subject`]: everything the erasure selector would match
/// for one identity, hydrated instead of erased (GDPR Art. 15 access /
/// Art. 20 portability; see docs/gdpr.md). "Show me everything, then delete
/// it" is this then that, over ONE selection.
#[derive(Debug, Default)]
pub struct SubjectReport {
    /// Every dictionary string the identity matched: the exact subject plus
    /// partition-style keys (`pat`, `pat#visit1` — never `patricia`).
    pub identity_names: Vec<String>,
    /// The matched grains — full history, not just heads — in insertion
    /// order, deserialized for export.
    pub grains: Vec<DeserializedGrain>,
}

/// Pluggable embedding backend. The host owns the model;
/// multilingual recall quality comes from choosing a multilingual model
/// (e.g. bge-m3 / multilingual-e5) — text reaches the backend as
/// NFC-normalized UTF-8, script untouched (Arabic/Mandarin/English alike).
pub trait EmbedBackend: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Model identifier recorded as file provenance (e.g. "bge-m3").
    /// Backends should override this; it lets a later open detect that the
    /// stored vectors came from a different model.
    fn model(&self) -> &str {
        "unspecified"
    }
}

/// [`EmbedBackend`] that shells out to a host-supplied command per call: the
/// text goes to the child's stdin, stdout must be a JSON array of numbers.
/// This is the dependency-free way to give every surface (CLI `--embed-cmd`,
/// MCP serve, bindings) a real vector leg — the host owns the model, the
/// engine still ships none. One process spawn per embed: fine for turn-level
/// recall and imports, not for the voice per-frame path.
pub struct CommandEmbed {
    argv: Vec<String>,
    dim: usize,
    model: String,
}

impl CommandEmbed {
    /// `cmd` is split on whitespace (no shell interpretation). The command is
    /// probed once here to learn the vector dimension, so a broken command
    /// fails loudly at setup rather than mid-recall.
    pub fn new(cmd: &str, model: Option<&str>) -> Result<Self> {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return Err(AreevError::Validation("embed command is empty".into()));
        }
        let mut ce = CommandEmbed {
            argv,
            dim: 0,
            model: model.unwrap_or("command").to_string(),
        };
        let probe = ce.run("dimension probe")?;
        if probe.is_empty() {
            return Err(AreevError::Validation(
                "embed command returned an empty vector".into(),
            ));
        }
        ce.dim = probe.len();
        Ok(ce)
    }

    fn run(&self, text: &str) -> Result<Vec<f32>> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let cmd_err = |e: std::io::Error| {
            AreevError::Storage(format!("embed command '{}': {e}", self.argv[0]))
        };
        let mut child = Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(cmd_err)?;
        {
            let mut stdin = child.stdin.take().expect("stdin piped");
            stdin.write_all(text.as_bytes()).map_err(cmd_err)?;
            // dropping stdin closes the pipe so the child sees EOF
        }
        let out = child.wait_with_output().map_err(cmd_err)?;
        if !out.status.success() {
            return Err(AreevError::Storage(format!(
                "embed command '{}' exited with {}",
                self.argv[0], out.status
            )));
        }
        serde_json::from_slice::<Vec<f32>>(&out.stdout).map_err(|e| {
            AreevError::Validation(format!(
                "embed command output must be a JSON array of numbers: {e}"
            ))
        })
    }
}

impl EmbedBackend for CommandEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let v = self.run(text)?;
        if v.len() != self.dim {
            return Err(AreevError::Validation(format!(
                "embed command returned {} dims, expected {}",
                v.len(),
                self.dim
            )));
        }
        Ok(v)
    }
    fn model(&self) -> &str {
        &self.model
    }
}

/// Pluggable cross-encoder reranker (Tier-2
/// retrieval). Like `EmbedBackend`, the host owns the model: inject a local
/// candle/ONNX cross-encoder (or any scorer) — the engine ships no model and
/// takes no ML dependency. Off by default; with no reranker installed recall
/// behaves exactly as before. Reranking is a **turn-level** refinement (tens
/// of ms), never on the voice per-frame path; `recall_hybrid_tuned` only
/// invokes it inside the deadline and falls back to fusion order otherwise.
pub trait RerankBackend: Send + Sync {
    /// Relevance score for each `(query, doc)` pair, positionally aligned with
    /// `docs`. Higher = more relevant. Scores are only ever compared among
    /// themselves, so raw cross-encoder logits are fine (no normalization
    /// required). Must return exactly `docs.len()` scores.
    fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
    /// Model identifier for observability (e.g. "ms-marco-MiniLM-L-6-v2").
    fn model(&self) -> &str {
        "unspecified"
    }
}

/// Pluggable rule-based query expander (Tier-1 retrieval). No LLM, no network.
/// Given a query it returns additional query *variants*; the caller runs one
/// extra BM25 leg per variant and fuses them via RRF, bridging vocabulary gaps
/// ("cell" ↔ "mobile" ↔ "phone") — the poor-man's semantic bridge for the
/// edge/BM25-only profile where no embedder is installed. The built-in
/// [`EnglishExpander`] is **English-only**; multilingual deployments install
/// their own or leave expansion off (it is opt-in per query).
pub trait QueryExpander: Send + Sync {
    /// Query variants to also search, NOT including the original. Empty = no
    /// expansion. Implementations should keep this small and bounded.
    fn expand(&self, query: &str) -> Vec<String>;
}

/// Built-in English query expander: synonym substitution + naive suffix
/// stemming, capped to a handful of variants. Deterministic and allocation-
/// light. English-only by design (see [`QueryExpander`]).
pub struct EnglishExpander {
    /// Cap on the number of variants returned (default 4).
    max_variants: usize,
}

impl Default for EnglishExpander {
    fn default() -> Self {
        Self { max_variants: 4 }
    }
}

impl EnglishExpander {
    pub fn new(max_variants: usize) -> Self {
        Self { max_variants: max_variants.clamp(1, 16) }
    }

    /// Synonyms for a lowercased token (both directions of each group).
    fn synonyms(token: &str) -> &'static [&'static str] {
        // Small, deterministic map. Each group lists the *other* members.
        match token {
            "cell" | "cellphone" => &["mobile", "phone"],
            "mobile" => &["cell", "phone"],
            "phone" => &["cell", "mobile", "telephone"],
            "email" | "e-mail" => &["mail"],
            "buy" | "bought" | "purchased" => &["purchase"],
            "purchase" => &["buy"],
            "car" | "automobile" => &["vehicle"],
            "vehicle" => &["car"],
            "doctor" | "physician" => &["doctor", "physician"],
            "kid" | "kids" | "child" => &["child", "children"],
            "spouse" => &["wife", "husband", "partner"],
            "job" => &["work", "employer"],
            "home" | "house" => &["residence", "address"],
            "birthday" => &["birthdate", "born"],
            "big" => &["large"],
            "small" => &["little"],
            _ => &[],
        }
    }

    /// Naive English suffix stemmer for a single token: strips one common
    /// inflection. Returns the stem only when it differs and stays ≥3 chars.
    fn stem(token: &str) -> Option<String> {
        let lower = token;
        for suf in ["ing", "ed", "es", "s"] {
            if lower.len() > suf.len() + 2 && lower.ends_with(suf) {
                let stem = &lower[..lower.len() - suf.len()];
                if stem.len() >= 3 {
                    return Some(stem.to_string());
                }
            }
        }
        None
    }
}

impl QueryExpander for EnglishExpander {
    fn expand(&self, query: &str) -> Vec<String> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut variants: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let original = tokens.join(" ");
        seen.insert(original.clone());

        // 1. Synonym substitution: one variant per (position, synonym).
        for (i, tok) in tokens.iter().enumerate() {
            for syn in Self::synonyms(tok) {
                let mut v = tokens.clone();
                v[i] = (*syn).to_string();
                let s = v.join(" ");
                if seen.insert(s.clone()) {
                    variants.push(s);
                    if variants.len() >= self.max_variants {
                        return variants;
                    }
                }
            }
        }

        // 2. A fully-stemmed variant (all tokens stemmed where possible).
        let stemmed: Vec<String> = tokens
            .iter()
            .map(|t| Self::stem(t).unwrap_or_else(|| t.clone()))
            .collect();
        let s = stemmed.join(" ");
        if seen.insert(s.clone()) {
            variants.push(s);
        }

        variants.truncate(self.max_variants);
        variants
    }
}

/// Post-fusion recall refinements. All default off — a bare
/// `recall_hybrid` behaves exactly as before. Applied inside the recall
/// deadline; each stage degrades to plain fusion order when its backend or
/// data is unavailable (fail-open, never an error).
#[derive(Debug, Clone, Copy, Default)]
pub struct RecallTuning {
    /// Tier-1: run rule-based query expansion (extra BM25 legs, RRF-fused).
    /// Uses the installed [`QueryExpander`], or the built-in [`EnglishExpander`].
    pub query_expansion: bool,
    /// Tier-2: cross-encoder rerank the fused candidate pool via the installed
    /// [`RerankBackend`]. Takes precedence over `diversity_lambda`.
    pub rerank: bool,
    /// Tier-1: MMR diversity reorder. `lambda` in `[0,1]` — 1.0 = pure
    /// relevance, 0.0 = maximum diversity. Requires an embedder + stored
    /// vectors; silently skipped otherwise.
    pub diversity_lambda: Option<f32>,
    /// Widen every leg to the whole supersession chain instead of heads only.
    ///
    /// All three legs are heads-only by default — the structural probe filters
    /// `cur=1`, BM25 drops non-live postings after scoring, and the vector leg
    /// joins on `svt IS NULL`. That is the right default: stale values in a
    /// model's context are the failure mode a memory engine exists to prevent.
    /// This opts a caller that is asking *about the past* back into the rest of
    /// the chain — CAL's `WITH superseded`, audit and drift reads.
    ///
    /// Forgotten grains stay gone: `forget` deletes the index rows outright
    /// rather than flagging them, so nothing tombstoned or crypto-erased can
    /// come back through this door.
    pub include_superseded: bool,
}

/// One extracted fact from a `remember()` extraction callback.
#[derive(Debug, Clone)]
pub struct FactDraft {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub confidence: f64,
}

/// The confidence a draft gets when the source omits one.
pub const DRAFT_DEFAULT_CONFIDENCE: f64 = 0.8;

impl FactDraft {
    /// Parse the `[{subject, relation, object, confidence}]` JSON that every
    /// non-Rust surface uses to hand over pre-extracted facts (`--facts`,
    /// `facts_json`) — a closure cannot cross FFI, so the drafts arrive as
    /// text. Lives here, next to the type, because the CLI, Python, and Node
    /// bindings all need exactly this parse.
    ///
    /// A row missing any of subject/relation/object is an error, not a silent
    /// empty-string Fact. Confidence defaults to
    /// [`DRAFT_DEFAULT_CONFIDENCE`] and is clamped into 0.0–1.0.
    pub fn from_json_array(json: &str) -> Result<Vec<FactDraft>> {
        let arr: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|e| {
            AreevError::Validation(format!(
                "facts must be a JSON array of {{subject, relation, object, confidence}}: {e}"
            ))
        })?;
        arr.iter()
            .enumerate()
            .map(|(i, v)| {
                let field = |k: &str| {
                    v.get(k)
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string()
                };
                let (subject, relation, object) = (field("subject"), field("relation"), field("object"));
                if subject.is_empty() || relation.is_empty() || object.is_empty() {
                    return Err(AreevError::Validation(format!(
                        "facts[{i}] needs a non-empty subject, relation, and object"
                    )));
                }
                Ok(FactDraft {
                    subject,
                    relation,
                    object,
                    confidence: v
                        .get("confidence")
                        .and_then(|c| c.as_f64())
                        .unwrap_or(DRAFT_DEFAULT_CONFIDENCE)
                        .clamp(0.0, 1.0),
                })
            })
            .collect()
    }
}

/// Provenance stamped on facts attached to their source grain by
/// [`Areev::attach_facts`].
///
/// The default — used by `remember()`'s own extractor seam and by the
/// host-supplied `--facts` path — adds nothing beyond the `derived_from` link:
/// a host asserting its own facts is not relaying a model's claim, so it gets
/// no model attribution and no verification status.
#[derive(Debug, Clone, Default)]
pub struct FactAttribution<'a> {
    /// OMS `verification_status` (`"unverified"` / `"verified"` / …). Facts a
    /// model extracted from free text are `"unverified"` until something
    /// checks them; it is CAL-filterable, so the queue is queryable.
    pub verification_status: Option<&'a str>,
    /// Identifier of the model that produced these drafts, recorded so an
    /// audit can answer "which model wrote this?" from the grain itself.
    ///
    /// It rides in `extra_fields` as `extractor_model` — the same mechanism
    /// Areev Loop uses to carry structured data on a grain. `GrainCommon` has a
    /// `provenance_chain` field that looks like the natural home, but nothing
    /// in `areev-core` serializes it, so a value written there would be
    /// silently dropped at the blob boundary.
    pub extractor_model: Option<&'a str>,
}

/// Who captured a piece of raw content, and where it sits in a conversation.
/// Every field is optional: `areev remember` names an observer, the MCP tool and
/// `capture-stop` carry a session and a role, and a bare call needs none.
#[derive(Debug, Clone, Default)]
pub struct Capture<'a> {
    /// The agent/process that captured the text. Kept in `extra_fields`
    /// (an Event's `role` is the *author*, which is a different question).
    pub observer: Option<&'a str>,
    /// Session/thread id — what puts the Event in the thread index, so a
    /// remembered turn is reachable as part of its transcript.
    pub session_id: Option<&'a str>,
    /// `user` | `assistant` | `system` | `tool`. Unrecognized values are
    /// dropped rather than rejected.
    pub role: Option<&'a str>,
    /// OMS §8.2 `run_id` — the correlation key `run_trace`, `run_yield` and
    /// `runs_touching` read back.
    ///
    /// Without this on the capture path, `run_id` was writable only by
    /// constructing an `Event` in Rust, so the run-history reads were
    /// unreachable by construction from the CLI, MCP and both bindings: they
    /// could ask which grains belong to a run, but nothing they could call ever
    /// put a grain in one.
    pub run_id: Option<&'a str>,
}

/// Result of `Areev::remember`.
#[derive(Debug, Clone)]
pub struct RememberResult {
    /// The Event grain holding the raw captured text.
    pub event: Hash,
    pub facts: Vec<Hash>,
}

/// Integrity report (`Areev::verify`).
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub integrity: String,
    /// Benign notes from Turso's experimental FTS internal indexes
    /// (integrity_check miscounts them; not data corruption).
    pub fts_notes: Vec<String>,
    pub grains: usize,
    pub hash_mismatches: usize,
    pub undecodable: usize,
}

/// Store statistics (`Areev::stats`).
#[derive(Debug, Clone)]
pub struct StoreStats {
    pub grains: usize,
    pub current: usize,
    pub triples: usize,
    pub terms: usize,
    pub ops: usize,
    pub events_indexed: usize,
}

/// One version in a supersession chain (`Areev::history`, newest first).
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub hash: Hash,
    pub object: String,
    pub created_at: i64,
    pub confidence: f64,
    pub superseded_by: Option<Hash>,
}

/// An open fork: a `(namespace, subject, relation)` that has more than one
/// live head, because two writers superseded the same value concurrently
/// (e.g. edits synced from two edges). The tips coexist — nothing is lost —
/// until an explicit merge closes the fork. `heads[0]` is the deterministic
/// provisional head every node agrees on.
#[derive(Debug, Clone)]
pub struct ForkGroup {
    pub namespace: String,
    pub subject: String,
    pub relation: String,
    pub heads: Vec<Hash>,
}

const BUNDLE_MAGIC: &[u8; 4] = b"MGB1";
/// A v2 bundle carries a registry meta segment (saved queries, templates,
/// retention policies) between the magic and the op records. Emitted ONLY
/// when the source file has such rows, so registry-free bundles stay
/// byte-identical to v1 and readable by older builds; an older build
/// importing a v2 bundle refuses loudly at the magic check rather than
/// silently dropping the registry.
const BUNDLE_MAGIC_V2: &[u8; 4] = b"MGB2";

/// Meta-row key prefixes that replicate in bundles: the file-carried
/// registries (saved queries `qry:`, templates `tpl:`) and the declarative
/// retention policies. Infrastructure file-truths (`text_index`,
/// `entity_relations`, embedding provenance, `link_index`,
/// `min_reader_version`) deliberately do NOT ride along — they describe the
/// *source* file's indexing and host capabilities, which a replica may not
/// share. Legal `hold:` rows stay file-local pending an erasure-docs
/// decision on whether holds must survive restore.
/// One live pseudonym mapping: `(namespace, mapping_id, placeholder → value)`.
pub type AnonMappingRow = (String, String, std::collections::BTreeMap<String, String>);

const REPLICABLE_META_PREFIXES: [&str; 5] =
    ["qry:", "tpl:", "retention:", "retention_floor:", "anon:"];

/// RRF fusion constant used by `recall_hybrid` (the standard k0 = 60).
/// Exported so observability surfaces can report the effective value.
pub const RRF_K0: f64 = 60.0;

/// Absolute cap on the candidate pool a refinement stage (rerank / MMR)
/// considers. Bounds cross-encoder cost and the MMR pairwise-similarity join
/// regardless of how far a caller over-fetches; a larger requested `k` still
/// widens the pool to at least `k`.
const REFINE_POOL: usize = 64;

/// Semver-ish `a < b` over dotted numeric versions. Non-numeric components
/// (a `-rc1` suffix) compare as 0, which errs toward *not* warning — a false
/// alarm on every open would train people to ignore the one that matters.
fn version_lt(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u32> {
        v.split(['.', '-', '+'])
            .map(|p| p.parse::<u32>().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or(0), pb.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

/// The highest `.mg` grain type byte an Areev build before OMS 1.5 could
/// decode. `deserialize_blob` **errors** on an unknown type byte rather than
/// skipping it, so a file carrying a newer grain is not merely partially
/// readable to an older build — the read fails.
const LEGACY_MAX_GRAIN_BYTE: u8 = 0x0B;

/// Reader version stamped into `meta` the first time a grain newer than
/// [`LEGACY_MAX_GRAIN_BYTE`] is written to a file.
///
/// OMS §4.5 guarantees an additive type byte leaves existing *content
/// addresses* valid; it says nothing about older *readers*. This turns the
/// resulting failure from a mid-recall decode error into a statement the file
/// makes about itself, which `open_warnings()` surfaces. It cannot help builds
/// that shipped before the check existed — for those the only safe posture is
/// not to sync a file containing new grain types.
/// `meta` key prefix for declared retention policies — one row per
/// namespace, the same file-truth pattern as `qry:` / `tpl:`.
const RETENTION_PREFIX: &str = "retention:";

/// `meta` key prefix for retention FLOORS — the ceiling machinery's missing
/// half (governed-agents §5.4): EU AI Act Art. 12 mandates ≥6-month
/// retention of lifecycle logs, and a `PURGE OLDER THAN 30d` against the
/// runs namespace would destroy them with a clean audit trail of having
/// done so. A floor makes every age-based destruction younger than
/// `min_days` a refusal, not a sweep.
const RETENTION_FLOOR_PREFIX: &str = "retention_floor:";

/// `meta` key prefix for LEGAL HOLDS — while a hold is live on a namespace,
/// ALL age-based destruction there refuses, floors notwithstanding
/// (litigation holds are not a retention parameter, they are a stop).
const HOLD_PREFIX: &str = "hold:";

const MIN_READER_VERSION_KEY: &str = "min_reader_version";

/// File-truth: the link indexes (`prov_idx`, `run_idx`, `related_to`
/// cross-links) are built and current for every grain in this file.
///
/// Its absence — not the tables being empty — is what marks a file written
/// before those indexes existed. A file can legitimately have zero rows in all
/// three (no grain carries `derived_from`, `run_id` or `related_to`), so
/// emptiness cannot tell "never indexed" from "nothing to index", and guessing
/// wrong either re-scans the corpus on every open or answers provenance
/// questions with silence forever.
const LINK_INDEX_KEY: &str = "link_index";

/// Bumped when a change to what the link indexes contain requires a rebuild of
/// files stamped with an earlier value.
const LINK_INDEX_VERSION: &str = "2";

/// Open options.
pub struct AreevOptions {
    /// Relations whose objects are entities (get OSP reverse-index rows).
    /// Defaults to the OMS `mg:` entity-valued vocabulary.
    pub entity_relations: HashSet<String>,
    /// Populate the FTS text column (BM25 leg). Turso's experimental FTS
    /// costs ~150ms per write txn on segment commits — voice/edge deployments
    /// set this false (structural + vector legs still serve recall; §6).
    pub index_text: bool,
    /// Encryption-at-rest key: 32 bytes → AES-256-GCM via Turso's page cipher.
    /// `None` = plaintext. Host-supplied capability, never persisted in the
    /// file — a bare `open()` cannot supply it, so
    /// encrypted files must be opened with `open_with`/`open_encrypted`.
    /// Destroying the key destroys the memory (crypto-erasure). Covers the
    /// memory database (grains, indexes, op-log, WAL), the telemetry
    /// sidecar, and — under an HKDF-derived subkey — the `.blobs` CAS
    /// sidecar. Attachments written before blob encryption landed stay
    /// plaintext until [`Areev::encrypt_blobs`] migrates them.
    pub encryption_key: Option<[u8; 32]>,
    /// Recall-telemetry retention for the `<file>.telemetry.db` sidecar.
    /// Host-only capability (never persisted in the file, like the embedder):
    /// a bare `open()` leaves it `Off`, so the library default records nothing;
    /// agent-facing hosts opt into `Aggregate`. The sidecar is encrypted under
    /// this same key when `encryption_key` is set. See [`telemetry`].
    pub telemetry: TelemetryMode,
}

impl Default for AreevOptions {
    fn default() -> Self {
        let ents = [
            "mg:delegates_to",
            "mg:owned_by",
            "mg:assigned_to",
            "mg:depends_on",
            "mg:handed_off_to",
            "mg:capable_of",
            "delegates_to",
            "reports_to",
            "part_of",
            "knows",
        ];
        AreevOptions {
            entity_relations: ents.iter().map(|s| s.to_string()).collect(),
            index_text: true,
            encryption_key: None,
            telemetry: TelemetryMode::Off,
        }
    }
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT)",
    "CREATE TABLE IF NOT EXISTS terms(id INTEGER PRIMARY KEY, term TEXT UNIQUE)",
    "CREATE TABLE IF NOT EXISTS grains(
        seq INTEGER PRIMARY KEY,
        hash BLOB,
        ns INTEGER, gtype INTEGER, created_at INTEGER,
        s INTEGER, p INTEGER, o INTEGER,
        vf INTEGER, vt INTEGER,
        svf INTEGER, svt INTEGER,
        superseded_by BLOB, supersedes BLOB,
        text TEXT,
        blob BLOB NOT NULL)",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_grains_hash ON grains(hash)",
    "CREATE TABLE IF NOT EXISTS embeddings(seq INTEGER PRIMARY KEY, vec BLOB)",
    // BM25 leg — our own inverted index. See `search_text` for why this is not
    // Turso's `USING fts`.
    "CREATE TABLE IF NOT EXISTS fts_vocab(id INTEGER PRIMARY KEY, term TEXT UNIQUE)",
    "CREATE TABLE IF NOT EXISTS fts_post(term INTEGER, seq INTEGER, ns INTEGER, tf INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_fts_post ON fts_post(term, ns)",
    "CREATE INDEX IF NOT EXISTS idx_fts_post_seq ON fts_post(seq)",
    "CREATE TABLE IF NOT EXISTS fts_doc(seq INTEGER PRIMARY KEY, len INTEGER)",
    "CREATE TABLE IF NOT EXISTS triples(ns INTEGER, s INTEGER, p INTEGER, o INTEGER, seq INTEGER, cur INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_spo ON triples(ns,s,p,o,seq)",
    "CREATE INDEX IF NOT EXISTS idx_pos ON triples(ns,p,o,s,seq)",
    "CREATE INDEX IF NOT EXISTS idx_triples_seq ON triples(seq)",
    "CREATE TABLE IF NOT EXISTS osp(ns INTEGER, o INTEGER, s INTEGER, p INTEGER, seq INTEGER, cur INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_osp ON osp(ns,o,s)",
    "CREATE INDEX IF NOT EXISTS idx_osp_seq ON osp(seq)",
    "CREATE TABLE IF NOT EXISTS entity_latest(ns INTEGER, s INTEGER, p INTEGER, o INTEGER, seq INTEGER, hash BLOB, PRIMARY KEY(ns,s,p))",
    "CREATE TABLE IF NOT EXISTS heads(ns INTEGER, s INTEGER, p INTEGER, seq INTEGER, hash BLOB, created_at INTEGER, PRIMARY KEY(ns,s,p,seq))",
    "CREATE TABLE IF NOT EXISTS oplog(op_seq INTEGER PRIMARY KEY, hlc INTEGER, op INTEGER, hash BLOB)",
    "CREATE TABLE IF NOT EXISTS thread_idx(ns INTEGER, session INTEGER, seq INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_thread ON thread_idx(ns, session, seq)",
    // Reverse provenance: parent content address -> the grains derived from it.
    // `derived_from` sits on *every* grain, so indexing it as triples would add
    // a row for a large fraction of the store and inflate the index that recall
    // scans. A narrow table keeps the cost proportional to the question.
    // The parent is the raw 32-byte hash, not a dictionary term — interning one
    // term per grain address would bloat `terms` for no lookup benefit.
    "CREATE TABLE IF NOT EXISTS prov_idx(ns INTEGER, parent BLOB, seq INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_prov ON prov_idx(ns, parent, seq)",
    // Run correlation: `run_id` -> the grains recorded during that run. Run ids
    // repeat across many grains, so this one *is* dictionary-encoded.
    "CREATE TABLE IF NOT EXISTS run_idx(ns INTEGER, run INTEGER, seq INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_run ON run_idx(ns, run, seq)",
    // Sparse, replicated registry index. `agent:harness` also carries sampled
    // assembly manifests and run configs, so finding corpus exports by scanning
    // that namespace becomes unbounded. The grain remains the source of truth;
    // this table is rebuilt from its canonical body on bundle import.
    "CREATE TABLE IF NOT EXISTS corpus_idx(seq INTEGER PRIMARY KEY)",
];

fn pi(x: i64) -> Value {
    Value::Integer(x)
}
fn pb(b: Vec<u8>) -> Value {
    Value::Blob(b)
}
fn pt(s: &str) -> Value {
    Value::Text(s.to_string())
}
fn opt_i(v: Option<i64>) -> Value {
    match v {
        Some(x) => Value::Integer(x),
        None => Value::Null,
    }
}

/// Hex-encode a 32-byte key. The engine-open copy lives in db.rs; this one
/// remains only for the byte-order pin below.
#[cfg(test)]
fn hex32(k: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in k {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---- passphrase key derivation (Argon2id) -------------------------------

/// Argon2id parameters for passphrase-derived encryption keys (OWASP 2024).
const KDF_M_COST: u32 = 19_456; // memory in KiB (19 MiB)
const KDF_T_COST: u32 = 2; // iterations
const KDF_P_COST: u32 = 1; // parallelism
const KDF_SALT_LEN: usize = 16;

fn kdf_err<E: std::fmt::Display>(e: E) -> AreevError {
    AreevError::CryptoError(e.to_string())
}

/// Load the KDF salt/params sidecar at `<db>.kdf`, creating it with a fresh
/// random salt if absent. The salt is not secret, but it must travel with the
/// database file so the same passphrase re-derives the same key.
fn load_or_create_kdf_sidecar(sidecar: &str) -> Result<([u8; KDF_SALT_LEN], u32, u32, u32)> {
    match std::fs::read_to_string(sidecar) {
        Ok(text) => parse_kdf_sidecar(&text, sidecar),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut salt = [0u8; KDF_SALT_LEN];
            getrandom::getrandom(&mut salt).map_err(kdf_err)?;
            let line = format!(
                "v1 argon2id {} {KDF_M_COST} {KDF_T_COST} {KDF_P_COST}\n",
                hex::encode(salt)
            );
            // Atomic create: if another process wrote the sidecar first, do not
            // clobber it — re-read so both derive from the same persisted salt.
            match std::fs::OpenOptions::new().write(true).create_new(true).open(sidecar) {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(line.as_bytes()).map_err(kdf_err)?;
                    Ok((salt, KDF_M_COST, KDF_T_COST, KDF_P_COST))
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let text = std::fs::read_to_string(sidecar).map_err(kdf_err)?;
                    parse_kdf_sidecar(&text, sidecar)
                }
                Err(e) => Err(kdf_err(e)),
            }
        }
        Err(e) => Err(kdf_err(e)),
    }
}

/// Parse and sanity-check a KDF sidecar's contents.
fn parse_kdf_sidecar(text: &str, sidecar: &str) -> Result<([u8; KDF_SALT_LEN], u32, u32, u32)> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    if toks.len() != 6 || toks[0] != "v1" || toks[1] != "argon2id" {
        return Err(kdf_err(format!("malformed KDF sidecar: {sidecar}")));
    }
    let salt_bytes = hex::decode(toks[2]).map_err(kdf_err)?;
    if salt_bytes.len() != KDF_SALT_LEN {
        return Err(kdf_err("KDF salt has wrong length"));
    }
    let mut salt = [0u8; KDF_SALT_LEN];
    salt.copy_from_slice(&salt_bytes);
    let m = toks[3].parse::<u32>().map_err(kdf_err)?;
    let t = toks[4].parse::<u32>().map_err(kdf_err)?;
    let p = toks[5].parse::<u32>().map_err(kdf_err)?;
    // Reject absurd parameters (e.g. a tampered sidecar forcing a multi-GiB
    // allocation → OOM). Bounds are generous but finite.
    if !(8..=1_048_576).contains(&m) || !(1..=16).contains(&t) || !(1..=16).contains(&p) {
        return Err(kdf_err("KDF parameters out of range"));
    }
    Ok((salt, m, t, p))
}

impl Areev {
    /// Derive a 32-byte AES-256 key from a passphrase using Argon2id. The salt
    /// and cost parameters live in a non-secret `<path>.kdf` sidecar created on
    /// first use. The returned key zeroizes on drop.
    ///
    /// Losing the passphrase destroys the key (crypto-erasure); losing the
    /// `.kdf` sidecar means the passphrase can no longer re-derive the key, so
    /// back it up alongside the database.
    pub fn derive_key_for(path: &str, passphrase: &str) -> Result<zeroize::Zeroizing<[u8; 32]>> {
        if passphrase.trim().is_empty() {
            return Err(kdf_err("passphrase must not be empty or whitespace-only"));
        }
        let sidecar = format!("{path}.kdf");
        let (salt, m, t, p) = load_or_create_kdf_sidecar(&sidecar)?;
        let params = argon2::Params::new(m, t, p, Some(32)).map_err(kdf_err)?;
        let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        argon
            .hash_password_into(passphrase.as_bytes(), &salt, &mut key[..])
            .map_err(kdf_err)?;
        Ok(key)
    }

    /// Open (or create) an encrypted memory using a passphrase-derived key
    /// (Argon2id + AES-256-GCM at rest). Convenience over
    /// [`Areev::derive_key_for`] + [`Areev::open_with`].
    pub fn open_with_passphrase(path: &str, passphrase: &str) -> Result<Self> {
        let key = Self::derive_key_for(path, passphrase)?;
        Self::open_with(
            path,
            AreevOptions { encryption_key: Some(*key), ..AreevOptions::default() },
        )
    }

    /// [`open_with_passphrase`](Self::open_with_passphrase) with a
    /// recall-telemetry sidecar (encrypted under the same passphrase-derived
    /// key). The agent-host binding path.
    pub fn open_with_passphrase_telemetry(
        path: &str,
        passphrase: &str,
        telemetry: TelemetryMode,
    ) -> Result<Self> {
        let key = Self::derive_key_for(path, passphrase)?;
        Self::open_with(
            path,
            AreevOptions {
                encryption_key: Some(*key),
                telemetry,
                ..AreevOptions::default()
            },
        )
    }
}


fn vec_to_json(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 { s.push(','); }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

fn now_ms() -> i64 {
    // Simulation/replay seam shared with `areev-loop-adapter`. It pins HLC wall
    // inputs, supersession timestamps, and the missing-created_at index
    // fallback. A malformed override is fatal by design: silently falling
    // back to wall time would make a supposedly deterministic replay lie.
    if let Ok(value) = std::env::var("AREEV_LOOP_NOW_MS") {
        return value.trim().parse().unwrap_or_else(|_| {
            panic!("AREEV_LOOP_NOW_MS is set but not epoch milliseconds: {value:?}")
        });
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// The read-side coercion contract now lives on `db::Row`'s accessors; these
// remain only for the pin below that documents it against raw `Value`s.
#[cfg(test)]
fn v_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        _ => None,
    }
}

#[cfg(test)]
fn v_blob(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Blob(b) => Some(b.clone()),
        _ => None,
    }
}

#[cfg(test)]
fn v_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Real(r) => Some(*r),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

/// Comma-separated list of i64 seqs for an inline `IN (...)` clause. Safe:
/// the values are engine-internal seq ids, never user text.
fn seq_csv(seqs: &[i64]) -> String {
    let mut s = String::with_capacity(seqs.len() * 6);
    for (i, x) in seqs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s
}

/// A grain fully prepared for insertion (serialized + extracted + encoded).
struct GrainPrep {
    blob: Vec<u8>,
    hash: Hash,
    ns_id: i64,
    s: Option<i64>,
    p: Option<i64>,
    o: Option<i64>,
    osp: bool,
    session: Option<i64>,
    vf: Option<i64>,
    vt: Option<i64>,
    created: i64,
    gtype: i64,
    text: Option<String>,
    embedding: Option<Vec<f32>>,
    /// `(fts_vocab.id, term frequency)` for this grain's text, and the
    /// document length in tokens. Empty when text indexing is off or deferred.
    tokens: Vec<(i64, i64)>,
    doc_len: i64,
    /// Term-encoded `related_to` cross-links: `(subject = this grain's own
    /// hash, predicate = relation_type, object = target hash)`. Indexed into
    /// `triples`/`osp` for retrieval but deliberately **not** into
    /// `heads`/`entity_latest` — OMS §15.3 is normative that a `related_to`
    /// link is an annotation and MUST NOT change the target's supersession
    /// state. Empty for the overwhelming majority of grains.
    links: Vec<(i64, i64, i64)>,
    /// Dictionary id of `run_id`, for the run index.
    run: Option<i64>,
    /// Raw 32-byte `derived_from` parent address, for reverse provenance.
    parent: Option<Vec<u8>>,
    corpus_export: bool,
}

/// Extracted index-relevant fields of a grain about to be stored.
struct GrainView {
    ns: String,
    subject: Option<String>,
    relation: Option<String>,
    object: Option<String>,
    session: Option<String>,
    vf: Option<i64>,
    vt: Option<i64>,
    created_at: i64,
    gtype: u8,
    /// `(relation_type, target hash)` pairs from `related_to`.
    links: Vec<(String, String)>,
    /// `run_id` — the only run-scoped correlation key in the grain model.
    run: Option<String>,
    /// `derived_from` — this grain's provenance parent, if any.
    parent: Option<String>,
    corpus_export: bool,
}

fn extract_view(view: &DeserializedGrain) -> GrainView {
    GrainView {
        ns: view.get_str("namespace").unwrap_or("shared").to_string(),
        subject: view.get_str("subject").map(str::to_string),
        relation: view.get_str("relation").map(str::to_string),
        object: view.get_str("object").map(str::to_string),
        session: view.get_str("session_id").map(str::to_string),
        vf: view.get_i64("valid_from"),
        vt: view.get_i64("valid_to"),
        created_at: view.get_i64("created_at").unwrap_or_else(now_ms),
        gtype: view.grain_type as u8,
        run: view.get_str("run_id").map(str::to_string),
        parent: view.get_str("derived_from").map(str::to_string),
        corpus_export: view.grain_type == areev_core::types::GrainType::Observation
            && view
                .fields
                .get("context")
                .and_then(|value| value.get("corpus_export"))
                .and_then(|value| value.as_bool())
                == Some(true),
        links: view
            .fields
            .get("related_to")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| {
                        Some((
                            l.get("relation_type")?.as_str()?.to_string(),
                            l.get("hash")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The single text projection the FTS and vector legs index: the grain's
/// explicit `embedding_text` override when present (its documented contract —
/// import pipelines set it to preserve original prose), else
/// "subject relation object" plus any top-level `content`. `None` = nothing
/// to index. Used by the write path, the reranker's candidate text, and the
/// `rebuild_text_index` backfill so all three stay in lockstep.
/// BM25 tuning. Textbook defaults; the corpus here is short documents
/// (a fact projects to "subject relation object"), so `b` matters more than
/// `k1` and neither is worth exposing until someone can show a workload where
/// tuning them helps.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// Longest token kept. Anything longer is a hash, a base64 blob or a URL —
/// never a search term, and unbounded vocabulary growth if indexed.
const MAX_TOKEN_LEN: usize = 64;

/// Split text into index terms.
///
/// Deliberately plain: lowercase, split on anything not alphanumeric, drop
/// what is too long. No stemming and no stopword list, because both make
/// results depend on a language guess — and the same function runs at index
/// time and query time, so whatever it does, the two agree. Grain text is
/// already NFC-normalized by the canonical serializer, so `is_alphanumeric`
/// is enough for non-ASCII scripts.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && t.chars().count() <= MAX_TOKEN_LEN)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Tokens with their in-document frequency, plus the document's length in
/// tokens (BM25 needs the length including repeats).
fn token_freqs(text: &str) -> (Vec<(String, i64)>, i64) {
    let tokens = tokenize(text);
    let len = tokens.len() as i64;
    let mut freqs: HashMap<String, i64> = HashMap::new();
    for t in tokens {
        *freqs.entry(t).or_insert(0) += 1;
    }
    (freqs.into_iter().collect(), len)
}

fn projected_text(view: &DeserializedGrain) -> Option<String> {
    if let Some(et) = view.get_str("embedding_text") {
        if !et.trim().is_empty() {
            return Some(et.to_string());
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if let (Some(s), Some(r), Some(o)) = (
        view.get_str("subject"),
        view.get_str("relation"),
        view.get_str("object"),
    ) {
        parts.push(format!("{s} {r} {o}"));
    }
    if let Some(c) = view.get_str("content") {
        parts.push(c.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Where the CAS blob payloads live: a `.blobs` fan-out directory next to
/// the memory file (embedded backend), or an in-schema `blobs` table
/// (postgres backend — a connection string has no "next to").
enum BlobStore {
    Fs(std::path::PathBuf),
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    Table,
}

fn fs_blob_path(dir: &std::path::Path, hex: &str) -> std::path::PathBuf {
    dir.join(&hex[..2]).join(&hex[2..])
}

/// Files this process currently holds open, so a second handle on one file is
/// refused rather than admitted.
///
/// The embedded backend is **single-writer per file**, and it enforces that
/// across processes with an OS file lock. Inside one process that lock is
/// already held, so a second `open()` succeeded — and then the two handles'
/// independent in-memory allocators (`next_seq`, `next_term`, the BM25 stats)
/// drifted apart until a write collided, surfacing as
/// `UNIQUE constraint failed: terms.id` attributed to *the other* handle. That
/// message names neither handles nor writers, and the failure arrives long
/// after the mistake.
///
/// Opening a handle per request or per agent turn is the obvious thing to do if
/// you think of the file as a database connection, so this needs to be decided
/// at open, with a message that names the real cause.
static OPEN_FILES: std::sync::Mutex<Option<HashSet<std::path::PathBuf>>> =
    std::sync::Mutex::new(None);

/// Releases this process's claim on a memory file when the handle drops.
struct OpenFileGuard(std::path::PathBuf);

impl OpenFileGuard {
    /// Claim `path` for this process, or report who holds it.
    ///
    /// The key is the absolute path rather than the canonical one: a file that
    /// does not exist yet cannot be canonicalized, and `open()` creates. Two
    /// spellings that resolve to the same file through a symlink are therefore
    /// not caught here — the cross-process lock still is the backstop for that.
    fn claim(path: &str) -> Result<Self> {
        let key = std::path::absolute(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
        let mut held = OPEN_FILES.lock().unwrap_or_else(|e| e.into_inner());
        let set = held.get_or_insert_with(HashSet::new);
        if !set.insert(key.clone()) {
            return Err(AreevError::StoreBusy(format!(
                "{} is already open in this process. One memory is one writer: \
                 share the handle (it is safe across threads) instead of opening \
                 a second one — a second handle keeps its own sequence and \
                 dictionary allocators, and their first collision fails the \
                 OTHER handle's write",
                key.display()
            )));
        }
        Ok(Self(key))
    }
}

impl Drop for OpenFileGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = OPEN_FILES.lock() {
            if let Some(set) = held.as_mut() {
                set.remove(&self.0);
            }
        }
    }
}

/// The embedded Areev store handle — one file per memory.
pub struct Areev {
    db: Box<dyn Db>,
    dict: HashMap<String, i64>,
    next_term: i64,
    next_seq: i64,
    next_op: i64,
    hlc_last: i64,
    entity_rels: HashSet<String>,
    index_text: bool,
    /// Lazily-filled token -> `fts_vocab.id` cache. Not preloaded at open:
    /// the text vocabulary is far larger than the triple dictionary and
    /// reading it eagerly would put corpus-sized work back into every open.
    fts_vocab: HashMap<String, i64>,
    /// Live document count and total token length, for BM25's `N` and `avgdl`.
    /// Kept in memory and adjusted on write for the same reason `next_seq` is:
    /// an aggregate over the postings on every search would be O(corpus), the
    /// exact thing this index exists to avoid.
    fts_docs: i64,
    fts_total_len: i64,
    /// Set by [`defer_text_index`](Self::defer_text_index) — suppresses
    /// posting writes during a bulk load until `rebuild_text_index` runs.
    fts_deferred: bool,
    embedder: Option<Box<dyn EmbedBackend>>,
    /// Optional cross-encoder reranker (Tier-2). Host-supplied, off by default.
    reranker: Option<Box<dyn RerankBackend>>,
    /// Optional query expander (Tier-1). `None` falls back to the built-in
    /// English expander when `RecallTuning::query_expansion` is set.
    expander: Option<Box<dyn QueryExpander>>,
    /// Embedding provenance declared by the file (meta table): model + dim
    /// of the vectors already stored, recorded when the first backend is
    /// installed.
    meta_embed: Option<(String, usize)>,
    /// Reconciliation notes from open / set_embedder (file declarations vs
    /// what this session supplied). Never fatal; surfaced by hosts.
    warnings: Vec<String>,
    /// False once the file carries a `min_reader_version` stamp, so the check
    /// on the write path costs one bool after the first newer-than-1.4 grain.
    needs_min_reader_stamp: bool,
    blob_store: BlobStore,
    /// Blob-encryption key for the `.blobs` CAS sidecar, HKDF-derived from
    /// the page-cipher key (`blobcrypt`). `None` for a plaintext memory.
    /// This is deliberately NOT the page key — see `blobcrypt` for why the
    /// handle retains a derived key rather than a copy of the original.
    blob_key: Option<zeroize::Zeroizing<[u8; 32]>>,
    /// Recall-telemetry sidecar (`<file>.telemetry.db`). `None` when the host
    /// left telemetry `Off` — the recall path then does nothing extra.
    telemetry: Option<Telemetry>,
    /// The egress boundary: `anon:<ns>` policy cache + session pseudonymizer
    /// state (docs/anonymization-proposal.md P1). Idle = one branch per read.
    anon: anon_gate::AnonGate,
    /// Host-scoped trajectory identifier copied into full recall telemetry.
    /// It is never persisted as a file truth and never changes rollup keys.
    ambient_run_id: Option<String>,
    /// This process's claim on the memory file, released on drop. `None` on
    /// the Postgres backend, which admits multiple concurrent writers per
    /// memory by design and arbitrates them with an in-schema counters row.
    _file_guard: Option<OpenFileGuard>,
}

impl Areev {
    /// Open honoring the file's own declarations (`meta` table) when
    /// present. A fresh file is stamped with the defaults. This is the
    /// file-truth path: settings like `text_index` travel with the file,
    /// so the same memory behaves identically on any host.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_internal(path, None, TelemetryMode::Off)
    }

    /// Open with explicit options. Explicit options are deliberate: they
    /// re-stamp the file's declarations, and a change to an existing
    /// declaration is recorded in `open_warnings()`.
    pub fn open_with(path: &str, opts: AreevOptions) -> Result<Self> {
        let telemetry = opts.telemetry;
        Self::open_internal(path, Some(opts), telemetry)
    }

    /// Open honoring the file's own declarations (like [`open`](Self::open))
    /// but with a recall-telemetry sidecar enabled. Telemetry is host config,
    /// **not** a file-truth, so this deliberately does *not* re-stamp the file's
    /// `index_text`/`entity_relations` declarations — it just attaches the
    /// sidecar (encrypted under the file's key when the file is encrypted; for
    /// an encrypted file, open with [`open_with`](Self::open_with) instead so
    /// the key is supplied).
    pub fn open_with_telemetry(path: &str, telemetry: TelemetryMode) -> Result<Self> {
        Self::open_internal(path, None, telemetry)
    }

    /// Open (or create) an encrypted memory: AES-256-GCM at rest with a
    /// host-supplied 32-byte key (Turso page cipher). The key lives only in
    /// the caller's process — never written to the file — so a bare `open()`
    /// of this path cannot read it, and destroying the key destroys the
    /// memory (crypto-erasure). Default index/relation options otherwise.
    pub fn open_encrypted(path: &str, key: [u8; 32]) -> Result<Self> {
        Self::open_with(
            path,
            AreevOptions { encryption_key: Some(key), ..AreevOptions::default() },
        )
    }

    fn open_internal(
        path: &str,
        explicit: Option<AreevOptions>,
        telemetry_mode: TelemetryMode,
    ) -> Result<Self> {
        // Keep the AEAD key only in a Zeroizing buffer for the duration of the
        // open; the raw copies (options + this local) are wiped once the engine
        // has ingested it, so no unzeroized key bytes linger on the heap.
        let enc_key = explicit
            .as_ref()
            .and_then(|o| o.encryption_key)
            .map(zeroize::Zeroizing::new);
        // Claim the file before touching it, so the refusal happens at open —
        // where the caller can act on it — rather than as a constraint
        // violation on some later write by a different handle.
        let guard = OpenFileGuard::claim(path)?;
        let dbh: Box<dyn Db> = Box::new(TursoDb::open(path, enc_key.as_deref())?);
        for sql in SCHEMA {
            dbh.execute(sql, vec![])?;
        }
        let mut warnings: Vec<String> = Vec::new();
        if enc_key.is_some() {
            warnings.push(
                "encryption-at-rest ON (AES-256-GCM): the memory database and the .blobs CAS \
                 sidecar are both encrypted (blobs under an HKDF-derived key). Attachments \
                 written by a build before blob encryption landed stay plaintext until \
                 migrated — run `areev blobs encrypt` on such a file"
                    .into(),
            );
        }
        let blob_dir = std::path::PathBuf::from(format!("{}.blobs", path));
        std::fs::create_dir_all(&blob_dir).map_err(db_err)?;
        // Telemetry sidecar (`<file>.telemetry.db`): opened under the SAME AEAD
        // key as the main file so crypto-erasure covers it. Only when the host
        // asked for it — `Off` opens no sidecar and costs the recall path
        // nothing. The mode is a separate argument, not read from `opts`, so
        // telemetry can be enabled on a declaration-honoring open without
        // re-stamping file-truths.
        let telemetry = match telemetry_mode {
            TelemetryMode::Off => None,
            mode => Some(Telemetry::open(path, enc_key.as_deref(), mode)?),
        };
        Self::finish_open(dbh, explicit, BlobStore::Fs(blob_dir), telemetry, warnings, Some(guard))
    }

    /// Open (or create) a memory in a PostgreSQL schema, honoring the
    /// schema's own `meta` declarations — the server-tier analogue of
    /// [`open`](Self::open). One memory = one schema (the unit of isolation,
    /// `pg_dump -n` export, and `DROP SCHEMA` erasure via
    /// [`pg::drop_postgres_schema`]). Unlike the single-writer file backend,
    /// this backend admits MULTIPLE CONCURRENT WRITERS per memory: any
    /// number of handles (across processes) may hold the same schema; write
    /// transactions serialize briefly on an in-schema counters row, reads
    /// never block, and racing supersedes/forgets resolve to one winner
    /// plus a clean error — see `pg.rs` for the model. CAS blobs live in an
    /// in-schema table; the telemetry sidecar (see
    /// [`open_postgres_with_telemetry`](Self::open_postgres_with_telemetry))
    /// rides the schema too. The page cipher is a file-backend capability
    /// and an `encryption_key` here is rejected.
    #[cfg(feature = "postgres")]
    pub fn open_postgres(url: &str, schema: &str) -> Result<Self> {
        Self::open_postgres_internal(url, schema, None, TelemetryMode::Off)
    }

    /// Explicit-options variant of [`open_postgres`](Self::open_postgres) —
    /// re-stamps the schema's declarations and records changes in
    /// [`open_warnings`](Self::open_warnings), mirroring [`open_with`](Self::open_with).
    #[cfg(feature = "postgres")]
    pub fn open_postgres_with(url: &str, schema: &str, opts: AreevOptions) -> Result<Self> {
        let telemetry = opts.telemetry;
        Self::open_postgres_internal(url, schema, Some(opts), telemetry)
    }

    /// Declaration-honoring open with the recall-telemetry sidecar attached
    /// — the postgres analogue of [`open_with_telemetry`](Self::open_with_telemetry).
    /// Telemetry tables live inside the memory's schema (their own
    /// connection), so erasure/export cover them with the memory.
    #[cfg(feature = "postgres")]
    pub fn open_postgres_with_telemetry(
        url: &str,
        schema: &str,
        telemetry: TelemetryMode,
    ) -> Result<Self> {
        Self::open_postgres_internal(url, schema, None, telemetry)
    }

    #[cfg(feature = "postgres")]
    fn open_postgres_internal(
        url: &str,
        schema: &str,
        explicit: Option<AreevOptions>,
        telemetry_mode: TelemetryMode,
    ) -> Result<Self> {
        if let Some(o) = &explicit {
            if o.encryption_key.is_some() {
                return Err(AreevError::Validation(
                    "encryption_key is a file-backend capability (page cipher); on the postgres \
                     backend use TDE/pgcrypto at the deployment layer"
                        .into(),
                ));
            }
        }
        // DDL + seeding run inside PgDb::open, under the schema's bootstrap
        // advisory lock — concurrent openers of a brand-new memory would
        // otherwise race the IF NOT EXISTS statements.
        let mut bootstrap: Vec<&str> = Vec::with_capacity(pg::PG_SCHEMA.len() + pg::PG_SEED.len());
        bootstrap.extend_from_slice(pg::PG_SCHEMA);
        bootstrap.extend_from_slice(pg::PG_SEED);
        let dbh: Box<dyn Db> = Box::new(pg::PgDb::open(url, schema, &bootstrap)?);
        let telemetry = match telemetry_mode {
            TelemetryMode::Off => None,
            mode => Some(Telemetry::open_pg(url, schema, mode)?),
        };
        Self::finish_open(dbh, explicit, BlobStore::Table, telemetry, Vec::new(), None)
    }

    /// Backend-independent tail of every open: meta reconciliation and
    /// stamping, dictionary/counter seeding, and the self-heal passes.
    fn finish_open(
        dbh: Box<dyn Db>,
        explicit: Option<AreevOptions>,
        blob_store: BlobStore,
        telemetry: Option<Telemetry>,
        mut warnings: Vec<String>,
        file_guard: Option<OpenFileGuard>,
    ) -> Result<Self> {
        // Derive the sidecar key BEFORE the page key is zeroized below.
        let blob_key = explicit
            .as_ref()
            .and_then(|o| o.encryption_key.as_ref())
            .map(blobcrypt::derive_blob_key);
        // The anon session key (keys `mapping_id`, D11): HKDF from the page
        // key on an encrypted file so it is stable per file+process, random
        // per handle otherwise. The memory key (value-derived tokens for
        // `memory` scope + ingress, D8) has no random fallback — those
        // features need cross-handle stability, so a plaintext file simply
        // has no memory key and refuses them at `set` time (Q5 resolved
        // conservatively). Neither key is ever persisted.
        let page_key = explicit.as_ref().and_then(|o| o.encryption_key.as_ref());
        let anon_memory_key: Option<[u8; 32]> = page_key.map(|pk| {
            let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, pk);
            let mut out = [0u8; 32];
            hk.expand(b"areev.anon.memory.v1", &mut out).expect("32-byte HKDF output");
            out
        });
        let anon_session_key: [u8; 32] = match page_key {
            Some(pk) => {
                let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, pk);
                let mut out = [0u8; 32];
                hk.expand(b"areev.anon.session.v1", &mut out)
                    .expect("32-byte HKDF output");
                out
            }
            None => {
                let mut out = [0u8; 32];
                getrandom::getrandom(&mut out).map_err(|e| {
                    AreevError::CryptoError(format!("anon session key: {e}"))
                })?;
                out
            }
        };
        // ---- file-carried declarations (meta k/v) --------------------
        let meta: HashMap<String, String> = {
            let mut m = HashMap::new();
            for row in dbh.query("SELECT k, v FROM meta", vec![])? {
                if let (Some(k), Some(v)) = (row.text(0), row.text(1)) {
                    m.insert(k.to_string(), v.to_string());
                }
            }
            m
        };
        let declared_text = meta.get("text_index").map(|v| v == "1");
        let declared_rels: Option<HashSet<String>> = meta
            .get("entity_relations")
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .map(|v| v.into_iter().collect());
        let meta_embed = match (
            meta.get("embedding_model"),
            meta.get("embedding_dim").and_then(|d| d.parse::<usize>().ok()),
        ) {
            (Some(m), Some(d)) => Some((m.clone(), d)),
            _ => None,
        };

        let mut opts = match explicit {
            Some(o) => {
                if let Some(d) = declared_text {
                    if d != o.index_text {
                        warnings.push(format!(
                            "file declared text_index={}; explicit open changed it to {} (re-stamped) — \
                             grains written under the old setting keep their old indexing",
                            if d { "on" } else { "off" },
                            if o.index_text { "on" } else { "off" },
                        ));
                    }
                }
                if let Some(ref d) = declared_rels {
                    if *d != o.entity_relations {
                        warnings.push(
                            "file-declared entity_relations differ from explicit options (re-stamped) — \
                             OSP rows indexed under the old set are unchanged"
                                .into(),
                        );
                    }
                }
                o
            }
            None => AreevOptions {
                index_text: declared_text.unwrap_or(true),
                entity_relations: declared_rels
                    .unwrap_or_else(|| AreevOptions::default().entity_relations),
                encryption_key: None,
                // Telemetry is host config, not a file-truth: a bare `open()`
                // never turns it on.
                telemetry: TelemetryMode::Off,
            },
        };
        // The plaintext key now lives only inside the storage engine (turso keeps
        // its own copy while the database is open); wipe the copy carried in the
        // open options so no unzeroized key bytes linger.
        opts.encryption_key.zeroize();

        // Stamp declarations + create the FTS index if wanted.
        dbh.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('text_index', ?1)",
            vec![pt(if opts.index_text { "1" } else { "0" })],
        )?;
        let mut rels: Vec<&String> = opts.entity_relations.iter().collect();
        rels.sort();
        let rels = serde_json::to_string(&rels).unwrap_or_else(|_| "[]".into());
        dbh.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('entity_relations', ?1)",
            vec![pt(&rels)],
        )?;
        // Files written before the BM25 leg moved off Turso's experimental
        // FTS carry an `idx_fts` index that nothing reads any more, and
        // that still taxes every write to this table. Drop it on sight.
        // Absent (the normal case) it errors; that is not a problem.
        let _ = dbh.execute("DROP INDEX idx_fts", vec![]);

        // Load dictionary + counters.
        let mut dict = HashMap::new();
        let mut next_term = 1i64;
        for row in dbh.query("SELECT id, term FROM terms", vec![])? {
            let id = row.i64(0).unwrap_or(0);
            if let Some(t) = row.text(1) {
                dict.insert(t.to_string(), id);
            }
            next_term = next_term.max(id + 1);
        }
        let one = |sql: &'static str| -> Result<i64> {
            Ok(dbh.query(sql, vec![])?.first().and_then(|r| r.i64(0)).unwrap_or(0))
        };
        let next_seq = one("SELECT COALESCE(MAX(seq),0) FROM grains")? + 1;
        let next_op = one("SELECT COALESCE(MAX(op_seq),0) FROM oplog")? + 1;
        let hlc_last = one("SELECT COALESCE(MAX(hlc),0) FROM oplog")?;
        let fts_docs = one("SELECT COUNT(*) FROM fts_doc")?;
        let fts_total_len = one("SELECT COALESCE(SUM(len),0) FROM fts_doc")?;
        let indexed_text = one("SELECT COUNT(*) FROM grains WHERE text IS NOT NULL")?;
        let grain_count = one("SELECT COUNT(*) FROM grains")?;

        let mut store = Areev {
            db: dbh,
            dict,
            next_term,
            next_seq,
            next_op,
            hlc_last,
            entity_rels: opts.entity_relations,
            index_text: opts.index_text,
            fts_vocab: HashMap::new(),
            fts_docs,
            fts_total_len,
            fts_deferred: false,
            embedder: None,
            reranker: None,
            expander: None,
            meta_embed,
            warnings,
            needs_min_reader_stamp: !meta.contains_key(MIN_READER_VERSION_KEY),
            blob_store,
            blob_key,
            telemetry,
            anon: anon_gate::AnonGate::new(anon_session_key, anon_memory_key),
            ambient_run_id: None,
            _file_guard: file_guard,
        };

        // Load the file's anonymization policies (+ seed known-identity
        // propagation from existing subjects). An unreadable row poisons the
        // gate (reads fail closed, D3) and is surfaced as an open warning so
        // the operator sees it before the first refused read.
        store.reload_anon_policies()?;
        if let Some(w) = store.anon.poison_warning() {
            store.warnings.push(w.to_string());
        }

        // The file may declare that it needs a newer reader than this build (a
        // grain type added after this binary was compiled). Say so at open
        // rather than letting a recall fail to decode a blob halfway through.
        if let Some(req) = meta.get(MIN_READER_VERSION_KEY) {
            if version_lt(env!("CARGO_PKG_VERSION"), req) {
                store.warnings.push(format!(
                    "this memory declares min_reader_version {req} but this build is {} — it \
                     contains grain types this version cannot decode, and reads that touch them \
                     will fail. Upgrade areev to {req} or later",
                    env!("CARGO_PKG_VERSION")
                ));
            }
        }

        // A file written by an older build has its text column populated but
        // no postings, because the BM25 leg used to be Turso's `USING fts`
        // index. Left alone, every free-text recall would answer "nothing
        // found" — the worst failure available, since it is indistinguishable
        // from an honest empty result. Rebuild once, here, and say so.
        if store.index_text && indexed_text > 0 && store.fts_docs == 0 {
            store.rebuild_text_index()?;
            // Report documents indexed, not the rebuild's backfill count —
            // that counts rows whose text column was NULL, which is zero on
            // exactly the files this branch exists for.
            let indexed = store.fts_docs;
            store.warnings.push(format!(
                "text index rebuilt on open ({indexed} grains): this file was written when the \
                 BM25 leg used Turso's experimental FTS index, which is no longer used. \
                 One-time; later opens skip it"
            ));
        }

        // Same reasoning for the link indexes (`prov_idx`, `run_idx`, and the
        // `related_to` cross-link triples). A file written before they existed
        // answers every provenance and run question with an empty result, which
        // is indistinguishable from an honest "nothing derived from this" — so
        // heal on open rather than leaving it to an `areev reindex` the caller has
        // no way to know they need.
        //
        // The stamp is what makes this decidable: emptiness alone cannot
        // distinguish "never indexed" from "nothing to index". A stamp that
        // does not match the current version heals too, so widening what the
        // indexes hold is a constant bump rather than a migration.
        if meta.get(LINK_INDEX_KEY).map(String::as_str) != Some(LINK_INDEX_VERSION) {
            if grain_count > 0 {
                let rows = store.rebuild_link_indexes()?;
                store.warnings.push(format!(
                    "link indexes rebuilt on open ({rows} rows across {grain_count} grains): this \
                     file was written before reverse provenance, run correlation and related_to \
                     cross-links were indexed. One-time; later opens skip it"
                ));
            } else {
                // Nothing to build, but stamp anyway so the next open of a file
                // that has since been written to does not re-scan it.
                store.meta_put(LINK_INDEX_KEY, LINK_INDEX_VERSION)?;
            }
        }

        Ok(store)
    }

    /// Install an embedding backend; subsequent adds embed their text
    /// and the vector leg joins hybrid recall.
    ///
    /// The first installed backend is recorded in the file's `meta` table
    /// as embedding provenance (model + dim). A later open that injects a
    /// different-dim backend gets a reconciliation warning instead of
    /// silently mixing vector spaces.
    pub fn set_embedder(&mut self, e: Box<dyn EmbedBackend>) {
        let (model, dim) = (e.model().to_string(), e.dim());
        // Make the vector storage usable FIRST (the postgres backend creates
        // its vector(dim) table here and hard-refuses a dim mismatch). On
        // failure the embedder is NOT installed — recall fails soft to the
        // structural/BM25 legs instead of every add failing mid-transaction.
        if let Err(err) = self.db.ensure_embeddings(dim) {
            self.warnings
                .push(format!("vector recall disabled — embedder {model}@{dim} not installed: {err}"));
            return;
        }
        match &self.meta_embed {
            Some((m, d)) => {
                if *d != dim {
                    self.warnings.push(format!(
                        "embedding mismatch: file vectors are {m}@{d}, injected backend is \
                         {model}@{dim} — vector recall may be degraded"
                    ));
                } else if *m != model && m != "unspecified" && model != "unspecified" {
                    self.warnings.push(format!(
                        "embedding model differs: file declares {m}, injected {model} (same dim {dim})"
                    ));
                }
            }
            None => {
                let ok = self
                    .db
                    .execute(
                        "INSERT OR REPLACE INTO meta(k, v) VALUES ('embedding_model', ?1)",
                        vec![pt(&model)],
                    )
                    .and_then(|_| {
                        self.db.execute(
                            "INSERT OR REPLACE INTO meta(k, v) VALUES ('embedding_dim', ?1)",
                            vec![pt(&dim.to_string())],
                        )
                    });
                if ok.is_ok() {
                    self.meta_embed = Some((model, dim));
                }
            }
        }
        self.embedder = Some(e);
    }

    /// Set the host's ambient trajectory identifier for subsequent recalls.
    /// Passing `None` clears it. This affects telemetry only; it is neither a
    /// persisted file truth nor part of the recall-intent rollup key.
    pub fn set_run_id(&mut self, run_id: Option<&str>) {
        self.ambient_run_id = run_id.filter(|id| !id.is_empty()).map(str::to_string);
    }

    /// Install a cross-encoder reranker (Tier-2). Opt-in per query via
    /// `RecallTuning::rerank`; with none installed, requesting rerank is a
    /// no-op (fusion order stands). Host owns the model — no ML dep in-engine.
    pub fn set_reranker(&mut self, r: Box<dyn RerankBackend>) {
        self.reranker = Some(r);
    }

    /// Whether a reranker backend is installed.
    pub fn has_reranker(&self) -> bool {
        self.reranker.is_some()
    }

    /// Install a custom query expander (Tier-1). When unset, requesting
    /// `RecallTuning::query_expansion` falls back to the built-in English
    /// [`EnglishExpander`]. Install your own for other languages/domains.
    pub fn set_query_expander(&mut self, e: Box<dyn QueryExpander>) {
        self.expander = Some(e);
    }

    /// Documents currently carried by the BM25 index.
    ///
    /// [`rebuild_text_index`](Self::rebuild_text_index) returns how many rows
    /// needed their *text column* backfilled, which is zero on a file that was
    /// already populated — this is the number that answers "did the rebuild
    /// actually index anything".
    pub fn indexed_documents(&self) -> i64 {
        self.fts_docs
    }

    /// Whether the BM25 text index is populated on writes (file-declared,
    /// honored or re-stamped at open).
    pub fn index_text_enabled(&self) -> bool {
        self.index_text
    }

    // ── CAL host metadata (saved queries, custom templates) ─────────────
    //
    // These are *not* memories. A saved query or a custom template is
    // host-managed metadata that belongs to the file so it travels with it
    // and works from the CLI and MCP as well as the console — so it rides
    // the `meta` k/v table, not the grain store. One row per entry
    // (`qry:<name>`, `tpl:<name>`) so recording a last-run timestamp does
    // not rewrite the whole set.

    /// Read every `meta` row whose key starts with `prefix`, returning
    /// `(key-without-prefix, value)` pairs.
    pub fn meta_scan(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        // `%` and `_` are LIKE wildcards, so a prefix containing either would
        // silently widen the scan. Escape them and say so with ESCAPE; the
        // `strip_prefix` check below is the backstop, not the mechanism.
        let pattern = format!("{}%", like_escape(prefix));
        let mut out = Vec::new();
        for row in self.db.query(
            "SELECT k, v FROM meta WHERE k LIKE ?1 ESCAPE '\\'",
            vec![pt(&pattern)],
        )? {
            if let (Some(k), Some(v)) = (row.text(0), row.text(1)) {
                if let Some(rest) = k.strip_prefix(prefix) {
                    out.push((rest.to_string(), v.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// Upsert a single `meta` row.
    /// Read one `meta` row. `None` for a missing key — a file that has never
    /// declared something is not an error.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let rows = self.db.query("SELECT v FROM meta WHERE k = ?1", vec![pt(key)])?;
        Ok(rows.first().and_then(|row| row.text(0)).map(str::to_string))
    }

    pub fn meta_put(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES (?1, ?2)",
            vec![pt(key), pt(value)],
        )?;
        Ok(())
    }

    /// Delete a single `meta` row. A missing key is not an error.
    pub fn meta_delete(&self, key: &str) -> Result<()> {
        self.db.execute("DELETE FROM meta WHERE k = ?1", vec![pt(key)])?;
        Ok(())
    }

    /// Declared retention policies (GDPR Art. 5(1)(e), storage limitation),
    /// one per namespace, newest read wins: `[(namespace, RetentionPolicy)]`
    /// sorted by namespace.
    ///
    /// Policy is a **file-truth**, stored as `retention:<ns>` meta rows
    /// exactly like saved queries and templates: it travels with the memory,
    /// so a file that says "events here live 90 days" keeps saying it after
    /// a copy, a sync, or a restore on a different host. Enforcement is a
    /// separate, explicit act ([`sweep_retention`](Self::sweep_retention) or
    /// the loop's governed queue) — declaring a policy never silently
    /// deletes anything.
    pub fn retention_policies(&self) -> Result<Vec<(String, RetentionPolicy)>> {
        let mut out = Vec::new();
        for (ns, raw) in self.meta_scan(RETENTION_PREFIX)? {
            match serde_json::from_str::<RetentionPolicy>(&raw) {
                Ok(p) => out.push((ns, p)),
                // A policy this build cannot parse must not silently mean
                // "no policy" — that would read as "keep forever" on a file
                // whose owner asked for deletion. Refuse loudly instead.
                Err(e) => {
                    return Err(AreevError::Validation(format!(
                        "unreadable retention policy for namespace '{ns}': {e}"
                    )))
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    // ---- anonymization policies (docs/anonymization-proposal.md P1) -------

    /// Declare (or replace) the anonymization policy for one namespace, as
    /// the JSON value of an `anon:<ns>` meta row — a file-truth that
    /// replicates write-if-absent, like `retention:<ns>`. Also stamps
    /// `min_reader_version`, so an older build opening this file warns
    /// loudly instead of silently serving it without the policy.
    pub fn set_anon_policy(&mut self, ns: &str, policy_json: &str) -> Result<()> {
        if ns.trim().is_empty() {
            return Err(AreevError::Validation(
                "anonymization policy needs a namespace".into(),
            ));
        }
        let policy = areev_core::anon::AnonPolicy::from_json(policy_json)?;
        // Value-derived pseudonyms (ingress modes, memory scope) need the
        // token key HKDF-derived from the page key — a plaintext file has no
        // key material, so those declarations refuse loudly here rather than
        // failing at the first read (Q5, resolved conservatively).
        let needs_memory_key =
            matches!(policy.mode.as_str(), "ingress" | "both") || policy.scope == "memory";
        if needs_memory_key && !self.anon.has_memory_key() {
            return Err(AreevError::Validation(format!(
                "anonymization mode \"{}\" / scope \"{}\" needs an encrypted memory: \
                 value-derived pseudonym tokens are keyed from the page key \
                 (open with --passphrase-env / open_encrypted first)",
                policy.mode, policy.scope
            )));
        }
        // Store the canonical re-serialization, so the row is byte-for-byte
        // what this build validated.
        let json = serde_json::to_string(&policy).map_err(db_err)?;
        self.meta_put(&format!("{}{ns}", anon_gate::ANON_PREFIX), &json)?;
        let current = env!("CARGO_PKG_VERSION");
        let needs_stamp = match self.meta_get(MIN_READER_VERSION_KEY)? {
            Some(existing) => version_lt(&existing, current),
            None => true,
        };
        if needs_stamp {
            self.meta_put(MIN_READER_VERSION_KEY, current)?;
            self.needs_min_reader_stamp = false;
        }
        self.reload_anon_policies()
    }

    /// Remove one namespace's anonymization policy. Missing is not an error.
    pub fn clear_anon_policy(&mut self, ns: &str) -> Result<()> {
        self.meta_delete(&format!("{}{ns}", anon_gate::ANON_PREFIX))?;
        self.reload_anon_policies()
    }

    /// All declared anonymization policies, re-read from the file. An
    /// unreadable row is a hard `VAL` error, not a skip — same fail-closed
    /// contract as `retention_policies()`.
    pub fn anon_policies(&self) -> Result<Vec<(String, areev_core::anon::AnonPolicy)>> {
        let mut out = Vec::new();
        for (ns, json) in self.meta_scan(anon_gate::ANON_PREFIX)? {
            let p = areev_core::anon::AnonPolicy::from_json(&json).map_err(|e| {
                AreevError::Validation(format!("anon:{ns} policy row is unreadable: {e}"))
            })?;
            out.push((ns, p));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn reload_anon_policies(&mut self) -> Result<()> {
        let rows = self.meta_scan(anon_gate::ANON_PREFIX)?;
        self.anon.reload(&rows);
        // Seed known-identity propagation from the file's existing subjects,
        // so prose-only grains are covered from the very first read (the
        // write path keeps the list current from here on). Bounded per ns.
        for (ns, _) in &rows {
            if let Some(ns_id) = self.term_lookup(ns)? {
                let ids: Vec<i64> = self
                    .db
                    .query(
                        "SELECT DISTINCT s FROM triples WHERE ns=?1 LIMIT 1024",
                        vec![pi(ns_id)],
                    )?
                    .iter()
                    .filter_map(|row| row.i64(0))
                    .collect();
                let mut subjects = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(t) = self.term_str(id)? {
                        subjects.push(t);
                    }
                }
                self.anon.seed_known(ns, subjects);
            }
        }
        Ok(())
    }

    /// Apply the namespace's ingress policy to free text (proposal §4.2):
    /// `Ok(None)` when no ingress policy covers `ns`. Public because the
    /// facade's structured write path and the memory-tool adapter run their
    /// transforms through this same gate.
    pub fn ingress_transform_text(&mut self, ns: &str, text: &str) -> Result<Option<String>> {
        self.anon.ingress_text(ns, text)
    }

    /// Ingress transform for a structural identity value (a subject is a
    /// `person` by construction). `Ok(None)` when no ingress policy covers.
    pub fn ingress_transform_identity(&mut self, ns: &str, value: &str) -> Result<Option<String>> {
        self.anon.ingress_identity(ns, value)
    }

    /// Whether an ingress policy covers `ns` — the cheap probe the facade's
    /// structured write path uses before cloning a fields map.
    pub fn ingress_active(&self, ns: &str) -> Result<bool> {
        self.anon.ingress_active(ns)
    }

    /// Host cap (per-process, never persisted): force egress anonymization
    /// on for namespaces with no declared policy, and upgrade a declared
    /// "off" to egress. It can never weaken a declared policy.
    pub fn set_anonymize_egress_floor(&mut self, on: bool) {
        self.anon.set_floor(on);
    }

    pub fn anonymize_egress_floor(&self) -> bool {
        self.anon.floor()
    }

    /// Effective anonymization mode for one namespace (`None` = untouched),
    /// considering both declared policy and the floor. Errors when the gate
    /// is poisoned by an unreadable row.
    pub fn anon_active_mode(&self, ns: &str) -> Result<Option<String>> {
        self.anon.active_mode(ns)
    }

    /// Declared policies as `(ns, mode)` pairs, from the handle's cache —
    /// cheap observability for config surfaces.
    pub fn anon_declared(&self) -> Vec<(String, String)> {
        self.anon.declared()
    }

    /// Live session mappings the in-process caller may rehydrate with:
    /// `(ns, mapping_id, placeholder → value)`. This is D5 custody — the
    /// mapping is available here, in process, and never rides MCP/server
    /// payloads.
    pub fn anon_mappings(&self) -> Result<Vec<AnonMappingRow>> {
        self.anon.mappings()
    }

    /// The `mapping_id` of the namespace's most recent egress state, for
    /// the `anonymized: true` payload flags surfaces attach.
    pub fn anon_last_mapping_id(&self, ns: &str) -> Result<Option<String>> {
        self.anon.last_mapping_id(ns)
    }

    /// `audit`-mode counters: `(ns, category, detections)`. In-memory,
    /// process-lifetime, never the spans' plaintext.
    pub fn anon_audit_counts(&self) -> Vec<(String, String, u64)> {
        self.anon
            .audit_counts()
            .iter()
            .map(|((ns, cat), n)| (ns.clone(), cat.clone(), *n))
            .collect()
    }

    /// Declare (or replace) the retention policy for one namespace.
    pub fn set_retention_policy(&self, ns: &str, policy: &RetentionPolicy) -> Result<()> {
        if ns.trim().is_empty() {
            return Err(AreevError::Validation(
                "retention policy needs a namespace".into(),
            ));
        }
        // 0 is legal and means "no minimum age" — the same semantics as
        // `areev purge-older-than 0`. Negative or non-finite is a caller bug:
        // a cutoff in the future would erase grains that are not yet due.
        if policy.days < 0.0 || !policy.days.is_finite() {
            return Err(AreevError::Validation(format!(
                "retention days must be a non-negative, finite number, got {}",
                policy.days
            )));
        }
        let json = serde_json::to_string(policy).map_err(db_err)?;
        self.meta_put(&format!("{RETENTION_PREFIX}{ns}"), &json)
    }

    /// Remove the retention policy for one namespace. Missing is not an error.
    pub fn clear_retention_policy(&self, ns: &str) -> Result<()> {
        self.meta_delete(&format!("{RETENTION_PREFIX}{ns}"))
    }

    /// Declare a retention FLOOR: age-based destruction in `ns` refuses any
    /// cutoff younger than `min_days`. A file-truth, like the ceiling
    /// policies — it travels with the memory, so the floor a regulated
    /// deployment declares survives copies and restores. `because` is
    /// mandatory: a floor with no recorded rationale is not auditable.
    pub fn set_retention_floor(&self, ns: &str, min_days: f64, because: &str) -> Result<()> {
        if ns.trim().is_empty() {
            return Err(AreevError::Validation("retention floor needs a namespace".into()));
        }
        if min_days <= 0.0 || !min_days.is_finite() {
            return Err(AreevError::Validation(format!(
                "retention floor min_days must be a positive, finite number, got {min_days}"
            )));
        }
        if because.trim().is_empty() {
            return Err(AreevError::Validation(
                "retention floor requires a non-empty because".into(),
            ));
        }
        let json = serde_json::json!({"min_days": min_days, "because": because}).to_string();
        self.meta_put(&format!("{RETENTION_FLOOR_PREFIX}{ns}"), &json)
    }

    pub fn clear_retention_floor(&self, ns: &str) -> Result<()> {
        self.meta_delete(&format!("{RETENTION_FLOOR_PREFIX}{ns}"))
    }

    /// Every declared floor, as `(namespace, min_days, because)`.
    pub fn retention_floors(&self) -> Result<Vec<(String, f64, String)>> {
        let mut out = Vec::new();
        for (ns, raw) in self.meta_scan(RETENTION_FLOOR_PREFIX)? {
            let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                AreevError::Validation(format!("unreadable retention floor for '{ns}': {e}"))
            })?;
            out.push((
                ns,
                v.get("min_days").and_then(|d| d.as_f64()).unwrap_or(0.0),
                v.get("because").and_then(|b| b.as_str()).unwrap_or("").to_string(),
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Place a legal hold on a namespace: all age-based destruction there
    /// refuses until the hold is released. `because` and `placed_by` are
    /// mandatory — a hold is a legal act with an owner.
    pub fn place_hold(&self, ns: &str, because: &str, placed_by: &str, at_ms: i64) -> Result<()> {
        if ns.trim().is_empty() {
            return Err(AreevError::Validation("hold needs a namespace".into()));
        }
        if because.trim().is_empty() || placed_by.trim().is_empty() {
            return Err(AreevError::Validation(
                "hold requires a non-empty because and placed_by".into(),
            ));
        }
        let json = serde_json::json!({
            "because": because, "placed_by": placed_by, "at_ms": at_ms
        })
        .to_string();
        self.meta_put(&format!("{HOLD_PREFIX}{ns}"), &json)
    }

    pub fn release_hold(&self, ns: &str) -> Result<()> {
        self.meta_delete(&format!("{HOLD_PREFIX}{ns}"))
    }

    /// Every live hold, as `(namespace, because, placed_by)`.
    pub fn holds(&self) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        for (ns, raw) in self.meta_scan(HOLD_PREFIX)? {
            let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                AreevError::Validation(format!("unreadable hold for '{ns}': {e}"))
            })?;
            out.push((
                ns,
                v.get("because").and_then(|b| b.as_str()).unwrap_or("").to_string(),
                v.get("placed_by").and_then(|b| b.as_str()).unwrap_or("").to_string(),
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// The floor/hold guard every age-based destruction passes through.
    /// `ns = None` (a global sweep) must clear EVERY namespace's floor and
    /// every hold — an operator who wants to sweep around a held namespace
    /// scopes the sweep instead.
    fn check_age_destruction_allowed(&self, ns: Option<&str>, cutoff_ms: i64) -> Result<()> {
        let holds = self.holds()?;
        match ns {
            Some(n) => {
                if let Some((_, because, placed_by)) = holds.iter().find(|(h, _, _)| h == n) {
                    return Err(AreevError::Validation(format!(
                        "namespace '{n}' is under a legal hold placed by {placed_by} \
                         ({because}) — age-based destruction refuses until the hold \
                         is released"
                    )));
                }
            }
            None => {
                if let Some((h, because, placed_by)) = holds.first() {
                    return Err(AreevError::Validation(format!(
                        "a global sweep is refused while any legal hold is live \
                         (namespace '{h}', placed by {placed_by}: {because}) — \
                         scope the sweep to unheld namespaces"
                    )));
                }
            }
        }
        let now = now_ms();
        for (fns, min_days, because) in self.retention_floors()? {
            let applies = ns.is_none_or(|n| n == fns);
            if !applies {
                continue;
            }
            let floor_cutoff = now - (min_days * 86_400_000.0) as i64;
            if cutoff_ms > floor_cutoff {
                return Err(AreevError::Validation(format!(
                    "cutoff is younger than the {min_days}-day retention floor on \
                     '{fns}' ({because}) — destruction refused"
                )));
            }
        }
        Ok(())
    }

    /// Enforce every declared retention policy. Returns one
    /// `(namespace, days, ErasureReport)` per policy applied, in namespace
    /// order.
    ///
    /// This is the cron-shaped path; the governed path is the loop's
    /// `retention_sweep` analyzer, which proposes the same purge through
    /// review + BECAUSE + audit instead of doing it directly. Both end in
    /// [`forget_older_than`](Self::forget_older_than), so the semantics —
    /// exclusive cutoff, namespace and type scoping, replicating tombstones —
    /// are identical.
    pub fn sweep_retention(&mut self, now_ms: i64) -> Result<Vec<RetentionSweep>> {
        let policies = self.retention_policies()?;
        let mut out = Vec::new();
        for (ns, policy) in policies {
            let cutoff = now_ms - (policy.days * 86_400_000.0) as i64;
            let gtype = match policy.grain_type.as_deref() {
                None => None,
                Some(t) => Some(areev_core::types::GrainType::from_str(t).ok_or_else(|| {
                    AreevError::Validation(format!(
                        "retention policy for '{ns}' names an unknown grain type: {t:?}"
                    ))
                })?),
            };
            // A held or floored namespace SKIPS with the refusal recorded —
            // the cron path must not die because one namespace is under
            // litigation, and it must not skip silently either.
            if let Err(e) = self.check_age_destruction_allowed(Some(&ns), cutoff) {
                out.push(RetentionSweep {
                    ns,
                    days: policy.days,
                    outcome: RetentionOutcome::Skipped { why: e.to_string() },
                });
                continue;
            }
            let report = self.forget_older_than(Some(&ns), cutoff, gtype)?;
            out.push(RetentionSweep {
                ns,
                days: policy.days,
                outcome: RetentionOutcome::Swept(report),
            });
        }
        Ok(out)
    }

    /// Suspend BM25 posting writes ahead of a bulk load.
    ///
    /// Postings cost is per-token, so a bulk load no longer *needs* this the
    /// way it did when the leg was Turso's FTS index — but writing postings
    /// once at the end still beats writing them per row, and importers already
    /// pair this with [`Self::rebuild_text_index`].
    ///
    /// The `text` column keeps populating throughout, so the rebuild has
    /// everything it needs. Crash-safe: deferral is process state, not file
    /// state, so a process that dies mid-load reopens with an incomplete index
    /// and the open-time check rebuilds it.
    ///
    /// Index-layer only — stored blobs are never touched. Returns `false`
    /// when there was nothing to defer (text indexing off, or already
    /// deferred).
    pub fn defer_text_index(&mut self) -> Result<bool> {
        if !self.index_text || self.fts_deferred {
            return Ok(false);
        }
        self.fts_deferred = true;
        Ok(true)
    }

    /// (Re)build the FTS index. Backfills the `text` column for rows written
    /// while text indexing was off — deriving the same `projected_text`
    /// the inline write path uses — then re-creates the index, which indexes
    /// every existing row. Pairs with [`Self::defer_text_index`] around bulk
    /// loads, and turns a file that flipped `--index-text true` after the
    /// fact into a fully searchable one. Index-layer only — stored blobs are
    /// never touched; forgotten grains are gone from `grains` and cannot be
    /// resurrected. Returns the number of rows whose text was backfilled.
    ///
    /// Errors when the file declares text indexing off — reopen with
    /// `index_text: true` (CLI `--index-text true`) first.
    pub fn rebuild_text_index(&mut self) -> Result<usize> {
        if !self.index_text {
            return Err(AreevError::Validation(
                "text indexing is off for this file — reopen it with text indexing on \
                 (open_with index_text, index_text=True from Python, indexText from \
                 Node, --index-text true on the CLI) before rebuilding the FTS index"
                    .to_string(),
            ));
        }
        // Warm pass, outside the transaction: intern every vocabulary token
        // the current corpus snapshot needs — including the tokens of text
        // that is only PROJECTED during the in-transaction backfill (a file
        // written with indexing off has NULL text for everything). Interning
        // is autocommit by convention, so orphan vocab rows on a later
        // failure are harmless; the transaction below re-reads its own
        // consistent corpus and only LOOKS UP ids, so nothing it does can
        // poison the caches on rollback.
        for row in self.db.query("SELECT text, blob FROM grains", vec![])? {
            let text = match row.text(0) {
                Some(t) => Some(t.to_string()),
                None => match row.blob(1) {
                    Some(b) => projected_text(&deserialize_blob(&b)?),
                    None => None,
                },
            };
            if let Some(t) = text {
                for (term, _) in token_freqs(&t).0 {
                    self.fts_term_id(&term)?;
                }
            }
        }
        self.fts_deferred = false;

        // One transaction for EVERYTHING — backfill, delete, corpus read,
        // posting writes — serialized against concurrent writers by the
        // zero-id reservation. Two transactions with autocommit deletes
        // between them let a concurrent add slip into the window and get its
        // postings doubled (its own txn's copy survives the delete, the
        // rebuild inserts them again).
        self.db.begin()?;
        let r = (|| -> Result<(usize, i64, i64)> {
            self.db.reserve_write(0, 0, 0, now_ms() << 16, 0)?;
            // Backfill NULL text from the immutable blobs (projection
            // identical to the write path's).
            let mut updates: Vec<(i64, String)> = Vec::new();
            for row in self.db.query("SELECT seq, blob FROM grains WHERE text IS NULL", vec![])? {
                let seq = row.i64(0).unwrap_or(0);
                if let Some(b) = row.blob(1) {
                    let view = deserialize_blob(&b)?;
                    if let Some(t) = projected_text(&view) {
                        updates.push((seq, t));
                    }
                }
            }
            for (seq, t) in &updates {
                self.db
                    .execute("UPDATE grains SET text = ?1 WHERE seq = ?2", vec![pt(t), pi(*seq)])?;
            }
            self.db.execute("DELETE FROM fts_post", vec![])?;
            self.db.execute("DELETE FROM fts_doc", vec![])?;
            let mut texts: Vec<(i64, i64, String)> = Vec::new();
            for row in self
                .db
                .query("SELECT seq, ns, text FROM grains WHERE text IS NOT NULL", vec![])?
            {
                let seq = row.i64(0).unwrap_or(0);
                let ns = row.i64(1).unwrap_or(0);
                if let Some(t) = row.text(2) {
                    texts.push((seq, ns, t.to_string()));
                }
            }
            let (mut docs, mut total_len) = (0i64, 0i64);
            for (seq, ns, text) in &texts {
                let (freqs, len) = token_freqs(text);
                if freqs.is_empty() {
                    continue;
                }
                let mut wrote = false;
                for (term, tf) in freqs {
                    // Lookup only: the warm pass interned the snapshot's
                    // tokens, and a grain committed since then had its own
                    // add intern its tokens. A miss is out-of-model; skip
                    // the token rather than intern inside the txn.
                    let Some(id) = self.fts_term_lookup(&term)? else {
                        continue;
                    };
                    self.db.execute_hot(
                        "INSERT INTO fts_post(term,seq,ns,tf) VALUES (?1,?2,?3,?4)",
                        vec![pi(id), pi(*seq), pi(*ns), pi(tf)],
                    )?;
                    wrote = true;
                }
                if wrote {
                    self.db.execute_hot(
                        "INSERT OR REPLACE INTO fts_doc(seq,len) VALUES (?1,?2)",
                        vec![pi(*seq), pi(len)],
                    )?;
                    docs += 1;
                    total_len += len;
                }
            }
            Ok((updates.len(), docs, total_len))
        })();
        match r {
            Ok((backfilled, docs, total_len)) => {
                self.db.commit()?;
                self.fts_docs = docs;
                self.fts_total_len = total_len;
                Ok(backfilled)
            }
            Err(e) => {
                let _ = self.db.rollback();
                Err(e)
            }
        }
    }

    /// Vocabulary id for `term` WITHOUT interning — cache, then the table.
    /// Rebuilds use this inside their transaction so a rollback can never
    /// leave the cache pointing at a vocab row that was never committed.
    fn fts_term_lookup(&mut self, term: &str) -> Result<Option<i64>> {
        if let Some(id) = self.fts_vocab.get(term) {
            return Ok(Some(*id));
        }
        let rows = self.db.query("SELECT id FROM fts_vocab WHERE term = ?1", vec![pt(term)])?;
        match rows.first().and_then(|r| r.i64(0)) {
            Some(id) => {
                self.fts_vocab.insert(term.to_string(), id);
                Ok(Some(id))
            }
            None => Ok(None),
        }
    }

    /// Dimension of the installed embedding backend, if any. `None` means
    /// the vector recall leg is off for this store.
    pub fn embedder_dim(&self) -> Option<usize> {
        self.embedder.as_ref().map(|e| e.dim())
    }

    /// Embedding provenance declared by the file (model, dim), if any
    /// vectors were ever written.
    pub fn declared_embedding(&self) -> Option<(&str, usize)> {
        self.meta_embed.as_ref().map(|(m, d)| (m.as_str(), *d))
    }

    /// Reconciliation warnings from open / set_embedder: file declarations
    /// vs what this session supplied. Empty when everything agrees.
    pub fn open_warnings(&self) -> &[String] {
        &self.warnings
    }

    fn next_hlc(&mut self) -> i64 {
        let wall = now_ms() << 16;
        self.hlc_last = if wall > self.hlc_last { wall } else { self.hlc_last + 1 };
        self.hlc_last
    }

    /// Dictionary-encode a term (cached; inserts on miss). Multi-writer
    /// backends own the id allocation (atomic get-or-create); the embedded
    /// path mints from the process-local counter under single-writer.
    fn term_id(&mut self, term: &str) -> Result<i64> {
        if let Some(id) = self.dict.get(term) {
            return Ok(*id);
        }
        if let Some(id) = self.db.intern_term(term)? {
            self.dict.insert(term.to_string(), id);
            return Ok(id);
        }
        let id = self.next_term;
        self.next_term += 1;
        self.db
            .execute("INSERT INTO terms(id, term) VALUES (?1, ?2)", vec![pi(id), pt(term)])?;
        self.dict.insert(term.to_string(), id);
        Ok(id)
    }

    /// Forward dictionary lookup. The process-local map answers hits; on a
    /// miss, multi-writer backends are consulted (another instance may have
    /// interned the term after this handle opened) and the answer cached.
    /// A miss stays "unknown term = empty result, never an error".
    fn term_lookup(&mut self, term: &str) -> Result<Option<i64>> {
        if let Some(id) = self.dict.get(term) {
            return Ok(Some(*id));
        }
        if let Some(id) = self.db.lookup_term(term)? {
            self.dict.insert(term.to_string(), id);
            return Ok(Some(id));
        }
        Ok(None)
    }

    fn term_str(&self, id: i64) -> Result<Option<String>> {
        if let Some(t) = self.dict.iter().find(|(_, v)| **v == id).map(|(k, _)| k.clone()) {
            return Ok(Some(t));
        }
        self.db.lookup_term_str(id)
    }

    // ----- write path -----

    /// Add one grain (full txn). Returns its content address.
    pub fn add<G: Grain + 'static>(&mut self, grain: &G) -> Result<Hash> {
        self.add_batch_inner(std::slice::from_ref(&(grain as &dyn AddableDyn)))
            .map(|mut v| v.remove(0))
    }

    /// Batched add — one txn for the whole slice (voice write-back path).
    pub fn add_batch(&mut self, grains: &[&dyn AddableDyn]) -> Result<Vec<Hash>> {
        self.add_batch_inner(grains)
    }

    /// Add a grain and index a **caller-supplied embedding** for it — the
    /// write half of the external-embedding seam ([`Self::nearest_vector`] is
    /// the read half). For hosts that own their embedding pipeline: the
    /// vector never enters the grain blob (embeddings are index-layer data,
    /// not content — putting hundreds of floats in the blob would change the
    /// content address and bloat every copy), so the grain's hash is
    /// identical with or without it.
    ///
    /// Dimension is checked against the file's declared provenance; the
    /// first external vector on an undeclared file stamps
    /// `embedding_model = "external"` so later mismatches are catchable.
    /// Honest boundary: bundles carry blobs, not index rows — a replica does
    /// **not** receive externally supplied vectors and must have them
    /// re-supplied by its own host.
    pub fn add_with_embedding<G: Grain + 'static>(
        &mut self,
        grain: &G,
        embedding: &[f32],
    ) -> Result<Hash> {
        self.check_embedding_dim(embedding.len())?;
        let hash = self.add(grain)?;
        self.set_grain_embedding(&hash, embedding)?;
        Ok(hash)
    }

    /// Install or replace the indexed embedding for an already-stored grain,
    /// by content address. The backfill primitive behind
    /// [`Self::add_with_embedding`]; also what an adapter uses to carry a
    /// framework's own vectors onto grains that were added earlier.
    pub fn set_grain_embedding(&mut self, hash: &Hash, embedding: &[f32]) -> Result<Hash> {
        self.check_embedding_dim(embedding.len())?;
        let rows = self.db.query(
            "SELECT seq FROM grains WHERE hash=?1",
            vec![pb(hash.as_bytes().to_vec())],
        )?;
        let Some(seq) = rows.first().and_then(|r| r.i64(0)) else {
            return Err(AreevError::NotFound(*hash));
        };
        // Stamp provenance on first external vector so later dimension
        // mismatches have something to check against (mirrors
        // `set_embedder`'s None arm).
        if self.meta_embed.is_none() {
            let dim = embedding.len();
            self.meta_put("embedding_model", "external")?;
            self.meta_put("embedding_dim", &dim.to_string())?;
            self.meta_embed = Some(("external".to_string(), dim));
        }
        let dbr = self.db.as_ref();
        let qjson = vec_to_json(embedding);
        with_txn(dbr, || {
            // DELETE + INSERT rather than an upsert: portable across the
            // embedded and Postgres backends without dialect-specific
            // ON CONFLICT / OR REPLACE translation.
            dbr.execute("DELETE FROM embeddings WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute(
                "INSERT INTO embeddings(seq, vec) VALUES (?1, vector32(?2))",
                vec![pi(seq), pt(&qjson)],
            )?;
            Ok(())
        })?;
        Ok(*hash)
    }

    /// Value-level idempotent add. When the grain carries a full
    /// `(subject, relation, object)` triple and the current head for
    /// `(ns, subject, relation)` already holds this exact object, nothing is
    /// written and the existing head's hash is returned with `false`.
    /// Otherwise it behaves like [`add`](Self::add) and returns `true`.
    ///
    /// This collapses a re-learned *value*, not merely a byte-identical
    /// replay: unlike content addressing it ignores `created_at` and the rest
    /// of the envelope, keying only on `(ns, subject, relation, object)`
    /// against the current provisional head. Grains without a full triple
    /// always insert. Paraphrased near-duplicates are a *different* object and
    /// out of scope here — those need a host-side (embedding) novelty check.
    pub fn add_if_novel<G: Grain + 'static>(&mut self, grain: &G) -> Result<(Hash, bool)> {
        self.add_dyn_if_novel(grain as &dyn AddableDyn)
    }

    fn add_dyn_if_novel(&mut self, grain: &dyn AddableDyn) -> Result<(Hash, bool)> {
        let (blob, _hash) = grain.serialize_dyn()?;
        let gv = extract_view(&deserialize_blob(&blob)?);
        if let (Some(sj), Some(rl), Some(ob)) =
            (gv.subject.as_deref(), gv.relation.as_deref(), gv.object.as_deref())
        {
            // All three terms must already exist for a prior head to match;
            // a never-seen object can't be a duplicate, so we skip the probe.
            if let (Some(ns_id), Some(s_id), Some(p_id), Some(o_id)) = (
                self.term_lookup(&gv.ns)?,
                self.term_lookup(sj)?,
                self.term_lookup(rl)?,
                self.term_lookup(ob)?,
            ) {
                if let Some(existing) = self.head_hash_for_object(ns_id, s_id, p_id, o_id)? {
                    return Ok((existing, false));
                }
            }
        }
        // The probe above ran OUTSIDE the write transaction; with concurrent
        // writers (postgres) another instance can land the same value between
        // probe and insert. Re-probe INSIDE the transaction, after the
        // serialization point (reserve_write's counter-row lock), where every
        // committed duplicate is visible and racing writers are blocked. On
        // the embedded single-writer backend this is one redundant point read.
        let (preps, first_seq, first_op, hlc0) =
            self.prep_and_reserve(std::slice::from_ref(&grain))?;
        if self.needs_min_reader_stamp
            && preps
                .iter()
                .any(|p| p.blob.get(2).is_some_and(|b| *b > LEGACY_MAX_GRAIN_BYTE))
        {
            self.meta_put(MIN_READER_VERSION_KEY, env!("CARGO_PKG_VERSION"))?;
            self.needs_min_reader_stamp = false;
        }
        let key = match (preps[0].s, preps[0].p, preps[0].o) {
            (Some(s), Some(p), Some(o)) => Some((preps[0].ns_id, s, p, o)),
            _ => None,
        };
        let hash = preps[0].hash;
        let (d_docs, d_len) = fts_delta(&preps);
        let dbr = self.db.as_ref();
        let duplicate = with_txn(dbr, || {
            // Serialize first WITHOUT consuming ids: a duplicate hit must
            // commit an empty transaction, not leave a gap in the op-log.
            dbr.reserve_write(0, 0, 0, now_ms() << 16, 0)?;
            if let Some((ns, s, p, o)) = key {
                let rows = dbr.query(
                    "SELECT hash FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND o=?4",
                    vec![pi(ns), pi(s), pi(p), pi(o)],
                )?;
                if let Some(b) = rows.first().and_then(|r| r.blob(0)) {
                    return Ok(Some(Hash::try_from_bytes(&b)?));
                }
            }
            let (s0, o0, h0) = match dbr.reserve_write(1, 1, 1, now_ms() << 16, 0)? {
                Some(w) => (w.seq0, w.op0, w.hlc0),
                None => (first_seq, first_op, hlc0),
            };
            insert_prepped(dbr, &preps, s0, o0, h0)?;
            Ok(None)
        })?;
        match duplicate {
            Some(existing) => Ok((existing, false)),
            None => {
                self.fts_docs += d_docs;
                self.fts_total_len += d_len;
                Ok((hash, true))
            }
        }
    }

    /// Hash of the current provisional head for `(ns, s, p)` iff its object is
    /// exactly `o` — the µs probe behind [`add_if_novel`](Self::add_if_novel).
    fn head_hash_for_object(&mut self, ns: i64, s: i64, p: i64, o: i64) -> Result<Option<Hash>> {
        let rows = self.db.query(
            "SELECT hash FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND o=?4",
            vec![pi(ns), pi(s), pi(p), pi(o)],
        )?;
        match rows.first().and_then(|row| row.blob(0)) {
            Some(b) => Ok(Some(Hash::try_from_bytes(&b)?)),
            None => Ok(None),
        }
    }

    /// Serialize-side preparation shared by `add_batch` and bundle import.
    fn prep_from_blob(&mut self, blob: Vec<u8>, hash: Hash) -> Result<GrainPrep> {
        let view = deserialize_blob(&blob)?;
        let gv = extract_view(&view);
        // Known-identity propagation (anon gate): a subject written now must
        // be detectable in prose the boundary transforms later, even if no
        // structural read has surfaced it yet. One map lookup when idle.
        if !self.anon.is_idle() {
            if let Some(subject) = &gv.subject {
                self.anon.note_written_identity(&gv.ns, subject);
            }
        }
        let ns_id = self.term_id(&gv.ns)?;
        let (mut s, mut p, mut o, mut osp) = (None, None, None, false);
        if let (Some(sj), Some(rl), Some(ob)) = (&gv.subject, &gv.relation, &gv.object) {
            s = Some(self.term_id(sj)?);
            p = Some(self.term_id(rl)?);
            o = Some(self.term_id(ob)?);
            // Harness links are content-address joins by definition: callers
            // need both run -> config and config -> runs even on files whose
            // older entity-relations declaration predates this reserved edge.
            osp = rl == "mg:harness" || self.entity_rels.contains(rl.as_str());
        }
        let session = match &gv.session {
            Some(x) => Some(self.term_id(x)?),
            None => None,
        };
        let projected = projected_text(&view);
        let text = if self.index_text { projected.clone() } else { None };
        // Resolving vocabulary ids needs `&mut self`, so it happens here in
        // prep rather than in the insert, which only has the connection.
        let (tokens, doc_len) = match (&text, self.fts_deferred) {
            (Some(t), false) => {
                let (freqs, len) = token_freqs(t);
                let mut ids = Vec::with_capacity(freqs.len());
                for (term, tf) in freqs {
                    ids.push((self.fts_term_id(&term)?, tf));
                }
                (ids, len)
            }
            _ => (Vec::new(), 0),
        };
        let embed_text = projected;
        let embedding = match (&self.embedder, &embed_text) {
            (Some(e), Some(t)) => Some(e.embed(t)?),
            _ => None,
        };
        // Cross-grain links are subject-ed on the grain's own hash, so a link
        // is queryable from either end without inventing a synthetic node.
        let mut links = Vec::with_capacity(gv.links.len());
        if !gv.links.is_empty() {
            let self_id = self.term_id(&hash.to_hex())?;
            for (rel, target) in &gv.links {
                links.push((self_id, self.term_id(rel)?, self.term_id(target)?));
            }
        }
        let run = match &gv.run {
            Some(r) => Some(self.term_id(r)?),
            None => None,
        };
        // A malformed parent address is dropped rather than failing the write:
        // provenance is an index, and an unindexable link must not cost the
        // grain itself.
        let parent = gv
            .parent
            .as_deref()
            .and_then(|p| Hash::from_hex(p).ok())
            .map(|h| h.as_bytes().to_vec());
        Ok(GrainPrep {
            blob,
            hash,
            ns_id,
            s,
            p,
            o,
            osp,
            session,
            vf: gv.vf,
            vt: gv.vt,
            created: gv.created_at,
            gtype: gv.gtype as i64,
            text,
            embedding,
            tokens,
            doc_len,
            links,
            run,
            parent,
            corpus_export: gv.corpus_export,
        })
    }

    /// Resolve a token to its `fts_vocab` id, assigning one if new.
    ///
    /// Unlike the triple dictionary this is not preloaded at open, so a miss
    /// costs a round trip. The cache makes that a once-per-token-per-process
    /// cost, and real text reuses tokens heavily.
    fn fts_term_id(&mut self, term: &str) -> Result<i64> {
        if let Some(id) = self.fts_vocab.get(term) {
            return Ok(*id);
        }
        self.db
            .execute("INSERT OR IGNORE INTO fts_vocab(term) VALUES (?1)", vec![pt(term)])?;
        let rows = self
            .db
            .query("SELECT id FROM fts_vocab WHERE term = ?1", vec![pt(term)])?;
        let id = match rows.first().and_then(|row| row.i64(0)) {
            Some(id) => id,
            None => return Err(AreevError::Storage("fts_vocab insert vanished".into())),
        };
        self.fts_vocab.insert(term.to_string(), id);
        Ok(id)
    }

    fn add_batch_inner(&mut self, grains: &[&dyn AddableDyn]) -> Result<Vec<Hash>> {
        // Adding a grain that is already stored is a no-op, not an error.
        //
        // A content address IS the content: two byte-identical grains are one
        // grain, and it is already here. Before this, the second insert hit
        // `UNIQUE constraint failed: grains.hash` — reachable whenever two
        // identical events land in the same millisecond, since `created_at` is
        // the only thing distinguishing them and it has ms resolution. An agent
        // retrying a failing tool in a tight loop is exactly that workload, and
        // it is the flagship analyzer's ingest path. Callers had to jitter their
        // payloads to avoid a storage error, which corrupts the data to satisfy
        // the store.
        //
        // The probe runs before the counters are reserved, so a skipped grain
        // consumes no seq/op/hlc and writes no op-log row — nothing changed, so
        // nothing replicates.
        let mut prepped: Vec<Option<GrainPrep>> = Vec::with_capacity(grains.len());
        let mut hashes: Vec<Hash> = Vec::with_capacity(grains.len());
        let mut seen_in_batch: HashSet<Hash> = HashSet::new();
        for g in grains {
            let (blob, hash) = g.serialize_dyn()?;
            hashes.push(hash);
            // `seen_in_batch` also covers duplicates *within* one batch, which
            // the `has` probe cannot see — neither is committed yet.
            if !seen_in_batch.insert(hash) || self.has(&hash)? {
                prepped.push(None);
                continue;
            }
            prepped.push(Some(self.prep_from_blob(blob, hash)?));
        }
        let preps: Vec<GrainPrep> = prepped.into_iter().flatten().collect();
        if preps.is_empty() {
            return Ok(hashes);
        }
        let (preps, first_seq, first_op, hlc0) = self.reserve_for(preps);
        // A grain type newer than any pre-1.5 build could decode makes this file
        // unreadable to those builds — `deserialize_blob` errors on an unknown
        // type byte rather than skipping it. Record that requirement in the file
        // so it is a statement the memory makes about itself (byte 2 of the .mg
        // header is the type byte).
        if self.needs_min_reader_stamp
            && preps
                .iter()
                .any(|p| p.blob.get(2).is_some_and(|b| *b > LEGACY_MAX_GRAIN_BYTE))
        {
            self.meta_put(MIN_READER_VERSION_KEY, env!("CARGO_PKG_VERSION"))?;
            self.needs_min_reader_stamp = false;
        }
        let (d_docs, d_len) = fts_delta(&preps);
        let n = preps.len() as i64;
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            // Multi-writer backends allocate the id block INSIDE the txn
            // (which also serializes concurrent write txns); the embedded
            // path keeps the process-local reservation made above.
            let (s0, o0, h0) = match dbr.reserve_write(n, n, n, now_ms() << 16, 0)? {
                Some(w) => (w.seq0, w.op0, w.hlc0),
                None => (first_seq, first_op, hlc0),
            };
            insert_prepped(dbr, &preps, s0, o0, h0)
        })?;
        self.fts_docs += d_docs;
        self.fts_total_len += d_len;
        Ok(hashes)
    }

    /// Serialize + dictionary-encode `grains` and reserve their seq/op/hlc
    /// counters — the pre-transaction half of an add, performing NO writes. Kept
    /// separate from [`insert_prepped`] so `supersede`/`merge_heads` can run the
    /// insert body inside their OWN atomic transaction, alongside the index
    /// flip, instead of committing the add first and flipping in a second txn.
    fn prep_and_reserve(
        &mut self,
        grains: &[&dyn AddableDyn],
    ) -> Result<(Vec<GrainPrep>, i64, i64, i64)> {
        // Serialize + extract + dictionary-encode before entering the txn.
        let mut preps = Vec::with_capacity(grains.len());
        for g in grains {
            let (blob, hash) = g.serialize_dyn()?;
            preps.push(self.prep_from_blob(blob, hash)?);
        }
        Ok(self.reserve_for(preps))
    }

    /// Reserve the seq/op/hlc block for already-prepped grains.
    ///
    /// Split out of [`prep_and_reserve`] so the add path can drop grains that
    /// are already stored *before* consuming counters — a skipped duplicate
    /// must not burn a sequence number.
    fn reserve_for(&mut self, preps: Vec<GrainPrep>) -> (Vec<GrainPrep>, i64, i64, i64) {
        let first_seq = self.next_seq;
        self.next_seq += preps.len() as i64;
        let first_op = self.next_op;
        self.next_op += preps.len() as i64;
        let hlc0 = self.next_hlc();
        self.hlc_last = hlc0 + preps.len() as i64 - 1;
        (preps, first_seq, first_op, hlc0)
    }

    // ----- read path -----

    /// Backfill the secondary indexes that were added after this file may have
    /// been written: reverse provenance (`prov_idx`), run correlation
    /// (`run_idx`), governed corpus manifests (`corpus_idx`), and the
    /// `related_to` cross-link triples.
    ///
    /// Returns the number of index rows written. Idempotent — the tables are
    /// cleared first, so running it twice is not double-counting. Reads every
    /// blob once, so it is a maintenance operation, not a hot path.
    pub fn rebuild_link_indexes(&mut self) -> Result<usize> {
        // Warm pass, outside the transaction: intern every dictionary term
        // the current snapshot's links/runs need (interning is autocommit by
        // convention). The transaction below re-reads its own consistent
        // corpus and only LOOKS UP ids, so a rollback can never leave the
        // dictionary cache pointing at uncommitted rows.
        for row in self.db.query("SELECT blob FROM grains ORDER BY seq", vec![])? {
            let Some(blob) = row.blob(0) else { continue };
            let view = deserialize_blob(&blob)?;
            let gv = extract_view(&view);
            if let Some(r) = &gv.run {
                self.term_id(r)?;
            }
            if !gv.links.is_empty() {
                self.term_id(&view.hash.to_hex())?;
                for (rel, target) in &gv.links {
                    self.term_id(rel)?;
                    self.term_id(target)?;
                }
            }
        }

        struct Row {
            seq: i64,
            ns: i64,
            run: Option<i64>,
            parent: Option<Vec<u8>>,
            corpus_export: bool,
            links: Vec<(i64, i64, i64)>,
        }
        // One transaction for delete + corpus read + plan + writes,
        // serialized against concurrent writers by the zero-id reservation —
        // autocommit deletes before the read let a concurrent add slip into
        // the window and lose (or double) its index rows.
        self.db.begin()?;
        let r = (|| -> Result<usize> {
            self.db.reserve_write(0, 0, 0, now_ms() << 16, 0)?;
            self.db.execute("DELETE FROM prov_idx", vec![])?;
            self.db.execute("DELETE FROM run_idx", vec![])?;
            self.db.execute("DELETE FROM corpus_idx", vec![])?;
            let mut rows: Vec<(i64, i64, Vec<u8>)> = Vec::new();
            for row in self
                .db
                .query("SELECT seq, ns, blob FROM grains ORDER BY seq", vec![])?
            {
                let (Some(seq), Some(ns), Some(blob)) = (row.i64(0), row.i64(1), row.blob(2))
                else {
                    continue;
                };
                rows.push((seq, ns, blob));
            }
            let mut plan: Vec<Row> = Vec::with_capacity(rows.len());
            for (seq, ns, blob) in &rows {
                let view = deserialize_blob(blob)?;
                let gv = extract_view(&view);
                // Lookup only: the warm pass interned this snapshot's terms,
                // and a grain committed since then had its own add intern
                // its terms. A miss is out-of-model; skip rather than
                // intern inside the txn.
                let run = match &gv.run {
                    Some(r) => self.term_lookup(r)?,
                    None => None,
                };
                let parent = gv
                    .parent
                    .as_deref()
                    .and_then(|p| Hash::from_hex(p).ok())
                    .map(|h| h.as_bytes().to_vec());
                let mut links = Vec::with_capacity(gv.links.len());
                if !gv.links.is_empty() {
                    if let Some(self_id) = self.term_lookup(&view.hash.to_hex())? {
                        for (rel, target) in &gv.links {
                            if let (Some(rl), Some(tg)) =
                                (self.term_lookup(rel)?, self.term_lookup(target)?)
                            {
                                links.push((self_id, rl, tg));
                            }
                        }
                    }
                }
                plan.push(Row {
                    seq: *seq,
                    ns: *ns,
                    run,
                    parent,
                    corpus_export: gv.corpus_export,
                    links,
                });
            }

            let mut n = 0usize;
            for r in &plan {
                if let Some(run) = r.run {
                    self.db.execute(
                        "INSERT INTO run_idx(ns,run,seq) VALUES (?1,?2,?3)",
                        vec![pi(r.ns), pi(run), pi(r.seq)],
                    )?;
                    n += 1;
                }
                if let Some(ref p) = r.parent {
                    self.db.execute(
                        "INSERT INTO prov_idx(ns,parent,seq) VALUES (?1,?2,?3)",
                        vec![pi(r.ns), pb(p.clone()), pi(r.seq)],
                    )?;
                    n += 1;
                }
                if r.corpus_export {
                    self.db.execute(
                        "INSERT INTO corpus_idx(seq) VALUES (?1)",
                        vec![pi(r.seq)],
                    )?;
                    n += 1;
                }
                for (ls, lp, lo) in &r.links {
                    // Replayed rather than appended: a re-run must not stack
                    // duplicate edges onto the traversal index.
                    self.db.execute(
                        "DELETE FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND o=?4 AND seq=?5",
                        vec![pi(r.ns), pi(*ls), pi(*lp), pi(*lo), pi(r.seq)],
                    )?;
                    self.db.execute(
                        "DELETE FROM osp WHERE ns=?1 AND o=?2 AND s=?3 AND p=?4 AND seq=?5",
                        vec![pi(r.ns), pi(*lo), pi(*ls), pi(*lp), pi(r.seq)],
                    )?;
                    self.db.execute(
                        "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                        vec![pi(r.ns), pi(*ls), pi(*lp), pi(*lo), pi(r.seq)],
                    )?;
                    self.db.execute(
                        "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                        vec![pi(r.ns), pi(*lo), pi(*ls), pi(*lp), pi(r.seq)],
                    )?;
                    n += 1;
                }
            }
            Ok(n)
        })();
        let written = match r {
            Ok(n) => {
                self.db.commit()?;
                n
            }
            Err(e) => {
                let _ = self.db.rollback();
                return Err(e);
            }
        };
        // Declare the file current, so open() stops re-scanning it.
        self.meta_put(LINK_INDEX_KEY, LINK_INDEX_VERSION)?;
        Ok(written)
    }

    /// Reverse provenance: every grain whose `derived_from` is exactly
    /// `parent`, newest first. This is the credit-assignment / episode-unlearn
    /// query — "which lessons were distilled from this observation?" or "what
    /// did the agent learn from this bad session?". Superseded versions are
    /// included so the full derived lineage is visible; the caller can revise
    /// or `forget` each hash.
    ///
    /// Served by `prov_idx`. It used to read and deserialize **every grain in
    /// the store** on each call, which made a provenance question cost the
    /// whole corpus. Files written before that index existed need `reindex`.
    pub fn grains_derived_from(&mut self, parent: &Hash) -> Result<Vec<DeserializedGrain>> {
        let key = parent.as_bytes().to_vec();
        let mut out: Vec<DeserializedGrain> = self
            .db
            .query(
                "SELECT g.blob FROM prov_idx p JOIN grains g ON g.seq = p.seq
                 WHERE p.parent = ?1 ORDER BY p.seq DESC",
                vec![pb(key)],
            )?
            .iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect::<Result<_>>()?;
        // Egress boundary; ns per grain (no ns parameter here). run_yield
        // inherits coverage through this and run_trace.
        self.anon.egress_grains(None, &mut out)?;
        Ok(out)
    }

    /// Every grain recorded during `run_id`, newest first — the run's own
    /// transcript.
    ///
    /// `run_id` is the only run-scoped correlation key in the grain model
    /// (OMS §8.2, Event). Pair with [`Self::run_yield`] for what the run
    /// *produced* downstream.
    pub fn run_trace(&mut self, ns: &str, run_id: &str, limit: usize) -> Result<Vec<DeserializedGrain>> {
        let (Some(ns_id), Some(run)) = (self.term_lookup(ns)?, self.term_lookup(run_id)?) else {
            return Ok(Vec::new());
        };
        let limit = limit.min(1024) as i64;
        let mut out: Vec<DeserializedGrain> = self
            .db
            .query(
                "SELECT g.blob FROM run_idx r JOIN grains g ON g.seq = r.seq
                 WHERE r.ns = ?1 AND r.run = ?2 ORDER BY r.seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(run), pi(limit)],
            )?
            .iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect::<Result<_>>()?;
        self.anon.egress_grains(Some(ns), &mut out)?;
        Ok(out)
    }

    /// One page of a run's grains, **oldest first**, from an exclusive
    /// `after_seq` cursor. Returns `(seq, grain)` pairs; pass the last `seq`
    /// back as the next `after_seq`, and a page shorter than `limit` means the
    /// run is exhausted.
    ///
    /// [`Self::run_trace`] clamps at 1024 with no cursor — fine for a
    /// transcript peek, a correctness hole for a runtime resuming a long run:
    /// past ~340 supersteps (intent + result + checkpoint per step) the
    /// journal would silently truncate and a resume would re-execute effects
    /// it can no longer see. This is the complete read. Ascending order is
    /// deliberate: a resume scans oldest→newest; canonical *presentation*
    /// order is the runtime's job (proposal §5.2), not the store's.
    ///
    /// Deliberately EXEMPT from the egress boundary: this is the machine
    /// replay read — the runtime reconstructs run state from these exact
    /// field values, and a pseudonymized journal would corrupt every resume.
    /// The model-facing views of the same data (`run_trace`, `run_yield`)
    /// are covered.
    pub fn run_grains(
        &mut self,
        ns: &str,
        run_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<(i64, DeserializedGrain)>> {
        let (Some(ns_id), Some(run)) = (self.term_lookup(ns)?, self.term_lookup(run_id)?) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 1024) as i64;
        self.db
            .query(
                "SELECT r.seq, g.blob FROM run_idx r JOIN grains g ON g.seq = r.seq
                 WHERE r.ns = ?1 AND r.run = ?2 AND r.seq > ?3
                 ORDER BY r.seq ASC LIMIT ?4",
                vec![pi(ns_id), pi(run), pi(after_seq), pi(limit)],
            )?
            .iter()
            .filter_map(|row| Some((row.i64(0)?, row.blob(1)?)))
            .map(|(seq, b)| Ok((seq, deserialize_blob(&b)?)))
            .collect()
    }

    /// What a run produced downstream: the grains derived from the run's own
    /// grains — extracted facts, distilled lessons — that are not themselves
    /// part of the run.
    ///
    /// This is the join the two indexes exist for: it crosses from execution
    /// history into semantic memory in one call. A transcript answers "what
    /// happened"; this answers "what did we keep".
    pub fn run_yield(&mut self, ns: &str, run_id: &str, limit: usize) -> Result<Vec<DeserializedGrain>> {
        let trace = self.run_trace(ns, run_id, limit)?;
        let in_run: HashSet<Hash> = trace.iter().map(|g| g.hash).collect();
        let mut seen: HashSet<Hash> = in_run.clone();
        let mut out = Vec::new();
        for g in &trace {
            for child in self.grains_derived_from(&g.hash)? {
                if !in_run.contains(&child.hash) && seen.insert(child.hash) {
                    out.push(child);
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Which runs touched this grain — the reverse join, from a piece of
    /// semantic memory back into execution history.
    ///
    /// Walks the provenance chain in both directions from `hash` (ancestors via
    /// `derived_from`, descendants via the reverse index) and collects the
    /// `run_id` of everything reachable, including the grain itself. Bounded by
    /// `depth` hops, because a long-lived memory's lineage is unbounded.
    ///
    /// Note this records runs that *produced or refined* the grain, not runs
    /// that merely read it — a read leaves no grain, so nothing in an
    /// append-only store can attest to it.
    pub fn runs_touching(&mut self, ns: &str, hash: &Hash, depth: usize) -> Result<Vec<String>> {
        let depth = depth.min(8);
        let mut runs: Vec<String> = Vec::new();
        let mut seen_run: HashSet<String> = HashSet::new();
        let mut visited: HashSet<Hash> = HashSet::new();
        let mut frontier = vec![*hash];
        visited.insert(*hash);

        for _ in 0..=depth {
            let mut next: Vec<Hash> = Vec::new();
            for h in std::mem::take(&mut frontier) {
                let Ok(g) = self.get(&h) else { continue };
                if let Some(r) = g.get_str("run_id") {
                    if seen_run.insert(r.to_string()) {
                        runs.push(r.to_string());
                    }
                }
                // Up: the grain this one was derived from. Namespace-filtered
                // like the downward leg — a lineage walk that stays in `ns` in
                // one direction and leaves it in the other reports runs the
                // caller did not ask about.
                if let Some(p) = g.get_str("derived_from").and_then(|p| Hash::from_hex(p).ok()) {
                    if !visited.contains(&p)
                        && self
                            .get(&p)
                            .is_ok_and(|pg| pg.get_str("namespace") == Some(ns))
                    {
                        visited.insert(p);
                        next.push(p);
                    }
                }
                // Down: grains derived from this one.
                for child in self.grains_derived_from(&h)? {
                    if child.get_str("namespace") == Some(ns) && visited.insert(child.hash) {
                        next.push(child.hash);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(runs)
    }

    /// Recent grains in a namespace, newest first, bounded by `limit`. With
    /// `gtype = None`, every type is returned. This is the "reflect over recent
    /// experience" read path — recent Events / Observations that have no
    /// subject or free-text anchor to hang a structural or BM25 leg on.
    pub fn recent(
        &mut self,
        ns: &str,
        gtype: Option<areev_core::types::GrainType>,
        limit: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        self.recent_inner(ns, gtype, limit, false)
    }

    /// `recent`, restricted to grains nothing has superseded.
    ///
    /// Supersession is index-layer state — the blob is immutable and carries no
    /// marker — so a caller reading grains back cannot tell a stale version from
    /// the head that replaced it. Recall needs that distinction; a scan that
    /// feeds an analyzer generally does not, which is why `recent` keeps its
    /// everything-in-order behaviour and this is a separate entry point.
    pub fn recent_live(
        &mut self,
        ns: &str,
        gtype: Option<areev_core::types::GrainType>,
        limit: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        self.recent_inner(ns, gtype, limit, true)
    }

    fn recent_inner(
        &mut self,
        ns: &str,
        gtype: Option<areev_core::types::GrainType>,
        limit: usize,
        live_only: bool,
    ) -> Result<Vec<DeserializedGrain>> {
        let ns_id = match self.term_lookup(ns)? {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let live = if live_only {
            " AND superseded_by IS NULL"
        } else {
            ""
        };
        // The `gtype` column stores the enum ordinal (see `extract_view`:
        // `view.grain_type as u8`), not the .mg header type-byte.
        let gt_ord = gtype.map(|g| g as u8 as i64);
        let rows = match gt_ord {
            Some(gt) => self.db.query(
                &format!("SELECT blob FROM grains WHERE ns=?1 AND gtype=?2{live} ORDER BY seq DESC LIMIT ?3"),
                vec![pi(ns_id), pi(gt), pi(limit as i64)],
            )?,
            None => self.db.query(
                &format!("SELECT blob FROM grains WHERE ns=?1{live} ORDER BY seq DESC LIMIT ?2"),
                vec![pi(ns_id), pi(limit as i64)],
            )?,
        };
        let mut out: Vec<DeserializedGrain> = rows
            .iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect::<Result<_>>()?;
        self.anon.egress_grains(Some(ns), &mut out)?;
        Ok(out)
    }

    /// Fetch a grain by content address.
    pub fn get(&mut self, hash: &Hash) -> Result<DeserializedGrain> {
        let rows = self.db.query(
            "SELECT blob FROM grains WHERE hash = ?1",
            vec![pb(hash.as_bytes().to_vec())],
        )?;
        let blob = match rows.first() {
            Some(row) => row
                .blob(0)
                .ok_or_else(|| AreevError::Storage("blob column not a blob".into()))?,
            None => return Err(AreevError::NotFound(*hash)),
        };
        let mut g = deserialize_blob(&blob)?;
        // Egress boundary: ns resolved from the grain itself (`get` has no
        // ns parameter). `latest` and the memory-tool `view` inherit this.
        self.egress_one(None, &mut g)?;
        Ok(g)
    }

    /// Apply the egress boundary to one grain (docs/anonymization-proposal.md
    /// P1). One `is_idle` branch when the feature is unused.
    #[inline]
    fn egress_one(&mut self, ns_hint: Option<&str>, g: &mut DeserializedGrain) -> Result<()> {
        if self.anon.is_idle() {
            return Ok(());
        }
        self.anon.egress_grains(ns_hint, std::slice::from_mut(g))
    }

    /// Structural recall: current grains about `subject` (optionally filtered
    /// by relation), newest first, k-bounded. The voice hot path.
    pub fn recall(
        &mut self,
        ns: &str,
        subject: &str,
        relation: Option<&str>,
        k: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        let mut out = self.recall_raw(ns, subject, relation, k)?;
        self.anon.egress_grains(Some(ns), &mut out)?;
        Ok(out)
    }

    /// `recall` without the egress boundary — for internal machinery whose
    /// correctness depends on raw field values (the authz engine reads
    /// `mg:permits` grants through here; a pseudonymized principal would
    /// fail every grant check). Never expose this on a public surface.
    fn recall_raw(
        &mut self,
        ns: &str,
        subject: &str,
        relation: Option<&str>,
        k: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        let start = std::time::Instant::now();
        let (ns_id, s_id) = match (self.term_lookup(ns)?, self.term_lookup(subject)?) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let p_id = match relation {
            Some(r) => match self.term_lookup(r)? {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        // Probe + blob fetch in ONE statement: the join replaces the old
        // per-seq fetch loop (1 + k round trips), which matters on any
        // backend where a statement is not a function call.
        let rows = match p_id {
            Some(p) => self.db.query_hot(
                "SELECT g.blob FROM triples t JOIN grains g ON g.seq = t.seq
                  WHERE t.ns=?1 AND t.s=?2 AND t.p=?3 AND t.cur=1
                  ORDER BY t.seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(s_id), pi(p), pi(k as i64)],
            )?,
            None => self.db.query_hot(
                "SELECT g.blob FROM triples t JOIN grains g ON g.seq = t.seq
                  WHERE t.ns=?1 AND t.s=?2 AND t.cur=1
                  ORDER BY t.seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(s_id), pi(k as i64)],
            )?,
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(b) = row.blob(0) {
                out.push(deserialize_blob(&b)?);
            }
        }
        // Structural recall feeds telemetry too, so `cold_grains` doesn't
        // false-positive on grains that are recalled by subject (not query).
        self.record_recall_event(ns, Some(subject), relation, None, &out, start);
        Ok(out)
    }

    /// Current value head for (subject, relation) — the µs point read.
    pub fn latest(&mut self, ns: &str, subject: &str, relation: &str) -> Result<Option<DeserializedGrain>> {
        let (ns_id, s_id, p_id) = match (
            self.term_lookup(ns)?,
            self.term_lookup(subject)?,
            self.term_lookup(relation)?,
        ) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Ok(None),
        };
        let hash = self
            .db
            .query_hot(
                "SELECT hash FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3",
                vec![pi(ns_id), pi(s_id), pi(p_id)],
            )?
            .first()
            .and_then(|row| row.blob(0));
        match hash {
            Some(h) => {
                let h = Hash::try_from_bytes(&h)?;
                Ok(Some(self.get(&h)?))
            }
            None => Ok(None),
        }
    }

    /// Last `n` events of a session, oldest→newest (transcript tail).
    pub fn thread_tail(&mut self, ns: &str, session: &str, n: usize) -> Result<Vec<DeserializedGrain>> {
        let (ns_id, sess_id) = match (self.term_lookup(ns)?, self.term_lookup(session)?) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let rows = self.db.query(
            "SELECT g.blob FROM thread_idx ti JOIN grains g ON g.seq = ti.seq
              WHERE ti.ns=?1 AND ti.session=?2 ORDER BY ti.seq DESC LIMIT ?3",
            vec![pi(ns_id), pi(sess_id), pi(n as i64)],
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows.iter().rev() {
            if let Some(b) = row.blob(0) {
                out.push(deserialize_blob(&b)?);
            }
        }
        self.anon.egress_grains(Some(ns), &mut out)?;
        Ok(out)
    }

    // ----- evolution path -----

    /// Supersede `old` with `new_grain` (atomic, OMS L2 semantics).
    /// Sets `derived_from` on the new grain; the old grain's blob is never
    /// touched — only its index-layer fields change.
    pub fn supersede<G: Grain + 'static>(&mut self, old: &Hash, new_grain: &mut G) -> Result<Hash> {
        // Old head must exist and be current.
        let rows = self.db.query(
            "SELECT seq, ns, s, p, svt FROM grains WHERE hash = ?1",
            vec![pb(old.as_bytes().to_vec())],
        )?;
        let (old_seq, _ns, old_s, old_p, old_svt) = match rows.first() {
            Some(row) => (
                row.i64(0).unwrap_or(0),
                row.i64(1),
                row.i64(2),
                row.i64(3),
                row.i64(4),
            ),
            None => return Err(AreevError::NotFound(*old)),
        };
        if old_svt.is_some() {
            return Err(AreevError::SupersessionConflict(*old));
        }

        new_grain.common_mut().derived_from = Some(old.to_hex());
        // Prep the new grain and reserve its counters WITHOUT writing, so the
        // insert body runs inside the SAME transaction as the flip below — the
        // add and the supersession commit atomically. (Previously add() committed
        // in its own txn, then a second txn flipped the old grain; a crash or
        // error between them left the new grain current while the old stayed
        // un-superseded, so recall surfaced both values — the OMS-L2 "atomic"
        // guarantee this method documents.)
        let new_dyn: &dyn AddableDyn = &*new_grain;
        let (preps, first_seq, first_op, hlc0) = self.prep_and_reserve(&[new_dyn])?;
        let new_hash = preps[0].hash;
        let now = now_ms();

        let ram_op = self.next_op;
        self.next_op += 1;
        let ram_hlc = self.next_hlc();
        let (d_docs, d_len) = fts_delta(&preps);
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            // Reserve ADD + SUPERSEDE ids in one block on multi-writer
            // backends (also the serialization point for concurrent write
            // txns); the embedded path keeps the process-local reservation.
            let (s0, o0, h0, op_seq, hlc) = match dbr.reserve_write(1, 2, 2, now_ms() << 16, 0)? {
                Some(w) => (w.seq0, w.op0, w.hlc0, w.op0 + 1, w.hlc0 + 1),
                None => (first_seq, first_op, hlc0, ram_op, ram_hlc),
            };
            // Re-resolve the head BY HASH under the row lock: with
            // concurrent writers the pre-transaction read can be stale in
            // two ways — the head may have been superseded (two racing
            // supersedes must produce one winner and one clean
            // SupersessionConflict), and a forget + re-add of identical
            // content can move this hash to a NEW seq (flipping the stale
            // seq would silently miss the live row).
            let recheck = dbr.query(
                &format!("SELECT seq, svt FROM grains WHERE hash=?1{}", dbr.for_update()),
                vec![pb(old.as_bytes().to_vec())],
            )?;
            let old_seq = match recheck.first() {
                None => return Err(AreevError::NotFound(*old)),
                Some(row) if row.i64(1).is_some() => {
                    return Err(AreevError::SupersessionConflict(*old))
                }
                Some(row) => row.i64(0).unwrap_or(old_seq),
            };
            insert_prepped(dbr, &preps, s0, o0, h0)?;
            dbr.execute(
                "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                vec![pb(new_hash.as_bytes().to_vec()), pi(now), pi(old_seq)],
            )?;
            dbr.execute(
                "UPDATE grains SET supersedes=?1 WHERE hash=?2",
                vec![pb(old.as_bytes().to_vec()), pb(new_hash.as_bytes().to_vec())],
            )?;
            dbr.execute("UPDATE triples SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            dbr.execute("UPDATE osp SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            // Reconcile the OLD key's head/entity_latest indexes. Normally
            // add(new) handles this because new shares (ns,s,p) with old; but
            // when the new grain carries a DIFFERENT (subject,relation) — or
            // no triple — add reconciles the new key and leaves the old key
            // pointing at the now-superseded grain. Mirror forget so
            // latest()/heads() for the old key don't surface it. Harmless
            // no-op in the common same-key case (old is already out of heads).
            if let (Some(ns), Some(s), Some(p)) = (_ns, old_s, old_p) {
                dbr.execute(
                    "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(old_seq)],
                )?;
                dbr.execute(
                    "DELETE FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(old_seq)],
                )?;
                // Key the join on (ns,s,p), not seq alone: a link-bearing
                // grain has extra triples rows for the same seq, and an
                // unkeyed join lets the engine pick a link target as t.o.
                let rows = dbr.query(
                    "SELECT t.o, h.seq, h.hash, h.created_at
                     FROM heads h JOIN triples t
                       ON t.seq=h.seq AND t.ns=h.ns AND t.s=h.s AND t.p=h.p
                     WHERE h.ns=?1 AND h.s=?2 AND h.p=?3
                     ORDER BY h.created_at DESC, h.hash DESC LIMIT 1",
                    vec![pi(ns), pi(s), pi(p)],
                )?;
                if let Some(row) = rows.first() {
                    let o = row.i64(0).unwrap_or(0);
                    let sq = row.i64(1).unwrap_or(0);
                    let h = row.blob(2).unwrap_or_default();
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(ns), pi(s), pi(p), pi(o), pi(sq), pb(h)],
                    )?;
                }
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc), pi(OP_SUPERSEDE), pb(new_hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })?;
        self.fts_docs += d_docs;
        self.fts_total_len += d_len;
        Ok(new_hash)
    }

    /// How many grant grains one principal may carry — a sanity bound, far
    /// above any real ACL.
    const AUTHZ_GRANT_CAP: usize = 256;

    /// The live grants for one principal: the `mg:permits` heads in the
    /// reserved `agent:authz` namespace (OMS 1.6 §12.6), parsed from their
    /// canonical object strings. Recall reads heads only, so a revoked
    /// grant — a retraction by supersession — simply stops appearing.
    /// A malformed grant grain is skipped: under-granting is the safe
    /// failure. Zero grants = the principal can do nothing (callers build a
    /// fail-closed `AuthzSet::restricted` from this).
    pub fn authz_grants(&mut self, principal: &str) -> Result<Vec<authz::Grant>> {
        // recall_raw, not recall: the authz engine must see the real grant
        // object strings — an egress policy on agent:authz would otherwise
        // pseudonymize every principal and fail every check (fail-closed in
        // the wrong place).
        let grains = self.recall_raw(
            authz::AUTHZ_NS,
            principal,
            Some(authz::REL_PERMITS),
            Self::AUTHZ_GRANT_CAP,
        )?;
        let mut grants = Vec::new();
        for g in &grains {
            if let Some(obj) = g.get_str("object") {
                if let Ok(grant) = authz::Grant::from_object_string(obj) {
                    grants.push(grant);
                }
            }
        }
        Ok(grants)
    }

    /// Forget (erase from hot store) — writes a tombstone to the op-log.
    /// File-level crypto-erasure remains the strong path.
    pub fn forget(&mut self, hash: &Hash) -> Result<()> {
        // Pre-transaction read for the (ns,s,p) key only. The SEQ is
        // deliberately not taken from here: it is re-resolved under the row
        // lock inside the transaction (a concurrent forget + re-add can move
        // this hash to a new seq). ns/s/p are stale-safe — identical content
        // means an identical key.
        let rows = self.db.query(
            "SELECT ns, s, p, blob FROM grains WHERE hash = ?1",
            vec![pb(hash.as_bytes().to_vec())],
        )?;
        let (ns, s, p) = match rows.first() {
            Some(row) => (row.i64(0), row.i64(1), row.i64(2)),
            None => return Err(AreevError::NotFound(*hash)),
        };
        // The grain's own CAS attachments, read pre-transaction (stale-safe:
        // identical hash means identical blob, so the ref list cannot drift).
        let mut cas_candidates: Vec<String> = rows
            .first()
            .and_then(|row| row.blob(3))
            .and_then(|b| deserialize_blob(&b).ok())
            .map(|view| cas_hexes(&view))
            .unwrap_or_default();
        cas_candidates.sort_unstable();
        cas_candidates.dedup();
        let ram_op = self.next_op;
        self.next_op += 1;
        let ram_hlc = self.next_hlc();
        let dbr = self.db.as_ref();
        let txn_out = with_txn(dbr, || {
            let (op_seq, hlc) = match dbr.reserve_write(0, 1, 1, now_ms() << 16, 0)? {
                Some(w) => (w.op0, w.hlc0),
                None => (ram_op, ram_hlc),
            };
            // Re-resolve BY HASH under the row lock and use the FRESH seq
            // for every delete: with concurrent writers, two racing forgets
            // must produce one success and one NotFound, and a forget +
            // re-add of identical content can move this hash to a NEW seq —
            // deleting the stale one would report success while erasing
            // nothing (and diverge replicas via the tombstone).
            let recheck = dbr.query(
                &format!("SELECT seq FROM grains WHERE hash=?1{}", dbr.for_update()),
                vec![pb(hash.as_bytes().to_vec())],
            )?;
            let seq = match recheck.first().and_then(|r| r.i64(0)) {
                Some(s) => s,
                None => return Err(AreevError::NotFound(*hash)),
            };
            // Document length before the delete removes it, so the BM25
            // collection stats can be corrected after commit.
            let doc_len = dbr
                .query("SELECT len FROM fts_doc WHERE seq = ?1", vec![pi(seq)])?
                .first()
                .and_then(|row| row.i64(0));
            dbr.execute("DELETE FROM triples WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM osp WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM embeddings WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM thread_idx WHERE seq=?1", vec![pi(seq)])?;
            // The join's two indexes, for the same reason every other index
            // is reconciled here. `seq` is re-derived as MAX(seq)+1 on open,
            // so forgetting the newest grain hands its seq to the next write
            // — a surviving row would then re-attach a stranger to this
            // grain's parent or run.
            dbr.execute("DELETE FROM prov_idx WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM run_idx WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM corpus_idx WHERE seq=?1", vec![pi(seq)])?;
            // Postings too, or a forgotten grain's words keep answering
            // free-text recall — a tombstone that leaves the text findable
            // is not a tombstone.
            dbr.execute("DELETE FROM fts_post WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM fts_doc WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM grains WHERE seq=?1", vec![pi(seq)])?;
            // Reconcile the head/entity_latest indexes for the cell.
            if let (Some(ns), Some(s), Some(p)) = (ns, s, p) {
                // Forget must drop the grain's fork-tip row too — every other
                // index is reconciled here, but `heads` was left dangling, so
                // heads()/open_forks() kept surfacing a hash whose get() fails
                // (and merge_heads could merge a forgotten tip).
                dbr.execute(
                    "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(seq)],
                )?;
                dbr.execute(
                    "DELETE FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(seq)],
                )?;
                // Re-elect the provisional head from the surviving tips using
                // the SAME (created_at, hash) rule as heads()/insert_blob —
                // was `ORDER BY seq DESC`, which can disagree with the
                // provisional-head rule in a 3+-way fork after a forget.
                // Key the join on (ns,s,p), not seq alone: a link-bearing
                // grain has extra triples rows for the same seq, and an
                // unkeyed join lets the engine pick a link target as t.o.
                let rows = dbr.query(
                    "SELECT t.o, h.seq, h.hash, h.created_at
                     FROM heads h JOIN triples t
                       ON t.seq=h.seq AND t.ns=h.ns AND t.s=h.s AND t.p=h.p
                     WHERE h.ns=?1 AND h.s=?2 AND h.p=?3
                     ORDER BY h.created_at DESC, h.hash DESC LIMIT 1",
                    vec![pi(ns), pi(s), pi(p)],
                )?;
                if let Some(row) = rows.first() {
                    let o = row.i64(0).unwrap_or(0);
                    let sq = row.i64(1).unwrap_or(0);
                    let h = row.blob(2).unwrap_or_default();
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(ns), pi(s), pi(p), pi(o), pi(sq), pb(h)],
                    )?;
                }
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc), pi(OP_FORGET), pb(hash.as_bytes().to_vec())],
            )?;
            // Targeted CAS reclamation, mirroring erase_where: a tombstone
            // that leaves the attachment bytes on disk is not a tombstone.
            // This is also the replica path — import_bundle replays OP_FORGET
            // through here, so an erasure's tombstones reach the replica's
            // blob sidecar, not just its index rows. The store-wide reference
            // scan runs only for blob-bearing grains; the common forget never
            // pays it. Table deletes are transactional; filesystem deletes
            // are decided here and executed post-commit.
            let mut fs_blobs: Vec<String> = Vec::new();
            if !cas_candidates.is_empty() {
                let mut referenced: HashSet<String> = HashSet::new();
                for row in dbr.query("SELECT blob FROM grains", vec![])? {
                    if let Some(b) = row.blob(0) {
                        if let Ok(view) = deserialize_blob(&b) {
                            referenced.extend(cas_hexes(&view));
                        }
                    }
                }
                for hex in &cas_candidates {
                    if referenced.contains(hex) {
                        continue;
                    }
                    match &self.blob_store {
                        BlobStore::Table => {
                            if let Ok(raw) = hex::decode(hex) {
                                dbr.execute("DELETE FROM blobs WHERE hash=?1", vec![pb(raw)])?;
                            }
                        }
                        BlobStore::Fs(_) => fs_blobs.push(hex.clone()),
                    }
                }
            }
            Ok((doc_len, fs_blobs))
        })?;
        let (doc_len, fs_blobs) = txn_out;
        if let Some(len) = doc_len {
            self.fts_docs = (self.fts_docs - 1).max(0);
            self.fts_total_len = (self.fts_total_len - len).max(0);
        }
        // Filesystem CAS deletes decided in-txn (single-writer file backend —
        // no concurrent put can race this).
        if let BlobStore::Fs(dir) = &self.blob_store {
            for hex in &fs_blobs {
                let _ = std::fs::remove_file(fs_blob_path(dir, hex));
            }
        }

        // Scrub the telemetry sidecar so a forgotten grain never lingers there.
        // Best-effort: the main erasure already committed, and the sidecar is
        // encrypted under the same key (crypto-erasure covers any residue) and
        // is rebuildable — a scrub hiccup must not fail an accomplished forget.
        if let Some(tel) = self.telemetry.as_mut() {
            let _ = tel.scrub(hash);
        }
        Ok(())
    }

    /// Erase every grain holding a STRUCTURED reference to `subject` in `ns`
    /// — the right-to-erasure primitive for one identity. Matching covers
    /// the exact identity AND every partition-style key (`subject` followed
    /// by a non-alphanumeric separator: `pat`, `pat#visit1` — never
    /// `patricia`). In one transaction this removes:
    ///
    /// - every matched grain (live AND superseded — the whole history, not
    ///   just the heads) referenced in triple subject/object position, as a
    ///   thread session, or as a run id;
    /// - the matched dictionary strings themselves once nothing references
    ///   them (identifier and key strings are erasable data), and vocabulary
    ///   tokens that occurred only in the erased text;
    ///
    /// then reclaims CAS blobs only those grains referenced and scrubs the
    /// telemetry sidecar for every matched identity string. Every erased
    /// grain gets its own op-log tombstone, so the erasure REPLICATES to
    /// peers exactly like individual forgets. See
    /// [`forget_subject_with`](Self::forget_subject_with) for the opt-in
    /// text-mention scope.
    ///
    /// Scope contract (see docs/erasure.md): only dictionary-indexed
    /// references are findable. A free-text mention of the identity inside
    /// ANOTHER subject's grain is not — hosts that need erasure must keep
    /// identity references in structured fields. Since CAL 1.3 this is also
    /// reachable as `FORGET SUBJECT "<id>" BECAUSE "…"` — authorization-
    /// gated and audit-logged (REQ-ERASE in docs/erasure.md).
    pub fn forget_subject(&mut self, ns: &str, subject: &str) -> Result<ErasureReport> {
        self.forget_subject_with(ns, subject, ErasureOptions::default())
    }

    /// [`forget_subject`](Self::forget_subject) with options — see
    /// [`ErasureOptions`] for the text-mention mode. Identity matching
    /// always covers PARTITION-STYLE KEYS too: any dictionary term equal to
    /// `subject` or starting with `subject` followed by a non-alphanumeric
    /// separator (`pat`, `pat#visit1`, `pat:thread-2` — but never
    /// `patricia`), case-exact on every backend.
    pub fn forget_subject_with(
        &mut self,
        ns: &str,
        subject: &str,
        opts: ErasureOptions,
    ) -> Result<ErasureReport> {
        self.check_subject_selector("forget_subject", subject, opts.text_mentions)?;
        let Some(ns_id) = self.term_lookup(ns)? else {
            return Ok(ErasureReport::default());
        };
        let mut report = self.erase_where(
            ErasureSelector::Identity {
                ns_id,
                subject: subject.to_string(),
                text_mentions: opts.text_mentions,
            },
            Some(subject.to_string()),
        )?;
        // REQ-ANON-7 (ingress stays erasable): under an ingress policy the
        // stored subject is a value-derived pseudonym — recompute it from
        // the real identity and erase under that name too, so
        // pseudonymized-at-rest never means erasure-proof.
        if let Some(alias) = self.anon.ingress_alias(ns, subject)? {
            if let Some(ns_id) = self.term_lookup(ns)? {
                let alias_report = self.erase_where(
                    ErasureSelector::Identity {
                        ns_id,
                        subject: alias.clone(),
                        text_mentions: opts.text_mentions,
                    },
                    Some(alias),
                )?;
                report.grains_erased += alias_report.grains_erased;
                report.terms_removed += alias_report.terms_removed;
                report.vocab_removed += alias_report.vocab_removed;
                report.blobs_reclaimed += alias_report.blobs_reclaimed;
                report.erased_hashes.extend(alias_report.erased_hashes);
            }
        }
        Ok(report)
    }

    /// Shared preconditions for the subject selector — erasure AND the
    /// read-only report must refuse identically, or "show me" and "erase"
    /// drift apart. An empty identity is a caller bug (an unset variable),
    /// and with prefix matching it would otherwise select EVERY
    /// punctuation-prefixed term — a silent mass-selection is the worst
    /// possible reading of "". Capability errors come before scope
    /// shortcuts: a mistyped namespace must not turn the documented
    /// Validation error into a silent zero-count success. `fts_deferred`
    /// counts as indexing-off — a deferred bulk load has grains with no
    /// postings yet, and selecting through a partial index is the silent
    /// partial coverage REQ-ERASE-8 forbids.
    fn check_subject_selector(&self, op: &str, subject: &str, text_mentions: bool) -> Result<()> {
        if subject.trim().is_empty() {
            return Err(AreevError::Validation(format!("{op} needs a non-empty subject")));
        }
        if text_mentions && (!self.index_text || self.fts_deferred) {
            return Err(AreevError::Validation(
                "text-mention erasure needs the text index on and fully built for this \
                 memory (the BM25 index is what makes prose mentions findable; finish the \
                 deferred rebuild first)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Read-only DSAR mirror of [`forget_subject`](Self::forget_subject):
    /// everything the erasure selector would match for `subject` in `ns` —
    /// exact identity + partition keys, full history — hydrated instead of
    /// erased (GDPR Art. 15 access / Art. 20 portability). "Show me
    /// everything, then delete it" is this method then that one, over ONE
    /// audited selection. Unknown namespace returns an empty report, the
    /// same contract as erasure.
    pub fn subject_report(&mut self, ns: &str, subject: &str) -> Result<SubjectReport> {
        self.subject_report_with(ns, subject, ErasureOptions::default())
    }

    /// [`subject_report`](Self::subject_report) with options —
    /// `text_mentions` extends the selection to indexed-text mentions with
    /// the same hard preconditions as erasure (never a silent partial
    /// answer).
    /// Deliberately EXEMPT from the egress boundary: the DSAR read must
    /// disclose what is actually stored (REQ-ERASE-9 — the report and the
    /// erasure share one selector), and a pseudonymized disclosure would
    /// misstate it. The reveal-grant gating of this read arrives with the
    /// vault phase (proposal D9/P3).
    pub fn subject_report_with(
        &mut self,
        ns: &str,
        subject: &str,
        opts: ErasureOptions,
    ) -> Result<SubjectReport> {
        self.check_subject_selector("subject_report", subject, opts.text_mentions)?;
        let Some(ns_id) = self.term_lookup(ns)? else {
            return Ok(SubjectReport::default());
        };
        let mut sel = self.select_identity(ns_id, subject, opts.text_mentions)?;
        // REQ-ANON-7 mirror: the DSAR must disclose ingress-pseudonymized
        // grains too — same alias, same selector, so "show me" and "erase"
        // keep covering the identical set (REQ-ERASE-9).
        if let Some(alias) = self.anon.ingress_alias(ns, subject)? {
            let alias_sel = self.select_identity(ns_id, &alias, opts.text_mentions)?;
            sel.seqs.extend(alias_sel.seqs);
            sel.identity_names.extend(alias_sel.identity_names);
        }
        let sel = sel;
        let mut seqs = sel.seqs;
        seqs.sort_unstable();
        seqs.dedup();
        let mut grains = Vec::with_capacity(seqs.len());
        if !seqs.is_empty() {
            let csv = seq_csv(&seqs);
            for row in self
                .db
                .query(&format!("SELECT blob FROM grains WHERE seq IN ({csv}) ORDER BY seq"), vec![])?
            {
                if let Some(b) = row.blob(0) {
                    grains.push(deserialize_blob(&b)?);
                }
            }
        }
        Ok(SubjectReport { identity_names: sel.identity_names, grains })
    }

    /// Export the subject selection as a portable MGB1 bundle — the
    /// Art. 20 (data portability) artifact: the same record format
    /// [`bundle_since`](Self::bundle_since) writes, restricted to the
    /// grains the DSAR selector matched, importable into any OMS store.
    /// Tombstones are never exported — a DSAR bundle carries the subject's
    /// data (history included), not deletions.
    pub fn subject_bundle_with(
        &mut self,
        ns: &str,
        subject: &str,
        opts: ErasureOptions,
        path: &str,
    ) -> Result<BundleStats> {
        self.check_subject_selector("subject_bundle", subject, opts.text_mentions)?;
        let mut out: Vec<u8> = Vec::with_capacity(16 * 1024);
        out.extend_from_slice(BUNDLE_MAGIC);
        let mut ops = 0usize;
        let mut last = 0i64;
        if let Some(ns_id) = self.term_lookup(ns)? {
            let sel = self.select_identity(ns_id, subject, opts.text_mentions)?;
            let mut seqs = sel.seqs;
            seqs.sort_unstable();
            seqs.dedup();
            if !seqs.is_empty() {
                let csv = seq_csv(&seqs);
                // The matched grains' own op-log records (ADD or SUPERSEDE),
                // in op order so supersession chains replay causally. The
                // grains join excludes forgotten hashes; OP_FORGET is
                // excluded explicitly so a forget + re-add of identical
                // content never smuggles a tombstone into the export.
                for row in self.db.query(
                    &format!(
                        "SELECT o.op_seq, o.hlc, o.op, g.hash, g.blob \
                         FROM oplog o JOIN grains g ON g.hash = o.hash \
                         WHERE g.seq IN ({csv}) AND o.op != ?1 ORDER BY o.op_seq"
                    ),
                    vec![pi(OP_FORGET)],
                )? {
                    let (Some(op_seq), Some(hlc), Some(op), Some(hash), Some(blob)) =
                        (row.i64(0), row.i64(1), row.i64(2), row.blob(3), row.blob(4))
                    else {
                        continue;
                    };
                    out.push(op as u8);
                    out.extend_from_slice(&hlc.to_le_bytes());
                    out.extend_from_slice(&hash);
                    out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
                    out.extend_from_slice(&blob);
                    ops += 1;
                    last = op_seq;
                }
            }
        }
        std::fs::write(path, &out).map_err(db_err)?;
        Ok(BundleStats {
            ops,
            bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            last_op_seq: last,
        })
    }

    /// Erase every grain older than `cutoff_ms` (`created_at < cutoff`),
    /// optionally scoped to one namespace and/or grain type — the
    /// retention-sweep primitive (nightly age-based deletion). Same erasure
    /// semantics as [`forget_subject`] minus the identity-dictionary sweep.
    pub fn forget_older_than(
        &mut self,
        ns: Option<&str>,
        cutoff_ms: i64,
        gtype: Option<areev_core::types::GrainType>,
    ) -> Result<ErasureReport> {
        self.forget_older_than_capped(ns, cutoff_ms, gtype, None)
    }

    /// The same sweep bounded to at most `limit` grains, **oldest first** —
    /// what CAL's `PURGE OLDER THAN … LIMIT n` means. A bounded sweep is how
    /// an operator pilots a retention rule: erase a reviewable batch, check
    /// the audit record, then widen. An ignored bound is not a smaller
    /// erasure, it is the whole namespace.
    pub fn forget_older_than_capped(
        &mut self,
        ns: Option<&str>,
        cutoff_ms: i64,
        gtype: Option<areev_core::types::GrainType>,
        limit: Option<usize>,
    ) -> Result<ErasureReport> {
        if limit == Some(0) {
            return Ok(ErasureReport::default());
        }
        // Floors and holds gate EVERY age-based destruction — this is the
        // one choke point CAL `PURGE`, `sweep_retention`, and the loop's
        // retention analyzer all flow through, so a mandated log (AI Act
        // Art. 12) cannot be destroyed by any of them.
        self.check_age_destruction_allowed(ns, cutoff_ms)?;
        let ns_id = match ns {
            Some(n) => match self.term_lookup(n)? {
                Some(x) => Some(x),
                None => return Ok(ErasureReport::default()),
            },
            None => None,
        };
        let gt = gtype.map(|g| g as u8 as i64);
        self.erase_where(
            ErasureSelector::OlderThan { ns_id, cutoff_ms, gtype: gt, limit },
            None,
        )
    }

    /// What the identity selector matched: dictionary ids, the matched
    /// STRINGS (exact + partition keys), and the grain seqs. Shared by
    /// [`erase_where`](Self::erase_where) (erasure mode) and
    /// [`subject_report_with`](Self::subject_report_with) (read-only DSAR
    /// mode), so "show me everything" and "erase everything" are provably
    /// ONE selection. `seqs` is returned unsorted and may hold duplicates;
    /// callers sort/dedup.
    fn select_identity(
        &mut self,
        ns_id: i64,
        subject: &str,
        text_mentions: bool,
    ) -> Result<IdentitySelection> {
        let mut sel = IdentitySelection::default();
        // Resolve the identity's dictionary ids: the exact term plus every
        // PARTITION-STYLE key — `subject` followed by a non-alphanumeric
        // separator (`pat#visit1`, `pat:thread-2`), never a longer word
        // (`patricia`). The LIKE is a coarse prefilter (case-insensitive on
        // the embedded engine); the Rust boundary check decides,
        // case-exactly, identically on every backend.
        let like = format!("{}%", like_escape(subject));
        for row in self.db.query(
            "SELECT id, term FROM terms WHERE term LIKE ?1 ESCAPE '\\'",
            vec![pt(&like)],
        )? {
            let (Some(id), Some(term)) = (row.i64(0), row.text(1)) else {
                continue;
            };
            let matched = term == subject
                || (term.starts_with(subject)
                    && term[subject.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| !c.is_alphanumeric()));
            if matched {
                sel.identity_ids.push(id);
                sel.identity_names.push(term.to_string());
            }
        }
        // Telemetry keys on the subject STRING even when the dictionary
        // never interned it (ring-log query text), so the bare subject
        // always joins the matched-name list.
        if !sel.identity_names.iter().any(|n| n == subject) {
            sel.identity_names.push(subject.to_string());
        }
        if !sel.identity_ids.is_empty() {
            let idcsv = seq_csv(&sel.identity_ids);
            sel.seqs.extend(
                self.db
                    .query(
                        &format!(
                            "SELECT DISTINCT seq FROM triples WHERE ns=?1 AND \
                               (s IN ({idcsv}) OR o IN ({idcsv})) \
                             UNION SELECT seq FROM thread_idx WHERE ns=?1 AND session IN ({idcsv}) \
                             UNION SELECT seq FROM run_idx WHERE ns=?1 AND run IN ({idcsv})"
                        ),
                        vec![pi(ns_id)],
                    )?
                    .iter()
                    .filter_map(|r| r.i64(0)),
            );
        }
        // Search symmetry, opt-in: whatever the BM25 leg can find by the
        // identity's tokens, the selection covers — a grain whose INDEXED
        // TEXT mentions the identity joins when every token occurs in it.
        if text_mentions {
            let mut tokens = tokenize(subject);
            tokens.sort_unstable();
            tokens.dedup();
            let mut inter: Option<HashSet<i64>> = None;
            for t in &tokens {
                let set: HashSet<i64> = match self.fts_term_lookup(t)? {
                    Some(vid) => self
                        .db
                        .query(
                            "SELECT seq FROM fts_post WHERE term=?1 AND ns=?2",
                            vec![pi(vid), pi(ns_id)],
                        )?
                        .iter()
                        .filter_map(|r| r.i64(0))
                        .collect(),
                    None => HashSet::new(),
                };
                inter = Some(match inter {
                    None => set,
                    Some(p) => p.intersection(&set).copied().collect(),
                });
                if inter.as_ref().is_some_and(|s| s.is_empty()) {
                    break;
                }
            }
            if let Some(set) = inter {
                sel.seqs.extend(set);
            }
        }
        Ok(sel)
    }

    /// Shared bulk-erasure core. Enumeration happens INSIDE the transaction,
    /// after the serialization point, so no concurrently-committed grain can
    /// slip between the census and the deletes (the lesson every
    /// read-then-write path here has had to learn).
    fn erase_where(
        &mut self,
        selector: ErasureSelector,
        identity: Option<String>,
    ) -> Result<ErasureReport> {
        struct Target {
            seq: i64,
            hash: Vec<u8>,
            ns: i64,
            s: Option<i64>,
            p: Option<i64>,
            o: Option<i64>,
            doc_len: Option<i64>,
            blob: Vec<u8>,
        }
        struct EraseOut {
            report: ErasureReport,
            hashes: Vec<Vec<u8>>,
            docs: i64,
            len_sum: i64,
            /// Fs-backend CAS files whose reclamation was decided in-txn;
            /// the filesystem deletes happen post-commit.
            fs_blobs: Vec<String>,
            /// Term ids whose strings were tombstoned — evicted from the
            /// local dictionary cache post-commit.
            scrubbed_terms: Vec<i64>,
            /// Every matched identity STRING (exact + partition keys) — the
            /// telemetry sidecar keys rows on these, so each one is
            /// scrubbed post-commit, not just the bare subject.
            identity_names: Vec<String>,
        }
        self.db.begin()?;
        let r = (|| -> Result<EraseOut> {
            let mut out = EraseOut {
                report: ErasureReport::default(),
                hashes: Vec::new(),
                docs: 0,
                len_sum: 0,
                fs_blobs: Vec::new(),
                scrubbed_terms: Vec::new(),
                identity_names: Vec::new(),
            };
            // Serialize with concurrent writers before enumerating.
            self.db.reserve_write(0, 0, 0, now_ms() << 16, 0)?;
            let mut identity_ids: Vec<i64> = Vec::new();
            let mut seqs: Vec<i64> = match &selector {
                ErasureSelector::Identity { ns_id, subject, text_mentions } => {
                    // The shared selector, run in-txn after the serialization
                    // point — the SAME selection the read-only subject report
                    // hydrates (select_identity), here feeding the deletes.
                    let sel = self.select_identity(*ns_id, subject, *text_mentions)?;
                    identity_ids = sel.identity_ids;
                    out.identity_names = sel.identity_names;
                    sel.seqs
                }
                ErasureSelector::OlderThan { ns_id, cutoff_ms, gtype, limit } => {
                    let mut sql = String::from("SELECT seq FROM grains WHERE created_at < ?1");
                    let mut params = vec![pi(*cutoff_ms)];
                    if let Some(n) = ns_id {
                        sql.push_str(" AND ns = ?2");
                        params.push(pi(*n));
                    }
                    if let Some(g) = gtype {
                        sql.push_str(if ns_id.is_some() { " AND gtype = ?3" } else { " AND gtype = ?2" });
                        params.push(pi(*g));
                    }
                    // A bounded sweep takes the OLDEST matches, so repeating
                    // it walks forward through the backlog deterministically
                    // instead of re-rolling an arbitrary slice. The bound is
                    // a Rust integer, never text — safe to inline, which
                    // keeps the hand-numbered placeholders above intact.
                    if let Some(n) = limit {
                        sql.push_str(&format!(" ORDER BY created_at ASC, seq ASC LIMIT {n}"));
                    }
                    self.db.query(&sql, params)?.iter().filter_map(|r| r.i64(0)).collect()
                }
            };
            seqs.sort_unstable();
            seqs.dedup();
            if seqs.is_empty() {
                // Nothing matched — but the IDENTIFIER (and its partition
                // keys) may still exist in the dictionary after per-grain
                // forgets. Erasure of the strings must still be possible.
                for id in &identity_ids {
                    let gone = self.scrub_term_if_unreferenced(*id)?;
                    out.report.terms_removed += gone as usize;
                    if gone > 0 {
                        out.scrubbed_terms.push(*id);
                    }
                }
                return Ok(out);
            }
            let n = seqs.len() as i64;
            // One tombstone per grain: ids from the counters on multi-writer
            // backends, from the process-local counters otherwise.
            let (op0, hlc0) = match self.db.reserve_write(0, n, n, now_ms() << 16, 0)? {
                Some(w) => (w.op0, w.hlc0),
                None => {
                    let op0 = self.next_op;
                    self.next_op += n;
                    let hlc0 = self.next_hlc();
                    self.hlc_last = hlc0 + n - 1;
                    (op0, hlc0)
                }
            };
            let csv = seq_csv(&seqs);
            // Vocabulary candidates BEFORE the postings are deleted.
            let vocab_candidates: Vec<i64> = self
                .db
                .query(&format!("SELECT DISTINCT term FROM fts_post WHERE seq IN ({csv})"), vec![])?
                .iter()
                .filter_map(|r| r.i64(0))
                .collect();
            // Dictionary candidates BEFORE their referencing rows are deleted:
            // every term the erased grains touch (their subject/relation/
            // object VALUES are the identity's data), plus session and run
            // ids. The scrub below only tombstones the ones left with zero
            // references, so shared vocabulary survives.
            let mut term_candidates: Vec<i64> = Vec::new();
            for sql in [
                format!("SELECT DISTINCT session FROM thread_idx WHERE seq IN ({csv})"),
                format!("SELECT DISTINCT run FROM run_idx WHERE seq IN ({csv})"),
            ] {
                for row in self.db.query(&sql, vec![])? {
                    if let Some(id) = row.i64(0) {
                        term_candidates.push(id);
                    }
                }
            }
            term_candidates.extend(identity_ids.iter().copied());
            let mut targets: Vec<Target> = Vec::with_capacity(seqs.len());
            for row in self.db.query(
                &format!(
                    "SELECT g.seq, g.hash, g.ns, g.s, g.p, g.o, d.len, g.blob \
                     FROM grains g LEFT JOIN fts_doc d ON d.seq = g.seq \
                     WHERE g.seq IN ({csv})"
                ),
                vec![],
            )? {
                let (Some(seq), Some(hash), Some(ns), Some(blob)) =
                    (row.i64(0), row.blob(1), row.i64(2), row.blob(7))
                else {
                    continue;
                };
                targets.push(Target {
                    seq,
                    hash,
                    ns,
                    s: row.i64(3),
                    p: row.i64(4),
                    o: row.i64(5),
                    doc_len: row.i64(6),
                    blob,
                });
            }
            for t in &targets {
                for id in [t.s, t.p, t.o].into_iter().flatten() {
                    term_candidates.push(id);
                }
            }
            term_candidates.sort_unstable();
            term_candidates.dedup();
            let mut affected_keys: Vec<(i64, i64, i64)> = Vec::new();
            for (i, t) in targets.iter().enumerate() {
                for table in [
                    "triples",
                    "osp",
                    "embeddings",
                    "thread_idx",
                    "prov_idx",
                    "run_idx",
                    "corpus_idx",
                    "fts_post",
                    "fts_doc",
                ]
                {
                    self.db
                        .execute(&format!("DELETE FROM {table} WHERE seq=?1"), vec![pi(t.seq)])?;
                }
                self.db.execute("DELETE FROM grains WHERE seq=?1", vec![pi(t.seq)])?;
                if let (Some(s), Some(p)) = (t.s, t.p) {
                    self.db.execute(
                        "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                        vec![pi(t.ns), pi(s), pi(p), pi(t.seq)],
                    )?;
                    if !affected_keys.contains(&(t.ns, s, p)) {
                        affected_keys.push((t.ns, s, p));
                    }
                }
                self.db.execute(
                    "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                    vec![
                        pi(op0 + i as i64),
                        pi(hlc0 + i as i64),
                        pi(OP_FORGET),
                        pb(t.hash.clone()),
                    ],
                )?;
            }
            // Reconcile entity_latest per affected key: drop rows pointing at
            // erased grains, then re-elect from the surviving heads with the
            // SAME (created_at, hash) rule as everywhere else.
            for (ns, s, p) in &affected_keys {
                self.db.execute(
                    &format!(
                        "DELETE FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND seq IN ({csv})"
                    ),
                    vec![pi(*ns), pi(*s), pi(*p)],
                )?;
                let rows = self.db.query(
                    "SELECT t.o, h.seq, h.hash, h.created_at
                     FROM heads h JOIN triples t
                       ON t.seq=h.seq AND t.ns=h.ns AND t.s=h.s AND t.p=h.p
                     WHERE h.ns=?1 AND h.s=?2 AND h.p=?3
                     ORDER BY h.created_at DESC, h.hash DESC LIMIT 1",
                    vec![pi(*ns), pi(*s), pi(*p)],
                )?;
                if let Some(row) = rows.first() {
                    let o = row.i64(0).unwrap_or(0);
                    let sq = row.i64(1).unwrap_or(0);
                    let h = row.blob(2).unwrap_or_default();
                    self.db.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(*ns), pi(*s), pi(*p), pi(o), pi(sq), pb(h)],
                    )?;
                }
            }
            // Vocabulary sweep: tokens that occurred only in the erased text
            // carry erasable content — remove them once unreferenced.
            for term in vocab_candidates {
                let gone = self.db.execute(
                    "DELETE FROM fts_vocab WHERE id=?1 AND NOT EXISTS \
                     (SELECT 1 FROM fts_post WHERE term=?1)",
                    vec![pi(term)],
                )?;
                out.report.vocab_removed += gone as usize;
            }
            // Dictionary sweep: TOMBSTONE (not delete) every candidate term
            // left with zero references. Deleting the row would leave other
            // instances' cached term ids dangling — grains written through a
            // stale cache would silently reference a nonexistent row.
            // Tombstoning replaces the STRING (the erasable data) while the
            // id stays referentially valid; a stale-cache write afterwards
            // degrades to "unfindable by the old name", never to corruption.
            for id in term_candidates {
                let gone = self.scrub_term_if_unreferenced(id)?;
                out.report.terms_removed += gone as usize;
                if gone > 0 {
                    out.scrubbed_terms.push(id);
                }
            }
            // CAS reclamation, TARGETED to the erased grains' own
            // attachments — never a store-wide gc (whose census would race a
            // concurrent writer's put_blob -> add and destroy an unrelated
            // blob). The surviving-reference check runs under the write
            // serialization this transaction already holds; table deletes
            // are transactional, filesystem deletes are decided here and
            // executed post-commit.
            let mut cas_candidates: Vec<String> = Vec::new();
            for t in &targets {
                if let Ok(view) = deserialize_blob(&t.blob) {
                    cas_candidates.extend(cas_hexes(&view));
                }
            }
            cas_candidates.sort_unstable();
            cas_candidates.dedup();
            if !cas_candidates.is_empty() {
                let mut referenced: HashSet<String> = HashSet::new();
                for row in self.db.query("SELECT blob FROM grains", vec![])? {
                    if let Some(b) = row.blob(0) {
                        if let Ok(view) = deserialize_blob(&b) {
                            referenced.extend(cas_hexes(&view));
                        }
                    }
                }
                for hex in cas_candidates {
                    if referenced.contains(&hex) {
                        continue;
                    }
                    match &self.blob_store {
                        BlobStore::Table => {
                            let Ok(raw) = hex::decode(&hex) else { continue };
                            let gone = self
                                .db
                                .execute("DELETE FROM blobs WHERE hash=?1", vec![pb(raw)])?;
                            out.report.blobs_reclaimed += gone as usize;
                        }
                        BlobStore::Fs(_) => out.fs_blobs.push(hex),
                    }
                }
            }
            out.report.grains_erased = targets.len();
            out.docs = targets.iter().filter(|t| t.doc_len.is_some()).count() as i64;
            out.len_sum = targets.iter().filter_map(|t| t.doc_len).sum();
            out.hashes = targets.into_iter().map(|t| t.hash).collect();
            Ok(out)
        })();
        match r {
            Ok(mut out) => {
                self.db.commit()?;
                out.report.erased_hashes = out
                    .hashes
                    .iter()
                    .filter_map(|bytes| Hash::try_from_bytes(bytes).ok())
                    .map(|hash| hash.to_hex())
                    .collect();
                self.fts_docs = (self.fts_docs - out.docs).max(0);
                self.fts_total_len = (self.fts_total_len - out.len_sum).max(0);
                if !out.scrubbed_terms.is_empty() {
                    // Evict every scrubbed id from the local dictionary cache
                    // (string -> id); the strings no longer resolve.
                    self.dict.retain(|_, id| !out.scrubbed_terms.contains(id));
                }
                if out.report.vocab_removed > 0 {
                    // Cheap and safe: the cache refills lazily from the table.
                    self.fts_vocab.clear();
                }
                // Filesystem CAS deletes decided in-txn (single-writer file
                // backend — no concurrent put can race this).
                if let BlobStore::Fs(dir) = &self.blob_store {
                    let dir = dir.clone();
                    for hex in &out.fs_blobs {
                        if std::fs::remove_file(fs_blob_path(&dir, hex)).is_ok() {
                            out.report.blobs_reclaimed += 1;
                        }
                    }
                }
                // Best-effort post-commit hygiene: the erasure itself has
                // already committed and replicated.
                if self.telemetry.is_some() {
                    let hashes: Vec<Hash> = out
                        .hashes
                        .iter()
                        .filter_map(|h| Hash::try_from_bytes(h).ok())
                        .collect();
                    if let Some(tel) = self.telemetry.as_mut() {
                        for h in &hashes {
                            let _ = tel.scrub(h);
                        }
                        if identity.is_some() {
                            // Query stats and the recall log key on IDENTITY
                            // STRINGS, not grain hashes — and a ring-log row
                            // recorded against a partition key (subject =
                            // "pat#visit1", query NULL) is only reachable by
                            // that exact string, so EVERY matched name is
                            // scrubbed, not just the bare subject.
                            for name in &out.identity_names {
                                let _ = tel.scrub_subject(name);
                            }
                        }
                    }
                }
                Ok(out.report)
            }
            Err(e) => {
                let _ = self.db.rollback();
                Err(e)
            }
        }
    }

    /// Tombstone one dictionary entry once nothing references it: the STRING
    /// is replaced with an unrecallable placeholder while the id stays valid
    /// (see the dictionary-sweep comment in [`erase_where`](Self::erase_where)
    /// for why deletion would be worse). Returns rows changed (0 or 1); the
    /// guard also skips rows already tombstoned so repeat erasures don't
    /// re-count them.
    fn scrub_term_if_unreferenced(&mut self, id: i64) -> Result<u64> {
        let placeholder = format!("\u{1}erased:{id}");
        self.db.execute(
            "UPDATE terms SET term = ?2 WHERE id = ?1 \
             AND NOT EXISTS (SELECT 1 FROM triples WHERE s=?1 OR p=?1 OR o=?1) \
             AND NOT EXISTS (SELECT 1 FROM thread_idx WHERE session=?1) \
             AND NOT EXISTS (SELECT 1 FROM run_idx WHERE run=?1) \
             AND NOT EXISTS (SELECT 1 FROM grains WHERE ns=?1) \
             AND term NOT LIKE ?3",
            vec![pi(id), pt(&placeholder), pt("\u{1}erased:%")],
        )
    }

    /// Append one op-log record (fresh local `op_seq`, caller-supplied `hlc`).
    /// The change-feed / sync primitive for state transitions that happen
    /// outside `add`/`supersede`/`forget` — a `merge_heads` fork-closure and an
    /// imported supersession whose grain already existed locally. Without it
    /// those transitions never replicate and downstream replicas diverge.
    fn log_op(&mut self, op: i64, hash: &Hash, hlc: i64) -> Result<()> {
        let ram_op = self.next_op;
        self.next_op += 1;
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            // The imported hlc is preserved on the row; the reservation only
            // raises the clock floor so local allocations stay ahead of it.
            let op_seq = match dbr.reserve_write(0, 1, 0, now_ms() << 16, hlc)? {
                Some(w) => w.op0,
                None => ram_op,
            };
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc), pi(op), pb(hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })
    }

    // ----- telemetry sidecar (host capability; off the recall path) -----

    /// The active telemetry mode — `Off` when no sidecar is attached.
    pub fn telemetry_mode(&self) -> TelemetryMode {
        self.telemetry
            .as_ref()
            .map(|t| t.mode())
            .unwrap_or(TelemetryMode::Off)
    }

    /// Drain buffered recall telemetry into the sidecar. A no-op when the
    /// buffer is empty; called from write ops and on close — never from recall.
    pub fn telemetry_flush(&mut self) -> Result<()> {
        if let Some(tel) = self.telemetry.as_mut() {
            tel.flush()?;
        }
        Ok(())
    }

    /// Record one assembly-budget sample (feeds the `budget_pressure` analyzer).
    pub fn telemetry_note_budget(&mut self, overflow: bool) -> Result<()> {
        if let Some(tel) = self.telemetry.as_mut() {
            tel.note_budget(overflow)?;
        }
        Ok(())
    }

    /// Grain-access rollups (feeds `cold_grains`). Flushes first so buffered
    /// recalls are counted; empty when telemetry is off.
    pub fn telemetry_access_stats(&mut self, ns: Option<&str>) -> Result<Vec<AccessStat>> {
        self.telemetry_flush()?;
        match self.telemetry.as_ref() {
            Some(tel) => tel.access_stats(ns),
            None => Ok(Vec::new()),
        }
    }

    /// Query rollups (feeds `coverage_gap`). Flushes first; empty when off.
    pub fn telemetry_query_stats(&mut self, ns: Option<&str>) -> Result<Vec<QueryStat>> {
        self.telemetry_flush()?;
        match self.telemetry.as_ref() {
            Some(tel) => tel.query_stats(ns),
            None => Ok(Vec::new()),
        }
    }

    /// Full-mode per-recall telemetry, optionally scoped to a trajectory id.
    pub fn telemetry_recall_log(
        &mut self,
        run_id: Option<&str>,
    ) -> Result<Vec<RecallLogEntry>> {
        self.telemetry_flush()?;
        match self.telemetry.as_ref() {
            Some(tel) => tel.recall_log(run_id),
            None => Ok(Vec::new()),
        }
    }

    /// The assembly-budget rollup (feeds `budget_pressure`). Empty when off.
    pub fn telemetry_budget_stats(&mut self) -> Result<BudgetStat> {
        self.telemetry_flush()?;
        match self.telemetry.as_ref() {
            Some(tel) => tel.budget_stats(),
            None => Ok(BudgetStat::default()),
        }
    }

    // ----- graph ops (bounded, indexed, capped) -----

    /// Execution records for a Workflow grain (OMS §8.4).
    ///
    /// Returns `(node_id, executing grain hash)` for every grain carrying a
    /// `mg:step_action:<node_id>` link to `workflow_hash`, newest first. This is
    /// how a run is reconstructed against its plan: the Workflow grain is
    /// immutable and content-addressed, so it never accumulates run state — the
    /// execution records point back at it instead.
    ///
    /// Pass `node_id` to narrow to one step. Results are capped.
    pub fn step_actions(
        &mut self,
        ns: &str,
        workflow_hash: &Hash,
        node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Hash)>> {
        let Some(ns_id) = self.term_lookup(ns)? else {
            return Ok(Vec::new());
        };
        let Some(target_id) = self.term_lookup(&workflow_hash.to_hex())? else {
            return Ok(Vec::new());
        };
        // Which predicates count: one exact relation, or every step_action.
        let pred_ids: Vec<i64> = match node_id {
            Some(n) => match self.term_lookup(&step_action_relation(n))? {
                Some(id) => vec![id],
                None => return Ok(Vec::new()),
            },
            None => self
                .terms_with_prefix(STEP_ACTION_PREFIX)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        };
        if pred_ids.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.min(1024);
        // The OSP index is keyed (ns, o, s) — the link's object is the workflow
        // hash, so this reads the reverse direction directly.
        let mut rows: Vec<(i64, i64, i64)> = Vec::new();
        for p in &pred_ids {
            for row in self.db.query(
                "SELECT seq, s, p FROM osp WHERE ns=?1 AND o=?2 AND p=?3 AND cur=1
                 ORDER BY seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(target_id), pi(*p), pi(limit as i64)],
            )? {
                let (Some(seq), Some(s), Some(p)) = (row.i64(0), row.i64(1), row.i64(2)) else {
                    continue;
                };
                rows.push((seq, s, p));
            }
        }

        // Each predicate was queried and capped separately, and with no
        // `node_id` the predicate set comes from a dictionary scan whose order
        // is a HashMap's — i.e. no order at all. Sort by seq before truncating,
        // or "newest first" is false across nodes and *which* records survive
        // the cap changes from one process to the next.
        rows.sort_unstable_by_key(|(seq, _, _)| std::cmp::Reverse(*seq));

        let mut result = Vec::with_capacity(rows.len());
        for (_, s, p) in rows {
            let (Some(subj), Some(rel)) = (self.term_str(s)?, self.term_str(p)?) else {
                continue;
            };
            let (Some(node), Ok(h)) = (step_action_node(&rel), Hash::from_hex(&subj)) else {
                continue;
            };
            result.push((node.to_string(), h));
        }
        result.truncate(limit);
        Ok(result)
    }

    /// One page of execution records for a workflow, **oldest first**, from an
    /// exclusive `after_seq` cursor — the paginated twin of
    /// [`Self::step_actions`], which clamps at 1024 with no cursor. Returns
    /// `(seq, node_id, executing grain hash)`; pass the last `seq` back as the
    /// next `after_seq`; a page shorter than `limit` is exhaustion.
    pub fn step_actions_page(
        &mut self,
        ns: &str,
        workflow_hash: &Hash,
        node_id: Option<&str>,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String, Hash)>> {
        let Some(ns_id) = self.term_lookup(ns)? else {
            return Ok(Vec::new());
        };
        let Some(target_id) = self.term_lookup(&workflow_hash.to_hex())? else {
            return Ok(Vec::new());
        };
        let pred_ids: Vec<i64> = match node_id {
            Some(n) => match self.term_lookup(&step_action_relation(n))? {
                Some(id) => vec![id],
                None => return Ok(Vec::new()),
            },
            None => self
                .terms_with_prefix(STEP_ACTION_PREFIX)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        };
        if pred_ids.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 1024);
        // Per-predicate queries, merged and re-sorted ascending before the
        // final truncate — same reasoning as `step_actions`: the predicate
        // set comes from a dictionary scan with no order, so sort-before-cut
        // or which rows survive the cap varies across processes.
        let mut rows: Vec<(i64, i64, i64)> = Vec::new();
        for p in &pred_ids {
            for row in self.db.query(
                "SELECT seq, s, p FROM osp WHERE ns=?1 AND o=?2 AND p=?3 AND cur=1
                 AND seq > ?4 ORDER BY seq ASC LIMIT ?5",
                vec![pi(ns_id), pi(target_id), pi(*p), pi(after_seq), pi(limit as i64)],
            )? {
                let (Some(seq), Some(s), Some(p)) = (row.i64(0), row.i64(1), row.i64(2)) else {
                    continue;
                };
                rows.push((seq, s, p));
            }
        }
        rows.sort_unstable_by_key(|(seq, _, _)| *seq);
        rows.truncate(limit);
        let mut result = Vec::with_capacity(rows.len());
        for (seq, s, p) in rows {
            let (Some(subj), Some(rel)) = (self.term_str(s)?, self.term_str(p)?) else {
                continue;
            };
            let (Some(node), Ok(h)) = (step_action_node(&rel), Hash::from_hex(&subj)) else {
                continue;
            };
            result.push((seq, node.to_string(), h));
        }
        Ok(result)
    }

    /// Dictionary terms starting with `prefix`, as `(id, term)`.
    ///
    /// The relation for an execution record is parameterized by node id, so the
    /// vocabulary cannot be enumerated statically. Bounded by the number of
    /// distinct node ids ever written, not by grain count.
    fn terms_with_prefix(&self, prefix: &str) -> Vec<(i64, String)> {
        self.dict
            .iter()
            .filter(|(term, _)| term.starts_with(prefix))
            .map(|(term, id)| (*id, term.clone()))
            .collect()
    }

    /// Grains that name `object` in their object position, newest first.
    ///
    /// The mirror of an anchored `recall_hybrid(ns, Some(subject), …)`: that
    /// answers "what does X point at", this answers "what points at X". Reads
    /// the OSP reverse index, so — like `related(Direction::In)` and for the
    /// same reason — it sees only relations the file declares as entity
    /// relations (`AreevOptions::entity_relations`). Reverse lookup over every
    /// relation would need a third full permutation of `triples`, which is the
    /// index the "2½ permutations" design exists to avoid.
    pub fn grains_by_object(
        &mut self,
        ns: &str,
        object: &str,
        limit: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        let (Some(ns_id), Some(obj_id)) = (self.term_lookup(ns)?, self.term_lookup(object)?) else {
            return Ok(Vec::new());
        };
        let limit = limit.min(1024) as i64;
        let mut out: Vec<DeserializedGrain> = self
            .db
            .query(
                "SELECT g.blob FROM osp o JOIN grains g ON g.seq = o.seq
                 WHERE o.ns = ?1 AND o.o = ?2 AND o.cur = 1 ORDER BY o.seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(obj_id), pi(limit)],
            )?
            .iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect::<Result<_>>()?;
        self.anon.egress_grains(Some(ns), &mut out)?;
        Ok(out)
    }

    /// Bounded k-hop traversal over the given relations.
    /// Returns reached entity terms (excluding the start), BFS order.
    /// `Direction::In`/`Both` use the selective OSP index, so reverse
    /// expansion only sees entity-valued relations.
    pub fn related(
        &mut self,
        ns: &str,
        start: &str,
        relations: &[&str],
        dir: Direction,
        depth: usize,
        cap: usize,
    ) -> Result<Vec<String>> {
        let ns_id = match self.term_lookup(ns)? {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let start_id = match self.term_lookup(start)? {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let mut rel_ids: Vec<i64> = Vec::new();
        for r in relations {
            if let Some(id) = self.term_lookup(r)? {
                rel_ids.push(id);
            }
        }
        if rel_ids.is_empty() {
            return Ok(Vec::new());
        }
        let depth = depth.min(4);
        let cap = cap.min(512);
        let reached = 'bfs: {
            let mut seen: HashSet<i64> = HashSet::new();
            seen.insert(start_id);
            let mut order: Vec<i64> = Vec::new();
            let mut frontier = vec![start_id];
            for _ in 0..depth {
                let mut next = Vec::new();
                for node in &frontier {
                    for p in &rel_ids {
                        if matches!(dir, Direction::Out | Direction::Both) {
                            for row in self.db.query(
                                "SELECT o FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 LIMIT 64",
                                vec![pi(ns_id), pi(*node), pi(*p)],
                            )? {
                                if let Some(o) = row.i64(0) {
                                    if seen.insert(o) {
                                        order.push(o);
                                        next.push(o);
                                        if order.len() >= cap {
                                            break 'bfs order;
                                        }
                                    }
                                }
                            }
                        }
                        if matches!(dir, Direction::In | Direction::Both) {
                            for row in self.db.query(
                                "SELECT s FROM osp WHERE ns=?1 AND o=?2 AND p=?3 AND cur=1 LIMIT 64",
                                vec![pi(ns_id), pi(*node), pi(*p)],
                            )? {
                                if let Some(s) = row.i64(0) {
                                    if seen.insert(s) {
                                        order.push(s);
                                        next.push(s);
                                        if order.len() >= cap {
                                            break 'bfs order;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
            order
        };
        let mut out = Vec::with_capacity(reached.len());
        for id in reached {
            if let Some(t) = self.term_str(id)? {
                out.push(t);
            }
        }
        // Egress boundary: reached entity terms are subject/object strings —
        // detection-based (an object like "tea" is not an identity).
        self.anon.egress_strings(ns, None, &mut out)?;
        Ok(out)
    }

    /// Bounded bidirectional-ish path search (forward BFS with parents).
    pub fn path(
        &mut self,
        ns: &str,
        from: &str,
        to: &str,
        relations: &[&str],
        max_depth: usize,
    ) -> Result<Option<Vec<String>>> {
        let ns_id = match self.term_lookup(ns)? {
            Some(x) => x,
            None => return Ok(None),
        };
        let (a, b) = match (self.term_lookup(from)?, self.term_lookup(to)?) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(None),
        };
        let mut rel_ids: Vec<i64> = Vec::new();
        for r in relations {
            if let Some(id) = self.term_lookup(r)? {
                rel_ids.push(id);
            }
        }
        if rel_ids.is_empty() {
            return Ok(None);
        }
        let max_depth = max_depth.min(6);
        let parents = {
            let mut parent: HashMap<i64, i64> = HashMap::new();
            let mut q = VecDeque::from([a]);
            let mut found = false;
            let mut hops = 0usize;
            let mut visited: HashSet<i64> = HashSet::from([a]);
            'outer: while !q.is_empty() && hops < max_depth {
                let level: Vec<i64> = q.drain(..).collect();
                for node in level {
                    for p in &rel_ids {
                        for row in self.db.query(
                            "SELECT o FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 LIMIT 64",
                            vec![pi(ns_id), pi(node), pi(*p)],
                        )? {
                            if let Some(o) = row.i64(0) {
                                if visited.insert(o) {
                                    parent.insert(o, node);
                                    if o == b {
                                        found = true;
                                        break 'outer;
                                    }
                                    q.push_back(o);
                                    if visited.len() > 2048 {
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                }
                hops += 1;
            }
            if found { Some(parent) } else { None }
        };
        let Some(parent) = parents else {
            return Ok(None);
        };
        let mut chain = vec![b];
        let mut cur = b;
        while let Some(pr) = parent.get(&cur) {
            chain.push(*pr);
            cur = *pr;
            if cur == a {
                break;
            }
        }
        chain.reverse();
        let mut out = Vec::with_capacity(chain.len());
        for id in chain {
            if let Some(t) = self.term_str(id)? {
                out.push(t);
            }
        }
        self.anon.egress_strings(ns, None, &mut out)?;
        Ok(Some(out))
    }

    /// Two-axis as-of read.
    pub fn entity_at(
        &mut self,
        ns: &str,
        subject: &str,
        relation: &str,
        t: i64,
        axis: Axis,
    ) -> Result<Option<DeserializedGrain>> {
        match axis {
            Axis::Knowledge => {
                // Walk the supersession chain backward from the head.
                let head = match self.latest(ns, subject, relation)? {
                    Some(g) => g.hash,
                    None => return Ok(None),
                };
                let mut cur = head;
                loop {
                    let rows = self.db.query(
                        "SELECT svf, supersedes, blob FROM grains WHERE hash = ?1",
                        vec![pb(cur.as_bytes().to_vec())],
                    )?;
                    let (svf, sup, blob) = match rows.first() {
                        Some(row) => (row.i64(0), row.blob(1), row.blob(2)),
                        None => return Ok(None),
                    };
                    if svf.unwrap_or(i64::MIN) <= t {
                        let mut g = deserialize_blob(&blob.unwrap_or_default())?;
                        self.egress_one(Some(ns), &mut g)?;
                        return Ok(Some(g));
                    }
                    match sup {
                        Some(prev) => cur = Hash::try_from_bytes(&prev)?,
                        None => return Ok(None),
                    }
                }
            }
            Axis::World => {
                // Current knowledge filtered by world validity at T.
                let (ns_id, s_id, p_id) = match (
                    self.term_lookup(ns)?,
                    self.term_lookup(subject)?,
                    self.term_lookup(relation)?,
                ) {
                    (Some(a), Some(b), Some(c)) => (a, b, c),
                    _ => return Ok(None),
                };
                let blob = self
                    .db
                    .query(
                        "SELECT g.blob FROM triples tr JOIN grains g ON g.seq = tr.seq
                         WHERE tr.ns=?1 AND tr.s=?2 AND tr.p=?3
                           AND g.svt IS NULL
                           AND (g.vf IS NULL OR g.vf <= ?4)
                           AND (g.vt IS NULL OR g.vt > ?4)
                         ORDER BY tr.seq DESC LIMIT 1",
                        vec![pi(ns_id), pi(s_id), pi(p_id), pi(t)],
                    )?
                    .first()
                    .and_then(|row| row.blob(0));
                match blob {
                    Some(b) => {
                        let mut g = deserialize_blob(&b)?;
                        self.egress_one(Some(ns), &mut g)?;
                        Ok(Some(g))
                    }
                    None => Ok(None),
                }
            }
        }
    }

    /// Whether a grain with this content address exists.
    pub fn has(&mut self, hash: &Hash) -> Result<bool> {
        self.has_grain(hash)
    }

    /// BM25 leg over grain text (facts as "s r o", event content). Returns
    /// live-grain seqs, best match first.
    ///
    /// This is our own inverted index rather than Turso's `USING fts`, and the
    /// reason is measurable: with that index in place, a single-row `INSERT`
    /// costs time proportional to the rows already stored — 1.6 ms at 500,
    /// 64 ms at 4,000, still climbing — and a `MATCH` lookup costs the same
    /// whether one row matches or every row does, which is what an index is
    /// supposed to avoid. Batching does not amortize it. Reproduced against
    /// bare `turso` with no Areev code involved, and identical on
    /// `0.8.0-pre.2`, so it is not something a version bump fixes:
    /// <https://github.com/tursodatabase/turso/issues/8170>.
    ///
    /// **When that issue is fixed**, this whole layer is deletable — see the
    /// removal note in `docs/facts/bm25-index.md`. It is deliberately kept to
    /// one table pair plus this function so that stays a small change.
    ///
    /// Postings are read per query term (`idx_fts_post` covers `(term, ns)`),
    /// scored in memory, and only the top candidates are checked for
    /// liveness — so cost tracks the number of documents containing the query
    /// terms, not the size of the file.
    pub fn search_text(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_text_inner(ns, query, k, false)
    }

    /// `search_text`, scoring the whole chain instead of the live heads.
    /// Superseded grains keep their postings (supersede only flips index
    /// state), so the historical text is still there to be found — the liveness
    /// filter is the only thing hiding it.
    pub fn search_text_all(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_text_inner(ns, query, k, true)
    }

    fn search_text_inner(
        &mut self,
        ns: &str,
        query: &str,
        k: usize,
        include_superseded: bool,
    ) -> Result<Vec<i64>> {
        if !self.index_text || k == 0 {
            return Ok(Vec::new()); // BM25 leg disabled (edge profile)
        }
        let ns_id = match self.term_lookup(ns)? {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let terms = tokenize(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // `N` counts every indexed document, including ones later superseded.
        // Their postings are still present and only filtered at the end, so
        // using the live count here would make idf disagree with the postings
        // actually being scored. The difference is immaterial to ranking.
        // Multi-writer backends supply live stats (the process-local counters
        // can't see other writers' documents); the embedded path stays on its
        // counters.
        let (docs_i, len_i) = match self.db.collection_stats()? {
            Some(s) => s,
            None => (self.fts_docs, self.fts_total_len),
        };
        let n_docs = docs_i.max(1) as f64;
        let avgdl = if docs_i > 0 { (len_i as f64 / docs_i as f64).max(1.0) } else { 1.0 };

        // One postings pull for ALL distinct tokens (document length comes
        // back with the posting). Scoring still iterates the query's tokens
        // per OCCURRENCE below, so a repeated token contributes twice exactly
        // as it did when each occurrence was its own query.
        let mut distinct: Vec<&String> = Vec::new();
        {
            let mut seen: HashSet<&str> = HashSet::new();
            for t in &terms {
                if seen.insert(t.as_str()) {
                    distinct.push(t);
                }
            }
        }
        let mut by_term: HashMap<String, Vec<(i64, i64, i64)>> = HashMap::new();
        if distinct.len() == 1 || !self.db.prefers_batched_reads() {
            // In-process: one cached indexed probe per distinct token.
            for t in &distinct {
                for row in &self.db.query_hot(
                    "SELECT v.term, p.seq, p.tf, d.len FROM fts_post p
                      JOIN fts_vocab v ON v.id = p.term
                      JOIN fts_doc d ON d.seq = p.seq
                     WHERE v.term = ?1 AND p.ns = ?2",
                    vec![pt(t), pi(ns_id)],
                )? {
                    if let (Some(t), Some(seq), Some(tf), Some(dl)) =
                        (row.text(0), row.i64(1), row.i64(2), row.i64(3))
                    {
                        by_term.entry(t.to_string()).or_default().push((seq, tf, dl.max(1)));
                    }
                }
            }
        } else {
            // Networked backend: all tokens in one round trip.
            let placeholders: Vec<String> =
                (2..distinct.len() + 2).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT v.term, p.seq, p.tf, d.len FROM fts_post p
                  JOIN fts_vocab v ON v.id = p.term
                  JOIN fts_doc d ON d.seq = p.seq
                 WHERE p.ns = ?1 AND v.term IN ({})",
                placeholders.join(",")
            );
            let mut params = vec![pi(ns_id)];
            params.extend(distinct.iter().map(|t| pt(t)));
            for row in &self.db.query(&sql, params)? {
                if let (Some(t), Some(seq), Some(tf), Some(dl)) =
                    (row.text(0), row.i64(1), row.i64(2), row.i64(3))
                {
                    by_term.entry(t.to_string()).or_default().push((seq, tf, dl.max(1)));
                }
            }
        }

        let mut scores: HashMap<i64, f64> = HashMap::new();
        for term in terms {
            let Some(postings) = by_term.get(&term) else {
                continue;
            };
            let df = postings.len() as f64;
            // Robertson/Sparck-Jones idf, +1 inside the log so a term present
            // in every document scores 0 rather than negative.
            let idf = (1.0 + (n_docs - df + 0.5) / (df + 0.5)).ln();
            for (seq, tf, dl) in postings {
                let tf = *tf as f64;
                let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * (*dl as f64 / avgdl));
                *scores.entry(*seq).or_insert(0.0) += idf * (tf * (BM25_K1 + 1.0)) / (tf + norm);
            }
        }
        if scores.is_empty() {
            return Ok(Vec::new());
        }

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        // Ties broken by seq descending, so equal-scoring matches come back
        // newest-first and the order is stable across runs.
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
            .then(b.0.cmp(&a.0)));
        if include_superseded {
            ranked.truncate(k);
            return Ok(ranked.into_iter().map(|(s, _)| s).collect());
        }
        // Over-fetch before the liveness filter: superseded grains keep their
        // postings, so trimming to k first could return fewer than k live hits
        // when a chain has been updated often.
        ranked.truncate(k.saturating_mul(4).max(k));
        let live = self.live_seqs(ranked.iter().map(|(s, _)| *s))?;
        Ok(ranked
            .into_iter()
            .filter(|(s, _)| live.contains(s))
            .map(|(s, _)| s)
            .take(k)
            .collect())
    }

    /// Which of `seqs` are still live (present and not superseded). One query
    /// for the whole candidate set — the ids come from our own tables, so
    /// inlining them carries no injection risk and avoids a bind per id.
    fn live_seqs(&mut self, seqs: impl Iterator<Item = i64>) -> Result<HashSet<i64>> {
        let list: Vec<String> = seqs.map(|s| s.to_string()).collect();
        if list.is_empty() {
            return Ok(HashSet::new());
        }
        let sql = format!(
            "SELECT seq FROM grains WHERE svt IS NULL AND seq IN ({})",
            list.join(",")
        );
        Ok(self
            .db
            .query(&sql, vec![])?
            .iter()
            .filter_map(|row| row.i64(0))
            .collect())
    }

    /// Which of `hashes` have been superseded, and by what.
    ///
    /// Supersession is index-layer state — the blob is immutable and carries no
    /// marker — so a caller holding recalled grains cannot tell a stale version
    /// from the head that replaced it. A recall widened to the whole chain
    /// (`RecallTuning::include_superseded`) needs that distinction to label what
    /// it returns: handing a model an outdated value that *looks* current is a
    /// worse answer than not returning the history at all.
    ///
    /// Indexed point reads on a cached statement, one per candidate. Only the
    /// deliberately-widened path calls this; heads-only recall never pays for it.
    pub fn supersession_map(&mut self, hashes: &[Hash]) -> Result<HashMap<Hash, Hash>> {
        let mut out = HashMap::new();
        if hashes.is_empty() {
            return Ok(out);
        }
        if !self.db.prefers_batched_reads() {
            // In-process: indexed point reads on the cached statement.
            for h in hashes {
                if let Some(sup) = self
                    .db
                    .query_hot(
                        "SELECT superseded_by FROM grains WHERE hash = ?1",
                        vec![pb(h.as_bytes().to_vec())],
                    )?
                    .first()
                    .and_then(|row| row.blob(0))
                    .and_then(|b| Hash::try_from_bytes(&b).ok())
                {
                    out.insert(*h, sup);
                }
            }
            return Ok(out);
        }
        // Networked backend: the whole candidate set in one round trip.
        let placeholders: Vec<String> = (1..hashes.len() + 1).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT hash, superseded_by FROM grains
              WHERE superseded_by IS NOT NULL AND hash IN ({})",
            placeholders.join(",")
        );
        let params: Vec<Value> = hashes.iter().map(|h| pb(h.as_bytes().to_vec())).collect();
        for row in &self.db.query(&sql, params)? {
            if let (Some(h), Some(sup)) = (
                row.blob(0).and_then(|b| Hash::try_from_bytes(&b).ok()),
                row.blob(1).and_then(|b| Hash::try_from_bytes(&b).ok()),
            ) {
                out.insert(h, sup);
            }
        }
        Ok(out)
    }

    /// Vector leg: cosine top-k over embedded grain text (brute force —
    /// exact search at per-memory scale, per M0 measurements).
    /// Semantic nearest-neighbours to `text` among current grains, optionally
    /// scoped to `(subject, relation)`, returned as `(hash, cosine_similarity)`
    /// most-similar first. This is the **advise** half of a write-time novelty
    /// gate: a reflection harness calls it before writing a distilled lesson
    /// and, if the top similarity clears its own threshold, *supersedes* the
    /// near-duplicate instead of adding a paraphrase — the paraphrase-rot the
    /// exact-value idempotent add (`add_if_novel`) can't catch. It never
    /// mutates: the host stays in control (advise, don't drop).
    ///
    /// Novelty is a vector operation, so this **requires an installed
    /// embedder** and errors loudly without one rather than silently returning
    /// nothing. `text` is embedded as-is and compared against each grain's
    /// stored embedding (subject·relation·object + content); scoping to
    /// `(subject, relation)` keeps the constant prefix out of the way so the
    /// object phrasing dominates the score.
    /// Reject a caller-supplied vector whose dimension disagrees with the
    /// file's declared embedding provenance. Vectors from two models are not
    /// comparable even at equal dimension — but unequal dimension is the case
    /// we can prove, so it fails loudly instead of scoring garbage.
    fn check_embedding_dim(&self, dim: usize) -> Result<()> {
        if dim == 0 {
            return Err(AreevError::Validation(
                "embedding vector must be non-empty".into(),
            ));
        }
        if let Some((model, declared)) = &self.meta_embed {
            if *declared != dim {
                return Err(AreevError::Validation(format!(
                    "embedding dimension mismatch: file declares {model}@{declared}, \
                     caller supplied a {dim}-dimensional vector"
                )));
            }
        }
        Ok(())
    }

    /// Vector-in nearest-neighbour search: cosine top-k against a
    /// **caller-supplied query vector**, no embedder required.
    ///
    /// The seam for hosts that own their embedding pipeline — a framework
    /// adapter (CrewAI hands its storage backend a pre-computed
    /// `query_embedding`) must search with *its* model's vector; re-embedding
    /// the query text with a different local model would compare vectors from
    /// two spaces and return silently wrong similarity. Dimension is checked
    /// against the file's declared embedding provenance
    /// ([`Self::declared_embedding`]); same `(subject, relation)` scoping and
    /// `(hash, cosine_similarity)` contract as [`Self::nearest_semantic`].
    pub fn nearest_vector(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<(Hash, f32)>> {
        self.check_embedding_dim(vector.len())?;
        let Some(ns_id) = self.term_lookup(ns)? else {
            return Ok(Vec::new());
        };
        let qjson = vec_to_json(vector);
        let s_id = match subject {
            Some(s) => match self.term_lookup(s)? {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let p_id = match relation {
            Some(r) => match self.term_lookup(r)? {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let base = "SELECT g.hash, vector_distance_cos(e.vec, vector32(?2)) AS dist \
                    FROM embeddings e JOIN grains g ON g.seq = e.seq \
                    WHERE g.ns = ?1 AND g.svt IS NULL";
        let rows = match (s_id, p_id) {
            (Some(s), Some(p)) => self.db.query(
                &format!("{base} AND g.s = ?3 AND g.p = ?4 ORDER BY dist LIMIT ?5"),
                vec![pi(ns_id), pt(&qjson), pi(s), pi(p), pi(k as i64)],
            )?,
            (Some(s), None) => self.db.query(
                &format!("{base} AND g.s = ?3 ORDER BY dist LIMIT ?4"),
                vec![pi(ns_id), pt(&qjson), pi(s), pi(k as i64)],
            )?,
            // Relation-only scoping used to fall through to the unscoped arm
            // — the filter was silently ignored and the search failed OPEN,
            // returning neighbours from every relation. Caught by asserting
            // what the scope EXCLUDES (vector_in_tests).
            (None, Some(p)) => self.db.query(
                &format!("{base} AND g.p = ?3 ORDER BY dist LIMIT ?4"),
                vec![pi(ns_id), pt(&qjson), pi(p), pi(k as i64)],
            )?,
            (None, None) => self.db.query(
                &format!("{base} ORDER BY dist LIMIT ?3"),
                vec![pi(ns_id), pt(&qjson), pi(k as i64)],
            )?,
        };
        let mut out = Vec::new();
        for row in rows {
            let h = row.blob(0).and_then(|b| Hash::try_from_bytes(&b).ok());
            let dist = row.f64(1).unwrap_or(1.0);
            if let Some(h) = h {
                out.push((h, (1.0 - dist) as f32));
            }
        }
        Ok(out)
    }

    pub fn nearest_semantic(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        text: &str,
        k: usize,
    ) -> Result<Vec<(Hash, f32)>> {
        if self.embedder.is_none() {
            // Surface-neutral on purpose. This used to name `--embed-cmd`, a
            // `areev` CLI flag that does not exist in Python or Node — and
            // `pip install areev` ships only the bindings, so a pip-only user
            // could not act on it at all. It also called this a "novelty
            // check", the CLI's `areev novelty` verb, when the caller had
            // reached it through `nearest()`. Each surface appends its own
            // remedy; this layer states only what is missing.
            return Err(AreevError::Validation(
                "semantic nearest-neighbour search requires an installed embedder; \
                 none is installed"
                    .into(),
            ));
        }
        let Some(ns_id) = self.term_lookup(ns)? else {
            return Ok(Vec::new());
        };
        let embedder = self.embedder.as_ref().expect("checked above");
        let qjson = vec_to_json(&embedder.embed(text)?);
        // A named subject/relation that was never interned can have no
        // neighbours — short-circuit rather than scan.
        let s_id = match subject {
            Some(s) => match self.term_lookup(s)? {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let p_id = match relation {
            Some(r) => match self.term_lookup(r)? {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let base = "SELECT g.hash, vector_distance_cos(e.vec, vector32(?2)) AS dist \
                    FROM embeddings e JOIN grains g ON g.seq = e.seq \
                    WHERE g.ns = ?1 AND g.svt IS NULL";
        let rows = match (s_id, p_id) {
            (Some(s), Some(p)) => self.db.query(
                &format!("{base} AND g.s = ?3 AND g.p = ?4 ORDER BY dist LIMIT ?5"),
                vec![pi(ns_id), pt(&qjson), pi(s), pi(p), pi(k as i64)],
            )?,
            (Some(s), None) => self.db.query(
                &format!("{base} AND g.s = ?3 ORDER BY dist LIMIT ?4"),
                vec![pi(ns_id), pt(&qjson), pi(s), pi(k as i64)],
            )?,
            // Relation-only scoping used to fall through to the unscoped arm
            // — the filter was silently ignored and the search failed OPEN,
            // returning neighbours from every relation. Caught by asserting
            // what the scope EXCLUDES (vector_in_tests).
            (None, Some(p)) => self.db.query(
                &format!("{base} AND g.p = ?3 ORDER BY dist LIMIT ?4"),
                vec![pi(ns_id), pt(&qjson), pi(p), pi(k as i64)],
            )?,
            (None, None) => self.db.query(
                &format!("{base} ORDER BY dist LIMIT ?3"),
                vec![pi(ns_id), pt(&qjson), pi(k as i64)],
            )?,
        };
        let mut out = Vec::new();
        for row in rows {
            let h = row.blob(0).and_then(|b| Hash::try_from_bytes(&b).ok());
            // vector_distance_cos is cosine *distance* (1 − similarity).
            let dist = row.f64(1).unwrap_or(1.0);
            if let Some(h) = h {
                out.push((h, (1.0 - dist) as f32));
            }
        }
        Ok(out)
    }

    pub fn search_vector(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_vector_inner(ns, query, k, false)
    }

    /// `search_vector` over the whole chain rather than the live heads —
    /// the vector half of `RecallTuning::include_superseded`.
    pub fn search_vector_all(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_vector_inner(ns, query, k, true)
    }

    fn search_vector_inner(
        &mut self,
        ns: &str,
        query: &str,
        k: usize,
        include_superseded: bool,
    ) -> Result<Vec<i64>> {
        let Some(ns_id) = self.term_lookup(ns)? else {
            return Ok(Vec::new());
        };
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        let qv = embedder.embed(query)?;
        let qjson = vec_to_json(&qv);
        Ok(self
            .db
            .query(
                if include_superseded {
                    "SELECT e.seq FROM embeddings e JOIN grains g ON g.seq = e.seq
                     WHERE g.ns = ?1
                     ORDER BY vector_distance_cos(e.vec, vector32(?2)) LIMIT ?3"
                } else {
                    "SELECT e.seq FROM embeddings e JOIN grains g ON g.seq = e.seq
                     WHERE g.ns = ?1 AND g.svt IS NULL
                     ORDER BY vector_distance_cos(e.vec, vector32(?2)) LIMIT ?3"
                },
                vec![pi(ns_id), pt(&qjson), pi(k as i64)],
            )?
            .iter()
            .filter_map(|row| row.i64(0))
            .collect())
    }

    /// Hybrid recall: structural leg + BM25 leg fused
    /// with Reciprocal Rank Fusion; optional deadline makes it fail-open
    /// (returns whatever is gathered when the budget expires). This is the
    /// plain path — see [`recall_hybrid_tuned`](Self::recall_hybrid_tuned) for
    /// the Tier-1/Tier-2 refinements (query expansion, MMR, rerank).
    pub fn recall_hybrid(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        query: Option<&str>,
        k: usize,
        deadline: Option<std::time::Duration>,
    ) -> Result<Vec<DeserializedGrain>> {
        self.recall_hybrid_tuned(ns, subject, relation, query, k, deadline, RecallTuning::default())
    }

    /// Hybrid recall with post-fusion refinements. Same three
    /// legs and RRF fusion as [`recall_hybrid`](Self::recall_hybrid), plus the
    /// opt-in `tuning` stages:
    ///
    /// - **query expansion** (Tier-1): extra BM25 legs from rule-based query
    ///   variants, RRF-fused — bridges vocabulary gaps with no embedder.
    /// - **rerank** (Tier-2): a cross-encoder re-scores a widened candidate
    ///   pool via the installed [`RerankBackend`]. Takes precedence over MMR.
    /// - **diversity** (Tier-1): MMR reorders the pool to cut near-duplicates,
    ///   using the query embedding + stored candidate vectors.
    ///
    /// Every stage is fail-open: past the deadline, or with its backend/data
    /// absent, it degrades to plain fusion order rather than erroring. All
    /// default off, so this is a strict superset of `recall_hybrid`.
    #[allow(clippy::too_many_arguments)] // tuning knobs are intentionally explicit params
    pub fn recall_hybrid_tuned(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        query: Option<&str>,
        k: usize,
        deadline: Option<std::time::Duration>,
        tuning: RecallTuning,
    ) -> Result<Vec<DeserializedGrain>> {
        let start = std::time::Instant::now();
        let over = |start: &std::time::Instant| match deadline {
            Some(d) => start.elapsed() >= d,
            None => false,
        };

        // A refinement stage reranks/reorders a candidate pool, so fetch a
        // wider net per leg when one is active.
        let refine = tuning.rerank || tuning.diversity_lambda.is_some();
        let leg_k = if refine {
            k.max(REFINE_POOL)
        } else {
            k.saturating_mul(2)
        };

        // leg 1: structural (the voice hot path — always runs first)
        let structural: Vec<i64> = match subject {
            Some(s) => self.recall_seqs(ns, s, relation, leg_k, tuning.include_superseded)?,
            None => Vec::new(),
        };
        // leg 2: BM25 — plus Tier-1 query-expansion variant legs. Skipped when
        // the deadline is already spent.
        let mut fts_legs: Vec<Vec<i64>> = Vec::new();
        if let Some(q) = query {
            if !over(&start) {
                // Fail-open: a BM25 leg that errors (e.g. the raw user query
                // trips the FTS query-grammar on `:` / quotes / parens — common
                // in DIDs, namespaces, timestamps) degrades to an empty leg,
                // exactly like a deadline-skipped one. recall_hybrid must never
                // error — the structural/vector legs still answer.
                fts_legs.push(
                    self.search_text_inner(ns, q, leg_k, tuning.include_superseded)
                        .unwrap_or_default(),
                );
                if tuning.query_expansion && self.index_text {
                    for variant in self.expand_query(q) {
                        if over(&start) {
                            break;
                        }
                        let hits = self
                            .search_text_inner(ns, &variant, leg_k, tuning.include_superseded)
                            .unwrap_or_default();
                        if !hits.is_empty() {
                            fts_legs.push(hits);
                        }
                    }
                }
            }
        }
        // leg 3: vector (multilingual path — CJK text that whitespace
        // tokenization can't serve rides this leg)
        let vecs: Vec<i64> = match query {
            Some(q) if self.embedder.is_some() && !over(&start) => {
                // Fail-open: e.g. a query embedded at a different dim than the
                // file's stored vectors errors inside vector_distance_cos (the
                // store permits a mismatched embedder with only a warning) —
                // degrade the vector leg rather than failing the whole recall.
                self.search_vector_inner(ns, q, leg_k, tuning.include_superseded)
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        if structural.is_empty() && fts_legs.iter().all(|l| l.is_empty()) && vecs.is_empty() {
            // Record the miss too: an empty-result query is the coverage-gap
            // signal, not a no-op.
            self.record_recall_event(ns, subject, relation, query, &[], start);
            return Ok(Vec::new());
        }

        // RRF fusion (k0 = 60, the standard constant) across every leg.
        let mut scores: HashMap<i64, f64> = HashMap::new();
        for (rank, seq) in structural.iter().enumerate() {
            *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
        }
        for leg in &fts_legs {
            for (rank, seq) in leg.iter().enumerate() {
                *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
            }
        }
        for (rank, seq) in vecs.iter().enumerate() {
            *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
        }
        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.0.cmp(&a.0))
        });

        // Refinement stage: rerank wins over diversity when both are asked for.
        let ordered: Vec<i64> = if let Some(q) =
            query.filter(|_| tuning.rerank && self.reranker.is_some() && !over(&start))
        {
            self.rerank_pool(q, &ranked, k)?
        } else if let (Some(lambda), Some(q)) = (tuning.diversity_lambda, query) {
            if self.embedder.is_some() && !over(&start) {
                self.mmr_pool(q, &ranked, lambda, k)?
            } else {
                ranked.iter().take(k).map(|(s, _)| *s).collect()
            }
        } else {
            ranked.iter().take(k).map(|(s, _)| *s).collect()
        };

        let mut out = Vec::new();
        if self.db.prefers_batched_reads() && !ordered.is_empty() && !over(&start) {
            // Networked backend: batched blob pulls in CHUNKS with a
            // deadline check between them, so the deadline bounds the fetch
            // phase within one chunk's round trip — the per-seq loop below
            // bounds it within one blob, and an unchunked pull would not
            // bound it at all on a stalled link.
            for chunk in ordered.chunks(16) {
                if over(&start) {
                    break;
                }
                let blobs = self.blobs_by_seqs(chunk)?;
                for seq in chunk {
                    if over(&start) {
                        break;
                    }
                    if let Some(b) = blobs.get(seq) {
                        out.push(deserialize_blob(b)?);
                    }
                }
            }
        } else {
            for seq in ordered {
                if over(&start) {
                    break; // fail-open: partial results beat a blown budget
                }
                if let Some(b) = self.blob_by_seq(seq)? {
                    out.push(deserialize_blob(&b)?);
                }
            }
        }

        // Telemetry: capture the recall (buffered, non-blocking). See
        // `record_recall_event` — the only recall-path work is an in-memory
        // push, and `Off` telemetry makes it a single branch. Recorded before
        // the egress pass on purpose: telemetry keys on hashes and counts,
        // which the transform preserves.
        self.record_recall_event(ns, subject, relation, query, &out, start);
        self.anon.egress_grains(Some(ns), &mut out)?;
        Ok(out)
    }

    /// Buffer one recall into the telemetry sidecar. **Non-blocking**: no
    /// SQLite I/O runs here, so this stays inside the recall latency budget;
    /// the buffer drains off-path (writes / close / explicit flush). When the
    /// host left telemetry `Off`, `self.telemetry` is `None` and this is a
    /// single branch that allocates nothing. Called on BOTH recall exit paths
    /// (including the empty-result early return — an empty query is the
    /// coverage-gap signal, so it must be recorded, not dropped).
    #[inline]
    fn record_recall_event(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        query: Option<&str>,
        out: &[DeserializedGrain],
        start: std::time::Instant,
    ) {
        let run_id = self.ambient_run_id.clone();
        if let Some(tel) = self.telemetry.as_mut() {
            tel.record(RecallEvent {
                ts_ms: now_ms(),
                run_id,
                ns: ns.to_string(),
                subject: subject.map(str::to_string),
                relation: relation.map(str::to_string),
                query: query.map(str::to_string),
                n_results: out.len(),
                latency_us: start.elapsed().as_micros() as i64,
                hashes: out.iter().map(|g| g.hash).collect(),
            });
        }
    }

    /// Query variants for Tier-1 expansion: the installed [`QueryExpander`],
    /// or the built-in [`EnglishExpander`] when none is set.
    fn expand_query(&self, q: &str) -> Vec<String> {
        match &self.expander {
            Some(e) => e.expand(q),
            None => EnglishExpander::default().expand(q),
        }
    }

    /// Text used to rerank a candidate — the same [`projected_text`] shape
    /// the FTS/embed legs index, derived from the grain so it works even
    /// when `index_text` is off.
    fn candidate_text(&mut self, seq: i64) -> Result<String> {
        let Some(b) = self.blob_by_seq(seq)? else {
            return Ok(String::new());
        };
        let g = deserialize_blob(&b)?;
        Ok(projected_text(&g).unwrap_or_default())
    }

    /// Tier-2: cross-encoder rerank a widened candidate pool. Fetches the
    /// top-N fused candidates' text, scores each `(query, doc)` pair via the
    /// installed reranker, and returns the top-`k` seqs by score. Fail-open —
    /// a backend error or a length mismatch falls back to fusion order.
    fn rerank_pool(&mut self, query: &str, ranked: &[(i64, f64)], k: usize) -> Result<Vec<i64>> {
        let pool_n = ranked.len().min(k.max(REFINE_POOL));
        let pool: Vec<i64> = ranked.iter().take(pool_n).map(|(s, _)| *s).collect();
        if pool.is_empty() {
            return Ok(pool);
        }
        let mut docs: Vec<String> = Vec::with_capacity(pool.len());
        for &seq in &pool {
            docs.push(self.candidate_text(seq)?);
        }
        let refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
        let reranker = self.reranker.as_ref().expect("caller checked reranker present");
        match reranker.rerank(query, &refs) {
            Ok(scores) if scores.len() == refs.len() => {
                let mut scored: Vec<(i64, f32)> = pool.iter().copied().zip(scores).collect();
                // stable-ish: higher score first, then lower seq for ties
                scored.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.0.cmp(&b.0))
                });
                Ok(scored.into_iter().take(k).map(|(s, _)| s).collect())
            }
            // Backend failed or returned the wrong shape: keep fusion order.
            _ => Ok(pool.into_iter().take(k).collect()),
        }
    }

    /// Tier-1: MMR diversity reorder. Greedy Maximal Marginal Relevance over
    /// the embedded candidates in the fused pool — `lambda·rel − (1−lambda)·
    /// max_sim_to_selected`, where `rel` is cosine-to-query and `sim` is
    /// candidate-to-candidate cosine (both via `vector_distance_cos`).
    /// Candidates lacking vectors keep fusion order after the MMR set.
    fn mmr_pool(
        &mut self,
        query: &str,
        ranked: &[(i64, f64)],
        lambda: f32,
        k: usize,
    ) -> Result<Vec<i64>> {
        let lambda = lambda.clamp(0.0, 1.0);
        let pool_n = ranked.len().min(k.max(REFINE_POOL));
        let pool: Vec<i64> = ranked.iter().take(pool_n).map(|(s, _)| *s).collect();
        if pool.len() < 2 {
            return Ok(pool);
        }
        let qv = match &self.embedder {
            Some(e) => e.embed(query)?,
            None => return Ok(pool.into_iter().take(k).collect()),
        };
        let qjson = vec_to_json(&qv);
        let rel = self.vec_rel_map(&pool, &qjson)?;
        // MMR is only meaningful with ≥2 embedded candidates.
        let embedded: Vec<i64> = pool.iter().copied().filter(|s| rel.contains_key(s)).collect();
        if embedded.len() < 2 {
            return Ok(pool.into_iter().take(k).collect());
        }
        let sim = self.vec_pairwise_map(&embedded)?;
        let sim_of = |a: i64, b: i64| -> f32 {
            if a == b {
                1.0
            } else {
                let key = if a < b { (a, b) } else { (b, a) };
                *sim.get(&key).unwrap_or(&0.0)
            }
        };

        let target = k.min(embedded.len());
        let mut selected: Vec<i64> = Vec::with_capacity(target);
        let mut remaining: Vec<i64> = embedded.clone();
        while selected.len() < target && !remaining.is_empty() {
            let mut best_idx = 0usize;
            let mut best_score = f32::MIN;
            for (i, &c) in remaining.iter().enumerate() {
                let relevance = *rel.get(&c).unwrap_or(&0.0);
                let max_sim = selected
                    .iter()
                    .map(|&s| sim_of(c, s))
                    .fold(0.0f32, f32::max);
                let mmr = lambda * relevance - (1.0 - lambda) * max_sim;
                if mmr > best_score {
                    best_score = mmr;
                    best_idx = i;
                }
            }
            selected.push(remaining.remove(best_idx));
        }
        // Fill remaining slots with non-embedded candidates in fusion order.
        for s in pool {
            if selected.len() >= k {
                break;
            }
            if !selected.contains(&s) {
                selected.push(s);
            }
        }
        selected.truncate(k);
        Ok(selected)
    }

    /// Cosine relevance (1 − distance) of each embedded candidate to the query
    /// vector. Candidates without stored vectors are simply absent from the map.
    fn vec_rel_map(&mut self, seqs: &[i64], qjson: &str) -> Result<HashMap<i64, f32>> {
        if seqs.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT seq, vector_distance_cos(vec, vector32(?1)) FROM embeddings WHERE seq IN ({})",
            seq_csv(seqs)
        );
        let mut out = HashMap::new();
        for row in self.db.query(&sql, vec![pt(qjson)])? {
            if let (Some(s), Some(d)) = (row.i64(0), row.f64(1)) {
                out.insert(s, 1.0 - d as f32);
            }
        }
        Ok(out)
    }

    /// Pairwise cosine similarity (1 − distance) among embedded candidates,
    /// keyed `(a, b)` with `a < b`. One upper-triangle self-join query.
    fn vec_pairwise_map(&mut self, seqs: &[i64]) -> Result<HashMap<(i64, i64), f32>> {
        if seqs.len() < 2 {
            return Ok(HashMap::new());
        }
        let csv = seq_csv(seqs);
        let sql = format!(
            "SELECT a.seq, b.seq, vector_distance_cos(a.vec, b.vec) \
             FROM embeddings a JOIN embeddings b ON a.seq < b.seq \
             WHERE a.seq IN ({csv}) AND b.seq IN ({csv})"
        );
        let mut out = HashMap::new();
        for row in self.db.query(&sql, vec![])? {
            if let (Some(a), Some(b), Some(d)) = (row.i64(0), row.i64(1), row.f64(2)) {
                out.insert((a, b), 1.0 - d as f32);
            }
        }
        Ok(out)
    }

    /// Structural leg. `all_versions` drops the `cur=1` predicate so the whole
    /// supersession chain comes back instead of the heads — the widened scan
    /// behind `RecallTuning::include_superseded`. It gets its own pair of cached
    /// statements so the heads-only hot path keeps its prepared plans.
    fn recall_seqs(
        &mut self,
        ns: &str,
        subject: &str,
        relation: Option<&str>,
        k: usize,
        all_versions: bool,
    ) -> Result<Vec<i64>> {
        let (ns_id, s_id) = match (self.term_lookup(ns)?, self.term_lookup(subject)?) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let p_id = match relation {
            Some(r) => match self.term_lookup(r)? {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        // Each probe variant is its own SQL literal, so each keeps its own
        // prepared-statement cache entry — the heads-only hot path never loses
        // its plan to the widened scan.
        let rows = match (p_id, all_versions) {
            (Some(p), true) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND p=?3 ORDER BY seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(s_id), pi(p), pi(k as i64)],
            )?,
            (Some(p), false) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 ORDER BY seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(s_id), pi(p), pi(k as i64)],
            )?,
            (None, true) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 ORDER BY seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(s_id), pi(k as i64)],
            )?,
            (None, false) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND cur=1 ORDER BY seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(s_id), pi(k as i64)],
            )?,
        };
        Ok(rows.iter().filter_map(|row| row.i64(0)).collect())
    }

    fn blob_by_seq(&mut self, seq: i64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_hot("SELECT blob FROM grains WHERE seq = ?1", vec![pi(seq)])?
            .first()
            .and_then(|row| row.blob(0)))
    }

    /// Batched blob fetch for a candidate set — one round trip via an inline
    /// id list (engine-internal seq ids, same rationale as `live_seqs`).
    /// Only for backends that `prefers_batched_reads`: on the embedded engine
    /// a parameterized IN over the PK is a table scan (measured ~8x on the
    /// voice frame path), so the in-process path keeps its point-read loop.
    fn blobs_by_seqs(&mut self, seqs: &[i64]) -> Result<HashMap<i64, Vec<u8>>> {
        let mut out = HashMap::new();
        if seqs.is_empty() {
            return Ok(out);
        }
        let sql = format!("SELECT seq, blob FROM grains WHERE seq IN ({})", seq_csv(seqs));
        for row in &self.db.query(&sql, vec![])? {
            if let (Some(s), Some(b)) = (row.i64(0), row.blob(1)) {
                out.insert(s, b);
            }
        }
        Ok(out)
    }

    /// Distinct subjects holding `relation` in `ns` (POS-index scan).
    /// Backs directory-style listings (memory-tool `view` on a dir).
    pub fn subjects_with_relation(&mut self, ns: &str, relation: &str) -> Result<Vec<String>> {
        let (ns_id, p_id) = match (self.term_lookup(ns)?, self.term_lookup(relation)?) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let ids: Vec<i64> = self
            .db
            .query(
                "SELECT DISTINCT s FROM triples WHERE ns=?1 AND p=?2 AND cur=1",
                vec![pi(ns_id), pi(p_id)],
            )?
            .iter()
            .filter_map(|row| row.i64(0))
            .collect();
        let mut subjects: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(t) = self.term_str(id)? {
                subjects.push(t);
            }
        }
        subjects.sort();
        // Egress boundary: subjects ARE identities by construction.
        self.anon.egress_strings(ns, Some("person"), &mut subjects)?;
        Ok(subjects)
    }

    /// Store raw content as an Event grain and return its hash. The first half
    /// of `remember()`, split out so a caller whose extraction can *fail* (an
    /// LLM call over the network) lands the raw text first and still has it
    /// after a failed or garbage extraction. Losing the source text to a flaky
    /// model call is the worst failure mode available to a memory engine.
    ///
    /// This is the one place raw remembered text is written, shared by
    /// `areev remember`, both bindings, the MCP `areev_remember` tool, and
    /// `capture-stop` — so the same input produces the same grain on every
    /// surface.
    pub fn capture(&mut self, ns: &str, content: &str, meta: &Capture<'_>) -> Result<Hash> {
        use areev_core::types::{Event, Role};
        // Ingress boundary (proposal §4.2): this is the one place raw
        // remembered text is written, so the transform happens BEFORE the
        // Event is serialized — the content address commits to the
        // transformed text (D8). One branch when no ingress policy declares.
        let ingressed = self.anon.ingress_text(ns, content)?;
        let content: &str = ingressed.as_deref().unwrap_or(content);
        let mut e = Event::new(content);
        e.common.namespace = Some(ns.to_string());
        e.session_id = meta.session_id.map(str::to_string);
        e.role = meta.role.and_then(Role::from_str);
        // Empty stays a silent skip (long-standing lenience on this path);
        // a *malformed* id is refused under the shared run_id contract —
        // this is one of the three write surfaces (JSON build, manifest,
        // capture) and they must agree on what a run_id is.
        e.run_id = match meta.run_id.filter(|r| !r.is_empty()) {
            Some(r) => {
                areev_core::types::validate_run_id(r)?;
                Some(r.to_string())
            }
            None => None,
        };
        // Event has no observer field (it models a transcript turn, where
        // `role` is the author). Who *captured* the turn is still worth
        // keeping, so it rides in extra_fields, which round-trips through the
        // blob.
        if let Some(observer) = meta.observer.filter(|o| !o.is_empty()) {
            e.common
                .extra_fields
                .insert("observer".to_string(), serde_json::json!(observer));
        }
        self.add(&e)
    }

    /// Store each draft as a Fact carrying `derived_from` provenance back to
    /// `source`, plus whatever `attr` supplies. The second half of
    /// `remember()`; call it after an out-of-band extraction (see
    /// [`Areev::capture`]).
    pub fn attach_facts(
        &mut self,
        ns: &str,
        source: &Hash,
        drafts: &[FactDraft],
        attr: &FactAttribution<'_>,
    ) -> Result<Vec<Hash>> {
        let source_hex = source.to_hex();
        let mut facts = Vec::with_capacity(drafts.len());
        for draft in drafts {
            // Ingress boundary: host-supplied drafts carry raw identities and
            // values; subjects are `person` by construction, objects go
            // through detection (proposal §4.2).
            let subject = match self.anon.ingress_identity(ns, &draft.subject)? {
                Some(t) => t,
                None => draft.subject.clone(),
            };
            let object = match self.anon.ingress_text(ns, &draft.object)? {
                Some(t) => t,
                None => draft.object.clone(),
            };
            let mut fact = areev_core::types::Fact::new(&subject, &draft.relation, &object);
            fact.common.confidence = draft.confidence.clamp(0.0, 1.0);
            fact.common.namespace = Some(ns.to_string());
            fact.common.derived_from = Some(source_hex.clone());
            fact.common.source_type = Some("derived".to_string());
            if let Some(status) = attr.verification_status {
                fact.common.verification_status = Some(status.to_string());
            }
            if let Some(model) = attr.extractor_model {
                fact.common
                    .extra_fields
                    .insert("extractor_model".to_string(), serde_json::json!(model));
            }
            facts.push(self.add(&fact)?);
        }
        Ok(facts)
    }

    /// The `remember()` seam: store raw content as an
    /// Event grain, run the caller-supplied extraction function
    /// (typically an LLM callback — the host owns the model relationship),
    /// and store each returned draft as a Fact with `derived_from`
    /// provenance back to that Event.
    ///
    /// This is the in-process shape, where the extractor cannot fail. A caller
    /// with a *fallible* extractor composes [`Areev::capture`] and
    /// [`Areev::attach_facts`] instead — what the CLI and the bindings do on
    /// their `--model` / `--llm-cmd` path.
    #[allow(clippy::type_complexity)] // extractor is a plain callback; a type alias would not clarify
    pub fn remember(
        &mut self,
        ns: &str,
        content: &str,
        observer: &str,
        extractor: Option<&dyn Fn(&str) -> Vec<FactDraft>>,
    ) -> Result<RememberResult> {
        // Ingress boundary: transform BEFORE the extractor sees the content,
        // so derived Fact drafts inherit the transform instead of leaking
        // around it (proposal §4.2). `capture` re-checks and finds nothing
        // new — already-tokenized text is settled.
        let ingressed = self.anon.ingress_text(ns, content)?;
        let content: &str = ingressed.as_deref().unwrap_or(content);
        let event = self.capture(ns, content, &Capture { observer: Some(observer), ..Default::default() })?;
        let drafts = extractor.map(|f| f(content)).unwrap_or_default();
        let facts = self.attach_facts(ns, &event, &drafts, &FactAttribution::default())?;
        Ok(RememberResult { event, facts })
    }

    /// Total number of grains in the hot store.
    pub fn count(&mut self) -> Result<usize> {
        Ok(self
            .db
            .query("SELECT COUNT(*) FROM grains", vec![])?
            .first()
            .and_then(|row| row.i64(0))
            .unwrap_or(0) as usize)
    }

    /// Open supersession tips for (subject, relation) — normally one; more
    /// than one means a fork (v4 grain-git model). Ordered provisional-first.
    /// Enumerate every open fork in the file — each `(ns, subject, relation)`
    /// whose `heads` table holds more than one live tip. This is the honest
    /// structural conflict signal: a true fork only arises from concurrent
    /// supersession of the same value (typically edits synced from two
    /// writers). Recall never surfaces this to stay off the hot path; operators
    /// call `areev forks` to find and merge them. Not a hot path (scans the
    /// heads table + reverse term lookups).
    pub fn open_forks(&mut self) -> Result<Vec<ForkGroup>> {
        let groups: Vec<(i64, i64, i64)> = self
            .db
            .query(
                "SELECT ns, s, p FROM heads GROUP BY ns, s, p HAVING COUNT(*) > 1",
                vec![],
            )?
            .iter()
            .filter_map(|row| match (row.i64(0), row.i64(1), row.i64(2)) {
                (Some(ns), Some(s), Some(p)) => Some((ns, s, p)),
                _ => None,
            })
            .collect();

        let mut forks = Vec::new();
        for (ns_id, s_id, p_id) in groups {
            let (Some(namespace), Some(subject), Some(relation)) =
                (self.term_str(ns_id)?, self.term_str(s_id)?, self.term_str(p_id)?)
            else {
                continue;
            };
            let heads = self
                .heads(&namespace, &subject, &relation)?
                .into_iter()
                .map(|(h, _)| h)
                .collect();
            forks.push(ForkGroup {
                namespace,
                subject,
                relation,
                heads,
            });
        }
        // Egress boundary: a fork's subject IS an identity by construction.
        if !self.anon.is_idle() {
            for f in forks.iter_mut() {
                let mut s = vec![std::mem::take(&mut f.subject)];
                let ns = f.namespace.clone();
                self.anon.egress_strings(&ns, Some("person"), &mut s)?;
                f.subject = s.pop().unwrap_or_default();
            }
        }
        Ok(forks)
    }

    pub fn heads(&mut self, ns: &str, subject: &str, relation: &str) -> Result<Vec<(Hash, i64)>> {
        let (Some(ns_id), Some(s_id), Some(p_id)) = (
            self.term_lookup(ns)?,
            self.term_lookup(subject)?,
            self.term_lookup(relation)?,
        ) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for row in self.db.query(
            "SELECT hash, created_at FROM heads WHERE ns=?1 AND s=?2 AND p=?3
             ORDER BY created_at DESC, hash DESC",
            vec![pi(ns_id), pi(s_id), pi(p_id)],
        )? {
            let h = row.blob(0).unwrap_or_default();
            let c = row.i64(1).unwrap_or(0);
            if let Ok(h) = Hash::try_from_bytes(&h) {
                out.push((h, c));
            }
        }
        Ok(out)
    }

    /// Close a fork: write `merged` superseding EVERY open tip, with all
    /// parents recorded in the provenance chain (git merge commit).
    pub fn merge_heads<G: Grain + 'static>(
        &mut self,
        ns: &str,
        subject: &str,
        relation: &str,
        merged: &mut G,
    ) -> Result<Hash> {
        let tips = self.heads(ns, subject, relation)?;
        if tips.len() < 2 {
            return Err(AreevError::Validation(format!(
                "merge_heads needs an open fork; {} head(s) present",
                tips.len()
            )));
        }
        // parents: provisional head as derived_from; ALL tips recorded in
        // context.merge_parents (context is serialized into the .mg blob;
        // provenance_chain is index-layer in this port)
        merged.common_mut().derived_from = Some(tips[0].0.to_hex());
        let parents: Vec<String> = tips.iter().map(|(h, _)| h.to_hex()).collect();
        let mut ctx = match merged.common().context.clone() {
            Some(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        ctx.insert("merge_parents".into(), serde_json::json!(parents));
        merged.common_mut().context = Some(serde_json::Value::Object(ctx));
        // Insert the merge grain AND close every tip in ONE transaction. A merge
        // that committed the add first and then closed tips in separate
        // (autocommit) statements could crash mid-loop, leaving heads={merge}
        // while some parent tips stayed cur=1 (heads/recall disagreement).
        let merged_dyn: &dyn AddableDyn = &*merged;
        let (preps, first_seq, first_op, hlc0) = self.prep_and_reserve(&[merged_dyn])?;
        let merge_hash = preps[0].hash;
        let now = now_ms();
        // Reserve the fork-closure OP_SUPERSEDE slot (written inside the txn so it
        // replicates; the merge grain's context.merge_parents lets import close
        // all tips — see superseded_parents + the import OP_SUPERSEDE branch).
        let ram_op = self.next_op;
        self.next_op += 1;
        let ram_hlc = self.next_hlc();
        let (d_docs, d_len) = fts_delta(&preps);
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            let (s0, o0, h0, op_seq, hlc) = match dbr.reserve_write(1, 2, 2, now_ms() << 16, 0)? {
                Some(w) => (w.seq0, w.op0, w.hlc0, w.op0 + 1, w.hlc0 + 1),
                None => (first_seq, first_op, hlc0, ram_op, ram_hlc),
            };
            insert_prepped(dbr, &preps, s0, o0, h0)?; // OP_ADD; collapses heads to {merge}
            for (tip, _) in &tips {
                let seq = dbr
                    .query(
                        "SELECT seq, svt FROM grains WHERE hash=?1",
                        vec![pb(tip.as_bytes().to_vec())],
                    )?
                    .first()
                    .and_then(|row| {
                        let seq = row.i64(0).unwrap_or(0);
                        row.i64(1).is_none().then_some(seq)
                    });
                if let Some(seq) = seq {
                    dbr.execute(
                        "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                        vec![pb(merge_hash.as_bytes().to_vec()), pi(now), pi(seq)],
                    )?;
                    dbr.execute("UPDATE triples SET cur=0 WHERE seq=?1", vec![pi(seq)])?;
                    dbr.execute("UPDATE osp SET cur=0 WHERE seq=?1", vec![pi(seq)])?;
                }
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc), pi(OP_SUPERSEDE), pb(merge_hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })?;
        self.fts_docs += d_docs;
        self.fts_total_len += d_len;
        Ok(merge_hash)
    }

    /// Supersession-chain history for (namespace, subject, relation),
    /// newest first — the HISTORY statement's backing read (§5.13).
    pub fn history(&mut self, ns: &str, subject: &str, relation: &str) -> Result<Vec<HistoryEntry>> {
        let head = match self.latest(ns, subject, relation)? {
            Some(g) => g.hash,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        let mut cur = Some(head);
        while let Some(h) = cur {
            let rows = self.db.query(
                "SELECT blob, superseded_by, supersedes FROM grains WHERE hash = ?1",
                vec![pb(h.as_bytes().to_vec())],
            )?;
            let (blob, sup_by, supersedes) = match rows.first() {
                Some(row) => (row.blob(0), row.blob(1), row.blob(2)),
                None => break,
            };
            if let Some(b) = blob {
                let g = deserialize_blob(&b)?;
                out.push(HistoryEntry {
                    hash: h,
                    object: g.get_str("object").unwrap_or_default().to_string(),
                    created_at: g.get_i64("created_at").unwrap_or(0),
                    confidence: g.get_f64("confidence").unwrap_or(0.0),
                    superseded_by: sup_by.and_then(|b| Hash::try_from_bytes(&b).ok()),
                });
            }
            cur = supersedes.and_then(|b| Hash::try_from_bytes(&b).ok());
            if out.len() > 512 {
                break; // chain-length safety cap
            }
        }
        // Egress boundary on the value projections (`object` carries grain
        // content even though no full grain is returned).
        if !self.anon.is_idle() {
            let mut objects: Vec<String> = out.iter().map(|e| e.object.clone()).collect();
            self.anon.egress_strings(ns, None, &mut objects)?;
            for (e, o) in out.iter_mut().zip(objects) {
                e.object = o;
            }
        }
        Ok(out)
    }

    /// Verify store integrity: Turso's own integrity check plus a full
    /// content-address re-verification (every blob re-hashed and compared
    /// to its stored hash — the tamper-evidence read).
    ///
    /// Scope: this detects *modification* of readable grains, not *removal*.
    /// WAL corruption makes the engine roll back to the last consistent
    /// state, and a truncated-but-consistent store verifies `ok`; detecting
    /// that requires an external anchor (stream segments, bundles, a
    /// replica). See docs/security-model.md "Known limitations".
    pub fn verify(&mut self) -> Result<VerifyReport> {
        // Collect every integrity line; Turso's experimental FTS keeps
        // internal dir indexes that integrity_check miscounts — classify
        // those as benign notes (candidate upstream report), never as
        // corruption. Content-address verification below is the real
        // tamper-evidence check and is unaffected.
        let mut real: Vec<String> = Vec::new();
        let mut fts_notes: Vec<String> = Vec::new();
        for row in self.db.query("PRAGMA integrity_check", vec![])? {
            if let Some(s) = row.text(0) {
                if s == "ok" {
                    continue;
                } else if s.contains("__turso_internal_fts") {
                    fts_notes.push(s.to_string());
                } else {
                    real.push(s.to_string());
                }
            }
        }
        let integrity = if real.is_empty() { "ok".to_string() } else { real.join("; ") };
        let rows: Vec<(Vec<u8>, Vec<u8>)> = self
            .db
            .query("SELECT hash, blob FROM grains", vec![])?
            .iter()
            .map(|row| (row.blob(0).unwrap_or_default(), row.blob(1).unwrap_or_default()))
            .collect();
        let mut report = VerifyReport {
            integrity,
            fts_notes,
            grains: rows.len(),
            hash_mismatches: 0,
            undecodable: 0,
        };
        for (stored, blob) in rows {
            match deserialize_blob(&blob) {
                Ok(g) => {
                    if g.hash.as_bytes().as_slice() != stored.as_slice() {
                        report.hash_mismatches += 1;
                    }
                }
                Err(_) => report.undecodable += 1,
            }
        }
        Ok(report)
    }

    /// Record a governed corpus export as an immutable Observation in the
    /// reserved harness namespace. Source linkage is expressed exclusively as
    /// reverse-indexable `related_to` edges; identities never enter the grain,
    /// only their stable fingerprints.
    pub fn record_corpus_export(
        &mut self,
        selector: &str,
        destination: &str,
        recipient: Option<&str>,
        exported_at_ms: i64,
        subject_fingerprints: &[String],
        source_hashes: &[String],
    ) -> Result<Hash> {
        use sha2::{Digest, Sha256};

        let mut fingerprints = subject_fingerprints.to_vec();
        fingerprints.sort();
        fingerprints.dedup();
        let mut sources = source_hashes.to_vec();
        sources.sort();
        sources.dedup();
        let mut id_material = selector.as_bytes().to_vec();
        id_material.extend_from_slice(destination.as_bytes());
        if let Some(recipient) = recipient {
            id_material.extend_from_slice(recipient.as_bytes());
        }
        id_material.extend_from_slice(&exported_at_ms.to_le_bytes());
        for hash in &sources {
            id_material.extend_from_slice(hash.as_bytes());
        }
        let export_id = format!("export:{}", hex::encode(&Sha256::digest(id_material)[..12]));
        let selector_digest = hex::encode(Sha256::digest(selector.as_bytes()));
        let mut observation = Observation::new("areev:corpus", "corpus_export")
            .subject(&export_id)
            .object("openai_chat_jsonl")
            .created_at(exported_at_ms);
        observation.common.namespace = Some(authz::HARNESS_NS.to_string());
        observation.common.context = Some(serde_json::json!({
            "corpus_export": true,
            // Raw CAL routinely contains the subject being exported. Keeping
            // it in an immutable replicating manifest would re-introduce that
            // identity after erasure; the digest proves the exact selector
            // without retaining its text.
            "selector_sha256": selector_digest,
            "selector_kind": "cal_read",
            "destination": destination,
            "recipient": recipient,
            "exported_at_ms": exported_at_ms,
            "subject_fingerprints": fingerprints,
            "source_count": sources.len(),
            "registry_status": "active",
        }));
        observation.common.related_to = sources
            .iter()
            .map(|hash| RelatedTo {
                hash: hash.clone(),
                relation_type: "mg:corpus_source".to_string(),
                weight: None,
            })
            .collect();
        self.add(&observation)
    }

    /// Reconstruct the export registry from grains. This deliberately reads no
    /// `meta` rows: registry entries must replicate in ordinary bundles.
    /// EXEMPT from the egress boundary: the export registry is an operator
    /// compliance read (destinations, recipients, subject *fingerprints* —
    /// already non-identifying by construction).
    pub fn corpus_exports(&mut self) -> Result<Vec<CorpusExportRegistry>> {
        let mut exports = Vec::new();
        let rows = self.db.query(
            "SELECT g.blob FROM corpus_idx ci JOIN grains g ON g.seq=ci.seq ORDER BY g.seq DESC",
            vec![],
        )?;
        for blob in rows.iter().filter_map(|row| row.blob(0)) {
            let grain = deserialize_blob(&blob)?;
            let Some(context) = grain.fields.get("context").and_then(|v| v.as_object()) else {
                continue;
            };
            if context.get("corpus_export").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            let export_id = grain.get_str("subject").unwrap_or_default().to_string();
            let destination = context
                .get("destination")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let recipient = context
                .get("recipient")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let exported_at_ms = context
                .get("exported_at_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or_else(|| grain.get_i64("created_at").unwrap_or(0));
            let mut subject_fingerprints: Vec<String> = context
                .get("subject_fingerprints")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            subject_fingerprints.sort();
            subject_fingerprints.dedup();
            let mut source_hashes: Vec<String> = grain
                .fields
                .get("related_to")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter(|link| {
                    link.get("relation_type").and_then(|v| v.as_str())
                        == Some("mg:corpus_source")
                })
                .filter_map(|link| link.get("hash").and_then(|v| v.as_str()).map(str::to_string))
                .collect();
            source_hashes.sort();
            source_hashes.dedup();
            exports.push(CorpusExportRegistry {
                manifest_hash: grain.hash.to_hex(),
                export_id,
                destination,
                recipient,
                exported_at_ms,
                subject_fingerprints,
                source_hashes,
            });
        }
        exports.sort_by(|a, b| {
            b.exported_at_ms
                .cmp(&a.exported_at_ms)
                .then_with(|| a.manifest_hash.cmp(&b.manifest_hash))
        });
        Ok(exports)
    }

    /// Registry entries that contain a subject fingerprint.
    pub fn corpus_exports_touching_subject(
        &mut self,
        subject: &str,
    ) -> Result<Vec<CorpusExportRegistry>> {
        let fingerprint = authz::subject_fingerprint(subject);
        Ok(self
            .corpus_exports()?
            .into_iter()
            .filter(|entry| entry.subject_fingerprints.contains(&fingerprint))
            .collect())
    }

    /// Registry entries derived from at least one of the supplied hashes.
    pub fn corpus_exports_touching_hashes(
        &mut self,
        hashes: &[String],
    ) -> Result<Vec<CorpusExportRegistry>> {
        Ok(exports_touching_hashes(&self.corpus_exports()?, hashes))
    }

    /// Store statistics (CLI `stats`).
    pub fn stats(&mut self) -> Result<StoreStats> {
        let one = |sql: &'static str| -> Result<i64> {
            Ok(self
                .db
                .query(sql, vec![])?
                .first()
                .and_then(|row| row.i64(0))
                .unwrap_or(0))
        };
        Ok(StoreStats {
            grains: one("SELECT COUNT(*) FROM grains")? as usize,
            current: one("SELECT COUNT(*) FROM grains WHERE svt IS NULL")? as usize,
            triples: one("SELECT COUNT(*) FROM triples")? as usize,
            terms: one("SELECT COUNT(*) FROM terms")? as usize,
            ops: one("SELECT COUNT(*) FROM oplog")? as usize,
            events_indexed: one("SELECT COUNT(*) FROM thread_idx")? as usize,
        })
    }

    /// Op-log cursor read — the change feed (backs sync + UIs).
    pub fn changes_since(&mut self, after_op_seq: i64, limit: usize) -> Result<Vec<OpRecord>> {
        let mut out = Vec::new();
        for row in self.db.query(
            "SELECT op_seq, hlc, op, hash FROM oplog WHERE op_seq > ?1 ORDER BY op_seq LIMIT ?2",
            vec![pi(after_op_seq), pi(limit as i64)],
        )? {
            out.push(OpRecord {
                op_seq: row.i64(0).unwrap_or(0),
                hlc: row.i64(1).unwrap_or(0),
                op: row.i64(2).unwrap_or(0),
                hash: Hash::try_from_bytes(&row.blob(3).unwrap_or_default())?,
            });
        }
        Ok(out)
    }
}

impl Areev {
    // ----- CAS blob store (`.blobs` fan-out dir / in-schema table) -----

    /// Store bytes in the per-memory CAS; returns the `cas://sha256:` URI.
    /// Idempotent — content addressing dedupes by construction.
    pub fn put_blob(&mut self, bytes: &[u8]) -> Result<String> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        let hex = hex::encode(digest);
        // Sealed under the sidecar key when this memory is encrypted; the
        // ADDRESS stays the plaintext digest so it is stable across
        // encrypted and plaintext stores (see `blobcrypt`).
        let stored = blobcrypt::write_bytes(self.blob_key.as_deref(), &hex, bytes)?;
        match &self.blob_store {
            BlobStore::Fs(dir) => {
                let path = fs_blob_path(dir, &hex);
                if !path.exists() {
                    std::fs::create_dir_all(path.parent().unwrap()).map_err(db_err)?;
                    let tmp = path.with_extension("tmp");
                    std::fs::write(&tmp, &stored).map_err(db_err)?;
                    std::fs::rename(&tmp, &path).map_err(db_err)?;
                }
            }
            BlobStore::Table => {
                self.db.execute(
                    "INSERT OR IGNORE INTO blobs(hash, body) VALUES (?1, ?2)",
                    vec![pb(digest.to_vec()), pb(stored)],
                )?;
            }
        }
        Ok(format!("cas://sha256:{hex}"))
    }

    /// Fetch bytes by `cas://sha256:` URI, verifying the hash on read.
    pub fn get_blob(&mut self, uri: &str) -> Result<Vec<u8>> {
        use sha2::{Digest, Sha256};
        let hex = uri
            .strip_prefix("cas://sha256:")
            .ok_or_else(|| AreevError::Validation(format!("not a cas uri: {uri}")))?;
        // Validate before anything byte-slices `hex[..2]` — an untrusted or
        // truncated URI (e.g. from an imported content_ref) must return Err,
        // never panic on a short or non-char-boundary slice.
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AreevError::Validation(format!("malformed cas uri: {uri}")));
        }
        let bytes = match &self.blob_store {
            BlobStore::Fs(dir) => std::fs::read(fs_blob_path(dir, hex))
                .map_err(|_| AreevError::Storage(format!("blob missing: {uri}")))?,
            BlobStore::Table => {
                let raw = hex::decode(hex).map_err(db_err)?;
                self.db
                    .query("SELECT body FROM blobs WHERE hash = ?1", vec![pb(raw)])?
                    .first()
                    .and_then(|row| row.blob(0))
                    .ok_or_else(|| AreevError::Storage(format!("blob missing: {uri}")))?
            }
        };
        // Decrypt first (a plaintext blob from an older build passes
        // through), then verify the content address over the CLEAR bytes —
        // so both the AEAD tag and the address independently catch tampering.
        let bytes = blobcrypt::read_maybe_sealed(self.blob_key.as_deref(), hex, bytes)?;
        if hex::encode(Sha256::digest(&bytes)) != hex {
            return Err(AreevError::Storage(format!("blob corrupt: {uri}")));
        }
        Ok(bytes)
    }

    /// Encrypt any `.blobs` attachments still stored in the clear, under
    /// this memory's derived sidecar key. Returns `(encrypted, already)`.
    ///
    /// Attachments written before blob encryption landed stay readable
    /// (the read path passes plaintext through), so migration is a separate,
    /// resumable step rather than a breaking open — but a file that has not
    /// run it still has cleartext media on disk beside an encrypted
    /// database, which is the Art. 32 finding this closes.
    ///
    /// Idempotent: sealed blobs are skipped, so re-running is free. Requires
    /// an encrypted memory — on a plaintext one there is no key to seal
    /// under, and silently doing nothing would read as success.
    pub fn encrypt_blobs(&mut self) -> Result<(usize, usize)> {
        let Some(key) = self.blob_key.as_deref().copied() else {
            return Err(AreevError::Validation(
                "encrypt_blobs needs an encrypted memory — open it with its key \
                 (open_encrypted / --passphrase-env) first"
                    .into(),
            ));
        };
        let mut encrypted = 0usize;
        let mut already = 0usize;
        match &self.blob_store {
            BlobStore::Fs(dir) => {
                let dir = dir.clone();
                let Ok(shards) = std::fs::read_dir(&dir) else { return Ok((0, 0)) };
                for shard in shards.flatten() {
                    let prefix = shard.file_name().to_string_lossy().to_string();
                    let Ok(files) = std::fs::read_dir(shard.path()) else { continue };
                    for f in files.flatten() {
                        let path = f.path();
                        let rest = f.file_name().to_string_lossy().to_string();
                        let hex = format!("{prefix}{rest}");
                        // Skip the tmp files put_blob renames from, and
                        // anything that isn't a content address.
                        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                            continue;
                        }
                        let raw = std::fs::read(&path).map_err(db_err)?;
                        if blobcrypt::is_sealed(&raw) {
                            already += 1;
                            continue;
                        }
                        let sealed = blobcrypt::seal(&key, &hex, &raw)?;
                        // tmp + rename: a crash mid-migration leaves the
                        // original readable, never a truncated blob.
                        let tmp = path.with_extension("tmp");
                        std::fs::write(&tmp, &sealed).map_err(db_err)?;
                        std::fs::rename(&tmp, &path).map_err(db_err)?;
                        encrypted += 1;
                    }
                }
            }
            BlobStore::Table => {
                // Postgres keeps blobs in-schema, where the database's own
                // encryption (TDE/pgcrypto) covers them — see docs/gdpr.md §5.
                return Err(AreevError::Validation(
                    "blob encryption is file-backend-only; on postgres the blobs table is \
                     covered by the database's own encryption at rest"
                        .into(),
                ));
            }
        }
        Ok((encrypted, already))
    }

    /// Remove CAS blobs not referenced by any live grain's `content_refs`.
    /// Returns the number of blobs removed. Full-store maintenance — invoke
    /// it while writers are quiescent (a blob uploaded but not yet
    /// referenced by its grain's commit looks unreferenced to the scan).
    pub fn gc_blobs(&mut self) -> Result<usize> {
        // Collect referenced hashes from live grains.
        let blobs: Vec<Vec<u8>> = self
            .db
            .query("SELECT blob FROM grains", vec![])?
            .iter()
            .filter_map(|row| row.blob(0))
            .collect();
        let mut referenced: HashSet<String> = HashSet::new();
        for b in &blobs {
            if let Ok(view) = deserialize_blob(b) {
                referenced.extend(cas_hexes(&view));
            }
        }
        let mut removed = 0usize;
        match &self.blob_store {
            BlobStore::Fs(dir) => {
                if let Ok(shards) = std::fs::read_dir(dir) {
                    for shard in shards.flatten() {
                        let prefix = shard.file_name().to_string_lossy().to_string();
                        if let Ok(files) = std::fs::read_dir(shard.path()) {
                            for f in files.flatten() {
                                let rest = f.file_name().to_string_lossy().to_string();
                                let hex = format!("{prefix}{rest}");
                                if !referenced.contains(&hex)
                                    && std::fs::remove_file(f.path()).is_ok()
                                {
                                    removed += 1;
                                }
                            }
                        }
                    }
                }
            }
            BlobStore::Table => {
                let stored: Vec<Vec<u8>> = self
                    .db
                    .query("SELECT hash FROM blobs", vec![])?
                    .iter()
                    .filter_map(|row| row.blob(0))
                    .collect();
                for raw in stored {
                    if !referenced.contains(&hex::encode(&raw)) {
                        self.db.execute("DELETE FROM blobs WHERE hash = ?1", vec![pb(raw)])?;
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }

    // ----- bundle: git-shaped incremental backup / fast-forward sync (§5.10) -----

    /// The registry meta rows a bundle carries (full key → export value).
    /// Usage stats (`last_run_at`) are stripped from `qry:`/`tpl:` values —
    /// definitions replicate, run counters would ping-pong between replicas.
    fn replicable_meta_rows(&self) -> Result<Vec<(String, String)>> {
        let mut rows = Vec::new();
        for prefix in REPLICABLE_META_PREFIXES {
            for (suffix, value) in self.meta_scan(prefix)? {
                let export_value = match serde_json::from_str::<serde_json::Value>(&value) {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
                            obj.remove("last_run_at");
                        }
                        v.to_string()
                    }
                    // An unparseable row still travels verbatim: the reader's
                    // unloadable-row contract (skip + warn) owns that case.
                    Err(_) => value,
                };
                rows.push((format!("{prefix}{suffix}"), export_value));
            }
        }
        rows.sort();
        Ok(rows)
    }

    /// Export all ops after `after_op_seq` to a bundle file.
    /// Record: op(u8) · hlc(i64 LE) · hash(32) · blob_len(u32 LE) · blob.
    /// Blobs of later-forgotten grains export as len 0 — the importer
    /// relies on the subsequent tombstone for net-equivalence.
    ///
    /// When the file carries registry meta rows (saved queries, templates,
    /// retention policies), the bundle is v2: magic `MGB2`, then
    /// `meta_len(u32 LE) · meta_json`, then the op records. The registry is
    /// part of what makes the file self-describing, so a backup or sync that
    /// silently dropped it would restore a lesser file.
    pub fn bundle_since(&mut self, after_op_seq: i64, path: &str) -> Result<BundleStats> {
        let ops = self.changes_since(after_op_seq, usize::MAX / 2)?;
        let meta_rows = self.replicable_meta_rows()?;
        let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
        if meta_rows.is_empty() {
            out.extend_from_slice(BUNDLE_MAGIC);
        } else {
            out.extend_from_slice(BUNDLE_MAGIC_V2);
            let meta_json = serde_json::to_string(
                &meta_rows
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect::<std::collections::BTreeMap<_, _>>(),
            )
            .map_err(|e| AreevError::Format(format!("bundle meta segment: {e}")))?;
            out.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
            out.extend_from_slice(meta_json.as_bytes());
        }
        let mut last = after_op_seq;
        for rec in &ops {
            let blob: Option<Vec<u8>> = if rec.op == OP_FORGET {
                None
            } else {
                self.db
                    .query_hot(
                        "SELECT blob FROM grains WHERE hash = ?1",
                        vec![pb(rec.hash.as_bytes().to_vec())],
                    )?
                    .first()
                    .and_then(|row| row.blob(0))
            };
            out.push(rec.op as u8);
            out.extend_from_slice(&rec.hlc.to_le_bytes());
            out.extend_from_slice(rec.hash.as_bytes());
            let b = blob.unwrap_or_default();
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(&b);
            last = rec.op_seq;
        }
        std::fs::write(path, &out).map_err(db_err)?;
        Ok(BundleStats {
            ops: ops.len(),
            bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            last_op_seq: last,
        })
    }

    fn blob_by_hash(&mut self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_hot(
                "SELECT blob FROM grains WHERE hash = ?1",
                vec![pb(hash.as_bytes().to_vec())],
            )?
            .first()
            .and_then(|row| row.blob(0)))
    }

    fn has_grain(&mut self, hash: &Hash) -> Result<bool> {
        Ok(!self
            .db
            .query_hot(
                "SELECT 1 FROM grains WHERE hash = ?1",
                vec![pb(hash.as_bytes().to_vec())],
            )?
            .is_empty())
    }

    /// Insert one already-serialized grain (bundle import path).
    fn insert_blob(&mut self, blob: Vec<u8>, hash: Hash, op: i64, hlc_in: i64) -> Result<()> {
        let pr = self.prep_from_blob(blob, hash)?;
        let ram_seq = self.next_seq;
        self.next_seq += 1;
        let ram_op = self.next_op;
        self.next_op += 1;
        self.hlc_last = self.hlc_last.max(hlc_in);
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            // The imported hlc is preserved on the op-log row; the
            // reservation raises the clock floor so local allocations stay
            // ahead of imported history.
            let (seq, op_seq) = match dbr.reserve_write(1, 1, 0, now_ms() << 16, hlc_in)? {
                Some(w) => (w.seq0, w.op0),
                None => (ram_seq, ram_op),
            };
            dbr.execute(
                "INSERT INTO grains(seq,hash,ns,gtype,created_at,s,p,o,vf,vt,svf,svt,superseded_by,supersedes,text,blob)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,NULL,?12,?13)",
                vec![
                    pi(seq),
                    pb(pr.hash.as_bytes().to_vec()),
                    pi(pr.ns_id),
                    pi(pr.gtype),
                    pi(pr.created),
                    opt_i(pr.s),
                    opt_i(pr.p),
                    opt_i(pr.o),
                    opt_i(pr.vf),
                    opt_i(pr.vt),
                    pi(pr.created),
                    match &pr.text { Some(t) => pt(t), None => Value::Null },
                    pb(pr.blob.clone()),
                ],
            )?;
            if let (Some(s), Some(p), Some(o)) = (pr.s, pr.p, pr.o) {
                dbr.execute(
                    "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq)],
                )?;
                if pr.osp {
                    dbr.execute(
                        "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                        vec![pi(pr.ns_id), pi(o), pi(s), pi(p), pi(seq)],
                    )?;
                }
                // import path: UNION into heads (never collapse other
                // tips — that's the local single-writer semantic only)
                dbr.execute(
                    "INSERT OR REPLACE INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                    vec![pi(pr.ns_id), pi(s), pi(p), pi(seq), pb(pr.hash.as_bytes().to_vec()), pi(pr.created)],
                )?;
                // provisional election for entity_latest: replace only if
                // (created_at, hash) beats the current head — deterministic
                // on every node, no coordination.
                let cur = dbr
                    .query(
                        "SELECT h.created_at, h.hash FROM heads h JOIN entity_latest e
                         ON e.ns=h.ns AND e.s=h.s AND e.p=h.p AND e.seq=h.seq
                         WHERE e.ns=?1 AND e.s=?2 AND e.p=?3",
                        vec![pi(pr.ns_id), pi(s), pi(p)],
                    )?
                    .first()
                    .map(|row| (row.i64(0).unwrap_or(0), row.blob(1).unwrap_or_default()));
                let wins = match &cur {
                    Some((c, h)) => (pr.created, pr.hash.as_bytes().as_slice()) > (*c, h.as_slice()),
                    None => true,
                };
                if wins {
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq), pb(pr.hash.as_bytes().to_vec())],
                    )?;
                }
            }
            // Cross-grain `related_to` links — same treatment as the local
            // write path: triples + osp for retrieval, never heads or
            // entity_latest (OMS §15.3).
            for (ls, lp, lo) in &pr.links {
                dbr.execute(
                    "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(*ls), pi(*lp), pi(*lo), pi(seq)],
                )?;
                dbr.execute(
                    "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(*lo), pi(*ls), pi(*lp), pi(seq)],
                )?;
            }
            if let Some(sess) = pr.session {
                dbr.execute(
                    "INSERT INTO thread_idx(ns,session,seq) VALUES (?1,?2,?3)",
                    vec![pi(pr.ns_id), pi(sess), pi(seq)],
                )?;
            }
            if let Some(run) = pr.run {
                dbr.execute(
                    "INSERT INTO run_idx(ns,run,seq) VALUES (?1,?2,?3)",
                    vec![pi(pr.ns_id), pi(run), pi(seq)],
                )?;
            }
            if pr.corpus_export {
                dbr.execute("INSERT INTO corpus_idx(seq) VALUES (?1)", vec![pi(seq)])?;
            }
            if let Some(ref parent) = pr.parent {
                dbr.execute(
                    "INSERT INTO prov_idx(ns,parent,seq) VALUES (?1,?2,?3)",
                    vec![pi(pr.ns_id), pb(parent.clone()), pi(seq)],
                )?;
            }
            if let Some(ref emb) = pr.embedding {
                dbr.execute(
                    "INSERT INTO embeddings(seq, vec) VALUES (?1, vector32(?2))",
                    vec![pi(seq), pt(&vec_to_json(emb))],
                )?;
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc_in), pi(op), pb(pr.hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })
    }

    /// Apply the index-layer supersession flip old → new (import path).
    /// Returns whether anything changed (false = idempotent no-op).
    /// One transaction: a crash mid-flip must not leave the fork model
    /// half-registered (this ran statement-by-statement in autocommit before
    /// the Db seam).
    fn apply_supersede_flip(&mut self, old: &Hash, new_hash: &Hash) -> Result<bool> {
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            // Serialize with concurrent writers (no ids consumed): the flip's
            // read-then-write election must not interleave with another
            // writer's on the same key.
            dbr.reserve_write(0, 0, 0, now_ms() << 16, 0)?;
            let rows = dbr.query(
                &format!(
                    "SELECT seq, svt, ns, s, p FROM grains WHERE hash = ?1{}",
                    dbr.for_update()
                ),
                vec![pb(old.as_bytes().to_vec())],
            )?;
            let (old_seq, old_svt, old_ns, old_s, old_p) = match rows.first() {
                Some(row) => (
                    row.i64(0).unwrap_or(0),
                    row.i64(1),
                    row.i64(2).unwrap_or(0),
                    row.i64(3),
                    row.i64(4),
                ),
                None => return Ok(false), // partial history — fast-forward tolerates
            };
            if old_svt.is_some() {
                // v4 grain-git: old head already superseded. Same superseder →
                // idempotent replay. Different superseder → a FORK: both tips
                // stay alive as heads; entity_latest gets the provisional head
                // (created_at, then hash — deterministic on every node).
                let existing = dbr
                    .query(
                        "SELECT superseded_by FROM grains WHERE seq=?1",
                        vec![pi(old_seq)],
                    )?
                    .first()
                    .and_then(|row| row.blob(0));
                if existing.as_deref() == Some(new_hash.as_bytes().as_slice()) {
                    return Ok(false); // same supersede — idempotent
                }
                // incoming tip row
                let inc = dbr
                    .query(
                        "SELECT seq, ns, s, p, o, created_at FROM grains WHERE hash=?1",
                        vec![pb(new_hash.as_bytes().to_vec())],
                    )?
                    .first()
                    .map(|row| {
                        (
                            row.i64(0).unwrap_or(0),
                            row.i64(1).unwrap_or(0),
                            row.i64(2).unwrap_or(0),
                            row.i64(3).unwrap_or(0),
                            row.i64(4).unwrap_or(0),
                            row.i64(5).unwrap_or(0),
                        )
                    });
                let Some((inc_seq, ns, s, p, o, inc_created)) = inc else { return Ok(false) };
                dbr.execute(
                    "INSERT OR REPLACE INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                    vec![pi(ns), pi(s), pi(p), pi(inc_seq), pb(new_hash.as_bytes().to_vec()), pi(inc_created)],
                )?;
                // provisional election vs current entity_latest head
                let cur = dbr
                    .query(
                        "SELECT h.created_at, h.hash FROM heads h JOIN entity_latest e
                         ON e.ns=h.ns AND e.s=h.s AND e.p=h.p AND e.seq=h.seq
                         WHERE e.ns=?1 AND e.s=?2 AND e.p=?3",
                        vec![pi(ns), pi(s), pi(p)],
                    )?
                    .first()
                    .map(|row| (row.i64(0).unwrap_or(0), row.blob(1).unwrap_or_default()));
                let incoming_wins = match &cur {
                    Some((c_created, c_hash)) => {
                        (inc_created, new_hash.as_bytes().as_slice()) > (*c_created, c_hash.as_slice())
                    }
                    None => true,
                };
                if incoming_wins {
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(ns), pi(s), pi(p), pi(o), pi(inc_seq), pb(new_hash.as_bytes().to_vec())],
                    )?;
                }
                return Ok(true); // fork registered
            }
            let now = now_ms();
            dbr.execute(
                "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                vec![pb(new_hash.as_bytes().to_vec()), pi(now), pi(old_seq)],
            )?;
            dbr.execute(
                "UPDATE grains SET supersedes=?1 WHERE hash=?2",
                vec![pb(old.as_bytes().to_vec()), pb(new_hash.as_bytes().to_vec())],
            )?;
            dbr.execute("UPDATE triples SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            dbr.execute("UPDATE osp SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            if let (Some(s), Some(p)) = (old_s, old_p) {
                dbr.execute(
                    "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(old_ns), pi(s), pi(p), pi(old_seq)],
                )?;
            }
            Ok(true)
        })
    }

    /// Import a bundle (idempotent; fast-forward replay in op order).
    pub fn import_bundle(&mut self, path: &str) -> Result<ImportStats> {
        self.import_bundle_until(path, None)
    }

    /// Import, applying only ops with `hlc <= max_hlc` when set — the
    /// point-in-time restore primitive (§5.10b): replay history to T.
    ///
    /// A v2 bundle's registry meta segment is applied only on a **full**
    /// import (`max_hlc` = None): meta rows carry no HLC, so a point-in-time
    /// restore cannot place them on the timeline and skips them rather than
    /// resurrecting definitions from after the cutoff.
    pub fn import_bundle_until(&mut self, path: &str, max_hlc: Option<i64>) -> Result<ImportStats> {
        let data = std::fs::read(path).map_err(db_err)?;
        if data.len() < 4 {
            return Err(AreevError::Format("not a MGB1/MGB2 bundle".into()));
        }
        let mut stats = ImportStats::default();
        let mut meta_entries: Vec<(String, String)> = Vec::new();
        let mut i = 4usize;
        match &data[..4] {
            m if m == BUNDLE_MAGIC => {}
            m if m == BUNDLE_MAGIC_V2 => {
                if data.len() < 8 {
                    return Err(AreevError::Format("truncated bundle meta segment".into()));
                }
                let meta_len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
                i = 8;
                if i.checked_add(meta_len).is_none_or(|end| end > data.len()) {
                    return Err(AreevError::Format("truncated bundle meta segment".into()));
                }
                let meta_json: std::collections::BTreeMap<String, String> =
                    serde_json::from_slice(&data[i..i + meta_len]).map_err(|e| {
                        AreevError::Format(format!("malformed bundle meta segment: {e}"))
                    })?;
                meta_entries = meta_json.into_iter().collect();
                i += meta_len;
            }
            _ => return Err(AreevError::Format("not a MGB1/MGB2 bundle".into())),
        }
        while i < data.len() {
            if i + 1 + 8 + 32 + 4 > data.len() {
                return Err(AreevError::Format("truncated bundle record".into()));
            }
            let op = data[i] as i64;
            i += 1;
            let hlc = i64::from_le_bytes(data[i..i + 8].try_into().unwrap());
            i += 8;
            let hash = Hash::try_from_bytes(&data[i..i + 32])?;
            i += 32;
            let len = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            if i.checked_add(len).is_none_or(|end| end > data.len()) {
                return Err(AreevError::Format("truncated bundle blob".into()));
            }
            let blob = data[i..i + len].to_vec();
            i += len;

            if let Some(t) = max_hlc {
                if hlc > t {
                    stats.skipped += 1;
                    continue; // beyond the requested point in time
                }
            }
            match op {
                OP_ADD => {
                    if self.has_grain(&hash)? || blob.is_empty() {
                        // exists already, or pruned (forgotten later at source)
                        stats.skipped += 1;
                        continue;
                    }
                    self.insert_blob(blob, hash, op, hlc)?;
                    stats.applied += 1;
                }
                OP_SUPERSEDE => {
                    // supersede() double-logs (OP_ADD for the new grain, then
                    // OP_SUPERSEDE); the grain may thus already exist here —
                    // the flip must still be applied idempotently.
                    let exists = self.has_grain(&hash)?;
                    let bytes: Option<Vec<u8>> = if !blob.is_empty() {
                        Some(blob)
                    } else if exists {
                        self.blob_by_hash(&hash)?
                    } else {
                        None
                    };
                    match bytes {
                        None => stats.skipped += 1,
                        Some(bb) => {
                            let mut changed = false;
                            let inserted = !exists;
                            if !exists {
                                // insert_blob logs its own OP_SUPERSEDE row.
                                self.insert_blob(bb.clone(), hash, op, hlc)?;
                                changed = true;
                            }
                            if let Ok(view) = deserialize_blob(&bb) {
                                // Close EVERY tip this grain supersedes: the
                                // linear derived_from parent and, for a merge
                                // commit, all merge_parents — else the other
                                // parents stay open and the replica forks.
                                for old in superseded_parents(&view) {
                                    changed |= self.apply_supersede_flip(&old, &hash)?;
                                }
                            }
                            // If the grain already existed (its OP_ADD twin was
                            // imported first) insert_blob didn't run, so the flip
                            // is missing from our op-log — record it here so a
                            // re-export (the B->C hop) carries the supersession
                            // instead of shipping two bare adds that fork.
                            // Idempotent replays return changed=false above.
                            if changed && !inserted {
                                self.log_op(OP_SUPERSEDE, &hash, hlc)?;
                            }
                            if changed {
                                stats.applied += 1;
                            } else {
                                stats.skipped += 1;
                            }
                        }
                    }
                }
                OP_FORGET => match self.forget(&hash) {
                    Ok(()) => stats.applied += 1,
                    Err(AreevError::NotFound(_)) => stats.skipped += 1,
                    Err(e) => return Err(e),
                },
                _ => return Err(AreevError::Format(format!("unknown bundle op {op}"))),
            }
        }
        if max_hlc.is_none() {
            let mut anon_changed = false;
            for (key, value) in &meta_entries {
                if self.apply_bundle_meta_row(key, value)? {
                    stats.meta_applied += 1;
                    anon_changed |= key.starts_with(anon_gate::ANON_PREFIX);
                } else {
                    stats.meta_skipped += 1;
                }
            }
            // A replicated anon policy takes effect on this handle now, not
            // at the next open — the gate cache mirrors the meta rows.
            if anon_changed {
                self.reload_anon_policies()?;
            }
        } else {
            stats.meta_skipped += meta_entries.len();
        }
        Ok(stats)
    }

    /// Merge one registry meta row from a bundle. Returns true when the
    /// local row changed.
    ///
    /// - A key outside the replicable allowlist never applies — a crafted
    ///   bundle must not overwrite `text_index` or `min_reader_version`.
    /// - `qry:`/`tpl:` rows converge latest-wins on `updated_at` (absent =
    ///   0; equal keeps local, which is what makes replays idempotent), and
    ///   the local `last_run_at` — usage, not definition — survives an
    ///   incoming update.
    /// - Retention rows apply only when locally absent: a restore fills the
    ///   gap, a sync never silently swaps a live policy.
    fn apply_bundle_meta_row(&mut self, key: &str, incoming: &str) -> Result<bool> {
        if !REPLICABLE_META_PREFIXES.iter().any(|p| key.starts_with(p)) {
            return Ok(false);
        }
        let local = self.meta_get(key)?;
        let registry_row = key.starts_with("qry:") || key.starts_with("tpl:");
        match local {
            None => {
                self.meta_put(key, incoming)?;
                Ok(true)
            }
            Some(_) if !registry_row => Ok(false),
            Some(local_raw) => {
                let updated_at = |raw: &str| {
                    serde_json::from_str::<serde_json::Value>(raw)
                        .ok()
                        .and_then(|v| v.get("updated_at").and_then(serde_json::Value::as_u64))
                        .unwrap_or(0)
                };
                let mut incoming_json = match serde_json::from_str::<serde_json::Value>(incoming) {
                    Ok(v) => v,
                    // An unparseable incoming row never replaces a present
                    // local one; the exporter only ships it verbatim for the
                    // locally-absent case.
                    Err(_) => return Ok(false),
                };
                if updated_at(incoming) <= updated_at(&local_raw) {
                    return Ok(false);
                }
                let local_last_run = serde_json::from_str::<serde_json::Value>(&local_raw)
                    .ok()
                    .and_then(|v| v.get("last_run_at").cloned())
                    .filter(|v| !v.is_null());
                if let (Some(obj), Some(lr)) = (incoming_json.as_object_mut(), local_last_run) {
                    obj.insert("last_run_at".into(), lr);
                }
                self.meta_put(key, &incoming_json.to_string())?;
                Ok(true)
            }
        }
    }
}

impl Drop for Areev {
    fn drop(&mut self) {
        // Persist any buffered recall telemetry on close. Best-effort: the
        // sidecar is disposable and rebuildable, so a failed final flush costs
        // evidence detail, never state — never let it surface from a destructor.
        let _ = self.telemetry_flush();
    }
}

mod anon_gate;
mod blobcrypt;
pub mod asyncdb;
pub mod memory_tool;
pub mod migrate;
pub mod telemetry;

pub use asyncdb::AsyncAreev;
pub use telemetry::{
    AccessStat, BudgetStat, QueryStat, RecallEvent, RecallLogEntry, Telemetry, TelemetryMode,
};

/// The insert body of an add — the grain row, triples/osp, entity_latest, head
/// collapse+insert, thread index, embedding, and the OP_ADD op-log row — WITHOUT
/// `BEGIN`/`COMMIT`. Runs inside a transaction the caller already opened, so
/// `supersede`/`merge_heads` can perform the grain insert AND the index flip in
/// ONE atomic transaction (previously they committed the add, then flipped in a
/// separate txn — a crash or error between the two left a torn state: the new
/// grain durable and current while the old grain stayed un-superseded). The
/// caller reserves `first_seq`/`first_op`/`hlc0` via [`Areev::prep_and_reserve`]
/// and owns the surrounding transaction.
/// `(documents, total token length)` a batch adds to the BM25 collection
/// statistics. Applied by the caller after its transaction commits, so a
/// rolled-back insert cannot leave the in-memory counters ahead of the file.
fn fts_delta(preps: &[GrainPrep]) -> (i64, i64) {
    let docs = preps.iter().filter(|p| !p.tokens.is_empty()).count() as i64;
    (docs, preps.iter().map(|p| p.doc_len).sum())
}

fn insert_prepped(
    db: &dyn Db,
    preps: &[GrainPrep],
    first_seq: i64,
    first_op: i64,
    hlc0: i64,
) -> Result<()> {
    // Every statement here is a fixed literal on the write hot path — the
    // backend's `_hot` cache makes each a prepare-once (this used to re-prepare
    // eight statements per call).
    for (i, pr) in preps.iter().enumerate() {
        let seq = first_seq + i as i64;
        db.execute_hot(
            "INSERT INTO grains(seq,hash,ns,gtype,created_at,s,p,o,vf,vt,svf,svt,superseded_by,supersedes,text,blob)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,NULL,?12,?13)",
            vec![
                pi(seq),
                pb(pr.hash.as_bytes().to_vec()),
                pi(pr.ns_id),
                pi(pr.gtype),
                pi(pr.created),
                opt_i(pr.s),
                opt_i(pr.p),
                opt_i(pr.o),
                opt_i(pr.vf),
                opt_i(pr.vt),
                pi(pr.created),
                match &pr.text { Some(t) => pt(t), None => Value::Null },
                pb(pr.blob.clone()),
            ],
        )?;
        // BM25 postings: one row per distinct token. Cost is proportional to
        // the grain's own length, not to how much is already stored.
        if !pr.tokens.is_empty() {
            for (term, tf) in &pr.tokens {
                db.execute_hot(
                    "INSERT INTO fts_post(term,seq,ns,tf) VALUES (?1,?2,?3,?4)",
                    vec![pi(*term), pi(seq), pi(pr.ns_id), pi(*tf)],
                )?;
            }
            db.execute_hot(
                "INSERT OR REPLACE INTO fts_doc(seq,len) VALUES (?1,?2)",
                vec![pi(seq), pi(pr.doc_len)],
            )?;
        }
        if let (Some(s), Some(p), Some(o)) = (pr.s, pr.p, pr.o) {
            db.execute_hot(
                "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq)],
            )?;
            if pr.osp {
                db.execute_hot(
                    "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(o), pi(s), pi(p), pi(seq)],
                )?;
            }
            db.execute_hot(
                "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq), pb(pr.hash.as_bytes().to_vec())],
            )?;
            db.execute_hot(
                "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3",
                vec![pi(pr.ns_id), pi(s), pi(p)],
            )?;
            db.execute_hot(
                "INSERT INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                vec![pi(pr.ns_id), pi(s), pi(p), pi(seq), pb(pr.hash.as_bytes().to_vec()), pi(pr.created)],
            )?;
        }
        // Cross-grain `related_to` links (OMS §6.1), e.g. the §8.4 execution
        // record `mg:step_action:<node>` pointing a Tool grain at the Workflow
        // it executed a step of. Indexed into triples + osp so both directions
        // are traversable — but NOT into heads/entity_latest: §15.3 is
        // normative that such a link must not alter the target's supersession
        // state, and heads is exactly that state. osp is unconditional here
        // because a link's object is always a grain hash, i.e. always an
        // entity, regardless of the file's `entity_relations` declaration.
        for (ls, lp, lo) in &pr.links {
            db.execute_hot(
                "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                vec![pi(pr.ns_id), pi(*ls), pi(*lp), pi(*lo), pi(seq)],
            )?;
            db.execute_hot(
                "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                vec![pi(pr.ns_id), pi(*lo), pi(*ls), pi(*lp), pi(seq)],
            )?;
        }
        if let Some(sess) = pr.session {
            db.execute_hot(
                "INSERT INTO thread_idx(ns,session,seq) VALUES (?1,?2,?3)",
                vec![pi(pr.ns_id), pi(sess), pi(seq)],
            )?;
        }
        // Run correlation + reverse provenance. Both are plain index rows: they
        // record where a grain came from and which run recorded it, and neither
        // participates in supersession.
        if let Some(run) = pr.run {
            db.execute_hot(
                "INSERT INTO run_idx(ns,run,seq) VALUES (?1,?2,?3)",
                vec![pi(pr.ns_id), pi(run), pi(seq)],
            )?;
        }
        if pr.corpus_export {
            db.execute_hot("INSERT INTO corpus_idx(seq) VALUES (?1)", vec![pi(seq)])?;
        }
        if let Some(ref parent) = pr.parent {
            db.execute_hot(
                "INSERT INTO prov_idx(ns,parent,seq) VALUES (?1,?2,?3)",
                vec![pi(pr.ns_id), pb(parent.clone()), pi(seq)],
            )?;
        }
        if let Some(ref emb) = pr.embedding {
            db.execute_hot(
                "INSERT INTO embeddings(seq, vec) VALUES (?1, vector32(?2))",
                vec![pi(seq), pt(&vec_to_json(emb))],
            )?;
        }
        db.execute_hot(
            "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
            vec![pi(first_op + i as i64), pi(hlc0 + i as i64), pi(OP_ADD), pb(pr.hash.as_bytes().to_vec())],
        )?;
    }
    Ok(())
}

/// Every parent hash a grain supersedes: the linear `derived_from`, plus any
/// `context.merge_parents` recorded by a merge commit. Used by bundle import to
/// close all tips a replicated supersession/merge closes at the source.
fn superseded_parents(view: &DeserializedGrain) -> Vec<Hash> {
    let mut out = Vec::new();
    if let Some(df) = view.get_str("derived_from") {
        if let Ok(h) = Hash::from_hex(df) {
            out.push(h);
        }
    }
    if let Some(arr) = view
        .fields
        .get("context")
        .and_then(|c| c.get("merge_parents"))
        .and_then(|v| v.as_array())
    {
        for p in arr {
            if let Some(h) = p.as_str().and_then(|s| Hash::from_hex(s).ok()) {
                if !out.contains(&h) {
                    out.push(h);
                }
            }
        }
    }
    out
}

/// Object-safe serialization adapter so `add_batch` can take mixed grain types.
pub trait AddableDyn {
    fn serialize_dyn(&self) -> Result<(Vec<u8>, Hash)>;
}

impl<G: Grain + 'static> AddableDyn for G {
    fn serialize_dyn(&self) -> Result<(Vec<u8>, Hash)> {
        serialize_grain(self)
    }
}

#[cfg(test)]
mod tests {
    //! Inline unit tests for the store's pure/internal helpers. These sit
    //! below the black-box integration suite in `tests/` and exercise the
    //! bits that never surface through the public API: dictionary interning,
    //! the HLC counter, RRF fusion math, the Value bridge, and the crypto KDF
    //! helpers. A `tests` child module can reach the crate root's private
    //! items (fns, methods, struct fields), so we test them directly.
    use super::*;
    use tempfile::TempDir;

    // ---- CAL host metadata (meta_scan / meta_put / meta_delete) ---------

    #[test]
    fn meta_rows_round_trip_and_scan_by_prefix() {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();

        m.meta_put("qry:brief", "{\"body\":\"a\"}").unwrap();
        m.meta_put("qry:wide", "{\"body\":\"b\"}").unwrap();
        m.meta_put("tpl:card", "{\"source\":\"c\"}").unwrap();

        let mut queries = m.meta_scan("qry:").unwrap();
        queries.sort();
        assert_eq!(
            queries,
            vec![
                ("brief".to_string(), "{\"body\":\"a\"}".to_string()),
                ("wide".to_string(), "{\"body\":\"b\"}".to_string()),
            ],
            "scan returns this prefix's rows with the prefix stripped"
        );
        assert_eq!(m.meta_scan("tpl:").unwrap().len(), 1);

        // Upsert, not insert: a second put replaces.
        m.meta_put("qry:brief", "{\"body\":\"z\"}").unwrap();
        assert_eq!(m.meta_scan("qry:").unwrap().len(), 2);

        m.meta_delete("qry:brief").unwrap();
        assert_eq!(m.meta_scan("qry:").unwrap().len(), 1);
        // A missing key is not an error — drop is idempotent.
        m.meta_delete("qry:brief").unwrap();
    }

    /// `%` and `_` are LIKE wildcards. An unescaped prefix would quietly match
    /// rows it does not own — `a_b:` matching `axb:` — and hand a caller
    /// another namespace's metadata.
    #[test]
    fn meta_scan_treats_like_wildcards_as_literals() {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();

        m.meta_put("a_b:mine", "1").unwrap();
        m.meta_put("axb:theirs", "2").unwrap();
        m.meta_put("pre%fix:mine", "3").unwrap();
        m.meta_put("preXfix:theirs", "4").unwrap();

        assert_eq!(
            m.meta_scan("a_b:").unwrap(),
            vec![("mine".to_string(), "1".to_string())],
            "`_` must not match an arbitrary character"
        );
        assert_eq!(
            m.meta_scan("pre%fix:").unwrap(),
            vec![("mine".to_string(), "3".to_string())],
            "`%` must not match an arbitrary run"
        );
    }

    // ---- pure string / format helpers ----------------------------------

    #[test]
    fn hex32_roundtrips_to_64_chars() {
        assert_eq!(hex32(&[0u8; 32]), "0".repeat(64));
        assert_eq!(hex32(&[0xffu8; 32]), "f".repeat(64));

        // A distinct byte-per-index pattern renders in order and round-trips.
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let hexed = hex32(&key);
        assert_eq!(hexed.len(), 64);
        assert_eq!(&hexed[..2], "00");
        assert_eq!(&hexed[62..], "1f"); // byte 31 == 0x1f
        assert_eq!(hex::decode(&hexed).unwrap(), key.to_vec());
    }

    #[test]
    fn seq_csv_formats_integer_lists() {
        assert_eq!(seq_csv(&[]), "");
        assert_eq!(seq_csv(&[42]), "42");
        assert_eq!(seq_csv(&[1, 2, 3]), "1,2,3");
        // i64s (incl. negatives) render verbatim — no quoting, no spaces.
        assert_eq!(seq_csv(&[-1, 0, 7]), "-1,0,7");
    }

    #[test]
    fn vec_to_json_renders_float_arrays() {
        assert_eq!(vec_to_json(&[]), "[]");
        assert_eq!(vec_to_json(&[1.0]), "[1]");
        assert_eq!(vec_to_json(&[1.0, 2.5, -3.25]), "[1,2.5,-3.25]");
    }

    // ---- Value bridge (SQL <-> Rust) -----------------------------------

    #[test]
    fn value_bridge_roundtrips() {
        // encode helpers produce the right Value variant, decode helpers only
        // accept a matching one.
        assert_eq!(v_i64(&pi(42)), Some(42));
        assert_eq!(v_i64(&pt("x")), None);
        assert_eq!(v_blob(&pb(vec![1, 2, 3])), Some(vec![1, 2, 3]));
        assert_eq!(v_blob(&pi(1)), None);
        // opt_i: Some -> Integer, None -> SQL NULL.
        assert_eq!(v_i64(&opt_i(Some(7))), Some(7));
        assert!(matches!(opt_i(None), Value::Null));
        // v_f64 accepts both Real and Integer.
        assert!((v_f64(&Value::Real(1.5)).unwrap() - 1.5).abs() < 1e-12);
        assert!((v_f64(&pi(3)).unwrap() - 3.0).abs() < 1e-12);
        assert_eq!(v_f64(&pt("x")), None);
    }

    // ---- KDF sidecar parsing (crypto-critical) -------------------------

    fn kdf_line(salt: &[u8], m: u32, t: u32, p: u32) -> String {
        format!("v1 argon2id {} {m} {t} {p}", hex::encode(salt))
    }

    #[test]
    fn parse_kdf_sidecar_accepts_valid() {
        let salt = [7u8; KDF_SALT_LEN];
        let text = kdf_line(&salt, KDF_M_COST, KDF_T_COST, KDF_P_COST);
        let (got_salt, m, t, p) = parse_kdf_sidecar(&text, "x.kdf").unwrap();
        assert_eq!(got_salt, salt);
        assert_eq!((m, t, p), (KDF_M_COST, KDF_T_COST, KDF_P_COST));
        // Tolerant of leading whitespace and a trailing newline.
        assert!(parse_kdf_sidecar(&format!("  {text}\n"), "x.kdf").is_ok());
        // Boundary params are accepted.
        let salt_hex = hex::encode([1u8; KDF_SALT_LEN]);
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt_hex} 8 1 1"), "x").is_ok());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt_hex} 1048576 16 16"), "x").is_ok());
    }

    #[test]
    fn parse_kdf_sidecar_rejects_malformed() {
        let salt = hex::encode([1u8; KDF_SALT_LEN]);
        // wrong token count (too few / too many)
        assert!(parse_kdf_sidecar("v1 argon2id", "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 1 2 3 4"), "x").is_err());
        // wrong version tag
        assert!(parse_kdf_sidecar(&format!("v2 argon2id {salt} 19456 2 1"), "x").is_err());
        // wrong algorithm
        assert!(parse_kdf_sidecar(&format!("v1 scrypt {salt} 19456 2 1"), "x").is_err());
        // non-hex salt / non-numeric params
        assert!(parse_kdf_sidecar("v1 argon2id zzzz 19456 2 1", "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} m 2 1"), "x").is_err());
    }

    #[test]
    fn parse_kdf_sidecar_rejects_wrong_salt_length() {
        // 8 bytes (too short) and 32 bytes (too long) both rejected; only the
        // 16-byte KDF_SALT_LEN is valid.
        let short = hex::encode([1u8; 8]);
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {short} 19456 2 1"), "x").is_err());
        let long = hex::encode([1u8; 32]);
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {long} 19456 2 1"), "x").is_err());
    }

    #[test]
    fn parse_kdf_sidecar_rejects_out_of_range_params() {
        let salt = hex::encode([1u8; KDF_SALT_LEN]);
        // m outside [8, 1_048_576] (guards against a tampered multi-GiB cost)
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 7 2 1"), "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 1048577 2 1"), "x").is_err());
        // t outside [1, 16]
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 0 1"), "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 17 1"), "x").is_err());
        // p outside [1, 16]
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 2 0"), "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 2 17"), "x").is_err());
    }

    // ---- passphrase key derivation (Argon2id) --------------------------

    #[test]
    fn derive_key_rejects_empty_or_whitespace_passphrase() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("k.db");
        let path = path.to_str().unwrap();
        assert!(Areev::derive_key_for(path, "").is_err());
        assert!(Areev::derive_key_for(path, "   ").is_err());
        assert!(Areev::derive_key_for(path, "\t\n ").is_err());
        // A rejected passphrase must not leave a sidecar behind.
        assert!(!std::path::Path::new(&format!("{path}.kdf")).exists());
    }

    #[test]
    fn derive_key_is_deterministic_for_same_salt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("k.db");
        let path = path.to_str().unwrap();
        // First call mints the .kdf sidecar (fresh salt); the second reuses it.
        let k1 = Areev::derive_key_for(path, "correct horse battery staple").unwrap();
        let k2 = Areev::derive_key_for(path, "correct horse battery staple").unwrap();
        assert_eq!(*k1, *k2);
        // Same salt, different passphrase -> different key.
        let k3 = Areev::derive_key_for(path, "a different passphrase").unwrap();
        assert_ne!(*k1, *k3);
    }

    #[test]
    fn derive_key_differs_across_salts() {
        let dir = TempDir::new().unwrap();
        let p1 = dir.path().join("a.db");
        let p2 = dir.path().join("b.db");
        let (p1, p2) = (p1.to_str().unwrap(), p2.to_str().unwrap());
        // Same passphrase, independent sidecars -> independent random salts ->
        // different keys.
        let k1 = Areev::derive_key_for(p1, "same-pass").unwrap();
        let k2 = Areev::derive_key_for(p2, "same-pass").unwrap();
        assert_ne!(*k1, *k2);
        assert!(std::path::Path::new(&format!("{p1}.kdf")).exists());
        assert!(std::path::Path::new(&format!("{p2}.kdf")).exists());
    }

    // ---- RRF fusion math -----------------------------------------------

    /// Mirror of the inline reciprocal-rank fusion in `recall_hybrid_tuned`:
    /// each leg contributes `1/(RRF_K0 + rank)`, scores sum across legs, and
    /// ties break by seq id descending. Kept here (rather than extracted from
    /// production) so these tests pin the fusion contract without changing the
    /// recall path; if the inline formula drifts, these expectations should
    /// be updated in lockstep.
    fn rrf_fuse(legs: &[&[i64]]) -> Vec<(i64, f64)> {
        let mut scores: HashMap<i64, f64> = HashMap::new();
        for leg in legs {
            for (rank, seq) in leg.iter().enumerate() {
                *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
            }
        }
        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.0.cmp(&a.0))
        });
        ranked
    }

    #[test]
    fn rrf_k0_constant_is_pinned() {
        // The standard k0 = 60; observability surfaces export this value.
        assert!((RRF_K0 - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rrf_single_leg_preserves_rank_order() {
        let leg = [10i64, 20, 30];
        let fused = rrf_fuse(&[&leg]);
        assert_eq!(fused.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![10, 20, 30]);
        assert!((fused[0].1 - 1.0 / 60.0).abs() < 1e-12);
        assert!((fused[1].1 - 1.0 / 61.0).abs() < 1e-12);
        assert!((fused[2].1 - 1.0 / 62.0).abs() < 1e-12);
        // Contribution strictly decreases with rank.
        assert!(fused[0].1 > fused[1].1 && fused[1].1 > fused[2].1);
    }

    #[test]
    fn rrf_rewards_agreement_across_legs() {
        // seq 1 tops both legs; seq 2 only in leg A; seq 3 only in leg B.
        let a = [1i64, 2];
        let b = [1i64, 3];
        let fused = rrf_fuse(&[&a, &b]);
        // seq 1 accrues 2/60 and must rank first.
        assert_eq!(fused[0].0, 1);
        let top = fused.iter().find(|(s, _)| *s == 1).unwrap().1;
        assert!((top - 2.0 / 60.0).abs() < 1e-12);
        // A doc in only one leg cannot beat the doc endorsed by both.
        let two = fused.iter().find(|(s, _)| *s == 2).unwrap().1;
        assert!(top > two);
    }

    #[test]
    fn rrf_breaks_ties_by_seq_desc() {
        // Two seqs at rank 0 of their own leg -> equal scores; the larger seq
        // id sorts first, matching the production tie-break.
        let l1 = [5i64];
        let l2 = [9i64];
        let fused = rrf_fuse(&[&l1, &l2]);
        assert!((fused[0].1 - fused[1].1).abs() < 1e-12);
        assert_eq!(fused[0].0, 9);
        assert_eq!(fused[1].0, 5);
    }

    // ---- rule-based query expansion (pure, deterministic) --------------

    #[test]
    fn english_expander_substitutes_synonyms() {
        let ex = EnglishExpander::default();
        let v = ex.expand("cell");
        assert!(v.contains(&"mobile".to_string()));
        assert!(v.contains(&"phone".to_string()));
        // The original query is never echoed back as a variant.
        assert!(!v.contains(&"cell".to_string()));
    }

    #[test]
    fn english_expander_stems_and_is_bounded() {
        let ex = EnglishExpander::new(4);
        // Plural -> singular stem bridges the vocabulary gap.
        assert!(ex.expand("cars").contains(&"car".to_string()));
        // Empty query yields no variants.
        assert!(ex.expand("").is_empty());
        // Variant count honors the cap.
        assert!(ex.expand("cell phone email car").len() <= 4);
    }

    // ---- HLC monotonicity + dictionary (need a live store handle) ------

    fn open_tmp() -> (Areev, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("unit.db");
        let db = Areev::open(path.to_str().unwrap()).unwrap();
        (db, dir)
    }

    #[test]
    fn next_hlc_is_strictly_monotonic_within_one_ms() {
        let (mut db, _d) = open_tmp();
        // Force the "same wall-clock millisecond" branch deterministically:
        // seed hlc_last far in the future so `wall <= hlc_last` on every call,
        // proving the in-memory +1 counter alone keeps HLCs strictly
        // increasing without any wall-clock advance (hence no sleep needed).
        db.hlc_last = (now_ms() + 1_000_000) << 16;
        let a = db.next_hlc();
        let b = db.next_hlc();
        let c = db.next_hlc();
        assert_eq!(b, a + 1);
        assert_eq!(c, b + 1);
    }

    #[test]
    fn next_hlc_tracks_wall_clock_when_it_advances() {
        let (mut db, _d) = open_tmp();
        let before = now_ms();
        db.hlc_last = 0;
        let first = db.next_hlc();
        let after = now_ms();
        // With a zero baseline the wall clock (ms << 16) dominates: the top
        // bits carry the millisecond of the call.
        assert!(first >> 16 >= before && first >> 16 <= after);
    }

    #[test]
    fn next_hlc_never_repeats_over_many_calls() {
        let (mut db, _d) = open_tmp();
        let mut last = db.next_hlc();
        for _ in 0..5000 {
            let n = db.next_hlc();
            assert!(n > last, "HLC must strictly increase: {n} !> {last}");
            last = n;
        }
    }

    #[test]
    fn term_id_interns_and_reverse_scans() {
        let (mut db, _d) = open_tmp();
        let a = db.term_id("alice").unwrap();
        let b = db.term_id("bob").unwrap();
        assert_ne!(a, b);
        // Re-interning a known term is a cache hit -> same id.
        assert_eq!(db.term_id("alice").unwrap(), a);
        // Forward lookup.
        assert_eq!(db.term_lookup("alice").unwrap(), Some(a));
        assert_eq!(db.term_lookup("bob").unwrap(), Some(b));
        assert_eq!(db.term_lookup("nobody").unwrap(), None);
        // Reverse scan (id -> term).
        assert_eq!(db.term_str(a).unwrap().as_deref(), Some("alice"));
        assert_eq!(db.term_str(b).unwrap().as_deref(), Some("bob"));
        assert_eq!(db.term_str(999_999).unwrap(), None);
    }

    #[test]
    fn term_ids_persist_and_continue_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dict.db");
        let path = path.to_str().unwrap();
        let (a, b);
        {
            let mut db = Areev::open(path).unwrap();
            a = db.term_id("x").unwrap();
            b = db.term_id("y").unwrap();
            assert!(b > a);
        }
        // Reopen: the dictionary reloads, existing terms keep their ids, and a
        // fresh term gets an id beyond the previous max (next_term continues).
        {
            let mut db = Areev::open(path).unwrap();
            assert_eq!(db.term_lookup("x").unwrap(), Some(a));
            assert_eq!(db.term_lookup("y").unwrap(), Some(b));
            let c = db.term_id("z").unwrap();
            assert!(c > b);
            assert_eq!(db.term_str(c).unwrap().as_deref(), Some("z"));
        }
    }
}
