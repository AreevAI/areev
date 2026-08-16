use super::grain::{Grain, GrainCommon, GrainType};

/// A State grain — agent state snapshot (portable save point).
///
/// `context_data` is the OMS §8.3 required `context` map. It shares the wire
/// key `ctx` with the common `context` field (§6.1) — for a State grain that
/// key means the snapshot, so `add_type_specific_fields` wins and
/// `add_common_fields` must not overwrite it. The map's inner keys are never
/// compacted (see `deserialize::KeyMode`), so arbitrary agent state round-trips
/// verbatim.
#[derive(Debug, Clone)]
pub struct State {
    pub context_data: serde_json::Value,
    /// OMS §8.3 optional `plan` — ordered steps the agent intends to take.
    pub plan: Option<Vec<String>>,
    /// OMS §8.3 optional `history` — prior state entries, newest last.
    pub history: Option<Vec<serde_json::Value>>,
    pub common: GrainCommon,
}

impl State {
    pub fn new(context: serde_json::Value) -> Self {
        State {
            context_data: context,
            plan: None,
            history: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn plan(mut self, plan: Vec<String>) -> Self {
        self.plan = Some(plan);
        self
    }

    pub fn history(mut self, history: Vec<serde_json::Value>) -> Self {
        self.history = Some(history);
        self
    }
}

impl Grain for State {
    fn grain_type(&self) -> GrainType {
        GrainType::State
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        if let Some(obj) = self.context_data.as_object() {
            for key in &["label", "description", "title", "name"] {
                if let Some(v) = obj.get(*key).and_then(|v| v.as_str()) {
                    if !v.trim().is_empty() {
                        return v.to_string();
                    }
                }
            }
        }
        String::new()
    }
}
