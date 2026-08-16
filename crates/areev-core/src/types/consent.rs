use super::grain::{Grain, GrainCommon, GrainType};

/// A Consent grain — DID-scoped, purpose-bounded permission grant or withdrawal.
#[derive(Debug, Clone)]
pub struct Consent {
    pub subject_did: String,
    pub grantee_did: Option<String>,
    pub scope: Option<String>,
    pub is_withdrawal: Option<bool>,
    pub basis: Option<String>,
    pub jurisdiction: Option<String>,
    pub prior_consent: Option<String>,
    pub witness_dids: Vec<String>,
    pub common: GrainCommon,
}

impl Consent {
    pub fn new(subject_did: &str) -> Self {
        Consent {
            subject_did: subject_did.to_string(),
            grantee_did: None,
            scope: None,
            is_withdrawal: None,
            basis: None,
            jurisdiction: None,
            prior_consent: None,
            witness_dids: Vec::new(),
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }
}

impl Grain for Consent {
    fn grain_type(&self) -> GrainType {
        GrainType::Consent
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        let action = if self.is_withdrawal == Some(true) {
            "withdraws"
        } else {
            "grants"
        };
        match &self.grantee_did {
            Some(g) => format!("{} {} {}", self.subject_did, action, g),
            None => format!("{} {}", self.subject_did, action),
        }
    }
}
