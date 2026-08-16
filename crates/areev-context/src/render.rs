//! Grain-type-aware renderers for context formatting.
//!
//! Each of the 11 OMS grain types has a renderer implementing `GrainRenderer`.
//! Renderers produce formatted strings in SML, Markdown, PlainText, or JSON.

use std::collections::HashMap;

use areev_cal::store_types::SearchHit;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::types::GrainType;

use super::policy::{FormatPolicy, MetadataLevel, OutputFormat};

/// Renders a single grain type into formatted context strings.
///
/// Object-safe: stored in `RendererRegistry` keyed by `GrainType`.
pub trait GrainRenderer: Send + Sync {
    /// Which grain type this renderer handles.
    fn grain_type(&self) -> GrainType;

    /// Full render of the grain's content.
    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String;

    /// Compact one-line summary for budget-constrained contexts.
    fn render_summary(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        self.render(grain, policy)
    }

    /// Estimated token count for the full render (~4 chars per token).
    fn token_estimate(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> usize {
        let chars: usize = grain
            .fields
            .iter()
            .map(|(k, v)| k.len() + json_display_len(v) + 4)
            .sum();
        let meta_overhead = match policy.metadata {
            MetadataLevel::None => 0,
            MetadataLevel::Minimal => 30,
            MetadataLevel::Full => 80,
        };
        (chars + meta_overhead) / 4
    }

    /// Context priority for this grain [0.0, 1.0].
    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32;
}

/// Approximate display length of a JSON value (for token estimation).
fn json_display_len(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        serde_json::Value::Number(n) => n.to_string().len(),
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(a) => a.iter().map(json_display_len).sum::<usize>() + a.len() * 2,
        serde_json::Value::Object(o) => o
            .iter()
            .map(|(k, v)| k.len() + json_display_len(v) + 4)
            .sum(),
    }
}

/// Registry mapping GrainType to its renderer.
pub struct RendererRegistry {
    renderers: HashMap<GrainType, Box<dyn GrainRenderer>>,
}

impl Default for RendererRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RendererRegistry {
    /// Create with all 10 default renderers registered.
    pub fn new() -> Self {
        let mut registry = Self {
            renderers: HashMap::with_capacity(10),
        };
        registry.register(Box::new(FactRenderer));
        registry.register(Box::new(EventRenderer));
        registry.register(Box::new(StateRenderer));
        registry.register(Box::new(WorkflowRenderer));
        registry.register(Box::new(ToolRenderer));
        registry.register(Box::new(ObservationRenderer));
        registry.register(Box::new(GoalRenderer));
        registry.register(Box::new(ReasoningRenderer));
        registry.register(Box::new(ConsensusRenderer));
        registry.register(Box::new(ConsentRenderer));
        registry.register(Box::new(SkillRenderer));
        registry.register(Box::new(RecommendationRenderer));
        registry
    }

    /// Get renderer for a grain type.
    pub fn get(&self, gt: GrainType) -> Option<&dyn GrainRenderer> {
        self.renderers.get(&gt).map(|r| r.as_ref())
    }

    /// Register a custom renderer (replaces existing for that type).
    pub fn register(&mut self, renderer: Box<dyn GrainRenderer>) {
        self.renderers.insert(renderer.grain_type(), renderer);
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Apply priority modifiers common to all grain types.
fn adjusted_priority(base: f32, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
    let score_boost = hit.score as f32 * 0.15;
    let confidence = grain.get_f64("confidence").unwrap_or(1.0) as f32;
    let confidence_boost = (confidence - 0.5).max(0.0) * 0.1;
    let verification_penalty = match grain.get_str("verification_status") {
        Some("retracted") => -0.3,
        Some("contested") => -0.15,
        _ => 0.0,
    };
    (base + score_boost + confidence_boost + verification_penalty).clamp(0.0, 1.0)
}

/// Extracted metadata fields.
struct MetadataFragment {
    confidence: Option<f64>,
    created_at_sec: u32,
    hash_hex: String,
    tags: Vec<String>,
    source_type: Option<String>,
    namespace: Option<String>,
    verification_status: Option<String>,
}

fn extract_metadata(grain: &DeserializedGrain, level: MetadataLevel) -> Option<MetadataFragment> {
    match level {
        MetadataLevel::None => None,
        MetadataLevel::Minimal | MetadataLevel::Full => {
            let confidence = grain.get_f64("confidence");
            let created_at_sec = grain.header.created_at_sec;
            let hash_hex = grain.hash.to_hex();

            let (tags, source_type, namespace, verification_status) =
                if level == MetadataLevel::Full {
                    let tags = grain
                        .fields
                        .get("structural_tags")
                        .or_else(|| grain.fields.get("tags"))
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        tags,
                        grain.get_str("source_type").map(String::from),
                        grain.get_str("namespace").map(String::from),
                        grain.get_str("verification_status").map(String::from),
                    )
                } else {
                    (Vec::new(), None, None, None)
                };

            Some(MetadataFragment {
                confidence,
                created_at_sec,
                hash_hex,
                tags,
                source_type,
                namespace,
                verification_status,
            })
        }
    }
}

/// Format epoch seconds as "YYYY-MM-DD".
fn format_date(epoch_sec: u32) -> String {
    let secs = epoch_sec as i64;
    // Days since epoch
    let days = secs / 86400;
    // Compute year/month/day from days since 1970-01-01
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days since epoch to (year, month, day).
fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    // Civil calendar algorithm (Howard Hinnant)
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn format_meta_sml_attrs(meta: &MetadataFragment, level: MetadataLevel) -> String {
    let mut attrs = Vec::new();
    if let Some(c) = meta.confidence {
        attrs.push(format!("confidence=\"{c:.2}\""));
    }
    if meta.created_at_sec > 0 {
        attrs.push(format!("date=\"{}\"", format_date(meta.created_at_sec)));
    }
    if level == MetadataLevel::Full {
        attrs.push(format!("hash=\"{}\"", &meta.hash_hex[..16]));
        if let Some(ref ns) = meta.namespace {
            attrs.push(format!("namespace=\"{}\"", sml_escape(ns)));
        }
        if let Some(ref vs) = meta.verification_status {
            attrs.push(format!("status=\"{}\"", sml_escape(vs)));
        }
        if !meta.tags.is_empty() {
            attrs.push(format!("tags=\"{}\"", sml_escape(&meta.tags.join(","))));
        }
    }
    if attrs.is_empty() {
        String::new()
    } else {
        format!(" {}", attrs.join(" "))
    }
}

fn format_meta_markdown(meta: &MetadataFragment, level: MetadataLevel) -> String {
    let mut parts = Vec::new();
    if let Some(c) = meta.confidence {
        parts.push(format!("{c:.2}"));
    }
    if meta.created_at_sec > 0 {
        parts.push(format_date(meta.created_at_sec));
    }
    if level == MetadataLevel::Full {
        if let Some(ref vs) = meta.verification_status {
            parts.push(vs.clone());
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" *({})*", parts.join(", "))
    }
}

fn format_meta_plain(meta: &MetadataFragment, level: MetadataLevel) -> String {
    let mut parts = Vec::new();
    if let Some(c) = meta.confidence {
        parts.push(format!("{c:.2}"));
    }
    if meta.created_at_sec > 0 {
        parts.push(format_date(meta.created_at_sec));
    }
    if level == MetadataLevel::Full {
        if let Some(ref vs) = meta.verification_status {
            parts.push(vs.clone());
        }
        if let Some(ref ns) = meta.namespace {
            parts.push(ns.clone());
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join(", "))
    }
}

/// Escape XML metacharacters (`&`, `<`, `>`, `"`, `'`) for SML output.
/// Bidi-control codepoints (U+202A–U+202E, U+2066–U+2069, U+200E/F, ZWJ, BOM)
/// are intentionally passed through — stripping is the LLM-output-layer's
/// responsibility and is out of scope for the SML escape helper.
fn sml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render a `content_blocks` JSON array (Event grain chat extension,
/// OMS 1.2 §8.2) as typed SML tags — `<text>`, `<tool_use>`, and
/// `<tool_result>`. Never renders block payloads as free text (security
/// SC1 — prevents injection via tool arguments). Falls back to the
/// plain Event `content` string when the value is malformed.
fn render_content_blocks_sml(blocks: &serde_json::Value, fallback: &str) -> String {
    let Some(arr) = blocks.as_array() else {
        return sml_escape(fallback);
    };
    let mut out = String::new();
    for block in arr {
        let Some(obj) = block.as_object() else {
            continue;
        };
        let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&format!("<text>{}</text>", sml_escape(text)));
            }
            "tool_use" => {
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let input_json = obj
                    .get("input")
                    .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
                    .unwrap_or_else(|| "{}".into());
                out.push_str(&format!(
                    "<tool_use name=\"{}\" id=\"{}\"><args format=\"json\">{}</args></tool_use>",
                    sml_escape(name),
                    sml_escape(id),
                    sml_escape(&input_json)
                ));
            }
            "tool_result" => {
                let tool_use_id = obj
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let is_error = obj
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push_str(&format!(
                    "<tool_result tool_use_id=\"{}\" is_error=\"{}\">{}</tool_result>",
                    sml_escape(tool_use_id),
                    is_error,
                    sml_escape(content)
                ));
            }
            _ => {}
        }
    }
    if out.is_empty() {
        sml_escape(fallback)
    } else {
        out
    }
}

