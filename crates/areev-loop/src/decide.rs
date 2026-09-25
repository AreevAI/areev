//! Optional decision backend (`docs/decision-model-proposal.md` §4, rows
//! E1–E3).
//!
//! A **decision** (System One) model answers named, typed questions about a
//! `state` with probabilities — no text generation. The loop uses one, when
//! a host installs it with [`Engine::with_decider`](crate::Engine::with_decider),
//! in three places:
//!
//! - **E1** — GROUND and VERIFY of LLM drafts: a calibrated `noul` per draft ×
//!   cited evidence replaces the LLM's grounding verdict, and a calibrated
//!   `noul` "is it sound?" supplies VERIFY's routing number in place of the
//!   verifier's self-reported confidence (the LLM's keep/kill still runs).
//! - **E2** — the duplicate and contradiction sweeps widen past token Jaccard
//!   and the seeded functional relations, asking "same claim?" / "can both be
//!   true?" of the candidate pairs the deterministic rules cannot decide.
//! - **E3** — a free-text tool failure cause is classified into the closed
//!   cause vocabulary with a `choice`.
//!
//! **A decision model may score; only code gates.** Nothing here approves,
//! applies or rolls back: a judged draft is still a pending recommendation,
//! and the engine refuses to auto-apply anything a model judged
//! (`Recommendation::judged_by`). **An uncalibrated backend never omits** — a
//! probability from one is not used to drop or propose anything (proposal §2
//! rule 2); those stages fall back to today's rule. **Fail-soft** — a backend
//! error or malformed answer drops that stage's contribution for the run
//! (`LOP-E051`, recorded on the run's [`DeciderReport`]), never the run.
//!
//! This crate has no Areev dependencies, so the seam is a minimal trait over
//! the proposal's WIRE JSON: `{"state": …, "questions": {…}}` in, `{"answers":
//! {…}, "provider", "model", "calibrated", "latency_ms"}` out. The Areev bridge
//! (`areev_loop_adapter::LoopDecider`) implements it over
//! `areev_core::decide::DecisionBackend`; a subprocess could implement it just
//! as well.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The seam: one wire request in, one wire response out.
pub trait DecideBackend: Send + Sync {
    /// Answer one request. `request_json` is `{"state": …, "questions": {id:
    /// question}}` in the proposal's wire shape; the returned text is the wire
    /// response plus provenance (`provider`, `model`, `calibrated`,
    /// `latency_ms`) and is validated by the caller.
    fn decide(&self, request_json: &str) -> Result<String>;
    /// Whether this backend's probabilities are calibrated. Uncalibrated
    /// backends may reorder, never omit.
    fn calibrated(&self) -> bool;
    /// Stable human label for provenance, e.g. `"typesafe:jev-latest"`.
    fn describe(&self) -> String;
}

impl<T: DecideBackend + ?Sized> DecideBackend for Box<T> {
    fn decide(&self, request_json: &str) -> Result<String> {
        (**self).decide(request_json)
    }
    fn calibrated(&self) -> bool {
        (**self).calibrated()
    }
    fn describe(&self) -> String {
        (**self).describe()
    }
}

/// The probability a `noul` answer must reach for a calibrated decision to
/// ground a draft, route it past VERIFY, or propose a sweep draft. The same
/// number as the LLM verifier's confidence floor.
pub const DECIDE_MIN_P: f64 = 0.75;
/// The probability the argmax of a tool-cause `choice` must reach to be used;
/// below it the cause stays `unknown`.
pub const CAUSE_MIN_P: f64 = 0.6;
/// The default per-sweep, per-run cap on pairs sent to the backend.
pub const DEFAULT_PAIR_CAP: usize = 200;
/// Questions batched into one request by the sweeps and the cause classifier.
pub const QUESTIONS_PER_REQUEST: usize = 16;

/// The closed tool-failure cause vocabulary with the description each option
/// is offered under. Mirrors `areev_core::types::FailureCause` (the adapter
/// pins the two against each other in a test — this crate cannot import it).
pub const TOOL_CAUSES: &[(&str, &str)] = &[
    ("timeout", "The call ran out of time: a deadline, timeout or no response in time."),
    (
        "executor_error",
        "The executor or the remote service failed while running the call: a transport fault, a crash, a 5xx, or an error the tool raised.",
    ),
    (
        "schema_validation_failed",
        "The call's input or output did not match the tool's schema: a missing, extra or wrongly typed field.",
    ),
    ("user_aborted", "A person or the calling agent cancelled or aborted the call."),
    (
        "context_overflow",
        "The model refused the request because the prompt was too long for its context window.",
    ),
    ("unknown", "None of the above, or the text does not say why the call failed."),
];

