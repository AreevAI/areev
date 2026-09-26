//! Decision backends — the pure seam (`docs/decision-model-proposal.md` §3).
//!
//! A **decision** (System One) model takes a `state` plus named, typed
//! questions and returns typed answers with probabilities, in one parallel
//! evaluation and with no text generation. Three question types exist:
//!
//! - [`Question::Noul`] — yes/no; the answer is `p(yes)`.
//! - [`Question::Choice`] — one of 2..=255 named options.
//! - [`Question::Score`] — one of 2..=10 ordered levels, index 0 lowest.
//!
//! This module holds only what every consumer shares — the questions, the
//! answers, the request and its [`Decision`], the [`DecisionBackend`] trait,
//! the `DEC` error domain, and the wire (de)serialization with its answer
//! validation. It does no I/O. It lives in `areev-core` so the memory stack
//! (`areev-store`, `areev-cal`, `areev-context`) can accept a backend without
//! depending on an LLM or HTTP crate; the adapters (the TypeSafe-shape HTTP
//! client, Cloudflare, the command backend, LLM emulation) and the provider
//! chain live in `areev_llm::decide`, which re-exports everything here under
//! its original paths.
//!
//! A decision model may score and order; only code omits, gates, approves or
//! applies — and code that omits on a probability must check
//! [`Decision::calibrated`] first (proposal §2, rules 1–2).
//!
//! Every answer is validated on the way OUT of an adapter
//! ([`parse_wire_answers`], [`Decision::from_wire`]): every requested id
//! present, the type matching the question, probabilities present and
//! summing to 1 within `1e-2` (then normalized exactly). `confidence` for
//! Choice/Score is `(n·p_max − 1)/(n − 1)` — computed here when a provider
//! omits it, and always for emulation, so the field means one thing across
//! providers.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

/// The per-call default deadline (`--decide-timeout-ms`,
/// `AREEV_DECIDE_TIMEOUT_MS`): 2000 ms.
pub const DEFAULT_DECIDE_TIMEOUT: Duration = Duration::from_millis(2000);

/// Option-count bounds for [`Question::Choice`].
pub const CHOICE_OPTIONS: std::ops::RangeInclusive<usize> = 2..=255;
/// Level-count bounds for [`Question::Score`].
pub const SCORE_LEVELS: std::ops::RangeInclusive<usize> = 2..=10;

/// How far a provider's probabilities may sum from 1 before the answer is
/// refused as malformed rather than normalized. 1e-2, not tighter: hosted
/// providers round each probability to two places, so an honest answer can
/// sum to 0.99 or 1.01.
const SUM_TOLERANCE: f32 = 1e-2;
/// Float slack on top of [`SUM_TOLERANCE`]: f32 addition of two-place
/// values lands a hair outside it (0.33+0.33+0.33 = 0.98999995), and an
/// honest 0.99 must not be refused for that.
const SUM_SLACK: f32 = 1e-4;

// ---- errors ----------------------------------------------------------------

/// Every decision-backend failure. `Display` leads with a stable `DEC-Ennn`
/// code and [`DecideError::code`] returns it; codes are append-only
/// (`ERROR_CODES.md`).
#[derive(Debug, Clone, PartialEq)]
pub enum DecideError {
    /// `DEC-E001` — no backend configured, or the spec did not parse
    /// (unknown provider, missing key — the message names the env var).
    NotConfigured(String),
    /// `DEC-E002` — provider transport or HTTP error. `status` is the HTTP
    /// status when there was one; `retryable` is true for 5xx and transport
    /// faults, false for 4xx.
    Provider {
        provider: String,
        status: Option<u16>,
        message: String,
        retryable: bool,
    },
    /// `DEC-E003` — malformed answer: a missing question id, probabilities
    /// absent or not summing to 1, a type mismatch, an unknown type.
    Malformed(String),
    /// `DEC-E004` — the deadline elapsed before an answer arrived.
    Deadline(String),
    /// `DEC-E005` — every chain entry failed; carries `(describe(), error)`
    /// per entry, in chain order.
    ChainExhausted(Vec<(String, DecideError)>),
    /// `DEC-E006` — an invalid question (option/level count out of range,
    /// empty instructions, no questions at all). Never retried.
    InvalidQuestion(String),
    /// `DEC-E007` — rate limited (HTTP 429). `retry_after_secs` is the
    /// `Retry-After` header when the provider sent one in seconds form. The
    /// adapter never sleeps on it.
    RateLimited {
        provider: String,
        retry_after_secs: Option<u64>,
    },
    /// `DEC-E008` — egress pseudonymization of the request's `state` failed
    /// (`areev_llm::PseudonymizingDecider`), so the request was NOT sent. It
    /// stops a chain ([`DecideError::stops_chain`]): a later entry may be
    /// unwrapped, and moving on would send the raw state the policy just
    /// refused to let out.
    EgressRefused(String),
}

