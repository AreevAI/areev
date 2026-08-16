use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The 11 OMS 1.4 grain types.
/// Byte values unchanged from OMS 1.1 for the first 7; 3 added in OMS 1.2,
/// and Skill (0x0B) added in OMS 1.4. The byte ↔ string ↔ plural mapping and
/// the rest of the data-only lookup facts live in
/// [`crate::types::registry`] — the methods below are thin wrappers over it
/// so a new type touches one registry row, not a dozen scattered lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrainType {
    Fact,
    Event,
    State,
    Workflow,
    Tool,
    Observation,
    Goal,
    Reasoning,
    Consensus,
    Consent,
    Skill,
    /// OMS 1.5 §8.12. Appended last on purpose: `areev-store` indexes the
    /// `gtype` column as this enum's ordinal rather than the .mg header byte,
    /// so inserting a variant mid-enum would silently renumber every stored
    /// row and break type-filtered recall in every existing file.
    Recommendation,
}

impl GrainType {
    /// Type byte for the .mg header.
    pub fn type_byte(&self) -> u8 {
        super::registry::meta(*self).byte
    }

    /// Parse from type byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        super::registry::from_byte(b)
    }

    /// Parse from OMS canonical string name.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        super::registry::from_str(s)
    }

    /// Canonical OMS string name for the type field in serialized form.
    /// Writers MUST emit canonical names.
    pub fn as_str(&self) -> &'static str {
        super::registry::meta(*self).name
    }
}

/// Temporal type for bi-temporal modeling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalType {
    State,
    Event,
    Interval,
}

/// Content reference to external content (PDFs, images, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentRef {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Embedding reference to external vector store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRef {
    pub vector_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modality_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_metric: Option<String>,
}

/// Provenance chain entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceEntry {
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// Validate a `run_id` against the run-correlation contract.
///
/// A top-level `run_id` on **any** grain type is indexed into `run_idx` and
/// read back by `run_trace`/`run_grains`/`runs_touching` — on Event it is a
/// typed field, on every other type it rides as a top-level extra field. That
/// behavior was mechanical and untested; this function is the contract's
/// choke point, called by every JSON write surface. Rules: 1–128 bytes,
/// non-blank, no control characters. Empty/whitespace ids would index but be
/// unqueryable-by-intent; oversized or control-laden ids are dictionary
/// pollution from untrusted input.
pub fn validate_run_id(run_id: &str) -> crate::error::Result<()> {
    if run_id.trim().is_empty() {
        return Err(crate::error::AreevError::Validation(
            "run_id must be non-empty".into(),
        ));
    }
    if run_id.len() > 128 {
        return Err(crate::error::AreevError::Validation(format!(
            "run_id exceeds 128 bytes ({} bytes)",
            run_id.len()
        )));
    }
    if run_id.chars().any(|c| c.is_control()) {
        return Err(crate::error::AreevError::Validation(
            "run_id contains control characters".into(),
        ));
    }
    Ok(())
}

/// Related-to link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedTo {
    pub hash: String,
    pub relation_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

/// Invalidation policy for protected grains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvalidationPolicy {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorized: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_until: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protection_reason: Option<String>,
}

/// Common fields shared by all grain types.
#[derive(Debug, Clone, Default)]
pub struct GrainCommon {
    pub namespace: Option<String>,
    pub user_id: Option<String>,
    pub tags: Vec<String>,
    pub confidence: f64,
    pub source_type: Option<String>,
    pub importance: Option<f64>,
    pub temporal_type: Option<TemporalType>,
    pub valid_from: Option<i64>,
    pub valid_to: Option<i64>,
    pub system_valid_from: Option<i64>,
    pub system_valid_to: Option<i64>,
    pub content_refs: Vec<ContentRef>,
    pub embedding_refs: Vec<EmbeddingRef>,
    pub provenance_chain: Vec<ProvenanceEntry>,
    pub related_to: Vec<RelatedTo>,
    pub author_did: Option<String>,
    pub origin_did: Option<String>,
    pub origin_namespace: Option<String>,
    pub derived_from: Option<String>,
    pub consolidation_level: Option<u32>,
    pub success_count: Option<u32>,
    pub failure_count: Option<u32>,
    pub superseded_by: Option<String>,
    /// OMS 1.2: replaces deprecated `contradicted` boolean.
    /// Values: "unverified", "verified", "contested", "retracted"
    pub verification_status: Option<String>,
    pub context: Option<serde_json::Value>,
    pub invalidation_policy: Option<InvalidationPolicy>,
    pub supersession_justification: Option<String>,
    pub supersession_auth: Option<Vec<serde_json::Value>>,
    pub created_at: Option<i64>,
    /// Optional override text for embedding generation. When set, this text is used
    /// instead of the auto-generated `text()` for vector embedding and BM25 content
    /// indexing. Typical use: document import pipelines that want to preserve
    /// original prose context alongside structured grain fields.
    /// Max 8192 bytes enforced at write time.
    pub embedding_text: Option<String>,
    /// Custom fields not recognized by the grain type's whitelist.
    /// Preserved through serialization round-trip. Stored as part of the
    /// .mg blob alongside structured fields.
    pub extra_fields: BTreeMap<String, serde_json::Value>,
}

/// Trait implemented by all grain types.
pub trait Grain: Send + Sync {
    fn grain_type(&self) -> GrainType;
    fn common(&self) -> &GrainCommon;
    fn common_mut(&mut self) -> &mut GrainCommon;

