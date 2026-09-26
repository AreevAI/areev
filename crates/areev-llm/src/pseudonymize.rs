//! `PseudonymizingBackend` — the egress decorator for in-process LLM calls
//! (docs/anonymization-proposal.md §4.1): pseudonymize the request before it
//! leaves the process, rehydrate the response before the caller sees it.
//! One wrap covers `remember()` extraction, the loop's verifier, and the
//! runtime's tool-calling LLM. [`PseudonymizingDecider`] is the same
//! decorator for decision backends: it rewrites `DecideRequest.state` only.
//!
//! Fail-closed (D6): a transform error fails the call — raw content never
//! goes out as a fallback. Rehydration is exact-token and never guesses;
//! unmatched placeholders in a response stay visible rather than being
//! papered over.

use std::sync::{Arc, Mutex};

use areev_core::anon::{AnonPolicy, SessionAnonymizer};
use areev_core::decide::{DecideError, DecideRequest, Decision, DecisionBackend};
use areev_loop::error::Error as LoopError;
use areev_loop::LlmBackend;
use serde_json::Value;

pub struct PseudonymizingBackend<L> {
    inner: L,
    /// One session for the decorator's lifetime, so tokens stay consistent
    /// across calls and every response can rehydrate against the
    /// accumulated mapping.
    session: Mutex<SessionAnonymizer>,
    /// Identities the host already holds (e.g. the session's subjects) —
    /// makes bare names in prose detectable with no NER.
    known: Vec<String>,
}

impl<L> PseudonymizingBackend<L> {
    /// Wrap `inner` under `policy`. `memory`-scope policies are refused here
    /// (they are keyed by the store's file key; the decorator is store-free)
    /// — use `context` or `session` scope, which is what a per-process
    /// decorator means anyway.
    pub fn new(inner: L, policy: AnonPolicy) -> Result<Self, areev_core::error::AreevError> {
        Ok(PseudonymizingBackend {
            inner,
            session: Mutex::new(SessionAnonymizer::new(policy)?),
            known: Vec::new(),
        })
    }

    pub fn with_known_identities(mut self, ids: Vec<String>) -> Self {
        self.known = ids;
        self
    }

    /// The accumulated placeholder → value mapping (D5 custody: the host
    /// process holds it; it never rides the wire).
    pub fn mapping(&self) -> std::collections::BTreeMap<String, String> {
        self.session.lock().unwrap().mapping().clone()
    }
}

impl<L: LlmBackend> LlmBackend for PseudonymizingBackend<L> {
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn complete(&self, request: &str) -> Result<String, LoopError> {
        let mut session = self.session.lock().unwrap();
        let (anon_request, _) = session
            .transform_text(request, &self.known)
            .map_err(|e| LoopError::LlmBackend(format!("pseudonymize request: {e}")))?;
        let response = self.inner.complete(&anon_request)?;
        let back = areev_core::anon::rehydrate(&response, session.mapping())
            .map_err(|e| LoopError::LlmBackend(format!("rehydrate response: {e}")))?;
        Ok(back.text)
    }
}

/// The egress decorator for decision backends (`docs/decision-model-proposal.md`
/// §2, "Egress"): `DecideRequest.state` sent to a remote backend is memory
/// egress, so it goes through the SAME session pseudonymization as
/// [`PseudonymizingBackend`] before the inner backend sees it.
///
/// - A string `state` is transformed whole; an object or array is walked
///   recursively and every string VALUE inside it is transformed (object
///   keys are the host's own schema — `query`, `candidates`, `text` — and
///   pass through). One session serves the whole walk, so a value repeated
///   across candidates gets one token.
/// - `questions` and `deadline` pass through untouched. A question's
///   `instructions` and criteria are the host's own text, not memory
///   content. This is a deliberate difference from
///   [`PseudonymizingBackend`], which transforms the WHOLE LLM request
///   (instructions included) because it cannot tell them apart from the
///   payload; here the typed request separates them. Hosts must therefore
///   not interpolate memory content into question text — put it in `state`.
/// - Answers come back unchanged. They carry probabilities over the host's
///   own option keys and levels, never memory text, so there is nothing to
///   rehydrate.
/// - Fail-closed exactly like the LLM wrapper (D6): if the transform
///   errors, the call fails with `DEC-E008` (`DecideError::EgressRefused`)
///   and nothing is sent; raw state never goes out as a fallback. Hosts wrap
///   the WHOLE resolved chain (what `resolve_chain`/`env_chain` return),
///   never individual entries — and `DEC-E008` stops a chain, so even a
///   hand-built chain with a wrapped entry ahead of an unwrapped one cannot
///   forward the raw state.
///
/// [`DecisionBackend::calibrated`] forwards; [`DecisionBackend::describe`]
/// appends `+anon` so provenance (recall explanation, `/api/config`, bench
/// output) shows that the backend saw pseudonymized state.
pub struct PseudonymizingDecider {
    inner: Arc<dyn DecisionBackend>,
    session: Mutex<SessionAnonymizer>,
    known: Vec<String>,
}