impl DecideError {
    /// Stable machine-readable code in `DEC-Ennn` form.
    pub fn code(&self) -> &'static str {
        match self {
            DecideError::NotConfigured(_) => "DEC-E001",
            DecideError::Provider { .. } => "DEC-E002",
            DecideError::Malformed(_) => "DEC-E003",
            DecideError::Deadline(_) => "DEC-E004",
            DecideError::ChainExhausted(_) => "DEC-E005",
            DecideError::InvalidQuestion(_) => "DEC-E006",
            DecideError::RateLimited { .. } => "DEC-E007",
            DecideError::EgressRefused(_) => "DEC-E008",
        }
    }

    /// The HTTP status behind a `DEC-E002`, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            DecideError::Provider { status, .. } => *status,
            _ => None,
        }
    }

    /// `Retry-After` seconds carried by a `DEC-E007`.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            DecideError::RateLimited { retry_after_secs, .. } => *retry_after_secs,
            _ => None,
        }
    }

    /// Whether a provider chain (`areev_llm::decide::Chain`) must stop here instead of trying the next entry:
    /// our own invalid question (`DEC-E006`), or a provider that refused the
    /// request itself as invalid (HTTP 400/422) — resending an invalid
    /// request elsewhere is not a fallback — or an egress pseudonymization
    /// failure (`DEC-E008`), where moving on could send raw state to an
    /// entry the wrap does not cover.
    pub fn stops_chain(&self) -> bool {
        matches!(
            self,
            DecideError::InvalidQuestion(_)
                | DecideError::Provider { status: Some(400 | 422), .. }
                | DecideError::EgressRefused(_)
        )
    }
}

impl fmt::Display for DecideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            DecideError::NotConfigured(m) => write!(f, "{code}: decision backend not configured: {m}"),
            DecideError::Provider { provider, status, message, retryable } => {
                let st = status.map(|s| format!(" HTTP {s}")).unwrap_or_default();
                let rt = if *retryable { " (retryable)" } else { "" };
                write!(f, "{code}: decision provider {provider}{st}{rt}: {message}")
            }
            DecideError::Malformed(m) => write!(f, "{code}: malformed decision answer: {m}"),
            DecideError::Deadline(m) => write!(f, "{code}: decision deadline exceeded: {m}"),
            DecideError::ChainExhausted(errs) => {
                write!(f, "{code}: every decision backend failed")?;
                for (i, (who, e)) in errs.iter().enumerate() {
                    let sep = if i == 0 { ": " } else { "; " };
                    write!(f, "{sep}{who} → {e}")?;
                }
                Ok(())
            }
            DecideError::InvalidQuestion(m) => write!(f, "{code}: invalid decision question: {m}"),
            DecideError::RateLimited { provider, retry_after_secs } => match retry_after_secs {
                Some(s) => write!(f, "{code}: decision provider {provider} rate limited (retry after {s}s)"),
                None => write!(f, "{code}: decision provider {provider} rate limited"),
            },
            DecideError::EgressRefused(m) => write!(
                f,
                "{code}: egress pseudonymization failed; the decision request was not sent: {m}"
            ),
        }
    }
}

impl std::error::Error for DecideError {}

// ---- questions ---------------------------------------------------------------

/// What the two outcomes of a yes/no question mean. Wire keys are `"true"`
/// and `"false"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoulCriteria {
    pub yes: String,
    pub no: String,
}

/// One typed question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Yes/no. `criteria` optionally describes the two outcomes.
    Noul { instructions: String, criteria: Option<NoulCriteria> },
    /// One of 2..=255 named options; the map value is the option's description.
    Choice { instructions: String, criteria: BTreeMap<String, String> },
    /// Ordered 2..=10 levels, index 0 = lowest.
    Score { instructions: String, levels: Vec<String> },
}

impl Question {
    /// A yes/no question with no outcome descriptions.
    pub fn noul(instructions: impl Into<String>) -> Self {
        Question::Noul { instructions: instructions.into(), criteria: None }
    }

    /// A choice over `(key, description)` options.
    pub fn choice<K: Into<String>, D: Into<String>>(
        instructions: impl Into<String>,
        options: impl IntoIterator<Item = (K, D)>,
    ) -> Self {
        Question::Choice {
            instructions: instructions.into(),
            criteria: options.into_iter().map(|(k, d)| (k.into(), d.into())).collect(),
        }
    }

    /// A score over ordered levels, lowest first.
    pub fn score<L: Into<String>>(
        instructions: impl Into<String>,
        levels: impl IntoIterator<Item = L>,
    ) -> Self {
        Question::Score {
            instructions: instructions.into(),
            levels: levels.into_iter().map(Into::into).collect(),
        }
    }

