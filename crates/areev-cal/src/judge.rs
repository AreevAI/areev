//! Decision-backend judgments for context assembly (decision-backend phase 3,
//! `docs/decision-model-proposal.md` §4 rows A2/A3).
//!
//! ONE home for the questions both assembly paths ask a
//! [`DecisionBackend`], so the wording, the thresholds and the answer
//! validation cannot drift between them:
//!
//! - **A2 disclosure** ([`judge_candidates`]) — per candidate, how relevant it
//!   is to the query (`score` over four levels) and whether a one-line summary
//!   would lose a detail the query needs (`noul`). `areev-context`'s allocator
//!   and CAL's multi-source `ASSEMBLE` trim both consume it.
//! - **A3 intent** ([`judge_intent`]) — one `choice` on the query: timeline,
//!   current state, or general. `areev-context` only.
//!
//! This module asks and validates; it never decides what to omit. The
//! callers do, under proposal §2's rules: a decision model may score and
//! order, only code omits, and an **uncalibrated** backend may reorder but
//! never omit. Every error here is a `DecideError` the caller turns into
//! "fall back to today's rule" (fail open).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use areev_core::decide::{Answer, DecideError, DecideRequest, Decision, DecisionBackend, Question};
use serde_json::{json, Value};

/// The four relevance levels, lowest first. `score / 3` is the relevance in
/// `[0, 1]` the allocators consume.
pub const RELEVANCE_LEVELS: [&str; 4] = ["off-topic", "tangential", "relevant", "directly answers"];

/// Estimated-token ceiling (chars / 4, the one estimator's heuristic) for one
/// request's serialized `state`. A candidate list past it is split across
/// several requests.
pub const MAX_STATE_TOKENS: usize = 28_000;

/// Each candidate's text is capped at this many characters before it goes
/// into `state` — enough to judge relevance, bounded so one long grain cannot
/// crowd the others out of a request.
pub const CANDIDATE_TEXT_CHARS: usize = 600;

/// Default calibrated drop line: relevance below this is omitted.
pub const DEFAULT_DROP_BELOW: f32 = 0.10;

/// Default calibrated verbatim line: `p(summary loses a needed detail)` at or
/// above this prefers the Full render.
pub const DEFAULT_FULL_ABOVE: f32 = 0.50;

/// The id of the intent question.
pub const INTENT_QUESTION: &str = "intent";

/// One candidate's judgment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Judgment {
    /// `score / 3` over [`RELEVANCE_LEVELS`], in `[0, 1]`.
    pub relevance: f32,
    /// `p(yes)` for "would a one-line summary lose a detail the query needs?".
    pub p_verbatim: f32,
}

/// Provenance shared by every judgment kind (proposal §2 rule 4).
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeProvenance {
    /// `Decision.provider`; several distinct values (a chain that fell back
    /// between requests) are joined with `,` in first-seen order.
    pub provider: String,
    /// `Decision.model`, joined the same way.
    pub model: String,
    /// AND over every decision that contributed.
    pub calibrated: bool,
    /// Backend calls made.
    pub requests: u32,
    /// Sum of each decision's `latency_ms`.
    pub latency_ms: u64,
}

impl JudgeProvenance {
    fn from_decisions(ds: &[Decision]) -> Self {
        let join = |f: fn(&Decision) -> &str| {
            let mut seen: Vec<&str> = Vec::new();
            for d in ds {
                if !seen.contains(&f(d)) {
                    seen.push(f(d));
                }
            }
            seen.join(",")
        };
        JudgeProvenance {
            provider: join(|d| d.provider.as_str()),
            model: join(|d| d.model.as_str()),
            calibrated: ds.iter().all(|d| d.calibrated),
            requests: ds.len() as u32,
            latency_ms: ds.iter().map(|d| d.latency_ms).sum(),
        }
    }

    /// Merge two provenances (intent + disclosure) into one record.
    pub fn merge(&self, other: &JudgeProvenance) -> JudgeProvenance {
        let join = |a: &str, b: &str| {
            let mut parts: Vec<&str> = a.split(',').filter(|s| !s.is_empty()).collect();
            for p in b.split(',').filter(|s| !s.is_empty()) {
                if !parts.contains(&p) {
                    parts.push(p);
                }
            }
            parts.join(",")
        };
        JudgeProvenance {
            provider: join(&self.provider, &other.provider),
            model: join(&self.model, &other.model),
            calibrated: self.calibrated && other.calibrated,
            requests: self.requests + other.requests,
            latency_ms: self.latency_ms + other.latency_ms,
        }
    }
}

/// A2's result: one [`Judgment`] per candidate, in input order.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgments {
    pub per_candidate: Vec<Judgment>,
    pub provenance: JudgeProvenance,
}

