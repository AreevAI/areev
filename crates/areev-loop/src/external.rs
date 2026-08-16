//! External command analyzers — the `--analyzer-cmd` seam (SDK §11).
//!
//! A subprocess that receives a live-grain snapshot on stdin and returns
//! advisory findings on stdout. It runs at trust class [`Command`] with
//! auto-apply [`Never`]: a domain-specific subprocess can *surface* an issue a
//! human then reviews, but can never mutate memory. Any failure — cannot spawn,
//! non-zero exit, garbled output — *skips the analyzer for the run*; it never
//! crashes the pass or the sibling analyzers (the engine already treats an
//! `analyze` error as a per-analyzer skip).
//!
//! ## Protocol (one JSON object on stdin, one on stdout)
//!
//! - **Probe** (at construction): `{"loop_analyzer":1,"op":"probe"}` →
//!   `{"id":"acme.pii/1","title":"PII scan","description":"…"}` — every field
//!   optional; a missing/garbled probe just falls back to an id derived from the
//!   command name.
//! - **Analyze** (per run): `{"loop_analyzer":1,"op":"analyze","now_ms":…,
//!   "watermark_ms":…,"grains":[<grain>…]}` → `{"findings":[{"target":
//!   "entity:ns/subject","summary":"…","severity":"low","evidence":["<hash>"],
//!   "confidence":0.8}]}`. Each `<grain>` is a `{hash,grain_type,namespace,
//!   created_at_ms,fields}` record; a finding must name a `target` and a
//!   `summary` (others are dropped).
//!
//! [`Command`]: crate::manifest::TrustClass::Command
//! [`Never`]: crate::manifest::AutoApplyClass::Never

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::{Error, Result};
use crate::manifest::{
    AnalyzerManifest, AutoApplyClass, CadenceClass, TargetClass, Tier, TrustClass,
};
use crate::model::{grain_type, ActionKind, GrainRecord, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use crate::substrate::ReadOpts;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::Write;
use std::process::{Command, Stdio};

/// Live grains handed to the subprocess per run — a pipe-size backstop, not a
/// correctness bound (the snapshot is best-effort context).
const MAX_GRAINS: usize = 2000;

/// An [`Analyzer`] backed by an external command (`--analyzer-cmd`).
pub struct CommandAnalyzer {
    argv: Vec<String>,
    manifest: AnalyzerManifest,
}

impl CommandAnalyzer {
    /// Construct and probe. The probe lets the command self-describe; a probe
    /// that fails to *spawn* errors here (at construction, not mid-run), while a
    /// probe that merely returns nothing usable falls back to an id generated
    /// from the command name. The resulting manifest is always trust class
    /// `Command` / auto-apply `Never` — advisory only, regardless of what the
    /// command claims.
    pub fn new(cmd: &str) -> Result<Self> {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return Err(Error::AnalyzerFailed {
                id: "command".into(),
                message: "--analyzer-cmd is empty".into(),
            });
        }
        let probe = run(&argv, r#"{"loop_analyzer":1,"op":"probe"}"#).map_err(|m| {
            Error::AnalyzerFailed {
                id: "command".into(),
                message: m,
            }
        })?;
        let reply: ProbeReply = serde_json::from_str(probe.trim()).unwrap_or_default();
        let id = normalize_id(&reply.id, &argv[0]);
        let title = non_empty(reply.title).unwrap_or_else(|| id.clone());
        let description =
            non_empty(reply.description).unwrap_or_else(|| format!("External analyzer: {cmd}"));
        let manifest = AnalyzerManifest {
            id,
            title,
            description,
            tier: Tier::T0,
            cadence: CadenceClass::Batch,
            requires: vec![],
            target_classes: vec![TargetClass::Memory],
            auto_apply: AutoApplyClass::Never, // advisory only — never trust a subprocess to mutate
            trust_class: TrustClass::Command,
            params: vec![],
            default_on: true, // registered explicitly → on
        };
        Ok(CommandAnalyzer { argv, manifest })
    }
}

impl Analyzer for CommandAnalyzer {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        // Snapshot the common user grain types (live, capped) as context.
        let mut grains: Vec<GrainRecord> = Vec::new();
        for gt in [
            grain_type::FACT,
            grain_type::OBSERVATION,
            grain_type::TOOL,
            grain_type::SKILL,
            grain_type::GOAL,
        ] {
            if grains.len() >= MAX_GRAINS {
                break;
            }
            if let Ok(mut g) = ctx.grains_of_type(gt, ReadOpts::default()) {
                grains.append(&mut g);
            }
        }
        grains.truncate(MAX_GRAINS);