    /// The wire `type`: `"noul"`, `"choice"` or `"score"`.
    pub fn kind(&self) -> &'static str {
        match self {
            Question::Noul { .. } => "noul",
            Question::Choice { .. } => "choice",
            Question::Score { .. } => "score",
        }
    }

    pub fn instructions(&self) -> &str {
        match self {
            Question::Noul { instructions, .. }
            | Question::Choice { instructions, .. }
            | Question::Score { instructions, .. } => instructions,
        }
    }

    /// `DEC-E006` unless the question is askable: non-empty instructions,
    /// 2..=255 non-empty option keys, 2..=10 levels.
    pub fn validate(&self, id: &str) -> Result<(), DecideError> {
        let bad = |m: String| Err(DecideError::InvalidQuestion(format!("{id:?}: {m}")));
        if self.instructions().trim().is_empty() {
            return bad("instructions are empty".into());
        }
        match self {
            Question::Noul { .. } => Ok(()),
            Question::Choice { criteria, .. } => {
                if !CHOICE_OPTIONS.contains(&criteria.len()) {
                    return bad(format!("a choice needs 2..=255 options, got {}", criteria.len()));
                }
                if criteria.keys().any(|k| k.trim().is_empty()) {
                    return bad("a choice option key is empty".into());
                }
                Ok(())
            }
            Question::Score { levels, .. } => {
                if !SCORE_LEVELS.contains(&levels.len()) {
                    return bad(format!("a score needs 2..=10 levels, got {}", levels.len()));
                }
                Ok(())
            }
        }
    }

    /// The wire form: `criteria` is an object for noul/choice and an ARRAY of
    /// levels for score.
    pub fn to_wire(&self) -> Value {
        match self {
            Question::Noul { instructions, criteria } => {
                let mut o = json!({"type": "noul", "instructions": instructions});
                if let Some(c) = criteria {
                    o["criteria"] = json!({"true": c.yes, "false": c.no});
                }
                o
            }
            Question::Choice { instructions, criteria } => {
                json!({"type": "choice", "instructions": instructions, "criteria": criteria})
            }
            Question::Score { instructions, levels } => {
                json!({"type": "score", "instructions": instructions, "criteria": levels})
            }
        }
    }

    /// Parse one wire question. Shape faults are `DEC-E006`; the counts are
    /// checked too (this calls [`Question::validate`]).
    pub fn from_wire(id: &str, v: &Value) -> Result<Self, DecideError> {
        let bad = |m: &str| DecideError::InvalidQuestion(format!("{id:?}: {m}"));
        let o = v.as_object().ok_or_else(|| bad("a question must be a JSON object"))?;
        let instructions = o
            .get("instructions")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("missing string `instructions`"))?
            .to_string();
        let criteria = o.get("criteria").filter(|c| !c.is_null());
        let q = match o.get("type").and_then(Value::as_str) {
            Some("noul") => {
                let criteria = match criteria {
                    None => None,
                    Some(c) => {
                        let c = c.as_object().ok_or_else(|| bad("noul `criteria` must be an object"))?;
                        let get = |a: &str, b: &str| {
                            c.get(a).or_else(|| c.get(b)).and_then(Value::as_str).map(str::to_string)
                        };
                        Some(NoulCriteria {
                            yes: get("true", "yes").ok_or_else(|| bad("noul `criteria` needs a \"true\" string"))?,
                            no: get("false", "no").ok_or_else(|| bad("noul `criteria` needs a \"false\" string"))?,
                        })
                    }
                };
                Question::Noul { instructions, criteria }
            }
            Some("choice") => {
                let c = criteria
                    .and_then(Value::as_object)
                    .ok_or_else(|| bad("choice `criteria` must be an object of option → description"))?;
                let mut criteria = BTreeMap::new();
                for (k, d) in c {
                    let d = d.as_str().ok_or_else(|| bad("choice option descriptions must be strings"))?;
                    criteria.insert(k.clone(), d.to_string());
                }
                Question::Choice { instructions, criteria }
            }
            Some("score") => {
                let c = criteria
                    .and_then(Value::as_array)
                    .ok_or_else(|| bad("score `criteria` must be an array of levels, lowest first"))?;
                let mut levels = Vec::with_capacity(c.len());
                for l in c {
                    levels.push(l.as_str().ok_or_else(|| bad("score levels must be strings"))?.to_string());
                }
                Question::Score { instructions, levels }
            }
            Some(other) => return Err(bad(&format!("unknown question type {other:?} (noul|choice|score)"))),
            None => return Err(bad("missing `type` (noul|choice|score)")),
        };
        q.validate(id)?;
        Ok(q)
    }
}

/// Parse the wire `questions` object (`{id: question, …}`) — the shape
/// `areev decide --questions` and the bindings' `decide()` take.
pub fn questions_from_wire(v: &Value) -> Result<BTreeMap<String, Question>, DecideError> {
    let o = v
        .as_object()
        .ok_or_else(|| DecideError::InvalidQuestion("`questions` must be a JSON object of id → question".into()))?;
    let mut out = BTreeMap::new();
    for (id, q) in o {
        out.insert(id.clone(), Question::from_wire(id, q)?);
    }
    if out.is_empty() {
        return Err(DecideError::InvalidQuestion("no questions".into()));
    }
    Ok(out)
}

/// The wire `questions` object.
pub fn questions_to_wire(questions: &BTreeMap<String, Question>) -> Value {
    Value::Object(questions.iter().map(|(id, q)| (id.clone(), q.to_wire())).collect())
}

// ---- answers -----------------------------------------------------------------

