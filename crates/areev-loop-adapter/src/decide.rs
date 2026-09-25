//! The bridge from Areev's decision seam to the loop's
//! (`docs/decision-model-proposal.md` §4, rows E1–E3).
//!
//! `areev_loop` has no Areev dependencies, so it defines its own minimal
//! [`areev_loop::DecideBackend`] over the proposal's WIRE JSON. This adapter
//! implements it over any [`areev_core::decide::DecisionBackend`] — a
//! provider chain from `areev_llm::decide::resolve_chain`, a command backend,
//! or a test fake — so a host installs its chain with
//! `engine.with_decider(Box::new(LoopDecider(chain)))`.

use areev_core::decide::{questions_from_wire, DecideRequest, DecisionBackend};
use std::sync::Arc;

/// An Areev decision backend, seen by the loop.
pub struct LoopDecider(pub Arc<dyn DecisionBackend>);

impl areev_loop::DecideBackend for LoopDecider {
    /// `{"state", "questions"}` in → the backend's validated `Decision` as
    /// `{model, answers, usage?, provider, calibrated, latency_ms}` out.
    /// Every failure — a request that does not parse, an invalid question,
    /// any `DEC-E…` from the backend — is `LOP-E051`, which the loop treats
    /// as "no contribution" for that stage.
    fn decide(&self, request_json: &str) -> areev_loop::Result<String> {
        let err = |m: String| areev_loop::Error::DecideBackend(m);
        let v: serde_json::Value =
            serde_json::from_str(request_json).map_err(|e| err(format!("request is not JSON: {e}")))?;
        let questions = questions_from_wire(v.get("questions").unwrap_or(&serde_json::Value::Null))
            .map_err(|e| err(e.to_string()))?;
        let state = v.get("state").cloned().unwrap_or(serde_json::Value::Null);
        let decision = self
            .0
            .decide(&DecideRequest::new(state, questions))
            .map_err(|e| err(e.to_string()))?;
        Ok(decision.to_json().to_string())
    }

    fn calibrated(&self) -> bool {
        self.0.calibrated()
    }

    fn describe(&self) -> String {
        self.0.describe()
    }
}