    /// Return the primary text representation of this grain for embedding and reranking.
    /// Default returns empty string — concrete types should override.
    fn text(&self) -> String {
        String::new()
    }

    /// Return enriched text optimized for embedding quality.
    ///
    /// Priority: explicit `embedding_text` override > auto-enriched `text()` with
    /// tags and namespace context. Falls back to plain `text()` if no enrichment
    /// is possible.
    fn embedding_text(&self) -> String {
        // 1. Explicit override — caller provided rich text
        if let Some(ref et) = self.common().embedding_text {
            if !et.is_empty() {
                return et.clone();
            }
        }
        // 2. Auto-enrich text() with tags + namespace
        let base = self.text();
        if base.is_empty() {
            return String::new();
        }
        let common = self.common();
        let mut parts = vec![base];
        if !common.tags.is_empty() {
            parts.push(format!("[{}]", common.tags.join(", ")));
        }
        if let Some(ref ns) = common.namespace {
            parts.push(format!("({})", ns));
        }
        parts.join(" ")
    }

    // Builder methods available on all grains
    fn confidence(mut self, c: f64) -> Self
    where
        Self: Sized,
    {
        self.common_mut().confidence = c;
        self
    }

    fn namespace(mut self, ns: &str) -> Self
    where
        Self: Sized,
    {
        self.common_mut().namespace = Some(ns.to_string());
        self
    }

    fn user_id(mut self, uid: &str) -> Self
    where
        Self: Sized,
    {
        self.common_mut().user_id = Some(uid.to_string());
        self
    }

    fn tags<I, S>(mut self, tags: I) -> Self
    where
        Self: Sized,
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.common_mut().tags = tags.into_iter().map(|s| s.as_ref().to_string()).collect();
        self
    }

    fn source_type(mut self, st: &str) -> Self
    where
        Self: Sized,
    {
        self.common_mut().source_type = Some(st.to_string());
        self
    }

    fn importance(mut self, i: f64) -> Self
    where
        Self: Sized,
    {
        self.common_mut().importance = Some(i);
        self
    }

    fn temporal_type(mut self, tt: TemporalType) -> Self
    where
        Self: Sized,
    {
        self.common_mut().temporal_type = Some(tt);
        self
    }

    fn valid_from(mut self, ts: i64) -> Self
    where
        Self: Sized,
    {
        self.common_mut().valid_from = Some(ts);
        self
    }

    fn valid_to(mut self, ts: i64) -> Self
    where
        Self: Sized,
    {
        self.common_mut().valid_to = Some(ts);
        self
    }

    fn author_did(mut self, did: &str) -> Self
    where
        Self: Sized,
    {
        self.common_mut().author_did = Some(did.to_string());
        self
    }

    fn content_ref(mut self, cr: ContentRef) -> Self
    where
        Self: Sized,
    {
        self.common_mut().content_refs.push(cr);
        self
    }

    fn embedding_ref(mut self, er: EmbeddingRef) -> Self
    where
        Self: Sized,
    {
        self.common_mut().embedding_refs.push(er);
        self
    }

    fn related_to_link(mut self, hash: &str, relation_type: &str, weight: f64) -> Self
    where
        Self: Sized,
    {
        self.common_mut().related_to.push(RelatedTo {
            hash: hash.to_string(),
            relation_type: relation_type.to_string(),
            weight: Some(weight),
        });
        self
    }

    /// Record that this grain is the execution record for `node_id` of the
    /// Workflow at `workflow_hash` (OMS §8.4).
    ///
    /// Normally called on a Tool grain in `call`/`result` phase. No weight — the
    /// link is a structural fact, not a similarity score.
    fn step_action(mut self, workflow_hash: &str, node_id: &str) -> Self
    where
        Self: Sized,
    {
        self.common_mut().related_to.push(RelatedTo {
            hash: workflow_hash.to_string(),
            relation_type: super::workflow::step_action_relation(node_id),
            weight: None,
        });
        self
    }

    fn created_at(mut self, ts: i64) -> Self
    where
        Self: Sized,
    {
        self.common_mut().created_at = Some(ts);
        self
    }

    /// Add a custom field that will be preserved through serialization.
    fn extra_field(mut self, key: &str, value: serde_json::Value) -> Self
    where
        Self: Sized,
    {
        self.common_mut()
            .extra_fields
            .insert(key.to_string(), value);
        self
    }

    /// Convert to a type-erased GrainData for heterogeneous collections.
    fn into_grain_data(self) -> GrainData
    where
        Self: Sized + 'static,
    {
        GrainData::from_grain(self)
    }
}

/// Type-erased grain for heterogeneous collections (e.g., add_many, ImportStream).
pub struct GrainData {
    inner: Box<dyn GrainDyn>,
}

trait GrainDyn: Send + Sync {
    fn grain_type(&self) -> GrainType;
    fn common(&self) -> &GrainCommon;
    fn as_any(&self) -> &dyn std::any::Any;
}

impl<T: Grain + 'static> GrainDyn for T {
    fn grain_type(&self) -> GrainType {
        Grain::grain_type(self)
    }
    fn common(&self) -> &GrainCommon {
        Grain::common(self)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl GrainData {
    pub fn from_grain<T: Grain + 'static>(grain: T) -> Self {
        GrainData {
            inner: Box::new(grain),
        }
    }

    pub fn grain_type(&self) -> GrainType {
        self.inner.grain_type()
    }

    pub fn common(&self) -> &GrainCommon {
        self.inner.common()
    }

    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.inner.as_any().downcast_ref()
    }
}