/// One typed answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// `p` = probability of yes/true.
    Noul { p: f32 },
    Choice { choice: String, probabilities: BTreeMap<String, f32>, confidence: f32 },
    /// `score` = Σ p_i · i over 0-based level indices (probability-weighted
    /// index). `probabilities` and `legend` are keyed by the index as a
    /// decimal string (`"0"`, `"1"`, …).
    Score {
        score: f32,
        probabilities: BTreeMap<String, f32>,
        confidence: f32,
        legend: BTreeMap<String, String>,
    },
}

impl Answer {
    /// The wire `type`.
    pub fn kind(&self) -> &'static str {
        match self {
            Answer::Noul { .. } => "noul",
            Answer::Choice { .. } => "choice",
            Answer::Score { .. } => "score",
        }
    }

    /// The wire form (TypeSafe's response shape for one answer).
    pub fn to_wire(&self) -> Value {
        let probs = |m: &BTreeMap<String, f32>| {
            Value::Object(m.iter().map(|(k, p)| (k.clone(), num(*p))).collect())
        };
        match self {
            Answer::Noul { p } => json!({"type": "noul", "noul": num(*p)}),
            Answer::Choice { choice, probabilities, confidence } => json!({
                "type": "choice", "choice": choice,
                "probabilities": probs(probabilities), "confidence": num(*confidence),
            }),
            Answer::Score { score, probabilities, confidence, legend } => json!({
                "type": "score", "score": num(*score),
                "probabilities": probs(probabilities), "legend": legend,
                "confidence": num(*confidence),
            }),
        }
    }
}

/// `(n·p_max − 1)/(n − 1)`: 0 for a uniform distribution, 1 for a certain
/// one. TypeSafe's formula; `n` is the option or level count.
pub fn confidence_from(p_max: f32, n: usize) -> f32 {
    if n < 2 {
        return 1.0;
    }
    let n = n as f32;
    ((n * p_max - 1.0) / (n - 1.0)).clamp(0.0, 1.0)
}

/// An f32 as JSON by its shortest decimal form (`0.8`, not
/// `0.800000011920929`). Non-finite values never reach here (validation
/// refuses them) but degrade to `null` rather than panicking.
fn num(x: f32) -> Value {
    format!("{x}")
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn as_f32(v: &Value) -> Option<f32> {
    v.as_f64().map(|x| x as f32).filter(|x| x.is_finite())
}

/// Parse strictness: providers get [`Strict`](Mode::Strict); an LLM asked to
/// self-report gets [`Lenient`](Mode::Lenient) (any positive sum
/// renormalizes, level names accepted as keys, confidence always computed).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Strict,
    Lenient,
}

/// Validate and normalize a provider's `answers` object against the
/// questions that were asked (`DEC-E003` on any fault). Every requested id
/// must be present with a matching type; answers to ids nobody asked are
/// dropped, as are unknown fields. Probabilities summing within `1e-2` of 1
/// are normalized exactly; anything further off is refused. A provider's own
/// `score` and `confidence` are kept when present and computed only when
/// absent.
pub fn parse_wire_answers(
    questions: &BTreeMap<String, Question>,
    answers: &Value,
) -> Result<BTreeMap<String, Answer>, DecideError> {
    parse_answers(questions, answers, Mode::Strict)
}

/// The lenient parse for an LLM asked to self-report (`areev_llm`'s
/// `LlmEmulated`): a bare number is accepted as a noul probability, any
/// positive sum renormalizes, level names are accepted as score keys, a
/// named choice with no distribution becomes one-hot, and `confidence` is
/// always recomputed so the field means one thing across providers. A
/// missing id or a type mismatch is still `DEC-E003`.
pub fn parse_emulated_answers(
    questions: &BTreeMap<String, Question>,
    answers: &Value,
) -> Result<BTreeMap<String, Answer>, DecideError> {
    parse_answers(questions, answers, Mode::Lenient)
}

fn parse_answers(
    questions: &BTreeMap<String, Question>,
    answers: &Value,
    mode: Mode,
) -> Result<BTreeMap<String, Answer>, DecideError> {
    let o = answers
        .as_object()
        .ok_or_else(|| DecideError::Malformed("`answers` is not a JSON object".into()))?;
    let mut out = BTreeMap::new();
    for (id, q) in questions {
        let a = o
            .get(id)
            .ok_or_else(|| DecideError::Malformed(format!("no answer for question {id:?}")))?;
        out.insert(id.clone(), parse_answer(id, q, a, mode)?);
    }
    Ok(out)
}

