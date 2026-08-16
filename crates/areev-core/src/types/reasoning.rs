use super::grain::{Grain, GrainCommon, GrainType};

/// A Reasoning grain — inference chain and thought audit trail.
#[derive(Debug, Clone)]
pub struct Reasoning {
    pub premises: Vec<String>,
    pub conclusion: Option<String>,
    pub inference_method: Option<String>,
    pub alternatives_considered: Vec<String>,
    pub thinking_content: Option<String>,
    pub thinking_redacted: Option<bool>,
    pub common: GrainCommon,
}

impl Default for Reasoning {
    fn default() -> Self {
        Self::new()
    }
}

impl Reasoning {
    pub fn new() -> Self {
        Reasoning {
            premises: Vec::new(),
            conclusion: None,
            inference_method: None,
            alternatives_considered: Vec::new(),
            thinking_content: None,
            thinking_redacted: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn conclusion(mut self, c: &str) -> Self {
        self.conclusion = Some(c.to_string());
        self
    }

    pub fn inference_method(mut self, m: &str) -> Self {
        self.inference_method = Some(m.to_string());
        self
    }

    pub fn thinking_content(mut self, t: &str) -> Self {
        self.thinking_content = Some(t.to_string());
        self
    }
}

impl Grain for Reasoning {
    fn grain_type(&self) -> GrainType {
        GrainType::Reasoning
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        match (&self.conclusion, &self.thinking_content) {
            (Some(c), _) => c.clone(),
            (None, Some(t)) => t.clone(),
            _ => String::new(),
        }
    }
}
