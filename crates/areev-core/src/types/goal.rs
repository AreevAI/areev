use super::grain::{Grain, GrainCommon, GrainType};

/// Goal priority level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Priority {
    Critical,
    High,
    Medium,
    Low,
}

impl Priority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Critical => "critical",
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
        }
    }
}

/// Goal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalState {
    Active,
    Satisfied,
    Failed,
    Suspended,
}

impl GoalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalState::Active => "active",
            GoalState::Satisfied => "satisfied",
            GoalState::Failed => "failed",
            GoalState::Suspended => "suspended",
        }
    }
}

/// A Goal grain — agent objectives with satisfaction criteria.
#[derive(Debug, Clone)]
pub struct Goal {
    pub description: String,
    pub goal_state: GoalState,
    pub priority: Option<Priority>,
    pub criteria: Option<String>,
    pub criteria_structured: Option<serde_json::Value>,
    pub parent_goals: Option<Vec<String>>,
    pub state_reason: Option<String>,
    pub satisfaction_evidence: Option<serde_json::Value>,
    pub progress: Option<f64>,
    pub delegate_to: Option<String>,
    pub delegate_from: Option<String>,
    pub expiry_policy: Option<serde_json::Value>,
    pub recurrence: Option<serde_json::Value>,
    pub evidence_required: Option<bool>,
    pub rollback_on_failure: Option<bool>,
    pub allowed_transitions: Option<Vec<String>>,
    /// Optional subject for triple-store indexing.
    pub subject: Option<String>,
    /// Optional object for triple-store indexing.
    pub object: Option<String>,
    pub common: GrainCommon,
}

impl Goal {
    pub fn new(description: &str) -> Self {
        Goal {
            description: description.to_string(),
            goal_state: GoalState::Active,
            priority: None,
            criteria: None,
            criteria_structured: None,
            parent_goals: None,
            state_reason: None,
            satisfaction_evidence: None,
            progress: None,
            delegate_to: None,
            delegate_from: None,
            expiry_policy: None,
            recurrence: None,
            evidence_required: None,
            rollback_on_failure: None,
            allowed_transitions: None,
            subject: None,
            object: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn state(mut self, state: GoalState) -> Self {
        self.goal_state = state;
        self
    }

    pub fn priority(mut self, p: Priority) -> Self {
        self.priority = Some(p);
        self
    }

    pub fn subject(mut self, s: &str) -> Self {
        self.subject = Some(s.to_string());
        self
    }

    pub fn object(mut self, o: &str) -> Self {
        self.object = Some(o.to_string());
        self
    }
}

impl Grain for Goal {
    fn grain_type(&self) -> GrainType {
        GrainType::Goal
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        match &self.criteria {
            Some(c) => format!("{} {}", self.description, c),
            None => self.description.clone(),
        }
    }
}