fn parse_answer(id: &str, q: &Question, a: &Value, mode: Mode) -> Result<Answer, DecideError> {
    let bad = |m: String| DecideError::Malformed(format!("{id:?}: {m}"));
    // A bare number is an LLM's shorthand for a noul probability.
    if let (Mode::Lenient, Question::Noul { .. }, Some(p)) = (mode, q, as_f32(a)) {
        return noul_p(p).map(|p| Answer::Noul { p }).map_err(bad);
    }
    let o = a.as_object().ok_or_else(|| bad("an answer must be a JSON object".into()))?;
    match o.get("type").and_then(Value::as_str) {
        Some(t) if t == q.kind() => {}
        Some(t) if matches!(t, "noul" | "choice" | "score") => {
            return Err(bad(format!("answered as {t:?} but asked as {:?}", q.kind())))
        }
        Some(t) => return Err(bad(format!("unknown answer type {t:?}"))),
        None if o.contains_key("type") => return Err(bad("`type` is not a string".into())),
        None => {} // inferred from the question
    }
    match q {
        Question::Noul { .. } => {
            let p = ["noul", "p", "probability"]
                .iter()
                .find_map(|k| o.get(*k))
                .ok_or_else(|| bad("noul answer has no `noul` probability".into()))?;
            let p = as_f32(p).ok_or_else(|| bad("noul probability is not a finite number".into()))?;
            noul_p(p).map(|p| Answer::Noul { p }).map_err(bad)
        }
        Question::Choice { criteria, .. } => {
            let mut dist: BTreeMap<String, f32> = criteria.keys().map(|k| (k.clone(), 0.0)).collect();
            let named = o.get("choice").and_then(Value::as_str).map(str::to_string);
            match o.get("probabilities").and_then(Value::as_object) {
                Some(ps) => {
                    for (k, p) in ps {
                        let slot = dist
                            .get_mut(k)
                            .ok_or_else(|| bad(format!("probability for unknown option {k:?}")))?;
                        *slot = as_f32(p).ok_or_else(|| bad(format!("probability for {k:?} is not a finite number")))?;
                    }
                }
                None => match (mode, &named) {
                    // An LLM that named a choice but gave no distribution: one-hot.
                    (Mode::Lenient, Some(c)) if dist.contains_key(c) => {
                        dist.insert(c.clone(), 1.0);
                    }
                    _ => return Err(bad("choice answer has no `probabilities`".into())),
                },
            }
            normalize(&mut dist, mode).map_err(bad)?;
            let (arg, p_max) = argmax(&dist);
            let choice = match named {
                Some(c) if dist.contains_key(&c) => c,
                Some(c) if mode == Mode::Strict => return Err(bad(format!("choice {c:?} is not an option"))),
                _ => arg,
            };
            let confidence = provided_confidence(o, mode)
                .map_err(bad)?
                .unwrap_or_else(|| confidence_from(p_max, criteria.len()));
            Ok(Answer::Choice { choice, probabilities: dist, confidence })
        }
        Question::Score { levels, .. } => {
            let n = levels.len();
            let mut dist: BTreeMap<String, f32> = (0..n).map(|i| (i.to_string(), 0.0)).collect();
            match o.get("probabilities").and_then(Value::as_object) {
                Some(ps) => {
                    for (k, p) in ps {
                        let idx = k
                            .parse::<usize>()
                            .ok()
                            .filter(|i| *i < n)
                            .or_else(|| match mode {
                                Mode::Lenient => levels.iter().position(|l| l == k),
                                Mode::Strict => None,
                            })
                            .ok_or_else(|| bad(format!("probability for unknown level {k:?}")))?;
                        let p = as_f32(p).ok_or_else(|| bad(format!("probability for {k:?} is not a finite number")))?;
                        dist.insert(idx.to_string(), p);
                    }
                }
                None => match (mode, o.get("score").and_then(as_f32)) {
                    (Mode::Lenient, Some(s)) if s >= 0.0 && s <= (n - 1) as f32 => {
                        dist.insert((s.round() as usize).to_string(), 1.0);
                    }
                    _ => return Err(bad("score answer has no `probabilities`".into())),
                },
            }
            normalize(&mut dist, mode).map_err(bad)?;
            let weighted: f32 = dist.iter().map(|(k, p)| k.parse::<f32>().unwrap_or(0.0) * p).sum();
            let score = match (mode, o.get("score").and_then(as_f32)) {
                (Mode::Strict, Some(s)) if s >= 0.0 && s <= (n - 1) as f32 => s,
                (Mode::Strict, Some(s)) => return Err(bad(format!("score {s} is outside 0..={}", n - 1))),
                _ => weighted,
            };
            let (_, p_max) = argmax(&dist);
            let confidence = provided_confidence(o, mode)
                .map_err(bad)?
                .unwrap_or_else(|| confidence_from(p_max, n));
            let legend = o
                .get("legend")
                .and_then(Value::as_object)
                .filter(|_| mode == Mode::Strict)
                .map(|l| {
                    l.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect::<BTreeMap<_, _>>()
                })
                .filter(|l| l.len() == n)
                .unwrap_or_else(|| levels.iter().enumerate().map(|(i, l)| (i.to_string(), l.clone())).collect());
            Ok(Answer::Score { score, probabilities: dist, confidence, legend })
        }
    }
}

fn noul_p(p: f32) -> Result<f32, String> {
    if (-SUM_TOLERANCE..=1.0 + SUM_TOLERANCE).contains(&p) {
        Ok(p.clamp(0.0, 1.0))
    } else {
        Err(format!("noul probability {p} is outside 0..=1"))
    }
}