impl PseudonymizingDecider {
    /// Wrap `inner` under `policy` — the same constructor shape as
    /// [`PseudonymizingBackend::new`], with the same refusal of
    /// `memory`-scope policies.
    pub fn new(
        inner: Arc<dyn DecisionBackend>,
        policy: AnonPolicy,
    ) -> Result<Self, areev_core::error::AreevError> {
        Ok(PseudonymizingDecider {
            inner,
            session: Mutex::new(SessionAnonymizer::new(policy)?),
            known: Vec::new(),
        })
    }

    pub fn with_known_identities(mut self, ids: Vec<String>) -> Self {
        self.known = ids;
        self
    }

    /// The accumulated placeholder → value mapping (D5 custody: the host
    /// process holds it; it never rides the wire).
    pub fn mapping(&self) -> std::collections::BTreeMap<String, String> {
        self.session.lock().unwrap().mapping().clone()
    }
}

/// Transform every string value in `v`, recursively. Keys pass through.
fn pseudonymize_value(
    session: &mut SessionAnonymizer,
    known: &[String],
    v: &Value,
) -> Result<Value, areev_core::error::AreevError> {
    Ok(match v {
        Value::String(s) => Value::String(session.transform_text(s, known)?.0),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| pseudonymize_value(session, known, i))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => {
            let mut out = serde_json::Map::with_capacity(o.len());
            for (k, x) in o {
                out.insert(k.clone(), pseudonymize_value(session, known, x)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

impl DecisionBackend for PseudonymizingDecider {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
        let state = {
            let mut session = self.session.lock().unwrap();
            pseudonymize_value(&mut session, &self.known, &req.state)
                .map_err(|e| DecideError::EgressRefused(format!("pseudonymize state: {e}")))?
        };
        let anon = DecideRequest { state, questions: req.questions.clone(), deadline: req.deadline };
        self.inner.decide(&anon)
    }
    fn calibrated(&self) -> bool {
        self.inner.calibrated()
    }
    fn describe(&self) -> String {
        format!("{}+anon", self.inner.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl LlmBackend for Echo {
        fn model(&self) -> &str {
            "echo"
        }
        fn complete(&self, request: &str) -> Result<String, LoopError> {
            // A model that quotes the prompt back — the worst case for
            // leakage, the best case for asserting what it actually saw.
            Ok(format!("model saw: {request}"))
        }
    }

    #[test]
    fn request_is_pseudonymized_and_response_rehydrated() {
        let backend = PseudonymizingBackend::new(Echo, AnonPolicy::default())
            .unwrap()
            .with_known_identities(vec!["caller:john".into()]);
        let out = backend
            .complete("extract facts from: john's pin number is 1462, mail j@x.io")
            .unwrap();
        // The response the CALLER sees is rehydrated back to real values...
        assert_eq!(
            out,
            "model saw: extract facts from: john's pin number is 1462, mail j@x.io"
        );
        // ...while the mapping proves the inner model saw placeholders only.
        let mapping = backend.mapping();
        assert!(mapping.values().any(|v| v == "john"));
        assert!(mapping.values().any(|v| v == "1462"));
        assert!(mapping.values().any(|v| v == "j@x.io"));
    }

    #[test]
    fn tokens_stay_stable_across_calls() {
        let backend = PseudonymizingBackend::new(Echo, AnonPolicy::default()).unwrap();
        backend.complete("mail a@b.co first").unwrap();
        backend.complete("mail a@b.co again, and c@d.io").unwrap();
        let mapping = backend.mapping();
        assert_eq!(mapping.len(), 2); // a@b.co reused its token
        assert_eq!(mapping["[EMAIL_1]"], "a@b.co");
        assert_eq!(mapping["[EMAIL_2]"], "c@d.io");
    }
}

#[cfg(test)]
mod decider_tests {
    use super::*;
    use areev_core::decide::{Answer, Question};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// Records the request it was sent and answers a fixed noul.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<DecideRequest>>,
    }
    impl DecisionBackend for Recorder {
        fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
            self.seen.lock().unwrap().push(req.clone());
            Ok(Decision {
                answers: req.questions.keys().map(|k| (k.clone(), Answer::Noul { p: 0.25 })).collect(),
                model: "fake-1".into(),
                provider: "fake".into(),
                calibrated: true,
                input_tokens: Some(7),
                output_tokens: Some(1),
                usd_micros: None,
                latency_ms: 0,
            })
        }
        fn calibrated(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "fake:fake-1".into()
        }
    }

    fn questions() -> BTreeMap<String, Question> {
        // Deliberately carries an e-mail in the INSTRUCTIONS: host text,
        // which must pass through untouched.
        [("q".to_string(), Question::noul("does this match ops@host.example?"))].into_iter().collect()
    }

    #[test]
    fn object_state_is_pseudonymized_recursively_and_the_rest_passes_through() {
        let rec = Arc::new(Recorder::default());
        let d = PseudonymizingDecider::new(rec.clone(), AnonPolicy::default()).unwrap();
        let req = DecideRequest {
            state: json!({
                "query": "what did j@x.io say?",
                "candidates": [
                    {"i": 0, "text": "j@x.io asked for the invoice"},
                    {"i": 1, "text": "nothing personal here"},
                    {"i": 2, "text": "reply to k@y.io and j@x.io"}
                ]
            }),
            questions: questions(),
            deadline: Some(std::time::Duration::from_millis(1234)),
        };
        let out = d.decide(&req).unwrap();

        let seen = rec.seen.lock().unwrap();
        let sent = &seen[0];
        let wire = sent.state.to_string();
        assert!(!wire.contains("j@x.io") && !wire.contains("k@y.io"), "raw value egressed: {wire}");
        // One session across the walk: the repeated value gets ONE token.
        let tok = d.mapping().into_iter().find(|(_, v)| v == "j@x.io").map(|(k, _)| k).unwrap();
        assert_eq!(sent.state["query"], json!(format!("what did {tok} say?")));
        assert!(sent.state["candidates"][2]["text"].as_str().unwrap().contains(&tok));
        // Structure, keys, non-strings and clean text are untouched.
        assert_eq!(sent.state["candidates"][1], json!({"i": 1, "text": "nothing personal here"}));
        assert_eq!(sent.questions, req.questions, "host question text is not egress");
        assert_eq!(sent.deadline, req.deadline);
        // The answer comes back exactly as the inner backend gave it.
        assert_eq!(out.answers["q"], Answer::Noul { p: 0.25 });
        assert_eq!((out.provider.as_str(), out.model.as_str()), ("fake", "fake-1"));
    }

    #[test]
    fn string_state_is_transformed_whole_and_provenance_is_marked() {
        let rec = Arc::new(Recorder::default());
        let d = PseudonymizingDecider::new(rec.clone(), AnonPolicy::default()).unwrap();
        d.decide(&DecideRequest::new("mail a@b.co today", questions())).unwrap();
        let sent = rec.seen.lock().unwrap()[0].state.clone();
        assert_eq!(sent, json!("mail [EMAIL_1] today"));
        assert_eq!(d.describe(), "fake:fake-1+anon");
        assert!(d.calibrated());
    }

    #[test]
    fn a_transform_failure_fails_closed_and_sends_nothing() {
        let rec = Arc::new(Recorder::default());
        // Demands a detector this wrapper has no backend for: the scan must
        // refuse rather than send the state with that detector skipped.
        let policy = AnonPolicy { detectors: vec!["tier0".into(), "llm".into()], ..Default::default() };
        let d = PseudonymizingDecider::new(rec.clone(), policy).unwrap();
        let err = d.decide(&DecideRequest::new(json!({"t": "mail a@b.co"}), questions())).unwrap_err();
        assert_eq!(err.code(), "DEC-E008");
        assert!(err.stops_chain());
        assert!(err.to_string().contains("pseudonymize state"), "{err}");
        assert!(rec.seen.lock().unwrap().is_empty(), "nothing may reach the backend");
    }

    /// Regression for the chain-composition leak: a wrapped entry that fails
    /// closed must stop the chain, so a later UNWRAPPED entry never receives
    /// the raw state.
    #[test]
    fn a_fail_closed_entry_stops_the_chain_before_an_unwrapped_entry() {
        struct Shared(Arc<Recorder>);
        impl DecisionBackend for Shared {
            fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
                self.0.decide(req)
            }
            fn calibrated(&self) -> bool {
                true
            }
            fn describe(&self) -> String {
                self.0.describe()
            }
        }
        let wrapped_inner = Arc::new(Recorder::default());
        let healthy = Arc::new(Recorder::default());
        let policy = AnonPolicy { detectors: vec!["tier0".into(), "llm".into()], ..Default::default() };
        let wrapped = PseudonymizingDecider::new(wrapped_inner.clone(), policy).unwrap();
        let chain = crate::decide::Chain::new(
            vec![Box::new(wrapped), Box::new(Shared(healthy.clone()))],
            None,
        );
        let err = chain.decide(&DecideRequest::new("mail a@b.co", questions())).unwrap_err();
        assert_eq!(err.code(), "DEC-E008", "{err}");
        assert!(wrapped_inner.seen.lock().unwrap().is_empty());
        assert!(healthy.seen.lock().unwrap().is_empty(), "the unwrapped entry must never be called");
    }

    #[test]
    fn memory_scope_is_refused_like_the_llm_wrapper() {
        let policy = AnonPolicy { scope: "memory".into(), ..Default::default() };
        assert!(PseudonymizingDecider::new(Arc::new(Recorder::default()), policy).is_err());
    }
}