/// The three intents A3 chooses between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryIntent {
    /// Sequence, history, what happened when → timeline rendering.
    Timeline,
    /// The latest/current value → recency (Knowledge Update) suppression.
    CurrentState,
    /// Anything else.
    General,
}

impl QueryIntent {
    /// The wire option key.
    pub fn as_str(self) -> &'static str {
        match self {
            QueryIntent::Timeline => "timeline",
            QueryIntent::CurrentState => "current_state",
            QueryIntent::General => "general",
        }
    }
}

/// A3's result.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentJudgment {
    pub intent: QueryIntent,
    pub provenance: JudgeProvenance,
}

/// `s` capped at `max` characters, on a char boundary.
pub fn cap_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The A3 request: one `choice` on `{"query": …}`.
pub fn intent_request(query: &str) -> DecideRequest {
    let mut qs = BTreeMap::new();
    qs.insert(
        INTENT_QUESTION.to_string(),
        Question::choice(
            "Which kind of answer does `query` ask for?",
            [
                ("timeline", "the question is about sequence, history, what happened when"),
                ("current_state", "the question wants the latest/current value"),
                ("general", "any other question"),
            ],
        ),
    );
    DecideRequest::new(json!({ "query": query }), qs)
}

/// The two A2 questions for the candidate at position `n` of a request's
/// `candidates` array (ids `rel_<n>` and `verbatim_<n>`).
fn candidate_questions(n: usize, qs: &mut BTreeMap<String, Question>) {
    qs.insert(
        format!("rel_{n}"),
        Question::score(
            format!("How relevant is candidates[{n}] to `query` for answering it?"),
            RELEVANCE_LEVELS,
        ),
    );
    qs.insert(
        format!("verbatim_{n}"),
        Question::noul(format!(
            "Would a one-line summary of candidates[{n}] lose a detail `query` needs?"
        )),
    );
}

/// The A2 requests for `texts`, each paired with the index of its first
/// candidate in `texts`. Within a request, `"i"` is the candidate's position
/// in that request's `candidates` array, so `candidates[n]` in a question is
/// literally that element. A request's serialized `state` stays under
/// [`MAX_STATE_TOKENS`] (chars / 4); one candidate always goes in even alone
/// over it. No request is built for an empty `texts`.
pub fn disclosure_requests(query: &str, texts: &[String]) -> Vec<(usize, DecideRequest)> {
    let budget_chars = MAX_STATE_TOKENS * 4;
    let base = json!({ "query": query, "candidates": [] }).to_string().len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut cands: Vec<Value> = Vec::new();
    let mut size = base;
    let flush = |start: usize, cands: &mut Vec<Value>, out: &mut Vec<(usize, DecideRequest)>| {
        let mut qs = BTreeMap::new();
        for n in 0..cands.len() {
            candidate_questions(n, &mut qs);
        }
        let state = json!({ "query": query, "candidates": std::mem::take(cands) });
        out.push((start, DecideRequest::new(state, qs)));
    };
    for (idx, text) in texts.iter().enumerate() {
        let n = cands.len();
        let item = json!({ "i": n, "text": cap_chars(text, CANDIDATE_TEXT_CHARS) });
        let item_len = item.to_string().len() + 1; // + the separating comma
        if !cands.is_empty() && size + item_len > budget_chars {
            flush(start, &mut cands, &mut out);
            start = idx;
            size = base;
            // Re-number: this candidate is now element 0 of the next request.
            let item = json!({ "i": 0, "text": cap_chars(text, CANDIDATE_TEXT_CHARS) });
            size += item.to_string().len() + 1;
            cands.push(item);
            continue;
        }
        size += item_len;
        cands.push(item);
    }
    if !cands.is_empty() {
        flush(start, &mut cands, &mut out);
    }
    out
}

/// What is left of `deadline` after `started`; `DEC-E004` once it is spent.
fn remaining(deadline: Option<Duration>, started: Instant) -> Result<Option<Duration>, DecideError> {
    match deadline {
        None => Ok(None),
        Some(d) => match d.checked_sub(started.elapsed()) {
            Some(r) if !r.is_zero() => Ok(Some(r)),
            _ => Err(DecideError::Deadline(format!(
                "the {} ms judgment budget was spent before every request was sent",
                d.as_millis()
            ))),
        },
    }
}