/// Normalize a distribution in place. Strict: the sum must already be within
/// [`SUM_TOLERANCE`] of 1. Lenient: any positive sum.
fn normalize(dist: &mut BTreeMap<String, f32>, mode: Mode) -> Result<(), String> {
    if let Some((k, p)) = dist.iter().find(|(_, p)| **p < 0.0) {
        return Err(format!("negative probability {p} for {k:?}"));
    }
    let sum: f32 = dist.values().sum();
    let ok = match mode {
        Mode::Strict => (sum - 1.0).abs() <= SUM_TOLERANCE + SUM_SLACK,
        Mode::Lenient => sum > 0.0,
    };
    if !ok {
        return Err(format!("probabilities sum to {sum}, not 1"));
    }
    for p in dist.values_mut() {
        *p /= sum;
    }
    Ok(())
}

/// The highest-probability key (first in key order on a tie) and its mass.
fn argmax(dist: &BTreeMap<String, f32>) -> (String, f32) {
    let mut best = (String::new(), f32::NEG_INFINITY);
    for (k, p) in dist {
        if *p > best.1 {
            best = (k.clone(), *p);
        }
    }
    best
}

/// A provider's own `confidence`, honoured in strict mode only (emulation
/// always recomputes so the field means one thing).
fn provided_confidence(o: &Map<String, Value>, mode: Mode) -> Result<Option<f32>, String> {
    if mode == Mode::Lenient {
        return Ok(None);
    }
    match o.get("confidence") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => as_f32(v)
            .map(|c| Some(c.clamp(0.0, 1.0)))
            .ok_or_else(|| "confidence is not a finite number".to_string()),
    }
}

// ---- request / decision --------------------------------------------------------

/// One decision call.
#[derive(Debug, Clone, PartialEq)]
pub struct DecideRequest {
    /// What is being judged: a string, object or array.
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
    /// Per-call budget; `None` = the backend's (or chain's) default.
    pub deadline: Option<Duration>,
}

impl DecideRequest {
    pub fn new(state: impl Into<Value>, questions: BTreeMap<String, Question>) -> Self {
        DecideRequest { state: state.into(), questions, deadline: None }
    }

    pub fn with_deadline(mut self, deadline: Option<Duration>) -> Self {
        self.deadline = deadline;
        self
    }

    /// `DEC-E006` unless there is at least one question and every question
    /// is askable, and `state` is a string, object or array.
    pub fn validate(&self) -> Result<(), DecideError> {
        if self.questions.is_empty() {
            return Err(DecideError::InvalidQuestion("no questions".into()));
        }
        if !(self.state.is_string() || self.state.is_object() || self.state.is_array()) {
            return Err(DecideError::InvalidQuestion(
                "`state` must be a string, object or array".into(),
            ));
        }
        for (id, q) in &self.questions {
            if id.trim().is_empty() {
                return Err(DecideError::InvalidQuestion("a question id is empty".into()));
            }
            q.validate(id)?;
        }
        Ok(())
    }

    /// The wire request. `model` is omitted when `None` (the Cloudflare and
    /// command shapes carry none).
    pub fn to_wire(&self, model: Option<&str>) -> Value {
        let mut o = Map::new();
        if let Some(m) = model {
            o.insert("model".into(), Value::String(m.to_string()));
        }
        o.insert("state".into(), self.state.clone());
        o.insert("questions".into(), questions_to_wire(&self.questions));
        Value::Object(o)
    }
}

/// A validated answer set plus its provenance (proposal §2 rule 4).
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub answers: BTreeMap<String, Answer>,
    /// As served, e.g. `"jev-1.13.0"`.
    pub model: String,
    /// Spec name, e.g. `"typesafe"`, `"cloudflare"`, `"cmd"`, `"llm"`.
    pub provider: String,
    pub calibrated: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// The provider-reported price of this decision (`usage.cost`, USD, as
    /// OpenRouter-style gateways report it) in micro-dollars, rounded UP so
    /// a sum of sub-micro calls never under-charges a budget. `None` when
    /// the provider reported no cost — never estimated from tokens.
    pub usd_micros: Option<u64>,
    pub latency_ms: u64,
}

impl Decision {
    /// The wire response plus provenance: `{model, answers, usage?, provider,
    /// calibrated, latency_ms}` — what `areev decide` and the bindings print.
    /// `usage` carries `usd_micros` when the provider reported a cost.
    pub fn to_json(&self) -> Value {
        let mut o = json!({
            "model": self.model,
            "answers": Value::Object(self.answers.iter().map(|(k, a)| (k.clone(), a.to_wire())).collect()),
            "provider": self.provider,
            "calibrated": self.calibrated,
            "latency_ms": self.latency_ms,
        });
        if self.input_tokens.is_some() || self.output_tokens.is_some() || self.usd_micros.is_some() {
            o["usage"] = json!({"input_tokens": self.input_tokens, "output_tokens": self.output_tokens});
            if let Some(n) = self.usd_micros {
                o["usage"]["usd_micros"] = json!(n);
            }
        }
        o
    }