/// Who judged a recommendation, with what, and what it answered — the
/// attribution record (proposal §2 rule 4) carried on a
/// [`Recommendation`](crate::Recommendation) a decision shaped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgedBy {
    /// The installed backend's `describe()`, e.g. `"typesafe:jev-latest"`.
    pub backend: String,
    /// The entry that answered (a chain reports the one that did).
    pub provider: String,
    pub model: String,
    pub calibrated: bool,
    pub latency_ms: u64,
    /// Which judgment: `ground_verify`, `duplicate`, `contradiction` or
    /// `tool_cause`.
    pub stage: String,
    /// The probabilities that decided it, by question (or option) id.
    #[serde(default)]
    pub answers: BTreeMap<String, f64>,
}

/// What the decision backend did during one run — on
/// [`RunResult::decider`](crate::RunResult::decider), beside the LLM funnel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeciderReport {
    /// The backend's `describe()`.
    pub backend: String,
    /// The backend's `calibrated()`. When false, no stage used a probability
    /// to drop or propose anything.
    pub calibrated: bool,
    /// Requests sent.
    pub calls: u64,
    /// Requests that failed or returned a malformed answer. Each dropped its
    /// stage's contribution for the run; the run continued.
    pub failed_calls: u64,
    /// The last failure's message (`LOP-E051 …`), when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// One validated response.
#[derive(Debug, Clone, PartialEq)]
pub struct Answered {
    /// `noul` answers: `p(yes)`. Choice answers: absent here, see `choices`.
    pub noul: BTreeMap<String, f64>,
    /// `choice` answers: the probability of every option.
    pub choices: BTreeMap<String, BTreeMap<String, f64>>,
    pub provider: String,
    pub model: String,
    /// The backend's flag AND the response's own `calibrated` field.
    pub calibrated: bool,
    pub latency_ms: u64,
}

