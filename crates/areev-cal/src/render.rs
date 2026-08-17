//! The one per-grain rendering implementation, shared by every surface.
//!
//! CAL's `FORMAT` arms (executor.rs) and `areev-context`'s `ContextAssembler`
//! both call these functions, so a given grain renders identically wherever it
//! appears. Envelopes — the `<grains>` wrapper, `GROUP BY` groups, budget
//! sections, timeline/census modes — remain per-surface policy; the unit that
//! is shared is the rendering of ONE grain in ONE format.
//!
//! Format ownership:
//! - `sml` — semantic per-type elements (`<fact confidence="0.95">…</fact>`),
//!   originally from areev-context; the richer of the two prior stacks.
//! - `markdown` — the documented CAL shape (cal-reference.md §6 "What
//!   markdown renders"): an assertion line, bold subject, trailing italics
//!   carrying only weight-relevant metadata — extended with per-type arms for
//!   the grain types the old cascade could only dump as field pairs.
//! - `text` — deliberately minimal, field-shape driven.
//! - `toon` — registry-driven columns (`registry::meta(ty).toon_columns`),
//!   one emitter for rows and section headers.
//! - `json` — the envelope (`hash`, `grain_type`, `fields`); the CAL executor
//!   adds its per-query annotations (score, breakdowns) on top.
//!
//! These functions are pure: no store access, no clock, no host config.

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Input view
// ---------------------------------------------------------------------------

/// How much metadata a render carries. Mirrors `areev-context`'s
/// `MetadataLevel` (which converts into this).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataDetail {
    /// Content only.
    None,
    /// Confidence and date — what changes how much weight to give the line.
    Minimal,
    /// Everything: hash prefix, namespace, verification status, tags.
    Full,
}

/// OMS §4 progressive disclosure: how much of a grain's BODY a render carries.
/// Deliberately orthogonal to [`MetadataDetail`], which governs the envelope
/// (hash, namespace, confidence) rather than the content.
///
/// The distinction that matters is at [`Disclosure::Full`]: some grain types
/// carry a long-form definition body that no other tier renders — a Skill's
/// `instructions` is the field that *is* the skill, and until `full` existed it
/// could not be reached through any rendered path at all, only through raw JSON
/// recall, which defeats budgeted assembly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disclosure {
    /// Terse: bodies clipped hard, long-form definitions omitted.
    Summary,
    /// Middle: bodies clipped, long-form definitions still omitted.
    Headlines,
    /// Everything, long-form definition bodies included.
    Full,
}

impl Disclosure {
    /// Character cap for a free-text body at this tier. Mirrors the ladder
    /// `templates::effective_truncate` already applies to budgeted template
    /// renders, so one word means one thing across both.
    fn body_cap(self) -> usize {
        match self {
            Disclosure::Summary => 40,
            Disclosure::Headlines => 80,
            Disclosure::Full => usize::MAX,
        }
    }

    /// Whether this tier carries long-form definition bodies.
    fn carries_definitions(self) -> bool {
        matches!(self, Disclosure::Full)
    }
}

/// Body cap for a render, given an optional requested tier. `None` — no
/// `progressive_disclosure` asked for — keeps each arm's established default,
/// so an unannotated query renders byte-for-byte as it always has.
fn body_cap(disclosure: Option<Disclosure>, default: usize) -> usize {
    disclosure.map_or(default, Disclosure::body_cap)
}

/// A borrowed, surface-neutral view of one grain. The CAL executor builds it
/// from a `CalGrainResult`, `areev-context` from a `DeserializedGrain`; both
/// views of the same stored grain render to the same bytes.
pub struct GrainView<'a> {
    /// Canonical grain type name (`"fact"`, `"event"`, …).
    pub grain_type: &'a str,
    /// Content-address hash, hex.
    pub hash: &'a str,
    /// The grain's fields as a JSON object.
    pub fields: &'a serde_json::Value,
    /// Creation time, epoch seconds. `0` = unknown.
    pub created_at_sec: u32,
}

impl<'a> GrainView<'a> {
    fn get_str(&self, key: &str) -> Option<&'a str> {
        self.fields.get(key).and_then(|v| v.as_str())
    }
    fn get_f64(&self, key: &str) -> Option<f64> {
        self.fields.get(key).and_then(|v| v.as_f64())
    }
    fn get_bool(&self, key: &str) -> Option<bool> {
        self.fields.get(key).and_then(|v| v.as_bool())
    }
    fn get_u64(&self, key: &str) -> Option<u64> {
        self.fields.get(key).and_then(|v| v.as_u64())
    }
    fn get_i64(&self, key: &str) -> Option<i64> {
        self.fields.get(key).and_then(|v| v.as_i64())
    }
}

/// Epoch seconds from a fields map whose `created_at` is epoch milliseconds
/// (the CAL result convention). Returns 0 when absent or non-positive.
pub fn created_at_sec_from_fields(fields: &serde_json::Value) -> u32 {
    fields
        .get("created_at")
        .and_then(serde_json::Value::as_i64)
        .filter(|ms| *ms > 0)
        .map(|ms| (ms / 1000) as u32)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Metadata fragment (shared by sml / markdown / json renders)
// ---------------------------------------------------------------------------

struct MetadataFragment {
    confidence: Option<f64>,
    created_at_sec: u32,
    hash_hex: String,
    tags: Vec<String>,
    namespace: Option<String>,
    verification_status: Option<String>,
}

fn extract_metadata(view: &GrainView, level: MetadataDetail) -> Option<MetadataFragment> {
    match level {
        MetadataDetail::None => None,
        MetadataDetail::Minimal | MetadataDetail::Full => {
            let (tags, namespace, verification_status) =
                if level == MetadataDetail::Full {
                    let tags = view
                        .fields
                        .get("structural_tags")
                        .or_else(|| view.fields.get("tags"))
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        tags,
                        view.get_str("namespace").map(String::from),
                        view.get_str("verification_status").map(String::from),
                    )
                } else {
                    (Vec::new(), None, None)
                };
            Some(MetadataFragment {
                confidence: view.get_f64("confidence"),
                created_at_sec: view.created_at_sec,
                hash_hex: view.hash.to_string(),
                tags,
                namespace,
                verification_status,
            })
        }
    }
}

