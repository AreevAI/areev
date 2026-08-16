use super::grain::{Grain, GrainCommon, GrainType};

/// A Consensus grain — multi-agent agreement record.
#[derive(Debug, Clone)]
pub struct Consensus {
    pub participating_observers: Vec<String>,
    pub threshold: Option<f64>,
    pub agreement_count: Option<i64>,
    pub dissent_count: Option<i64>,
    pub dissent_grains: Vec<String>,
    pub agreed_content: Option<String>,
    pub common: GrainCommon,
}

impl Default for Consensus {
    fn default() -> Self {
        Self::new()
    }
}

impl Consensus {
    pub fn new() -> Self {
        Consensus {
            participating_observers: Vec::new(),
            threshold: None,
            agreement_count: None,
            dissent_count: None,
            dissent_grains: Vec::new(),
            agreed_content: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }
}

impl Grain for Consensus {
    fn grain_type(&self) -> GrainType {
        GrainType::Consensus
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        if let Some(ref content) = self.agreed_content {
            return content.clone();
        }
        match (self.agreement_count, self.threshold) {
            (Some(ac), Some(t)) => format!("{}/{} agreement", ac, t),
            _ => "consensus".to_string(),
        }
    }
}