/// Run the A2 requests over `texts` (one per candidate, in order) and return
/// one [`Judgment`] each. `deadline` bounds the WHOLE call: each request gets
/// what is left of it. All-or-nothing: any backend error, and any answer that
/// is not a `score` in `0..=3` / a `noul` in `0..=1`, is an `Err` — the
/// caller falls back to today's allocation rather than acting on a partial
/// set. An empty `texts` is `Ok` with no requests made.
pub fn judge_candidates(
    backend: &dyn DecisionBackend,
    query: &str,
    texts: &[String],
    deadline: Option<Duration>,
) -> Result<Judgments, DecideError> {
    let started = Instant::now();
    let mut per_candidate = Vec::with_capacity(texts.len());
    let mut decisions = Vec::new();
    for (start, req) in disclosure_requests(query, texts) {
        let req = req.with_deadline(remaining(deadline, started)?);
        let decision = backend.decide(&req)?;
        let count = req.questions.len() / 2;
        for n in 0..count {
            let rel = match decision.answers.get(&format!("rel_{n}")) {
                Some(Answer::Score { score, .. }) if score.is_finite() && (0.0..=3.0).contains(score) => {
                    score / 3.0
                }
                other => {
                    return Err(DecideError::Malformed(format!(
                        "candidate {}: rel_{n} is not a score in 0..=3 ({other:?})",
                        start + n
                    )))
                }
            };
            let verbatim = match decision.answers.get(&format!("verbatim_{n}")) {
                Some(Answer::Noul { p }) if p.is_finite() && (0.0..=1.0).contains(p) => *p,
                other => {
                    return Err(DecideError::Malformed(format!(
                        "candidate {}: verbatim_{n} is not a noul in 0..=1 ({other:?})",
                        start + n
                    )))
                }
            };
            per_candidate.push(Judgment { relevance: rel, p_verbatim: verbatim });
        }
        decisions.push(decision);
    }
    Ok(Judgments { per_candidate, provenance: JudgeProvenance::from_decisions(&decisions) })
}

/// Run the A3 request. An answer that is not one of the three options is
/// `DEC-E003` (fail open to the keyword lists).
pub fn judge_intent(
    backend: &dyn DecisionBackend,
    query: &str,
    deadline: Option<Duration>,
) -> Result<IntentJudgment, DecideError> {
    let req = intent_request(query).with_deadline(deadline);
    let decision = backend.decide(&req)?;
    let intent = match decision.answers.get(INTENT_QUESTION) {
        Some(Answer::Choice { choice, .. }) => match choice.as_str() {
            "timeline" => QueryIntent::Timeline,
            "current_state" => QueryIntent::CurrentState,
            "general" => QueryIntent::General,
            other => return Err(DecideError::Malformed(format!("intent choice {other:?} is not an option"))),
        },
        other => return Err(DecideError::Malformed(format!("intent is not a choice ({other:?})"))),
    };
    Ok(IntentJudgment { intent, provenance: JudgeProvenance::from_decisions(&[decision]) })
}

/// Order `idx` (positions into `judgments`) by relevance descending; ties
/// keep their input order. The order a relevance-aware trim spends a budget
/// in.
pub fn by_relevance_desc(idx: &mut [usize], judgments: &[Judgment]) {
    idx.sort_by(|&a, &b| {
        judgments[b]
            .relevance
            .partial_cmp(&judgments[a].relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_split_under_the_state_cap_and_renumber() {
        let texts: Vec<String> = (0..400).map(|i| format!("{i:03}").repeat(200)).collect();
        let reqs = disclosure_requests("q", &texts);
        assert!(reqs.len() >= 2, "400 × 600 chars must split");
        let mut covered = 0;
        for (start, r) in &reqs {
            assert_eq!(*start, covered);
            assert!(r.state.to_string().len() / 4 <= MAX_STATE_TOKENS);
            let cands = r.state["candidates"].as_array().unwrap();
            assert_eq!(cands[0]["i"], 0);
            assert_eq!(r.questions.len(), cands.len() * 2);
            assert!(r.validate().is_ok());
            covered += cands.len();
        }
        assert_eq!(covered, texts.len());
    }

    #[test]
    fn candidate_text_is_capped_on_a_char_boundary() {
        let long = "é".repeat(700);
        let reqs = disclosure_requests("q", &[long]);
        let text = reqs[0].1.state["candidates"][0]["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), CANDIDATE_TEXT_CHARS);
    }

    #[test]
    fn intent_request_is_askable() {
        let r = intent_request("when did it happen?");
        assert!(r.validate().is_ok());
        assert_eq!(r.questions[INTENT_QUESTION].kind(), "choice");
    }

    #[test]
    fn provenance_merge_joins_distinct_and_ands_calibration() {
        let a = JudgeProvenance { provider: "x".into(), model: "m".into(), calibrated: true, requests: 1, latency_ms: 3 };
        let b = JudgeProvenance { provider: "y".into(), model: "m".into(), calibrated: false, requests: 2, latency_ms: 4 };
        let m = a.merge(&b);
        assert_eq!(m.provider, "x,y");
        assert_eq!(m.model, "m");
        assert!(!m.calibrated);
        assert_eq!((m.requests, m.latency_ms), (3, 7));
    }
}