fn format_meta_sml_attrs(meta: &MetadataFragment, level: MetadataDetail) -> String {
    let mut attrs = Vec::new();
    if let Some(c) = meta.confidence {
        attrs.push(format!("confidence=\"{c:.2}\""));
    }
    if meta.created_at_sec > 0 {
        attrs.push(format!("date=\"{}\"", format_date(meta.created_at_sec)));
    }
    if level == MetadataDetail::Full {
        let prefix_len = meta.hash_hex.len().min(16);
        attrs.push(format!("hash=\"{}\"", &meta.hash_hex[..prefix_len]));
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

/// `2026-01-05` from epoch seconds — civil date from days since the epoch
/// (Howard Hinnant's civil_from_days), so this needs no calendar crate and no
/// local timezone.
pub fn format_date(epoch_sec: u32) -> String {
    let days = (epoch_sec as i64) / 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Escape XML metacharacters (`&`, `<`, `>`, `"`, `'`) for SML output.
/// Bidi-control codepoints are intentionally passed through — stripping is the
/// LLM-output-layer's responsibility, out of scope for the escape helper.
pub fn sml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Truncate to `max_chars` at a valid UTF-8 boundary.
fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        let mut end = max_chars.saturating_sub(3).min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

// ---------------------------------------------------------------------------
// SML — semantic per-type elements
// ---------------------------------------------------------------------------

/// Render one grain as a single-line semantic SML element.
///
/// Known grain types get their own element (`<fact>`, `<tool status="ok">`,
/// …); unknown types fall back to a generic `<grain type="…">` element with
/// the fields as child tags, so an unusual grain still renders truthfully.
pub fn render_grain_sml(view: &GrainView, level: MetadataDetail) -> String {
    render_grain_sml_at(view, level, None)
}

/// [`render_grain_sml`] at an explicit progressive-disclosure tier. `None` is
/// the historical render, unchanged byte for byte.
pub fn render_grain_sml_at(
    view: &GrainView,
    level: MetadataDetail,
    disclosure: Option<Disclosure>,
) -> String {
    let meta = extract_metadata(view, level);
    let attrs = meta
        .as_ref()
        .map(|m| format_meta_sml_attrs(m, level))
        .unwrap_or_default();

    match view.grain_type {
        "fact" => {
            let s = view.get_str("subject").unwrap_or("?");
            let r = view.get_str("relation").unwrap_or("?");
            let o = view.get_str("object").unwrap_or("?");
            format!(
                "<fact{attrs}>{} {} {}</fact>",
                sml_escape(s),
                sml_escape(r),
                sml_escape(o)
            )
        }
        "event" => {
            let content = view.get_str("content").unwrap_or("");
            let display = truncate(content, body_cap(disclosure, 500));
            // The speaker matters: `relation` carries it on CAL results,
            // `role`/`speaker` on raw event fields. A conversation whose
            // turns lose their speakers is unreadable.
            let role_attr = view
                .get_str("relation")
                .or_else(|| view.get_str("role"))
                .or_else(|| view.get_str("speaker"))
                .filter(|r| !r.is_empty())
                .map(|r| format!(" role=\"{}\"", sml_escape(r)))
                .unwrap_or_default();
            if let Some(blocks) = view.fields.get("content_blocks") {
                let inner = render_content_blocks_sml(blocks, display);
                format!("<event{role_attr}{attrs}>{inner}</event>")
            } else {
                format!("<event{role_attr}{attrs}>{}</event>", sml_escape(display))
            }
        }
        "state" => {
            format!("<state{attrs}>{}</state>", sml_escape(&state_label(view)))
        }
        "workflow" => {
            let trigger = view.get_str("trigger").unwrap_or("workflow");
            let node_count = field_array_len(view, "nodes");
            let edge_count = field_array_len(view, "edges");
            let bindings = workflow_bindings(view)
                .map(|b| format!(" bindings=\"{}\"", sml_escape(&b)))
                .unwrap_or_default();
            format!(
                "<workflow trigger=\"{}\" nodes=\"{node_count}\" edges=\"{edge_count}\"{bindings}{attrs}>{}</workflow>",
                sml_escape(trigger),
                sml_escape(&workflow_topology(view))
            )
        }
        "tool" => {
            let tool = view.get_str("tool_name").unwrap_or("unknown");
            let is_error = view.get_bool("is_error").unwrap_or(false);
            let status = if is_error { "fail" } else { "ok" };
            let content = view
                .get_str("tool_content")
                .or_else(|| view.get_str("content"))
                .unwrap_or("");
            let result_str = if is_error {
                view.get_str("error").unwrap_or(content)
            } else {
                content
            };
            let mut sml = format!("<tool tool=\"{}\" status=\"{status}\"{attrs}>", sml_escape(tool));
            if !result_str.is_empty() {
                sml.push_str(&sml_escape(truncate(result_str, body_cap(disclosure, 300))));
            }
            if let Some(d) = view.get_u64("duration_ms") {
                sml.push_str(&format!(" ({d}ms)"));
            }
            sml.push_str("</tool>");
            sml
        }
        "observation" => {
            let observer = view.get_str("observer_id").unwrap_or("?");
            format!(
                "<observation observer=\"{}\"{attrs}>{}</observation>",
                sml_escape(observer),
                sml_escape(truncate(&observation_content(view), body_cap(disclosure, usize::MAX)))
            )
        }
        "goal" => {
            let desc = view.get_str("description").unwrap_or("?");
            let state = view.get_str("goal_state").unwrap_or("active");
            let priority = view.get_str("priority").unwrap_or("medium");
            let mut sml = format!(
                "<goal state=\"{}\" priority=\"{}\"{attrs}>{}</goal>",
                sml_escape(state),
                sml_escape(priority),
                sml_escape(truncate(desc, body_cap(disclosure, usize::MAX)))
            );
            if let Some(c) = view.get_str("criteria") {
                sml = sml.replace(
                    "</goal>",
                    &format!("<criteria>{}</criteria></goal>", sml_escape(c)),
                );
            }
            sml
        }
        "reasoning" => {
            let conclusion = view.get_str("conclusion").unwrap_or("");
            let premise_count = field_array_len(view, "premises");
            let mut sml = format!("<reasoning{attrs}>");
            if !conclusion.is_empty() {
                sml.push_str(&format!(
                    "<conclusion>{}</conclusion>",
                    sml_escape(truncate(conclusion, body_cap(disclosure, usize::MAX)))
                ));
            }
            if premise_count > 0 {
                sml.push_str(&format!("<premises count=\"{premise_count}\"/>"));
            }
            sml.push_str("</reasoning>");
            sml
        }
        "consensus" => {
            let agreed = view.get_str("agreed_content").unwrap_or("");
            let agree_count = view.get_i64("agreement_count").unwrap_or(0);
            let dissent_count = view.get_i64("dissent_count").unwrap_or(0);
            format!(
                "<consensus agreed=\"{agree_count}\" dissent=\"{dissent_count}\"{attrs}>{}</consensus>",
                sml_escape(truncate(agreed, body_cap(disclosure, usize::MAX)))
            )
        }
        "consent" => {
            let subject = view.get_str("subject_did").unwrap_or("?");
            let action = if view.get_bool("is_withdrawal").unwrap_or(false) {
                "withdraws"
            } else {
                "grants"
            };
            let mut sml = format!(
                "<consent action=\"{action}\" subject=\"{}\"{attrs}>",
                sml_escape(subject)
            );
            if let Some(scope) = view.get_str("scope").filter(|s| !s.is_empty()) {
                sml.push_str(&format!("<scope>{}</scope>", sml_escape(scope)));
            }
            if let Some(g) = view.get_str("grantee_did") {
                sml.push_str(&format!("<grantee>{}</grantee>", sml_escape(g)));
            }
            sml.push_str("</consent>");
            sml
        }
        "skill" => {
            let name = view.get_str("name").unwrap_or("?");
            let domain_attr = view
                .get_str("domain")
                .map(|d| format!(" domain=\"{}\"", sml_escape(d)))
                .unwrap_or_default();
            let mut sml = format!("<skill name=\"{}\"{domain_attr}{attrs}>", sml_escape(name));
            if let Some(desc) = view.get_str("description").filter(|d| !d.is_empty()) {
                sml.push_str(&sml_escape(desc));
            }
            // `instructions` is the field that IS the skill, and `when_to_use`
            // is what tells a model whether to read it. Both are long-form, so
            // they ride only at full disclosure — a recall of twenty skills at
            // the default tier must stay a listing, not twenty playbooks.
            if disclosure.is_some_and(Disclosure::carries_definitions) {
                if let Some(w) = view.get_str("when_to_use").filter(|w| !w.is_empty()) {
                    sml.push_str(&format!("<when_to_use>{}</when_to_use>", sml_escape(w)));
                }
                if let Some(i) = view.get_str("instructions").filter(|i| !i.is_empty()) {
                    sml.push_str(&format!("<instructions>{}</instructions>", sml_escape(i)));
                }
            }
            sml.push_str("</skill>");
            sml
        }
        "recommendation" => {
            let target = view.get_str("target_ref").unwrap_or("");
            let severity = view.get_str("severity").unwrap_or("info");
            format!(
                "<recommendation target=\"{}\" severity=\"{}\"{attrs}>{}</recommendation>",
                sml_escape(target),
                sml_escape(severity),
                sml_escape(&recommendation_summary(view))
            )
        }
        other => {
            // Unknown type: generic element with fields as child tags.
            let mut sml = format!("<grain type=\"{}\"{attrs}>", sml_escape(other));
            if let serde_json::Value::Object(map) = view.fields {
                let sorted: BTreeMap<_, _> = map.iter().collect();
                for (k, v) in sorted {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        _ => v.to_string(),
                    };
                    sml.push_str(&format!("<{k}>{}</{k}>", sml_escape(&val)));
                }
            }
            sml.push_str("</grain>");
            sml
        }
    }
}

/// Render a `content_blocks` JSON array (Event grain chat extension, OMS 1.2
/// §8.2) as typed SML tags — `<text>`, `<tool_use>`, `<tool_result>`. Never
/// renders block payloads as free text (security SC1 — prevents injection via
/// tool arguments). Falls back to the plain Event `content` string when the
/// value is malformed.
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

// ---------------------------------------------------------------------------
// Type-specific label helpers (shared by sml and markdown arms)
// ---------------------------------------------------------------------------

fn field_array_len(view: &GrainView, key: &str) -> usize {
    view.fields
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// A state's human label: search the snapshot for label/description/title/name.
/// The wire key is `ctx`, which expands to `context` (OMS §8.3).
fn state_label(view: &GrainView) -> String {
    if let Some(ctx) = view.fields.get("context") {
        for key in &["label", "description", "title", "name"] {
            if let Some(s) = ctx.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        if let Some(obj) = ctx.as_object() {
            return format!("{} keys", obj.len());
        }
    }
    "state".to_string()
}

/// How many edges a full workflow render spells out before eliding. A plan the
/// model cannot see is a plan it cannot follow, but an unbounded graph would
/// swallow the whole context budget.
const WORKFLOW_EDGE_RENDER_CAP: usize = 12;

/// A workflow's topology as `src -> dst` steps, carrying edge conditions and
/// cycle bounds.
fn workflow_topology(view: &GrainView) -> String {
    let Some(edges) = view.fields.get("edges").and_then(|v| v.as_array()) else {
        return view
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
fn workflow_bindings(view: &GrainView) -> Option<String> {
    let obj = view.fields.get("bindings")?.as_object()?;
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

fn observation_content(view: &GrainView) -> String {
    let subject = view.get_str("subject").unwrap_or("");
    let object = view.get_str("object").unwrap_or("");
    if !subject.is_empty() && !object.is_empty() {
        format!("{subject}: {object}")
    } else if !subject.is_empty() {
        subject.to_string()
    } else {
        object.to_string()
    }
}

/// A recommendation's `summary` is a template id plus args, never prose
/// (OMS §8.12 rule 4). Without the template registry to render it, show the
/// argument values — what a reviewer actually needs to see.
fn recommendation_summary(view: &GrainView) -> String {
    let args = view.fields.get("summary").and_then(|s| s.get("args"));
    let Some(obj) = args.and_then(|a| a.as_object()) else {
        return view
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
        view.get_str("target_ref")
            .unwrap_or("recommendation")
            .to_string()
    } else {
        parts.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Summary — the compact one-liner for budget-squeezed renders
// ---------------------------------------------------------------------------

/// Compact one-line summary of a grain — what survives when a budget squeezes
/// the render below full disclosure (`ELEMENT_SUMMARY`, `Allocation::Summary`).
pub fn render_grain_summary(view: &GrainView) -> String {
    known_type_summary(view).unwrap_or_else(|| render_grain_text_line(view, None))
}

/// The per-type summary for the twelve known grain types; `None` for an
/// unknown type string.
fn known_type_summary(view: &GrainView) -> Option<String> {
    let summary = match view.grain_type {
        "fact" => format!(
            "{} {} {}",
            view.get_str("subject").unwrap_or("?"),
            view.get_str("relation").unwrap_or("?"),
            view.get_str("object").unwrap_or("?")
        ),
        "event" => truncate(view.get_str("content").unwrap_or(""), 80).to_string(),
        "state" => state_label(view),
        "workflow" => format!(
            "{} ({} nodes, {} edges)",
            view.get_str("trigger").unwrap_or("workflow"),
            field_array_len(view, "nodes"),
            field_array_len(view, "edges")
        ),
        "tool" => {
            let status = if view.get_bool("is_error").unwrap_or(false) {
                "FAIL"
            } else {
                "OK"
            };
            format!("{} [{status}]", view.get_str("tool_name").unwrap_or("unknown"))
        }
        "observation" => {
            let observer = view.get_str("observer_id").unwrap_or("?");
            match view.get_str("subject").filter(|s| !s.is_empty()) {
                Some(subject) => format!("{observer}: {subject}"),
                None => observer.to_string(),
            }
        }
        "goal" => format!(
            "[{}/{}] {}",
            view.get_str("priority").unwrap_or("medium"),
            view.get_str("goal_state").unwrap_or("active"),
            view.get_str("description").unwrap_or("?")
        ),
        "reasoning" => view.get_str("conclusion").unwrap_or("reasoning").to_string(),
        "consensus" => view
            .get_str("agreed_content")
            .unwrap_or("consensus")
            .to_string(),
        "consent" => {
            let subject = view.get_str("subject_did").unwrap_or("?");
            let action = if view.get_bool("is_withdrawal").unwrap_or(false) {
                "withdraws"
            } else {
                "grants"
            };
            match view.get_str("scope").filter(|s| !s.is_empty()) {
                Some(scope) => format!("{subject} {action} {scope}"),
                None => format!("{subject} {action}"),
            }
        }
        "skill" => {
            let name = view.get_str("name").unwrap_or("?");
            match view.get_str("domain") {
                Some(d) => format!("{name} [{d}]"),
                None => name.to_string(),
            }
        }
        "recommendation" => format!(
            "[{}] {}",
            view.get_str("severity").unwrap_or("info"),
            recommendation_summary(view)
        ),
        _ => return None,
    };
    Some(summary)
}

// ---------------------------------------------------------------------------
// Markdown — the documented CAL shape
// ---------------------------------------------------------------------------

/// Storage bookkeeping that never helps a model reason about the memory.
const RENDER_SKIP: [&str; 8] = [
    "type",
    "namespace",
    "created_at",
    "confidence",
    "derived_from",
    "superseded_by",
    "observer_id",
    "observer_type",
];

/// Render a grain as one markdown assertion line (`- **subject** …`), no
/// trailing newline — line separation is the envelope's job.
///
/// The point of `FORMAT markdown` is that the output can be pasted straight
/// into a prompt, so it says the thing the memory asserts and keeps only the
/// metadata that changes how much weight to give it — confidence below 1.0,
/// the date, `superseded`. Storage bookkeeping is left out. Grain types whose
/// meaning lives in structured fields (state, workflow, reasoning, consensus,
/// consent, recommendation) get dedicated arms; everything else renders by
/// field shape, matching the golden-pinned output
/// (`**john** likes jazz *(0.80, 2026-01-05)*`).
pub fn render_grain_markdown(view: &GrainView) -> String {
    render_grain_markdown_at(view, None)
}

/// [`render_grain_markdown`] at an explicit progressive-disclosure tier. `None`
/// is the historical render, unchanged byte for byte.
pub fn render_grain_markdown_at(view: &GrainView, disclosure: Option<Disclosure>) -> String {
    let serde_json::Value::Object(map) = view.fields else {
        return String::new();
    };
    let get = |k: &str| map.get(k).and_then(|v| v.as_str()).unwrap_or("");

    let line = match view.grain_type {
        "state" => format!("**State**: {}", state_label(view)),
        "workflow" => {
            let label = view
                .get_str("name")
                .or_else(|| view.get_str("trigger"))
                .unwrap_or("workflow");
            let trigger = view.get_str("trigger").unwrap_or("workflow");
            let topology = workflow_topology(view);
            if topology.is_empty() {
                format!("**{label}** (on: {trigger})")
            } else {
                format!("**{label}** (on: {trigger}): {topology}")
            }
        }
        "reasoning" => {
            let conclusion = view.get_str("conclusion").unwrap_or("");
            let method = view
                .get_str("inference_method")
                .map(|m| format!(" ({m})"))
                .unwrap_or_default();
            if conclusion.is_empty() {
                format!(
                    "Reasoning{method} ({} premises)",
                    field_array_len(view, "premises")
                )
            } else {
                format!("{conclusion}{method}")
            }
        }
        "consensus" => {
            let agreed = view.get_str("agreed_content").unwrap_or("");
            let agree_count = view.get_i64("agreement_count").unwrap_or(0);
            let dissent_count = view.get_i64("dissent_count").unwrap_or(0);
            let votes = if agree_count > 0 || dissent_count > 0 {
                format!(" ({agree_count} agreed, {dissent_count} dissent)")
            } else {
                String::new()
            };
            if agreed.is_empty() {
                format!("Consensus{votes}")
            } else {
                format!("{agreed}{votes}")
            }
        }
        "consent" => {
            let subject = view.get_str("subject_did").unwrap_or("?");
            let action = if view.get_bool("is_withdrawal").unwrap_or(false) {
                "withdraws"
            } else {
                "grants"
            };
            match view.get_str("scope").filter(|s| !s.is_empty()) {
                Some(scope) => format!("**{subject}** {action} for {scope}"),
                None => format!("**{subject}** {action}"),
            }
        }
        "recommendation" => {
            let severity = view.get_str("severity").unwrap_or("info");
            let target = view.get_str("target_ref").unwrap_or("");
            format!(
                "[{severity}] {} → `{target}`",
                recommendation_summary(view)
            )
        }
        _ => {
            // Field-shape cascade — the strongest shape available.
            let (subject, relation, object) = (get("subject"), get("relation"), get("object"));
            let content = get("content");
            let text = [content, get("body"), get("tool_content"), get("description")]
                .into_iter()
                .find(|t| !t.is_empty())
                .unwrap_or_default();
            let tool_name = get("tool_name");
            let skill_name = get("name");

            // The object must be present for this to be a triple. A grain with
            // a subject and a relation but no object is a message whose
            // relation is the speaker — matching on subject+relation alone
            // silently dropped its text.
            if !subject.is_empty() && !relation.is_empty() && !object.is_empty() {
                // A triple reads as a sentence: subject, then what is claimed.
                format!("**{subject}** {relation} {object}")
            } else if !content.is_empty() && !relation.is_empty() {
                // A turn in a conversation: the relation names who said it.
                format!("**{relation}**: {content}")
            } else if !tool_name.is_empty() {
                // Whether the call worked is the whole point of logging it.
                let failed = map
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let verb = if failed { "failed" } else { "returned" };
                if text.is_empty() {
                    format!("**{tool_name}** {verb}")
                } else {
                    format!("**{tool_name}** {verb}: {text}")
                }
            } else if !skill_name.is_empty() {
                let mut line = format!("**{skill_name}**");
                if !text.is_empty() {
                    line.push_str(&format!(" — {text}"));
                }
                if let Some(p) = map.get("proficiency").and_then(serde_json::Value::as_f64) {
                    let practised = map
                        .get("practice_count")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    line.push_str(&format!(" (proficiency {p:.2}, practised {practised}×)"));
                }
                // The long-form definition, at full disclosure only — same rule
                // as the SML arm, so a skill discloses identically in both.
                if disclosure.is_some_and(Disclosure::carries_definitions) {
                    let when = map.get("when_to_use").and_then(|v| v.as_str()).unwrap_or("");
                    if !when.is_empty() {
                        line.push_str(&format!("\n\n*Use when*: {when}"));
                    }
                    let instr = map.get("instructions").and_then(|v| v.as_str()).unwrap_or("");
                    if !instr.is_empty() {
                        line.push_str(&format!("\n\n{instr}"));
                    }
                }
                line
            } else if !subject.is_empty() && !text.is_empty() {
                // Observations and goals: the subject is what it is about, the
                // text is what was noticed or intended.
                let mut line = format!("**{subject}**: {text}");
                if let Some(state) = map.get("goal_state").and_then(|v| v.as_str()) {
                    let pct = map
                        .get("progress")
                        .and_then(serde_json::Value::as_f64)
                        .map(|p| format!(", {:.0}% done", p * 100.0))
                        .unwrap_or_default();
                    line.push_str(&format!(" ({state}{pct})"));
                }
                line
            } else if !text.is_empty() {
                // Free-text grains (events) carry the text; the relation is the
                // speaker or role when there is one.
                if relation.is_empty() {
                    text.to_string()
                } else {
                    format!("**{relation}**: {text}")
                }
            } else if !subject.is_empty() {
                format!("**{subject}**")
            } else {
                // Nothing triple-shaped and no text — fall back to the fields,
                // so an unusual grain type still renders something truthful.
                let mut pairs: Vec<String> = Vec::new();
                for (k, v) in map {
                    if RENDER_SKIP.contains(&k.as_str()) {
                        continue;
                    }
                    match v {
                        serde_json::Value::String(sv) => pairs.push(format!("{k}: {sv}")),
                        other => pairs.push(format!("{k}: {other}")),
                    }
                }
                pairs.join(", ")
            }
        }
    };

    // Confidence earns its place only when it is not a plain assertion; a date
    // is worth carrying because recency changes how a model should weigh it.
    let mut notes: Vec<String> = Vec::new();
    if let Some(c) = map.get("confidence").and_then(serde_json::Value::as_f64) {
        if c < 1.0 {
            notes.push(format!("{c:.2}"));
        }
    }
    if view.created_at_sec > 0 {
        notes.push(format_date(view.created_at_sec));
    }
    if map.get("superseded_by").is_some() {
        notes.push("superseded".to_string());
    }

    let mut out = String::from("- ");
    out.push_str(&line);
    if !notes.is_empty() {
        out.push_str(&format!(" *({})*", notes.join(", ")));
    }
    out
}

// ---------------------------------------------------------------------------
// Text — deliberately minimal
// ---------------------------------------------------------------------------

/// Render a grain as one plain-text line, no trailing newline.
///
/// Triple-shaped grains: `subject relation object`. Content-shaped grains:
/// `[relation: ]content`. Other known types: their compact summary, so a
/// goal or a state still says what it is about. Unknown types:
/// `[type: hashprefix]`.
pub fn render_grain_text_line(view: &GrainView, line_num: Option<usize>) -> String {
    let mut out = String::new();
    if let serde_json::Value::Object(map) = view.fields {
        let subject = map.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        let relation = map.get("relation").and_then(|v| v.as_str()).unwrap_or("");
        let object = map.get("object").and_then(|v| v.as_str()).unwrap_or("");
        let prefix = line_num.map_or(String::new(), |n| format!("[{}] ", n));
        if !subject.is_empty() || !relation.is_empty() || !object.is_empty() {
            out.push_str(&format!("{}{} {} {}", prefix, subject, relation, object));
        } else if let Some(content) = map.get("content").and_then(|v| v.as_str()) {
            let role_prefix = map
                .get("relation")
                .and_then(|v| v.as_str())
                .filter(|r| !r.is_empty())
                .map_or(String::new(), |r| format!("{}: ", r));
            out.push_str(&format!("{}{}{}", prefix, role_prefix, content));
        } else if let Some(summary) = known_type_summary(view) {
            out.push_str(&format!("{}{}", prefix, summary));
        } else {
            out.push_str(&format!(
                "{}[{}: {}]",
                prefix,
                view.grain_type,
                &view.hash[..view.hash.len().min(8)]
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JSON — the envelope
// ---------------------------------------------------------------------------

/// The per-grain JSON envelope: `hash`, `grain_type`, `fields`. Surfaces add
/// their own per-query annotations (score, breakdowns) around this.
pub fn grain_json(view: &GrainView) -> serde_json::Value {
    serde_json::json!({
        "hash": view.hash,
        "grain_type": view.grain_type,
        "fields": view.fields.clone(),
    })
}

// ---------------------------------------------------------------------------
// TOON — registry-driven columns
// ---------------------------------------------------------------------------

/// Plural grain type name for TOON headers. Sourced from the registry (D1);
/// unknown strings pass through unchanged.
pub fn toon_plural_name(grain_type: &str) -> &str {
    match areev_core::types::registry::from_str(grain_type) {
        Some(ty) => areev_core::types::registry::meta(ty).plural,
        None => grain_type,
    }
}

/// TOON column names per grain type, per CAL spec §10.9.3. Sourced from the
/// registry (D1); unknown types fall back to a single `content` column.
pub fn toon_columns_for_type(grain_type: &str) -> &'static [&'static str] {
    match areev_core::types::registry::from_str(grain_type) {
        Some(ty) => areev_core::types::registry::meta(ty).toon_columns,
        None => &["content"],
    }
}

/// One TOON CSV row for a grain, columns per `toon_columns_for_type`.
pub fn toon_row(view: &GrainView) -> String {
    let fields = view.fields;
    let get_str = |key: &str| -> String {
        fields
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let get_f64 = |key: &str| -> Option<f64> { fields.get(key).and_then(|v| v.as_f64()) };

    let values: Vec<String> = match view.grain_type {
        "fact" => {
            let subject = get_str("subject");
            let relation = get_str("relation");
            let object = get_str("object");
            let content = if !relation.is_empty() && !object.is_empty() {
                format!("{relation} {object}")
            } else if !object.is_empty() {
                object
            } else {
                relation
            };
            let confidence = get_f64("confidence")
                .map(toon_canonicalize)
                .unwrap_or_default();
            vec![
                toon_escape(&subject),
                toon_escape(&content),
                confidence,
            ]
        }
        "event" => {
            let role = fields
                .get("role")
                .or(fields.get("speaker"))
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .to_string();
            // `created_at` may ride the fields as a preformatted string; when
            // it does not, the view's timestamp fills the time column.
            let time = fields
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    if view.created_at_sec > 0 {
                        format_date(view.created_at_sec)
                    } else {
                        String::new()
                    }
                });
            let content = get_str("content");
            vec![
                toon_escape(&role),
                toon_escape(&time),
                toon_escape(&content),
            ]
        }
        "goal" => {
            let subject = fields
                .get("subject")
                .and_then(|v| v.as_str())
                .or(fields.get("description").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            let content = get_str("description");
            let state = fields
                .get("goal_state")
                .or(fields.get("state"))
                .and_then(|v| v.as_str())
                .unwrap_or("active")
                .to_string();
            vec![
                toon_escape(&subject),
                toon_escape(&content),
                toon_escape(&state),
            ]
        }
        "tool" => {
            let tool = fields
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let is_error = fields
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let phase = if is_error { "fail" } else { "ok" };
            let content = fields
                .get("tool_content")
                .or(fields.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            vec![
                toon_escape(&tool),
                toon_escape(phase),
                toon_escape(&content),
            ]
        }
        "observation" => {
            let observer = fields
                .get("observer_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let subject = get_str("subject");
            let object = get_str("object");
            let content = if !subject.is_empty() && !object.is_empty() {
                format!("{subject}: {object}")
            } else if !subject.is_empty() {
                subject
            } else {
                object
            };
            vec![toon_escape(&observer), toon_escape(&content)]
        }
        "reasoning" => {
            let type_val = get_str("inference_method");
            let type_str = if type_val.is_empty() {
                "reasoning".to_string()
            } else {
                type_val
            };
            let content = get_str("conclusion");
            vec![toon_escape(&type_str), toon_escape(&content)]
        }
        "state" => {
            // The wire key is `ctx`, which expands to `context` (OMS §8.3).
            let label = state_label(view);
            let content = fields
                .get("context")
                .and_then(|ctx| ctx.as_object())
                .map(|obj| {
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
                })
                .unwrap_or_else(|| label.clone());
            vec![toon_escape(&label), toon_escape(&content)]
        }
        "workflow" => {
            let trigger = get_str("trigger");
            let node_count = field_array_len(view, "nodes");
            let edge_count = field_array_len(view, "edges");
            let content = format!("{node_count} nodes, {edge_count} edges");
            vec![toon_escape(&trigger), toon_escape(&content)]
        }
        "consensus" => {
            let threshold = get_str("threshold");
            let threshold_str = if threshold.is_empty() {
                "-".to_string()
            } else {
                threshold
            };
            let count = fields
                .get("agreement_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let content = get_str("agreed_content");
            vec![
                toon_escape(&threshold_str),
                count.to_string(),
                toon_escape(&content),
            ]
        }
        "consent" => {
            let grantor = get_str("subject_did");
            let grantee = get_str("grantee_did");
            let is_withdrawal = fields
                .get("is_withdrawal")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let action = if is_withdrawal { "withdraws" } else { "grants" };
            let content = get_str("scope");
            vec![
                toon_escape(&grantor),
                toon_escape(&grantee),
                toon_escape(action),
                toon_escape(&content),
            ]
        }
        "skill" => {
            // Columns: name, domain, proficiency (matches the registry).
            let name = get_str("name");
            let domain = get_str("domain");
            let proficiency = get_f64("proficiency")
                .map(toon_canonicalize)
                .unwrap_or_default();
            vec![
                toon_escape(&name),
                toon_escape(&domain),
                toon_escape(&proficiency),
            ]
        }
        "recommendation" => {
            // Columns: target_ref, severity, content (matches the registry).
            let target = get_str("target_ref");
            let severity = if get_str("severity").is_empty() {
                "info".to_string()
            } else {
                get_str("severity")
            };
            let summary = fields
                .get("summary")
                .and_then(|s| s.get("args"))
                .and_then(|a| a.as_object())
                .map(|o| {
                    let mut vs: Vec<String> = o
                        .iter()
                        .map(|(k, v)| match v {
                            serde_json::Value::String(t) => format!("{k}={t}"),
                            other => format!("{k}={other}"),
                        })
                        .collect();
                    vs.sort();
                    vs.join("; ")
                })
                .unwrap_or_default();
            vec![
                toon_escape(&target),
                toon_escape(&severity),
                toon_escape(&summary),
            ]
        }
        _ => {
            // Fallback: emit all fields as content
            if let Some(obj) = fields.as_object() {
                obj.values()
                    .map(|v| {
                        let s = match v {
                            serde_json::Value::String(s) => s.clone(),
                            _ => v.to_string(),
                        };
                        toon_escape(&s)
                    })
                    .collect()
            } else {
                vec![toon_escape(&fields.to_string())]
            }
        }
    };

    values.join(",")
}

/// Whole-result TOON: grains grouped by type (BTreeMap, deterministic), one
/// `plural[N]{cols}:` section per type, one CSV row per grain.
pub fn render_toon(views: &[GrainView]) -> String {
    let mut groups: BTreeMap<&str, Vec<&GrainView>> = BTreeMap::new();
    for view in views {
        groups.entry(view.grain_type).or_default().push(view);
    }
    let mut sections = Vec::new();
    for (grain_type, group) in &groups {
        let plural = toon_plural_name(grain_type);
        let columns = toon_columns_for_type(grain_type);
        let header = format!("{}[{}]{{{}}}:", plural, group.len(), columns.join(","));
        let mut section = header;
        for view in group {
            section.push('\n');
            section.push_str(&toon_row(view));
        }
        sections.push(section);
    }
    sections.join("\n\n")
}

/// TOON value escaping/quoting per TOON spec §7.2 — quoted only when needed.
pub fn toon_escape(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quoting = s != s.trim()
        || matches!(s, "true" | "false" | "null")
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
    if !needs_quoting {
        // Numbers don't need quoting in TOON — they're valid as-is.
        let stripped = s.strip_prefix('-').unwrap_or(s);
        if !stripped.is_empty()
            && stripped.parse::<f64>().is_ok()
            && stripped.chars().all(|c| {
                c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-'
            })
        {
            return s.to_string();
        }
    }
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

/// Canonicalize a number for TOON: no exponent, no trailing zeros,
/// NaN/Inf -> "null".
pub fn toon_canonicalize(n: f64) -> String {
    if n.is_nan() || n.is_infinite() {
        return "null".to_string();
    }
    format!("{n}")
}

// ---------------------------------------------------------------------------
// Token estimation — the one estimator
// ---------------------------------------------------------------------------

/// Estimated token count for a grain's render (~4 chars per token), plus a
/// flat overhead for the metadata the level will carry. The heuristic every
/// budget consumer shares — `ASSEMBLE … BUDGET` and the context allocators —
/// so a budget means the same thing on every path. No real tokenizer.
pub fn estimate_tokens(view: &GrainView, level: MetadataDetail) -> usize {
    let chars: usize = match view.fields.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| k.len() + json_display_len(v) + 4)
            .sum(),
        None => view.fields.to_string().len(),
    };
    let meta_overhead = match level {
        MetadataDetail::None => 0,
        MetadataDetail::Minimal => 30,
        MetadataDetail::Full => 80,
    };
    ((chars + meta_overhead) / 4).max(1)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fact_fields() -> serde_json::Value {
        serde_json::json!({
            "subject": "john",
            "relation": "likes",
            "object": "coffee",
            "confidence": 0.95,
        })
    }

    fn view<'a>(grain_type: &'a str, fields: &'a serde_json::Value) -> GrainView<'a> {
        GrainView {
            grain_type,
            hash: "abcdef0123456789abcdef0123456789",
            fields,
            created_at_sec: 1_740_000_000, // 2025-02-19
        }
    }

    #[test]
    fn sml_fact_is_semantic() {
        let fields = fact_fields();
        let v = view("fact", &fields);
        let out = render_grain_sml(&v, MetadataDetail::Minimal);
        assert_eq!(
            out,
            "<fact confidence=\"0.95\" date=\"2025-02-19\">john likes coffee</fact>"
        );
    }

    #[test]
    fn sml_none_level_has_no_attrs() {
        let fields = fact_fields();
        let v = view("fact", &fields);
        let out = render_grain_sml(&v, MetadataDetail::None);
        assert_eq!(out, "<fact>john likes coffee</fact>");
    }

    #[test]
    fn sml_full_level_carries_hash_prefix() {
        let fields = fact_fields();
        let v = view("fact", &fields);
        let out = render_grain_sml(&v, MetadataDetail::Full);
        assert!(out.contains("hash=\"abcdef0123456789\""));
    }

    #[test]
    fn sml_tool_carries_status() {
        let fields = serde_json::json!({
            "tool_name": "search_api",
            "content": "12 results",
            "is_error": false,
        });
        let v = view("tool", &fields);
        let out = render_grain_sml(&v, MetadataDetail::None);
        assert_eq!(out, "<tool tool=\"search_api\" status=\"ok\">12 results</tool>");
    }

    #[test]
    fn sml_escapes_content() {
        let fields = serde_json::json!({
            "subject": "a<b",
            "relation": "says",
            "object": "\"x\" & 'y'",
        });
        let v = view("fact", &fields);
        let out = render_grain_sml(&v, MetadataDetail::None);
        assert_eq!(
            out,
            "<fact>a&lt;b says &quot;x&quot; &amp; &apos;y&apos;</fact>"
        );
    }

    #[test]
    fn sml_event_content_blocks_stay_typed() {
        // SC1: tool arguments must never render as free text.
        let fields = serde_json::json!({
            "content": "fallback",
            "content_blocks": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "name": "run", "id": "t1", "input": {"cmd": "ls"}},
            ],
        });
        let v = view("event", &fields);
        let out = render_grain_sml(&v, MetadataDetail::None);
        assert!(out.contains("<text>hi</text>"));
        assert!(out.contains("<tool_use name=\"run\" id=\"t1\">"));
        assert!(!out.contains("fallback"));
    }

    #[test]
    fn sml_unknown_type_renders_generic_element() {
        let fields = serde_json::json!({"alpha": "1", "beta": 2});
        let v = view("mystery", &fields);
        let out = render_grain_sml(&v, MetadataDetail::None);
        assert_eq!(
            out,
            "<grain type=\"mystery\"><alpha>1</alpha><beta>2</beta></grain>"
        );
    }

    #[test]
    fn markdown_fact_matches_documented_shape() {
        let fields = serde_json::json!({
            "subject": "john",
            "relation": "likes",
            "object": "jazz",
            "confidence": 0.80,
        });
        let v = view("fact", &fields);
        assert_eq!(
            render_grain_markdown(&v),
            "- **john** likes jazz *(0.80, 2025-02-19)*"
        );
    }

    #[test]
    fn markdown_confidence_one_is_omitted() {
        let fields = serde_json::json!({
            "subject": "john",
            "relation": "likes",
            "object": "jazz",
            "confidence": 1.0,
        });
        let mut v = view("fact", &fields);
        v.created_at_sec = 0;
        assert_eq!(render_grain_markdown(&v), "- **john** likes jazz");
    }

    #[test]
    fn markdown_conversation_turn_names_speaker() {
        let fields = serde_json::json!({
            "relation": "user",
            "content": "what's the refund window?",
        });
        let mut v = view("event", &fields);
        v.created_at_sec = 0;
        assert_eq!(
            render_grain_markdown(&v),
            "- **user**: what's the refund window?"
        );
    }

    #[test]
    fn markdown_workflow_shows_topology_not_field_dump() {
        let fields = serde_json::json!({
            "name": "deploy",
            "trigger": "push",
            "nodes": ["build", "test", "ship"],
            "edges": [
                {"src": "build", "dst": "test"},
                {"src": "test", "dst": "ship", "cond": "green"},
            ],
        });
        let mut v = view("workflow", &fields);
        v.created_at_sec = 0;
        assert_eq!(
            render_grain_markdown(&v),
            "- **deploy** (on: push): build -> test; test -> ship [when green]"
        );
    }

    #[test]
    fn markdown_state_shows_label_not_field_dump() {
        let fields = serde_json::json!({
            "context": {"label": "checkout step 2", "cart": ["a", "b"]},
        });
        let mut v = view("state", &fields);
        v.created_at_sec = 0;
        assert_eq!(render_grain_markdown(&v), "- **State**: checkout step 2");
    }

    #[test]
    fn text_line_triple() {
        let fields = fact_fields();
        let v = view("fact", &fields);
        assert_eq!(
            render_grain_text_line(&v, None),
            "john likes coffee"
        );
    }

    #[test]
    fn text_line_falls_back_to_summary_for_known_types() {
        let fields = serde_json::json!({
            "description": "Deploy v2",
            "goal_state": "active",
        });
        let v = view("goal", &fields);
        assert_eq!(
            render_grain_text_line(&v, None),
            "[medium/active] Deploy v2"
        );
    }

    #[test]
    fn text_line_falls_back_to_type_and_hash_for_unknown_types() {
        let fields = serde_json::json!({"weird": true});
        let v = view("mystery", &fields);
        assert_eq!(render_grain_text_line(&v, None), "[mystery: abcdef01]");
    }

    #[test]
    fn toon_groups_by_type_with_registry_columns() {
        let f1 = fact_fields();
        let f2 = serde_json::json!({
            "subject": "bob", "relation": "knows", "object": "john",
        });
        let views = vec![view("fact", &f1), view("fact", &f2)];
        let out = render_toon(&views);
        let cols = toon_columns_for_type("fact").join(",");
        assert!(out.starts_with(&format!("facts[2]{{{cols}}}:")));
        assert!(out.contains("john,likes coffee,0.95"));
        assert!(out.contains("bob,knows john,"));
    }

    #[test]
    fn json_envelope_has_hash_type_fields() {
        let fields = fact_fields();
        let v = view("fact", &fields);
        let j = grain_json(&v);
        assert_eq!(j["grain_type"], "fact");
        assert_eq!(j["fields"]["subject"], "john");
        assert_eq!(j["hash"], "abcdef0123456789abcdef0123456789");
    }

    #[test]
    fn estimate_tokens_counts_fields_plus_overhead() {
        let fields = fact_fields();
        let v = view("fact", &fields);
        let none = estimate_tokens(&v, MetadataDetail::None);
        let full = estimate_tokens(&v, MetadataDetail::Full);
        assert!(none >= 1);
        assert_eq!(full, none + 20); // 80 chars of overhead / 4
    }
}