    /// Build from a wire response body (`{model?, answers, usage?}`),
    /// validating it strictly against the request's questions
    /// ([`parse_wire_answers`]). `model` falls back to `default_model` when
    /// the body names none; `latency_ms` is measured from `started`. The
    /// constructor every HTTP and command adapter shares.
    pub fn from_wire(
        req: &DecideRequest,
        body: &Value,
        provider: &str,
        default_model: &str,
        calibrated: bool,
        started: Instant,
    ) -> Result<Decision, DecideError> {
        let answers = body
            .get("answers")
            .ok_or_else(|| DecideError::Malformed("response has no `answers`".into()))?;
        let answers = parse_answers(&req.questions, answers, Mode::Strict)?;
        let tokens = |k: &str| body.get("usage").and_then(|u| u.get(k)).and_then(Value::as_u64);
        Ok(Decision {
            answers,
            model: body
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or(default_model)
                .to_string(),
            provider: provider.to_string(),
            calibrated,
            input_tokens: tokens("input_tokens"),
            output_tokens: tokens("output_tokens"),
            usd_micros: body
                .get("usage")
                .and_then(|u| u.get("cost"))
                .and_then(Value::as_f64)
                .and_then(usd_to_micros),
            latency_ms: elapsed_ms(started),
        })
    }
}