/// Escape/quote a TOON value per TOON spec Section 7.2.
/// Values are quoted only when necessary (contains special chars, matches keywords, etc.).
fn toon_escape(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quoting = s != s.trim()
        || matches!(s, "true" | "false" | "null")
        || toon_looks_numeric(s)
        || s.contains(':')
        || s.contains('"')
        || s.contains('\\')
        || s.contains('[')
        || s.contains(']')
        || s.contains('{')
        || s.contains('}')
        || s.contains(',')
        || s.contains('\n')
        || s.contains('\r')
        || s.contains('\t')
        || s.starts_with('-');
    if needs_quoting {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        format!("\"{}\"", escaped)
    } else {
        s.to_string()
    }
}

/// Check if a string looks like a TOON numeric literal.
fn toon_looks_numeric(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    if s.is_empty() {
        return false;
    }
    // Leading zeros like "05" are treated as strings in TOON but still need quoting
    if s.len() > 1
        && s.starts_with('0')
        && !s.starts_with("0.")
        && !s.starts_with("0e")
        && !s.starts_with("0E")
    {
        return true;
    }
    s.parse::<f64>().is_ok()
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
}

/// TOON column names per grain type, per CAL spec Section 10.9.3.
///
/// Reads from the grain-type metadata registry (D1) — the single source of
/// truth for the byte ↔ string ↔ plural mapping plus the per-type TOON column
/// set — so this is not a third hand-maintained copy of the same data.
pub fn toon_columns(gt: &GrainType) -> &'static [&'static str] {
    areev_core::types::registry::meta(*gt).toon_columns
}

/// Canonicalize a number for TOON output.
/// Per TOON spec: no exponent notation, no trailing fractional zeros, NaN/Infinity → "null".
fn toon_canonicalize_number(n: f64) -> String {
    if n.is_nan() || n.is_infinite() {
        return "null".to_string();
    }
    // Rust Display for f64 already gives shortest representation without exponent
    format!("{}", n)
}

/// Truncate a string to max_chars at a valid UTF-8 char boundary.
fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        let mut end = max_chars.saturating_sub(3).min(s.len());
        // Walk backwards to find a valid UTF-8 char boundary
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

// ---------------------------------------------------------------------------
// Fact renderer
// ---------------------------------------------------------------------------

struct FactRenderer;

impl GrainRenderer for FactRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Fact
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let subject = grain.get_str("subject").unwrap_or("?");
        let relation = grain.get_str("relation").unwrap_or("?");
        let object = grain.get_str("object").unwrap_or("?");
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                format!(
                    "<fact{attrs}>{} {} {}</fact>",
                    sml_escape(subject),
                    sml_escape(relation),
                    sml_escape(object)
                )
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                format!("**{subject}** {relation} {object}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                format!("{subject} {relation} {object}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "fact",
                    "subject": subject,
                    "relation": relation,
                    "object": object,
                });
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — subject, content, confidence
                let content = if !relation.is_empty() && !object.is_empty() {
                    format!("{relation} {object}")
                } else if !object.is_empty() {
                    object.to_string()
                } else {
                    relation.to_string()
                };
                let confidence_str = grain
                    .get_f64("confidence")
                    .map(toon_canonicalize_number)
                    .unwrap_or_default();
                format!(
                    "{},{},{}",
                    toon_escape(subject),
                    toon_escape(&content),
                    confidence_str
                )
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let s = grain.get_str("subject").unwrap_or("?");
        let r = grain.get_str("relation").unwrap_or("?");
        let o = grain.get_str("object").unwrap_or("?");
        format!("{s} {r} {o}")
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.7, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Event renderer
// ---------------------------------------------------------------------------

struct EventRenderer;