impl Answered {
    /// The attribution record for this response.
    pub fn judged_by(&self, backend: &str, stage: &str, answers: BTreeMap<String, f64>) -> JudgedBy {
        JudgedBy {
            backend: backend.to_string(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            calibrated: self.calibrated,
            latency_ms: self.latency_ms,
            stage: stage.to_string(),
            answers,
        }
    }
}

/// A question to ask: `(id, instructions)` for a `noul`, or with options for
/// a `choice`.
#[derive(Debug, Clone)]
pub enum Ask {
    Noul { id: String, instructions: String },
    Choice { id: String, instructions: String, options: Vec<(String, String)> },
}

impl Ask {
    fn id(&self) -> &str {
        match self {
            Ask::Noul { id, .. } | Ask::Choice { id, .. } => id,
        }
    }
    fn to_wire(&self) -> Value {
        match self {
            Ask::Noul { instructions, .. } => json!({"type": "noul", "instructions": instructions}),
            Ask::Choice { instructions, options, .. } => {
                let criteria: Map<String, Value> =
                    options.iter().map(|(k, d)| (k.clone(), Value::from(d.clone()))).collect();
                json!({"type": "choice", "instructions": instructions, "criteria": criteria})
            }
        }
    }
}

/// A classified tool cause: the vocabulary key, its probability, and the
/// decision that produced it — `None` when the cause stays `unknown`.
pub type CauseVerdict = Option<(String, f64, JudgedBy)>;

/// The engine's handle on an installed backend: the typed helpers every
/// stage uses, the per-run counters behind [`DeciderReport`], and the per-run
/// tool-cause cache.
pub struct Decider {
    backend: Box<dyn DecideBackend>,
    pair_cap: usize,
    stats: Mutex<DeciderReport>,
    cause_cache: Mutex<BTreeMap<String, CauseVerdict>>,
}

impl Decider {
    pub fn new(backend: Box<dyn DecideBackend>) -> Self {
        Decider {
            backend,
            pair_cap: DEFAULT_PAIR_CAP,
            stats: Mutex::new(DeciderReport::default()),
            cause_cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// Set the per-sweep, per-run pair cap (default [`DEFAULT_PAIR_CAP`]).
    pub fn with_pair_cap(mut self, cap: usize) -> Self {
        self.pair_cap = cap;
        self
    }

    pub fn pair_cap(&self) -> usize {
        self.pair_cap
    }
    pub fn calibrated(&self) -> bool {
        self.backend.calibrated()
    }
    pub fn describe(&self) -> String {
        self.backend.describe()
    }

    /// Clear the counters and the cause cache — the engine calls this at the
    /// start of every analysis pass, so both are per-run.
    pub(crate) fn reset(&self) {
        if let Ok(mut s) = self.stats.lock() {
            *s = DeciderReport::default();
        }
        if let Ok(mut c) = self.cause_cache.lock() {
            c.clear();
        }
    }

    /// This run's report so far.
    pub fn report(&self) -> DeciderReport {
        let mut r = self.stats.lock().map(|s| s.clone()).unwrap_or_default();
        r.backend = self.describe();
        r.calibrated = self.calibrated();
        r
    }

    /// Ask `questions` about `state` in ONE request and validate the answer
    /// against what was asked. Every failure is `LOP-E051` and is counted on
    /// the run's report; callers drop their stage's contribution on `Err`.
    pub fn ask(&self, state: Value, questions: &[Ask]) -> Result<Answered> {
        let out = self.ask_inner(state, questions);
        if let Ok(mut s) = self.stats.lock() {
            s.calls += 1;
            if let Err(e) = &out {
                s.failed_calls += 1;
                s.last_error = Some(e.to_string());
            }
        }
        out
    }

    fn ask_inner(&self, state: Value, questions: &[Ask]) -> Result<Answered> {
        if questions.is_empty() {
            return Err(Error::DecideBackend("no questions".into()));
        }
        let qs: Map<String, Value> =
            questions.iter().map(|q| (q.id().to_string(), q.to_wire())).collect();
        let body = json!({"state": state, "questions": qs}).to_string();
        let raw = self.backend.decide(&body).map_err(|e| match e {
            Error::DecideBackend(_) => e,
            other => Error::DecideBackend(other.to_string()),
        })?;
        parse_response(&raw, questions, self.backend.calibrated())
    }

    /// The tool-cause classifier (E3): for each distinct free-text cause,
    /// the closed-vocabulary cause and its probability when a CALIBRATED
    /// backend's argmax reaches [`CAUSE_MIN_P`]; `None` otherwise (the caller
    /// keeps `unknown`). Cached by string for the run, so one string is asked
    /// at most once; at most `pair_cap` distinct strings are asked per run.
    /// A failed request leaves its strings `unknown` for the run (cached as
    /// `None`, so a later caller does not retry into the same failure).
    pub fn classify_causes(&self, texts: &[String]) -> BTreeMap<String, CauseVerdict> {
        let mut out = BTreeMap::new();
        if !self.calibrated() {
            for t in texts {
                out.insert(t.clone(), None);
            }
            return out;
        }
        let mut todo: Vec<String> = Vec::new();
        {
            let cache = self.cause_cache.lock().ok();
            for t in texts {
                match cache.as_ref().and_then(|c| c.get(t)) {
                    Some(hit) => {
                        out.insert(t.clone(), hit.clone());
                    }
                    None if !todo.contains(t) => todo.push(t.clone()),
                    None => {}
                }
            }
        }
        let asked_before = self.cause_cache.lock().map(|c| c.len()).unwrap_or(0);
        let budget = self.pair_cap.saturating_sub(asked_before);
        let (ask, skip) = todo.split_at(todo.len().min(budget));
        for t in skip {
            out.insert(t.clone(), None);
        }
        let backend = self.describe();
        for chunk in ask.chunks(QUESTIONS_PER_REQUEST) {
            let mut failures = Map::new();
            let mut questions = Vec::new();
            for (i, t) in chunk.iter().enumerate() {
                let id = format!("c{i}");
                failures.insert(id.clone(), Value::from(t.clone()));
                questions.push(Ask::Choice {
                    instructions: format!(
                        "Which cause best explains the tool failure described by item \"{id}\" (in state.failures)?"
                    ),
                    id,
                    options: TOOL_CAUSES.iter().map(|(k, d)| (k.to_string(), d.to_string())).collect(),
                });
            }
            let answered = self.ask(json!({"failures": failures}), &questions);
            for (i, t) in chunk.iter().enumerate() {
                let verdict = answered.as_ref().ok().and_then(|a| {
                    if !a.calibrated {
                        return None;
                    }
                    let probs = a.choices.get(&format!("c{i}"))?;
                    let (best, p) = argmax(probs)?;
                    (p >= CAUSE_MIN_P).then(|| {
                        let judged = a.judged_by(&backend, "tool_cause", probs.clone());
                        (best, p, judged)
                    })
                });
                if let Ok(mut c) = self.cause_cache.lock() {
                    c.insert(t.clone(), verdict.clone());
                }
                out.insert(t.clone(), verdict);
            }
        }
        out
    }
}

/// The highest-probability option; ties break to the lexicographically
/// smallest key (deterministic).
fn argmax(probs: &BTreeMap<String, f64>) -> Option<(String, f64)> {
    let mut best: Option<(&String, f64)> = None;
    for (k, &p) in probs {
        if best.is_none_or(|(_, bp)| p > bp) {
            best = Some((k, p));
        }
    }
    best.map(|(k, p)| (k.clone(), p))
}

fn prob(v: &Value) -> Option<f64> {
    v.as_f64().filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
}

/// Validate a wire response against what was asked: every id present with
/// the matching type, every probability finite and in `[0, 1]`, choice
/// options drawn from what was offered. Anything else is `LOP-E051`.
pub fn parse_response(raw: &str, asked: &[Ask], backend_calibrated: bool) -> Result<Answered> {
    let bad = |m: String| Error::DecideBackend(format!("malformed answer: {m}"));
    let v: Value = serde_json::from_str(raw.trim()).map_err(|e| bad(format!("not JSON: {e}")))?;
    let answers = v
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("no `answers` object".into()))?;
    let mut noul = BTreeMap::new();
    let mut choices = BTreeMap::new();
    for q in asked {
        let a = answers.get(q.id()).ok_or_else(|| bad(format!("no answer for {:?}", q.id())))?;
        match q {
            Ask::Noul { id, .. } => {
                if a.get("type").and_then(Value::as_str).is_some_and(|t| t != "noul") {
                    return Err(bad(format!("{id:?} is not a noul answer")));
                }
                let p = a
                    .get("noul")
                    .and_then(prob)
                    .ok_or_else(|| bad(format!("{id:?} has no probability in [0, 1]")))?;
                noul.insert(id.clone(), p);
            }
            Ask::Choice { id, options, .. } => {
                if a.get("type").and_then(Value::as_str).is_some_and(|t| t != "choice") {
                    return Err(bad(format!("{id:?} is not a choice answer")));
                }
                let probs = a
                    .get("probabilities")
                    .and_then(Value::as_object)
                    .ok_or_else(|| bad(format!("{id:?} has no probabilities")))?;
                let mut m = BTreeMap::new();
                for (k, p) in probs {
                    if !options.iter().any(|(o, _)| o == k) {
                        return Err(bad(format!("{id:?} answered an option nobody offered: {k:?}")));
                    }
                    let p = prob(p).ok_or_else(|| bad(format!("{id:?} option {k:?} is not in [0, 1]")))?;
                    m.insert(k.clone(), p);
                }
                if m.is_empty() {
                    return Err(bad(format!("{id:?} has no probabilities")));
                }
                choices.insert(id.clone(), m);
            }
        }
    }
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Answered {
        noul,
        choices,
        provider: s("provider"),
        model: s("model"),
        calibrated: backend_calibrated && v.get("calibrated").and_then(Value::as_bool).unwrap_or(true),
        latency_ms: v.get("latency_ms").and_then(Value::as_u64).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_must_answer_every_question_with_a_probability() {
        let asked = vec![
            Ask::Noul { id: "a".into(), instructions: "?".into() },
            Ask::Choice {
                id: "b".into(),
                instructions: "?".into(),
                options: vec![("x".into(), "".into()), ("y".into(), "".into())],
            },
        ];
        let ok = r#"{"answers":{"a":{"type":"noul","noul":0.9},
            "b":{"type":"choice","choice":"x","probabilities":{"x":0.7,"y":0.3}}},
            "provider":"fake","model":"m","calibrated":true,"latency_ms":3}"#;
        let a = parse_response(ok, &asked, true).unwrap();
        assert_eq!(a.noul["a"], 0.9);
        assert_eq!(a.choices["b"]["x"], 0.7);
        assert!(a.calibrated);
        assert_eq!(a.latency_ms, 3);
        // The response's own flag can only lower the backend's.
        let unc = ok.replace("\"calibrated\":true", "\"calibrated\":false");
        assert!(!parse_response(&unc, &asked, true).unwrap().calibrated);
        assert!(!parse_response(ok, &asked, false).unwrap().calibrated);

        for broken in [
            "not json",
            r#"{"answers":{"a":{"type":"noul","noul":0.9}}}"#,
            r#"{"answers":{"a":{"type":"noul","noul":1.5},"b":{"probabilities":{"x":1}}}}"#,
            r#"{"answers":{"a":{"type":"noul","noul":0.5},"b":{"probabilities":{"z":1}}}}"#,
            r#"{"answers":{"a":{"type":"choice","noul":0.5},"b":{"probabilities":{"x":1}}}}"#,
        ] {
            let e = parse_response(broken, &asked, true).unwrap_err();
            assert_eq!(e.code(), "LOP-E051", "{broken}");
        }
    }

    #[test]
    fn argmax_breaks_ties_deterministically() {
        let m: BTreeMap<String, f64> = [("b".to_string(), 0.5), ("a".to_string(), 0.5)].into();
        assert_eq!(argmax(&m), Some(("a".to_string(), 0.5)));
        assert_eq!(argmax(&BTreeMap::new()), None);
    }
}
