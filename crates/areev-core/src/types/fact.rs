use super::grain::{Grain, GrainCommon, GrainType};

/// A Fact grain — a structured knowledge claim as a semantic triple (subject, relation, object).
#[derive(Debug, Clone)]
pub struct Fact {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub common: GrainCommon,
}

impl Fact {
    pub fn new(subject: &str, relation: &str, object: &str) -> Self {
        Fact {
            subject: subject.to_string(),
            relation: relation.to_string(),
            object: object.to_string(),
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }
}

impl Grain for Fact {
    fn grain_type(&self) -> GrainType {
        GrainType::Fact
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        format!("{} {} {}", self.subject, self.relation, self.object)
    }
}
