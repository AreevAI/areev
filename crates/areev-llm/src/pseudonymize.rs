//! `PseudonymizingBackend` — the egress decorator for in-process LLM calls
//! (docs/anonymization-proposal.md §4.1): pseudonymize the request before it
//! leaves the process, rehydrate the response before the caller sees it.
//! One wrap covers `remember()` extraction, the loop's verifier, and the
//! runtime's tool-calling LLM.
//!
//! Fail-closed (D6): a transform error fails the call — raw content never
//! goes out as a fallback. Rehydration is exact-token and never guesses;
//! unmatched placeholders in a response stay visible rather than being
//! papered over.

use std::sync::Mutex;

use areev_core::anon::{AnonPolicy, SessionAnonymizer};
use areev_loop::error::Error as LoopError;
use areev_loop::LlmBackend;

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