impl GrainRenderer for EventRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Event
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let content = grain.get_str("content").unwrap_or("");
        let display = truncate(content, 500);
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                if let Some(blocks_json) = grain.fields.get("content_blocks") {
                    let inner = render_content_blocks_sml(blocks_json, display);
                    format!("<event{attrs}>{inner}</event>")
                } else {
                    format!("<event{attrs}>{}</event>", sml_escape(display))
                }
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                format!("{display}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                format!("{display}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "event",
                    "content": display,
                });
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — role, time, content
                let role = grain
                    .get_str("role")
                    .or_else(|| grain.get_str("speaker"))
                    .unwrap_or("user");
                let time = if grain.header.created_at_sec > 0 {
                    format_date(grain.header.created_at_sec)
                } else {
                    String::new()
                };
                format!(
                    "{},{},{}",
                    toon_escape(role),
                    toon_escape(&time),
                    toon_escape(display)
                )
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let content = grain.get_str("content").unwrap_or("");
        truncate(content, 80).to_string()
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.6, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// State renderer
// ---------------------------------------------------------------------------

struct StateRenderer;

impl GrainRenderer for StateRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::State
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let label = state_label(grain);
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                format!("<state{attrs}>{}</state>", sml_escape(&label))
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                format!("**State**: {label}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                format!("State: {label}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "state",
                    "label": label,
                });
                // Include the state snapshot if present. The wire key is `ctx`,
                // which expands to `context` (OMS §8.3) — not `context_data`.
                if let Some(ctx) = grain.fields.get("context") {
                    obj["context"] = ctx.clone();
                }
                // OMS §8.3 optional fields — a checkpoint whose plan is invisible
                // is a checkpoint the model cannot resume from.
                for key in ["plan", "history"] {
                    if let Some(v) = grain.fields.get(key) {
                        obj[key] = v.clone();
                    }
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — context, content
                let content_summary = if let Some(ctx) = grain.fields.get("context") {
                    if let Some(obj) = ctx.as_object() {
                        let summary: Vec<String> = obj
                            .iter()
                            .filter(|(k, _)| {
                                !matches!(k.as_str(), "label" | "description" | "title" | "name")
                            })
                            .take(3)
                            .map(|(k, v)| {
                                let val = match v {
                                    serde_json::Value::String(s) => s.clone(),
                                    _ => v.to_string(),
                                };
                                format!("{k}={val}")
                            })
                            .collect();
                        if summary.is_empty() {
                            label.clone()
                        } else {
                            summary.join("; ")
                        }
                    } else {
                        label.clone()
                    }
                } else {
                    label.clone()
                };
                format!("{},{}", toon_escape(&label), toon_escape(&content_summary))
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        state_label(grain)
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.9, grain, hit)
    }
}