/// A provider's USD `cost` as whole micro-dollars, rounded up. A negative,
/// non-finite or absurd value is not a price — `None`, never a guess.
fn usd_to_micros(usd: f64) -> Option<u64> {
    let micros = (usd * 1_000_000.0).ceil();
    (usd.is_finite() && usd >= 0.0 && micros < u64::MAX as f64).then_some(micros as u64)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The seam. Implementations validate the request (`DEC-E006`) and their
/// answers (`DEC-E003`) and honour `req.deadline`.
pub trait DecisionBackend: Send + Sync {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError>;
    /// Whether this backend's probabilities are calibrated. Policy that
    /// omits/drops/skips on a probability must fall back to rank-only
    /// behaviour when this is false.
    fn calibrated(&self) -> bool;
    /// Stable human label for provenance, e.g. `"typesafe:jev-latest"`.
    fn describe(&self) -> String;
}
#[cfg(test)]
mod tests {
    use super::*;

    fn qs(pairs: Vec<(&str, Question)>) -> BTreeMap<String, Question> {
        pairs.into_iter().map(|(k, q)| (k.to_string(), q)).collect()
    }

    #[test]
    fn provider_cost_is_micro_dollars_rounded_up_and_never_guessed() {
        assert_eq!(usd_to_micros(0.00001575), Some(16));
        assert_eq!(usd_to_micros(0.0), Some(0));
        assert_eq!(usd_to_micros(1.5), Some(1_500_000));
        assert_eq!(usd_to_micros(-0.01), None);
        assert_eq!(usd_to_micros(f64::NAN), None);
        assert_eq!(usd_to_micros(f64::INFINITY), None);
        let req = DecideRequest::new("s", qs(vec![("ok", Question::noul("ok?"))]));
        let body = json!({"answers": {"ok": {"type": "noul", "noul": 0.9}}, "usage": {"input_tokens": 3}});
        let d = Decision::from_wire(&req, &body, "p", "m", true, Instant::now()).unwrap();
        assert_eq!(d.usd_micros, None, "no reported cost is no cost, not an estimate");
        assert!(d.to_json()["usage"].get("usd_micros").is_none());
    }

    #[test]
    fn codes_are_unique_well_formed_and_lead_display() {
        let all = [
            DecideError::NotConfigured(String::new()),
            DecideError::Provider { provider: String::new(), status: None, message: String::new(), retryable: false },
            DecideError::Malformed(String::new()),
            DecideError::Deadline(String::new()),
            DecideError::ChainExhausted(Vec::new()),
            DecideError::InvalidQuestion(String::new()),
            DecideError::RateLimited { provider: String::new(), retry_after_secs: None },
            DecideError::EgressRefused(String::new()),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for e in &all {
            let c = e.code();
            assert!(c.starts_with("DEC-E") && c.len() == 8, "bad code {c}");
            assert!(seen.insert(c), "duplicate code {c}");
            assert!(e.to_string().starts_with(&format!("{c}: ")), "{e}");
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn confidence_formula() {
        assert_eq!(confidence_from(0.5, 2), 0.0);
        assert_eq!(confidence_from(1.0, 4), 1.0);
        assert!((confidence_from(0.8, 2) - 0.6).abs() < 1e-6);
        assert!((confidence_from(0.5, 3) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn question_bounds_are_enforced() {
        let one = Question::choice("pick", [("a", "A")]);
        assert_eq!(one.validate("q").unwrap_err().code(), "DEC-E006");
        let many = Question::Choice {
            instructions: "pick".into(),
            criteria: (0..256).map(|i| (format!("o{i}"), String::new())).collect(),
        };
        assert_eq!(many.validate("q").unwrap_err().code(), "DEC-E006");
        let max = Question::Choice {
            instructions: "pick".into(),
            criteria: (0..255).map(|i| (format!("o{i}"), String::new())).collect(),
        };
        assert!(max.validate("q").is_ok());
        assert!(Question::score("s", ["lo"]).validate("q").is_err());
        assert!(Question::score("s", (0..11).map(|i| i.to_string())).validate("q").is_err());
        assert!(Question::score("s", (0..10).map(|i| i.to_string())).validate("q").is_ok());
        assert!(Question::noul("  ").validate("q").is_err());
        let empty = DecideRequest::new("s", BTreeMap::new());
        assert_eq!(empty.validate().unwrap_err().code(), "DEC-E006");
        let numeric_state = DecideRequest::new(json!(3), qs(vec![("a", Question::noul("x"))]));
        assert_eq!(numeric_state.validate().unwrap_err().code(), "DEC-E006");
    }

    #[test]
    fn question_wire_roundtrips() {
        let q = qs(vec![
            ("n", Question::Noul {
                instructions: "is it?".into(),
                criteria: Some(NoulCriteria { yes: "it is".into(), no: "it is not".into() }),
            }),
            ("c", Question::choice("which", [("a", "A"), ("b", "B")])),
            ("s", Question::score("how much", ["low", "mid", "high"])),
        ]);
        let wire = questions_to_wire(&q);
        assert_eq!(wire["n"]["criteria"], json!({"true": "it is", "false": "it is not"}));
        assert_eq!(wire["s"]["criteria"], json!(["low", "mid", "high"]));
        assert_eq!(questions_from_wire(&wire).unwrap(), q);
        let bad = json!({"x": {"type": "rank", "instructions": "?"}});
        assert_eq!(questions_from_wire(&bad).unwrap_err().code(), "DEC-E006");
    }

    #[test]
    fn strict_parse_normalizes_within_tolerance_and_refuses_beyond() {
        let q = qs(vec![("c", Question::choice("w", [("a", ""), ("b", "")]))]);
        let near = json!({"c": {"type": "choice", "choice": "a", "probabilities": {"a": 0.8, "b": 0.2005}}});
        let a = parse_wire_answers(&q, &near).unwrap();
        let Answer::Choice { probabilities, confidence, .. } = &a["c"] else { panic!() };
        assert!((probabilities.values().sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((confidence - 0.6).abs() < 1e-3, "computed when omitted: {confidence}");
        let rounded = json!({"c": {"type": "choice", "probabilities": {"a": 0.8, "b": 0.19}}});
        assert!(parse_wire_answers(&q, &rounded).is_ok(), "two-place rounding (sum 0.99) normalizes");
        let far = json!({"c": {"type": "choice", "choice": "a", "probabilities": {"a": 1.0, "b": 1.0}}});
        assert_eq!(parse_wire_answers(&q, &far).unwrap_err().code(), "DEC-E003");
        let mismatch = json!({"c": {"type": "noul", "noul": 0.5}});
        assert_eq!(parse_wire_answers(&q, &mismatch).unwrap_err().code(), "DEC-E003");
        let unknown = json!({"c": {"type": "rank"}});
        assert_eq!(parse_wire_answers(&q, &unknown).unwrap_err().code(), "DEC-E003");
        let no_probs = json!({"c": {"type": "choice", "choice": "a"}});
        assert_eq!(parse_wire_answers(&q, &no_probs).unwrap_err().code(), "DEC-E003");
        let stray = json!({"c": {"type": "choice", "probabilities": {"a": 0.5, "z": 0.5}}});
        assert_eq!(parse_wire_answers(&q, &stray).unwrap_err().code(), "DEC-E003");
    }

    /// Regression (LoCoMo A/B, 2026-09-25): TypeSafe via OpenRouter rounds
    /// each probability to two places, so a four-level score can sum to
    /// 0.99 — which f32 addition turns into 0.98999995, refused by a bare
    /// `<= 1e-2` check. 55 of 1,972 rerank requests failed on exactly this.
    #[test]
    fn two_place_rounding_at_the_tolerance_edge_normalizes() {
        let q = qs(vec![("s", Question::score("s", ["a", "b", "c", "d"]))]);
        for target in [99u32, 101] {
            for a in 0..=target.min(100) {
                for b in 0..=(target - a).min(100) {
                    let rest = target - a - b;
                    let (c, d) = (rest / 2, rest - rest / 2);
                    if c > 100 || d > 100 {
                        continue;
                    }
                    let p = |x: u32| x as f64 / 100.0;
                    let ans = json!({"s": {"type": "score", "probabilities":
                        {"0": p(a), "1": p(b), "2": p(c), "3": p(d)}}});
                    assert!(
                        parse_wire_answers(&q, &ans).is_ok(),
                        "{a}+{b}+{c}+{d} = {target}/100 must normalize"
                    );
                }
            }
        }
        // Still refused beyond the tolerance.
        let far = json!({"s": {"type": "score", "probabilities": {"0": 0.5, "1": 0.2, "2": 0.2, "3": 0.08}}});
        assert_eq!(parse_wire_answers(&q, &far).unwrap_err().code(), "DEC-E003");
    }

    #[test]
    fn floats_serialize_by_shortest_form() {
        assert_eq!(num(0.8).to_string(), "0.8");
        assert_eq!(Answer::Noul { p: 0.93 }.to_wire(), json!({"type": "noul", "noul": 0.93}));
    }
}