        let req = AnalyzeRequest {
            loop_analyzer: 1,
            op: "analyze",
            now_ms: ctx.now_ms(),
            watermark_ms: ctx.watermark_ms(),
            grains: &grains,
        };
        let body = serde_json::to_string(&req).map_err(|e| Error::AnalyzerFailed {
            id: self.manifest.id.clone(),
            message: format!("serialize request: {e}"),
        })?;
        let raw = run(&self.argv, &body).map_err(|m| Error::AnalyzerFailed {
            id: self.manifest.id.clone(),
            message: m,
        })?;
        // Garbled output yields no findings, never an error (the model/command
        // can't crash the run) — same fail-soft posture as the LLM parsers.
        let reply: AnalyzeReply = serde_json::from_str(raw.trim()).unwrap_or_default();
        Ok(reply.findings.into_iter().filter_map(to_draft).collect())
    }
}

/// Spawn the command, write `request` to stdin, return stdout. Plain-string
/// errors so callers can wrap them with the right analyzer id.
fn run(argv: &[String], request: &str) -> std::result::Result<String, String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn --analyzer-cmd {:?}: {e}", argv[0]))?;
    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(request.as_bytes())
            .map_err(|e| format!("write to --analyzer-cmd: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("--analyzer-cmd wait: {e}"))?;
    if !out.status.success() {
        return Err(format!("--analyzer-cmd exited with {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("--analyzer-cmd stdout not UTF-8: {e}"))
}

/// Coerce the probe-reported id into `publisher.name/major` shape, or generate
/// one from the command's basename — a stable, non-empty dedup family either way.
fn normalize_id(reported: &str, argv0: &str) -> String {
    let r = reported.trim();
    if !r.is_empty() {
        return if r.contains('/') {
            r.to_string()
        } else {
            format!("{r}/1") // no major → append one so family() = the whole id
        };
    }
    let base = argv0
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(argv0)
        .trim_end_matches(".sh")
        .trim_end_matches(".py");
    let sanitized: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let name = sanitized.trim_matches('_');
    format!("command.{}/1", if name.is_empty() { "external" } else { name })
}

fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Map an external finding to an advisory [`RecDraft`]. Drops findings that name
/// neither a target nor a summary. The engine stamps origin/dedup afterward and
/// re-validates the target ref (a bad ref drops just that draft).
fn to_draft(f: ExternalFinding) -> Option<RecDraft> {
    if f.target.trim().is_empty() || f.summary.trim().is_empty() {
        return None;
    }
    let mut args = Map::new();
    args.insert("text".into(), Value::from(f.summary));
    let mut data = Map::new();
    data.insert("source".into(), Value::from("command"));
    if let Some(g) = non_empty(f.guidance) {
        data.insert("guidance".into(), Value::from(g));
    }
    Some(RecDraft {
        target_ref: f.target,
        action_kind: ActionKind::Flag,
        summary: Summary::new("command.finding", args),
        severity: parse_severity(&f.severity),
        proposal: Proposal::Data { data },
        evidence: f.evidence,
        evidence_query: None,
        metric: None,
        confidence: if f.confidence > 0.0 {
            f.confidence.clamp(0.0, 1.0)
        } else {
            0.5
        },
        importance: if f.importance > 0.0 {
            f.importance.clamp(0.0, 1.0)
        } else {
            0.3
        },
        // External analyzers cannot pin evalsets: code_revision drafts from
        // a subprocess would fail Rule E1 at stamp anyway (advisory only).
        evalset_hash: None,
    })
}

fn parse_severity(s: &str) -> Severity {
    match s.trim().to_ascii_lowercase().as_str() {
        "high" => Severity::High,
        "medium" | "med" => Severity::Medium,
        "info" => Severity::Info,
        _ => Severity::Low,
    }
}

// ---- wire schema -----------------------------------------------------------

#[derive(Serialize)]
struct AnalyzeRequest<'a> {
    loop_analyzer: u8,
    op: &'a str,
    now_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    watermark_ms: Option<i64>,
    grains: &'a [GrainRecord],
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProbeReply {
    id: String,
    title: String,
    description: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AnalyzeReply {
    findings: Vec<ExternalFinding>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExternalFinding {
    target: String,
    summary: String,
    severity: String,
    evidence: Vec<String>,
    guidance: String,
    confidence: f64,
    importance: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_id_shapes() {
        assert_eq!(normalize_id("acme.pii/2", "x"), "acme.pii/2");
        assert_eq!(normalize_id("acme.pii", "x"), "acme.pii/1");
        assert_eq!(normalize_id("", "/usr/local/bin/pii-check.sh"), "command.pii_check/1");
        assert_eq!(normalize_id("  ", "weird!!name"), "command.weird__name/1");
    }

    #[test]
    fn to_draft_requires_target_and_summary() {
        assert!(to_draft(ExternalFinding::default()).is_none());
        let ok = to_draft(ExternalFinding {
            target: "entity:caller/acme".into(),
            summary: "looks off".into(),
            severity: "high".into(),
            confidence: 0.0,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(ok.severity, Severity::High);
        assert_eq!(ok.confidence, 0.5); // 0.0 → default floor
        assert_eq!(ok.summary.render(), "looks off");
    }
}