fn state_label(grain: &DeserializedGrain) -> String {
    // Search the state snapshot for label/description/title/name. The wire key is
    // `ctx`, which expands to `context` (OMS §8.3) — not `context_data`.
    if let Some(ctx) = grain.fields.get("context") {
        for key in &["label", "description", "title", "name"] {
            if let Some(s) = ctx.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        // Count keys
        if let Some(obj) = ctx.as_object() {
            return format!("{} keys", obj.len());
        }
    }
    "state".to_string()
}

// ---------------------------------------------------------------------------
// Workflow renderer
// ---------------------------------------------------------------------------

struct WorkflowRenderer;

/// How many edges a full workflow render will spell out before eliding. A plan
/// the model cannot see is a plan it cannot follow, but an unbounded graph would
/// swallow the whole context budget.
const WORKFLOW_EDGE_RENDER_CAP: usize = 12;

/// Render a workflow's topology as `src -> dst` steps, carrying edge conditions
/// and cycle bounds. Renderers used to emit only `(N nodes, M edges)`, so an
/// agent that recalled a workflow got its size and never its shape.
fn workflow_topology(grain: &DeserializedGrain) -> String {
    let Some(edges) = grain.fields.get("edges").and_then(|v| v.as_array()) else {
        // No edges: the nodes are unconnected steps (OMS §8.4).
        return grain
            .fields
            .get("nodes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
    };

    let mut parts: Vec<String> = Vec::new();
    for e in edges.iter().take(WORKFLOW_EDGE_RENDER_CAP) {
        let (Some(src), Some(dst)) = (
            e.get("src").and_then(|v| v.as_str()),
            e.get("dst").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let mut step = format!("{src} -> {dst}");
        if let Some(cond) = e.get("cond").and_then(|v| v.as_str()) {
            step.push_str(&format!(" [when {cond}]"));
        }
        if let Some(mc) = e.get("max_cycles").and_then(|v| v.as_u64()) {
            step.push_str(&format!(" [max {mc}x]"));
        }
        parts.push(step);
    }
    let elided = edges.len().saturating_sub(WORKFLOW_EDGE_RENDER_CAP);
    if elided > 0 {
        parts.push(format!("… +{elided} more"));
    }
    parts.join("; ")
}

/// `node=tool-hash` pairs, so a bound step shows what it will actually run.
fn workflow_bindings(grain: &DeserializedGrain) -> Option<String> {
    let obj = grain.fields.get("bindings")?.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut pairs: Vec<String> = obj
        .iter()
        .filter_map(|(node, hash)| hash.as_str().map(|h| format!("{node}={h}")))
        .collect();
    pairs.sort(); // deterministic output regardless of map order
    Some(pairs.join(", "))
}

impl GrainRenderer for WorkflowRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Workflow
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        // `name` has no OMS §8.4 field — it rides in extra_fields at top level.
        let label = grain
            .get_str("name")
            .or_else(|| grain.get_str("trigger"))
            .unwrap_or("workflow");
        let trigger = grain.get_str("trigger").unwrap_or("workflow");
        let node_count = grain
            .fields
            .get("nodes")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let edge_count = grain
            .fields
            .get("edges")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let topology = workflow_topology(grain);
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                let bindings = workflow_bindings(grain)
                    .map(|b| format!(" bindings=\"{}\"", sml_escape(&b)))
                    .unwrap_or_default();
                format!(
                    "<workflow trigger=\"{}\" nodes=\"{node_count}\" edges=\"{edge_count}\"{bindings}{attrs}>{}</workflow>",
                    sml_escape(trigger),
                    sml_escape(&topology)
                )
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                format!("**{label}** (on: {trigger}): {topology}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                format!("{label} (on: {trigger}): {topology}{suffix}")
            }
            OutputFormat::Json => {
                // Machine consumers get the real structures, not a summary.
                let mut obj = serde_json::json!({
                    "type": "workflow",
                    "trigger": trigger,
                    "node_count": node_count,
                    "edge_count": edge_count,
                });
                if let Some(n) = grain.get_str("name") {
                    obj["name"] = serde_json::Value::String(n.to_string());
                }
                for key in ["nodes", "edges", "bindings", "retries"] {
                    if let Some(v) = grain.fields.get(key) {
                        obj[key] = v.clone();
                    }
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — trigger, content
                format!("{},{}", toon_escape(trigger), toon_escape(&topology))
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let trigger = grain.get_str("trigger").unwrap_or("workflow");
        let node_count = grain
            .fields
            .get("nodes")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let edge_count = grain
            .fields
            .get("edges")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        format!("{trigger} ({node_count} nodes, {edge_count} edges)")
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.5, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Tool renderer
// ---------------------------------------------------------------------------

struct ToolRenderer;

impl GrainRenderer for ToolRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Tool
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let tool = grain.get_str("tool_name").unwrap_or("unknown");
        let is_error = grain.get_bool("is_error").unwrap_or(false);
        let status = if is_error { "FAIL" } else { "OK" };
        let content = grain
            .get_str("tool_content")
            .or_else(|| grain.get_str("content"))
            .unwrap_or("");
        let error_msg = grain.get_str("error");
        let duration = grain.get_u64("duration_ms");
        let meta = extract_metadata(grain, policy.metadata);

        let dur_str = duration.map(|d| format!(" ({d}ms)")).unwrap_or_default();
        let result_str = if is_error {
            error_msg.unwrap_or(content)
        } else {
            content
        };

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                let mut sml = format!(
                    "<tool tool=\"{}\" status=\"{}\"{attrs}>",
                    sml_escape(tool),
                    status.to_lowercase()
                );
                if !result_str.is_empty() {
                    sml.push_str(&sml_escape(truncate(result_str, 300)));
                }
                if let Some(d) = duration {
                    sml.push_str(&format!(" ({d}ms)"));
                }
                sml.push_str("</tool>");
                sml
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                let result_part = if !result_str.is_empty() {
                    format!(": {}", truncate(result_str, 200))
                } else {
                    String::new()
                };
                format!("`{tool}` [{status}]{dur_str}{result_part}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                let result_part = if !result_str.is_empty() {
                    format!(": {}", truncate(result_str, 200))
                } else {
                    String::new()
                };
                format!("{tool} [{status}]{dur_str}{result_part}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "tool",
                    "tool_name": tool,
                    "status": status.to_lowercase(),
                });
                if !result_str.is_empty() {
                    obj["content"] = serde_json::Value::String(result_str.to_string());
                }
                if let Some(d) = duration {
                    obj["duration_ms"] = serde_json::json!(d);
                }
                if let Some(e) = error_msg {
                    obj["error"] = serde_json::Value::String(e.to_string());
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — tool, phase, content
                let phase = status.to_lowercase();
                let content_val = if !result_str.is_empty() {
                    truncate(result_str, 300)
                } else {
                    ""
                };
                format!(
                    "{},{},{}",
                    toon_escape(tool),
                    toon_escape(&phase),
                    toon_escape(content_val)
                )
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let tool = grain.get_str("tool_name").unwrap_or("unknown");
        let is_error = grain.get_bool("is_error").unwrap_or(false);
        let status = if is_error { "FAIL" } else { "OK" };
        format!("{tool} [{status}]")
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        let is_error = grain.get_bool("is_error").unwrap_or(false);
        let error_boost = if is_error { 0.15 } else { 0.0 };
        adjusted_priority(0.5 + error_boost, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Observation renderer
// ---------------------------------------------------------------------------

struct ObservationRenderer;

impl GrainRenderer for ObservationRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Observation
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let observer = grain.get_str("observer_id").unwrap_or("?");
        let subject = grain.get_str("subject").unwrap_or("");
        let object = grain.get_str("object").unwrap_or("");
        let meta = extract_metadata(grain, policy.metadata);

        let content = if !subject.is_empty() && !object.is_empty() {
            format!("{subject}: {object}")
        } else if !subject.is_empty() {
            subject.to_string()
        } else if !object.is_empty() {
            object.to_string()
        } else {
            String::new()
        };

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                format!(
                    "<observation observer=\"{}\"{attrs}>{}</observation>",
                    sml_escape(observer),
                    sml_escape(&content)
                )
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                if content.is_empty() {
                    format!("*{observer}*{suffix}")
                } else {
                    format!("*{observer}*: {content}{suffix}")
                }
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                if content.is_empty() {
                    format!("{observer}{suffix}")
                } else {
                    format!("{observer}: {content}{suffix}")
                }
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "observation",
                    "observer_id": observer,
                });
                if !subject.is_empty() {
                    obj["subject"] = serde_json::Value::String(subject.to_string());
                }
                if !object.is_empty() {
                    obj["object"] = serde_json::Value::String(object.to_string());
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — observer, content
                format!("{},{}", toon_escape(observer), toon_escape(&content))
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let observer = grain.get_str("observer_id").unwrap_or("?");
        let subject = grain.get_str("subject").unwrap_or("");
        if subject.is_empty() {
            observer.to_string()
        } else {
            format!("{observer}: {subject}")
        }
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.6, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Goal renderer
// ---------------------------------------------------------------------------

struct GoalRenderer;

impl GrainRenderer for GoalRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Goal
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let desc = grain.get_str("description").unwrap_or("?");
        let state = grain.get_str("goal_state").unwrap_or("active");
        let priority = grain.get_str("priority").unwrap_or("medium");
        let progress = grain.get_f64("progress");
        let criteria = grain.get_str("criteria");
        let meta = extract_metadata(grain, policy.metadata);

        let progress_str = progress
            .map(|p| format!(" ({:.0}%)", p * 100.0))
            .unwrap_or_default();

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                let mut sml = format!(
                    "<goal state=\"{}\" priority=\"{}\"{attrs}>{}</goal>",
                    sml_escape(state),
                    sml_escape(priority),
                    sml_escape(desc)
                );
                if let Some(c) = criteria {
                    // Insert criteria before closing tag
                    sml = sml.replace(
                        "</goal>",
                        &format!("<criteria>{}</criteria></goal>", sml_escape(c)),
                    );
                }
                sml
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                format!("[{priority}/{state}] {desc}{progress_str}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                format!("[{priority}/{state}] {desc}{progress_str}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "goal",
                    "description": desc,
                    "goal_state": state,
                    "priority": priority,
                });
                if let Some(p) = progress {
                    obj["progress"] = serde_json::json!(p);
                }
                if let Some(c) = criteria {
                    obj["criteria"] = serde_json::Value::String(c.to_string());
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — subject, content, state
                let subject = grain.get_str("subject").unwrap_or(desc);
                format!(
                    "{},{},{}",
                    toon_escape(subject),
                    toon_escape(desc),
                    toon_escape(state)
                )
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let desc = grain.get_str("description").unwrap_or("?");
        let state = grain.get_str("goal_state").unwrap_or("active");
        let priority = grain.get_str("priority").unwrap_or("medium");
        format!("[{priority}/{state}] {desc}")
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        let state_mod = match grain.get_str("goal_state") {
            Some("active") => 0.15,
            Some("suspended") => 0.05,
            Some("satisfied" | "failed") => -0.2,
            _ => 0.0,
        };
        let priority_mod = match grain.get_str("priority") {
            Some("critical") => 0.1,
            Some("high") => 0.05,
            Some("low") => -0.1,
            _ => 0.0,
        };
        adjusted_priority(0.8 + state_mod + priority_mod, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Reasoning renderer
// ---------------------------------------------------------------------------

struct ReasoningRenderer;

impl GrainRenderer for ReasoningRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Reasoning
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let conclusion = grain.get_str("conclusion").unwrap_or("");
        let method = grain.get_str("inference_method");
        let premise_count = grain
            .fields
            .get("premises")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let meta = extract_metadata(grain, policy.metadata);

        let method_str = method.map(|m| format!(" ({m})")).unwrap_or_default();

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                let mut sml = format!("<reasoning{attrs}>");
                if !conclusion.is_empty() {
                    sml.push_str(&format!(
                        "<conclusion>{}</conclusion>",
                        sml_escape(conclusion)
                    ));
                }
                if premise_count > 0 {
                    sml.push_str(&format!("<premises count=\"{premise_count}\"/>"));
                }
                sml.push_str("</reasoning>");
                sml
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                if conclusion.is_empty() {
                    format!("Reasoning{method_str} ({premise_count} premises){suffix}")
                } else {
                    format!("{conclusion}{method_str}{suffix}")
                }
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                if conclusion.is_empty() {
                    format!("Reasoning{method_str} ({premise_count} premises){suffix}")
                } else {
                    format!("{conclusion}{method_str}{suffix}")
                }
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "reasoning",
                    "premise_count": premise_count,
                });
                if !conclusion.is_empty() {
                    obj["conclusion"] = serde_json::Value::String(conclusion.to_string());
                }
                if let Some(m) = method {
                    obj["inference_method"] = serde_json::Value::String(m.to_string());
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — type, content
                let type_val = method.unwrap_or("reasoning");
                let content_val = if conclusion.is_empty() {
                    format!("{} premises", premise_count)
                } else {
                    conclusion.to_string()
                };
                format!("{},{}", toon_escape(type_val), toon_escape(&content_val))
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        grain
            .get_str("conclusion")
            .unwrap_or("reasoning")
            .to_string()
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.7, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Consensus renderer
// ---------------------------------------------------------------------------

struct ConsensusRenderer;

impl GrainRenderer for ConsensusRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Consensus
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let agreed = grain.get_str("agreed_content").unwrap_or("");
        let agree_count = grain.get_i64("agreement_count").unwrap_or(0);
        let dissent_count = grain.get_i64("dissent_count").unwrap_or(0);
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                format!(
                    "<consensus agreed=\"{agree_count}\" dissent=\"{dissent_count}\"{attrs}>{}</consensus>",
                    sml_escape(agreed)
                )
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                let vote_str = if agree_count > 0 || dissent_count > 0 {
                    format!(" ({agree_count} agreed, {dissent_count} dissent)")
                } else {
                    String::new()
                };
                if agreed.is_empty() {
                    format!("Consensus{vote_str}{suffix}")
                } else {
                    format!("{agreed}{vote_str}{suffix}")
                }
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                let vote_str = if agree_count > 0 || dissent_count > 0 {
                    format!(" ({agree_count}/{dissent_count})")
                } else {
                    String::new()
                };
                if agreed.is_empty() {
                    format!("Consensus{vote_str}{suffix}")
                } else {
                    format!("{agreed}{vote_str}{suffix}")
                }
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "consensus",
                    "agreement_count": agree_count,
                    "dissent_count": dissent_count,
                });
                if !agreed.is_empty() {
                    obj["agreed_content"] = serde_json::Value::String(agreed.to_string());
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — threshold, count, content
                let threshold = grain.get_str("threshold").unwrap_or("-");
                format!(
                    "{},{},{}",
                    toon_escape(threshold),
                    agree_count,
                    toon_escape(agreed)
                )
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        grain
            .get_str("agreed_content")
            .unwrap_or("consensus")
            .to_string()
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.65, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Consent renderer
// ---------------------------------------------------------------------------

struct ConsentRenderer;

impl GrainRenderer for ConsentRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Consent
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let subject = grain.get_str("subject_did").unwrap_or("?");
        let is_withdrawal = grain.get_bool("is_withdrawal").unwrap_or(false);
        let action = if is_withdrawal { "withdraws" } else { "grants" };
        let scope = grain.get_str("scope").unwrap_or("");
        let grantee = grain.get_str("grantee_did");
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                let mut sml = format!(
                    "<consent action=\"{action}\" subject=\"{}\"{attrs}>",
                    sml_escape(subject)
                );
                if !scope.is_empty() {
                    sml.push_str(&format!("<scope>{}</scope>", sml_escape(scope)));
                }
                if let Some(g) = grantee {
                    sml.push_str(&format!("<grantee>{}</grantee>", sml_escape(g)));
                }
                sml.push_str("</consent>");
                sml
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                let scope_part = if scope.is_empty() {
                    String::new()
                } else {
                    format!(" for {scope}")
                };
                format!("{subject} {action}{scope_part}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                let scope_part = if scope.is_empty() {
                    String::new()
                } else {
                    format!(" for {scope}")
                };
                format!("{subject} {action}{scope_part}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "consent",
                    "subject_did": subject,
                    "action": action,
                });
                if !scope.is_empty() {
                    obj["scope"] = serde_json::Value::String(scope.to_string());
                }
                if let Some(g) = grantee {
                    obj["grantee_did"] = serde_json::Value::String(g.to_string());
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — grantor, grantee, action, content
                let grantee_val = grantee.unwrap_or("-");
                format!(
                    "{},{},{},{}",
                    toon_escape(subject),
                    toon_escape(grantee_val),
                    toon_escape(action),
                    toon_escape(scope)
                )
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let subject = grain.get_str("subject_did").unwrap_or("?");
        let is_withdrawal = grain.get_bool("is_withdrawal").unwrap_or(false);
        let action = if is_withdrawal { "withdraws" } else { "grants" };
        let scope = grain.get_str("scope").unwrap_or("");
        if scope.is_empty() {
            format!("{subject} {action}")
        } else {
            format!("{subject} {action} {scope}")
        }
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        // Consent grains get highest base priority — legally required context
        adjusted_priority(0.95, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// Skill renderer (OMS 1.4) — data projection only (name + description +
// domain + proficiency). `instructions`/`when_to_use` are deliberately NOT
// rendered raw (design §13 non-blocking note).
// ---------------------------------------------------------------------------

/// SML 1.1 `<recommendation>` — renders the proposal summary with `target` and
/// `severity` attributes.
struct RecommendationRenderer;

/// A recommendation's `summary` is a template id plus args, never prose
/// (OMS §8.12 rule 4). Without the template registry to render it here, show
/// the argument values — what a reviewer actually needs to see — rather than
/// the template id, which means nothing on its own.
fn recommendation_summary(grain: &DeserializedGrain) -> String {
    let args = grain.fields.get("summary").and_then(|s| s.get("args"));
    let Some(obj) = args.and_then(|a| a.as_object()) else {
        return grain
            .get_str("target_ref")
            .unwrap_or("recommendation")
            .to_string();
    };
    let mut parts: Vec<String> = obj
        .iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect();
    parts.sort(); // deterministic: the arg map has no inherent order
    if parts.is_empty() {
        grain
            .get_str("target_ref")
            .unwrap_or("recommendation")
            .to_string()
    } else {
        parts.join(", ")
    }
}

impl GrainRenderer for RecommendationRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Recommendation
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let target = grain.get_str("target_ref").unwrap_or("");
        let severity = grain.get_str("severity").unwrap_or("info");
        let summary = recommendation_summary(grain);
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                format!(
                    "<recommendation target=\"{}\" severity=\"{}\"{attrs}>{}</recommendation>",
                    sml_escape(target),
                    sml_escape(severity),
                    sml_escape(&summary)
                )
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                format!("[{severity}] {summary} → `{target}`{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                format!("[{severity}] {summary} -> {target}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "recommendation",
                    "target_ref": target,
                    "severity": severity,
                    "summary": summary,
                });
                // The reviewable substance: who proposed it, the dedup identity,
                // the change itself, and the status the index layer resolved.
                for key in [
                    "analyzer",
                    "dedup_key",
                    "proposal_cal",
                    "proposal_edit",
                    "proposal_data",
                    "metric_snapshot",
                    "evidence_query",
                    "rec_status",
                ] {
                    if let Some(v) = grain.fields.get(key) {
                        obj[key] = v.clone();
                    }
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => format!(
                "{},{},{}",
                toon_escape(target),
                toon_escape(severity),
                toon_escape(&summary)
            ),
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        format!(
            "[{}] {}",
            grain.get_str("severity").unwrap_or("info"),
            recommendation_summary(grain)
        )
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        // A pending proposal is something a reviewer must act on, so it ranks
        // above passive records but below the facts it is reasoning about.
        adjusted_priority(0.7, grain, hit)
    }
}

struct SkillRenderer;

impl GrainRenderer for SkillRenderer {
    fn grain_type(&self) -> GrainType {
        GrainType::Skill
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        let name = grain.get_str("name").unwrap_or("?");
        let description = grain.get_str("description").unwrap_or("");
        let domain = grain.get_str("domain");
        // proficiency aliases confidence (D3) — present only on held instances.
        let proficiency = grain
            .get_f64("proficiency")
            .or_else(|| grain.get_f64("confidence"));
        let meta = extract_metadata(grain, policy.metadata);

        match &policy.format {
            OutputFormat::Sml => {
                let attrs = meta
                    .as_ref()
                    .map(|m| format_meta_sml_attrs(m, policy.metadata))
                    .unwrap_or_default();
                let domain_attr = domain
                    .map(|d| format!(" domain=\"{}\"", sml_escape(d)))
                    .unwrap_or_default();
                let mut sml = format!("<skill name=\"{}\"{domain_attr}{attrs}>", sml_escape(name));
                if !description.is_empty() {
                    sml.push_str(&sml_escape(description));
                }
                sml.push_str("</skill>");
                sml
            }
            OutputFormat::Markdown => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_markdown(m, policy.metadata))
                    .unwrap_or_default();
                let domain_part = domain.map(|d| format!(" [{d}]")).unwrap_or_default();
                format!("**{name}**: {description}{domain_part}{suffix}")
            }
            OutputFormat::PlainText => {
                let suffix = meta
                    .as_ref()
                    .map(|m| format_meta_plain(m, policy.metadata))
                    .unwrap_or_default();
                let domain_part = domain.map(|d| format!(" [{d}]")).unwrap_or_default();
                format!("{name}: {description}{domain_part}{suffix}")
            }
            OutputFormat::Json => {
                let mut obj = serde_json::json!({
                    "type": "skill",
                    "name": name,
                    "description": description,
                });
                if let Some(d) = domain {
                    obj["domain"] = serde_json::Value::String(d.to_string());
                }
                if let Some(p) = proficiency {
                    obj["proficiency"] = serde_json::json!(p);
                }
                if let Some(ref m) = meta {
                    add_json_metadata(&mut obj, m, policy.metadata);
                }
                obj.to_string()
            }
            OutputFormat::Toon => {
                // CAL spec Section 10.9.3: CSV row — name, domain, proficiency
                let prof_str = proficiency
                    .map(toon_canonicalize_number)
                    .unwrap_or_default();
                format!(
                    "{},{},{}",
                    toon_escape(name),
                    toon_escape(domain.unwrap_or("-")),
                    toon_escape(&prof_str)
                )
            }
        }
    }

    fn render_summary(&self, grain: &DeserializedGrain, _policy: &FormatPolicy) -> String {
        let name = grain.get_str("name").unwrap_or("?");
        match grain.get_str("domain") {
            Some(d) => format!("{name} [{d}]"),
            None => name.to_string(),
        }
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        adjusted_priority(0.6, grain, hit)
    }
}

// ---------------------------------------------------------------------------
// JSON metadata helper
// ---------------------------------------------------------------------------

fn add_json_metadata(obj: &mut serde_json::Value, meta: &MetadataFragment, level: MetadataLevel) {
    if let Some(c) = meta.confidence {
        obj["confidence"] = serde_json::json!(c);
    }
    if meta.created_at_sec > 0 {
        obj["created_at"] = serde_json::Value::String(format_date(meta.created_at_sec));
    }
    if level == MetadataLevel::Full {
        obj["hash"] = serde_json::Value::String(meta.hash_hex[..16].to_string());
        if let Some(ref ns) = meta.namespace {
            obj["namespace"] = serde_json::Value::String(ns.clone());
        }
        if let Some(ref vs) = meta.verification_status {
            obj["verification_status"] = serde_json::Value::String(vs.clone());
        }
        if !meta.tags.is_empty() {
            obj["tags"] = serde_json::json!(meta.tags);
        }
        if let Some(ref st) = meta.source_type {
            obj["source_type"] = serde_json::Value::String(st.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use areev_core::format::deserialize::DeserializedGrain;
    use areev_core::format::header::MgHeader;
    use std::collections::HashMap;

    /// Build a test grain with given type and fields.
    fn test_grain(gt: GrainType, fields: Vec<(&str, serde_json::Value)>) -> DeserializedGrain {
        let mut map = HashMap::new();
        for (k, v) in fields {
            map.insert(k.to_string(), v);
        }
        DeserializedGrain {
            header: MgHeader {
                version: 1,
                flags: 0,
                grain_type: gt.type_byte(),
                ns_hash: 0,
                created_at_sec: 1740000000, // 2025-02-19
            },
            grain_type: gt,
            fields: map,
            hash: areev_core::error::Hash::from_bytes(&[0u8; 32]),
        }
    }

    fn test_hit(grain: DeserializedGrain) -> SearchHit {
        SearchHit {
            hash: grain.hash,
            score: 0.85,
            grain,
            score_breakdown: None,
            #[cfg(feature = "rerank")]
            rerank_score: None,
            #[cfg(feature = "llm-rerank")]
            llm_rerank_score: None,
            explanation: None,
            scope_depth: None,
            source_namespace: None,
            relative_time: None,
            conflict_status: None,
            supersession_status: None,
            superseded_by_hash: None,
            recall_source: None,
        }
    }

    #[test]
    fn test_fact_sml() {
        let grain = test_grain(
            GrainType::Fact,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
                ("confidence", serde_json::json!(0.95)),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::Minimal);
        let registry = RendererRegistry::new();
        let renderer = registry.get(GrainType::Fact).unwrap();
        let result = renderer.render(&grain, &policy);
        assert!(result.starts_with("<fact"));
        assert!(result.contains("confidence=\"0.95\""));
        assert!(result.contains("john likes coffee"));
        assert!(result.ends_with("</fact>"));
    }

    #[test]
    fn test_fact_markdown() {
        let grain = test_grain(
            GrainType::Fact,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::Fact)
            .unwrap()
            .render(&grain, &policy);
        assert_eq!(result, "**john** likes coffee");
    }

    #[test]
    fn test_fact_summary() {
        let grain = test_grain(
            GrainType::Fact,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
            ],
        );
        let policy = FormatPolicy::default();
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::Fact)
            .unwrap()
            .render_summary(&grain, &policy);
        assert_eq!(result, "john likes coffee");
    }

    #[test]
    fn test_tool_sml() {
        let grain = test_grain(
            GrainType::Tool,
            vec![
                ("tool_name", serde_json::json!("search_api")),
                ("is_error", serde_json::json!(false)),
                ("tool_content", serde_json::json!("12 results")),
                ("duration_ms", serde_json::json!(340)),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::Tool)
            .unwrap()
            .render(&grain, &policy);
        assert!(result.contains("tool=\"search_api\""));
        assert!(result.contains("status=\"ok\""));
        assert!(result.contains("12 results"));
        assert!(result.contains("340ms"));
    }

    #[test]
    fn test_goal_plaintext() {
        let grain = test_grain(
            GrainType::Goal,
            vec![
                ("description", serde_json::json!("Deploy v2")),
                ("goal_state", serde_json::json!("active")),
                ("priority", serde_json::json!("high")),
                ("progress", serde_json::json!(0.78)),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::Goal)
            .unwrap()
            .render(&grain, &policy);
        assert_eq!(result, "[high/active] Deploy v2 (78%)");
    }

    #[test]
    fn test_consent_markdown() {
        let grain = test_grain(
            GrainType::Consent,
            vec![
                ("subject_did", serde_json::json!("did:john")),
                ("is_withdrawal", serde_json::json!(false)),
                ("scope", serde_json::json!("analytics")),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::Consent)
            .unwrap()
            .render(&grain, &policy);
        assert_eq!(result, "did:john grants for analytics");
    }

    /// A workflow fixture: `build -> test -> deploy`, with a condition on the
    /// last hop and a tool bound to `test`.
    fn workflow_fixture() -> DeserializedGrain {
        test_grain(
            GrainType::Workflow,
            vec![
                ("name", serde_json::json!("CI pipeline")),
                ("trigger", serde_json::json!("merge to main")),
                ("nodes", serde_json::json!(["build", "test", "deploy"])),
                (
                    "edges",
                    serde_json::json!([
                        { "src": "build", "dst": "test" },
                        { "src": "test", "dst": "deploy", "cond": "tests green" },
                    ]),
                ),
                ("bindings", serde_json::json!({ "test": "sha256:abc" })),
            ],
        )
    }

    fn render_with(grain: &DeserializedGrain, fmt: OutputFormat) -> String {
        let policy = FormatPolicy::new(fmt).metadata(MetadataLevel::None);
        RendererRegistry::new()
            .get(grain.grain_type)
            .unwrap()
            .render(grain, &policy)
    }

    #[test]
    fn workflow_render_shows_topology_not_just_counts() {
        let g = workflow_fixture();

        let plain = render_with(&g, OutputFormat::PlainText);
        assert!(
            plain.contains("build -> test") && plain.contains("test -> deploy"),
            "topology must be visible, got: {plain}"
        );
        assert!(
            plain.contains("CI pipeline") && plain.contains("merge to main"),
            "name and trigger must both appear, got: {plain}"
        );
        assert!(
            plain.contains("tests green"),
            "edge conditions decide the path — they must render: {plain}"
        );
    }

    #[test]
    fn workflow_render_json_carries_the_real_structures() {
        let out = render_with(&workflow_fixture(), OutputFormat::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["nodes"], serde_json::json!(["build", "test", "deploy"]));
        assert_eq!(v["edges"][1]["cond"], "tests green");
        assert_eq!(v["bindings"]["test"], "sha256:abc");
        assert_eq!(v["name"], "CI pipeline");
        // Counts stay for consumers that only want the shape's size.
        assert_eq!(v["node_count"], 3);
        assert_eq!(v["edge_count"], 2);
    }

    #[test]
    fn workflow_render_elides_very_large_graphs() {
        // Rendering an unbounded graph would swallow the context budget.
        let edges: Vec<serde_json::Value> = (0..40)
            .map(|i| serde_json::json!({ "src": format!("n{i}"), "dst": format!("n{}", i + 1) }))
            .collect();
        let g = test_grain(
            GrainType::Workflow,
            vec![
                ("trigger", serde_json::json!("big")),
                ("edges", serde_json::Value::Array(edges)),
            ],
        );
        let plain = render_with(&g, OutputFormat::PlainText);
        assert!(plain.contains("n0 -> n1"), "got: {plain}");
        assert!(
            plain.contains("+28 more"),
            "must report what it elided, got: {plain}"
        );
        assert!(!plain.contains("n39 -> n40"), "cap must hold: {plain}");
    }

    #[test]
    fn workflow_render_handles_unconnected_nodes() {
        // OMS §8.4: "When absent, nodes are unconnected."
        let g = test_grain(
            GrainType::Workflow,
            vec![
                ("trigger", serde_json::json!("adhoc")),
                ("nodes", serde_json::json!(["a", "b"])),
            ],
        );
        let plain = render_with(&g, OutputFormat::PlainText);
        assert!(plain.contains('a') && plain.contains('b'), "got: {plain}");
    }

    #[test]
    fn state_render_json_includes_plan_and_history() {
        let g = test_grain(
            GrainType::State,
            vec![
                ("context", serde_json::json!({ "label": "mid-run" })),
                ("plan", serde_json::json!(["fetch", "load"])),
                ("history", serde_json::json!([{ "node": "fetch" }])),
            ],
        );
        let out = render_with(&g, OutputFormat::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["plan"], serde_json::json!(["fetch", "load"]));
        assert_eq!(v["history"][0]["node"], "fetch");
    }

    #[test]
    fn test_state_label_extraction() {
        // The key is `context` — what `ctx` expands to on deserialize (OMS §8.3).
        // This test used to fabricate `context_data`, a key the serializer never
        // writes, which masked the renderer reading a field that is never present.
        let grain = test_grain(
            GrainType::State,
            vec![(
                "context",
                serde_json::json!({"label": "planning_phase", "data": {}}),
            )],
        );
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::State)
            .unwrap()
            .render(&grain, &policy);
        assert_eq!(result, "State: planning_phase");
    }

    #[test]
    fn test_context_priority_consent_highest() {
        let grain = test_grain(
            GrainType::Consent,
            vec![("subject_did", serde_json::json!("did:x"))],
        );
        let hit = test_hit(grain.clone());
        let registry = RendererRegistry::new();
        let consent_pri = registry
            .get(GrainType::Consent)
            .unwrap()
            .context_priority(&grain, &hit);

        let fact_grain = test_grain(GrainType::Fact, vec![("subject", serde_json::json!("x"))]);
        let fact_hit = test_hit(fact_grain.clone());
        let fact_pri = registry
            .get(GrainType::Fact)
            .unwrap()
            .context_priority(&fact_grain, &fact_hit);

        assert!(
            consent_pri > fact_pri,
            "Consent ({consent_pri}) should outrank Fact ({fact_pri})"
        );
    }

    #[test]
    fn test_json_format_includes_type() {
        let grain = test_grain(
            GrainType::Fact,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::Fact)
            .unwrap()
            .render(&grain, &policy);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["type"], "fact");
        assert_eq!(parsed["subject"], "john");
    }

    #[test]
    fn test_format_date() {
        assert_eq!(format_date(1740000000), "2025-02-19");
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1609459200), "2021-01-01");
    }

    #[test]
    fn test_sml_escape() {
        assert_eq!(sml_escape("a < b & c"), "a &lt; b &amp; c");
        assert_eq!(sml_escape("\"hello\""), "&quot;hello&quot;");
    }

    #[test]
    fn test_fact_toon() {
        let grain = test_grain(
            GrainType::Fact,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
                ("confidence", serde_json::json!(0.95)),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Toon).metadata(MetadataLevel::Minimal);
        let registry = RendererRegistry::new();
        let renderer = registry.get(GrainType::Fact).unwrap();
        let rendered = renderer.render(&grain, &policy);
        // TOON tabular: CSV row with subject, content (humanized), confidence
        assert_eq!(rendered, "john,likes coffee,0.95");
    }

    #[test]
    fn test_toon_escape() {
        assert_eq!(toon_escape("hello"), "hello");
        assert_eq!(toon_escape(""), "\"\"");
        assert_eq!(toon_escape("true"), "\"true\"");
        assert_eq!(toon_escape("has:colon"), "\"has:colon\"");
        assert_eq!(toon_escape("has,comma"), "\"has,comma\"");
        assert_eq!(toon_escape("has\"quote"), "\"has\\\"quote\"");
        assert_eq!(toon_escape("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(toon_escape("42"), "\"42\"");
        assert_eq!(toon_escape("simple text"), "simple text");
    }

    #[test]
    fn test_toon_columns() {
        assert_eq!(
            toon_columns(&GrainType::Fact),
            &["subject", "content", "confidence"]
        );
        assert_eq!(
            toon_columns(&GrainType::Event),
            &["role", "time", "content"]
        );
        assert_eq!(
            toon_columns(&GrainType::Consent),
            &["grantor", "grantee", "action", "content"]
        );
    }

    #[test]
    fn test_event_sml_renders_content_blocks_as_typed_tags() {
        let blocks = serde_json::json!([
            { "type": "text", "text": "hello <world>" },
            { "type": "tool_use", "id": "tu_1", "name": "search", "input": {"q": "a&b"} },
            { "type": "tool_result", "tool_use_id": "tu_1", "content": "done", "is_error": false },
        ]);
        let grain = test_grain(
            GrainType::Event,
            vec![
                ("content", serde_json::json!("fallback")),
                ("content_blocks", blocks),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let out = registry
            .get(GrainType::Event)
            .unwrap()
            .render(&grain, &policy);
        assert!(out.starts_with("<event"));
        assert!(out.contains("<text>hello &lt;world&gt;</text>"));
        assert!(out.contains("<tool_use name=\"search\" id=\"tu_1\">"));
        assert!(out.contains("<args format=\"json\">"));
        assert!(out.contains("a&amp;b"));
        assert!(
            out.contains("<tool_result tool_use_id=\"tu_1\" is_error=\"false\">done</tool_result>")
        );
        // Fallback text is bypassed when content_blocks renders anything.
        assert!(!out.contains("fallback"));
    }

    #[test]
    fn test_event_sml_empty_blocks_falls_back_to_content() {
        let grain = test_grain(
            GrainType::Event,
            vec![
                ("content", serde_json::json!("plain text")),
                ("content_blocks", serde_json::json!([])),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let out = registry
            .get(GrainType::Event)
            .unwrap()
            .render(&grain, &policy);
        assert!(out.contains("plain text"));
    }

    #[test]
    fn test_all_renderers_registered() {
        let registry = RendererRegistry::new();
        let types = [
            GrainType::Fact,
            GrainType::Event,
            GrainType::State,
            GrainType::Workflow,
            GrainType::Tool,
            GrainType::Observation,
            GrainType::Goal,
            GrainType::Reasoning,
            GrainType::Consensus,
            GrainType::Consent,
        ];
        for gt in &types {
            assert!(registry.get(*gt).is_some(), "Missing renderer for {:?}", gt);
        }
    }

    /// XML metacharacters must all be escaped; bidi controls pass through
    /// (documented scope — stripping is the LLM-output-layer's job).
    #[test]
    fn test_sml_escape_metacharacters_and_bidi_passthrough() {
        assert_eq!(sml_escape("<a>"), "&lt;a&gt;");
        assert_eq!(sml_escape("a & b"), "a &amp; b");
        assert_eq!(sml_escape("\"q\""), "&quot;q&quot;");
        assert_eq!(sml_escape("it's"), "it&apos;s");
        // Bidi controls (U+202E right-to-left override, U+2066 LRI) and ZWJ
        // must survive verbatim — documented passthrough.
        let bidi = "safe\u{202E}evil\u{2066}x\u{200D}y";
        assert_eq!(sml_escape(bidi), bidi);
    }
}
