use super::grain::{Grain, GrainCommon, GrainType};

/// Observation mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationMode {
    Realtime,
    Batch,
    Streaming,
}

impl ObservationMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObservationMode::Realtime => "realtime",
            ObservationMode::Batch => "batch",
            ObservationMode::Streaming => "streaming",
        }
    }
}

/// Observation scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationScope {
    Private,
    Shared,
    Public,
}

impl ObservationScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObservationScope::Private => "private",
            ObservationScope::Shared => "shared",
            ObservationScope::Public => "public",
        }
    }
}

/// An Observation grain — cognitive observer perceptions.
#[derive(Debug, Clone)]
pub struct Observation {
    pub observer_id: String,
    pub observer_type: String,
    pub subject: Option<String>,
    pub object: Option<String>,
    pub observer_model: Option<String>,
    pub frame_id: Option<String>,
    pub sync_group: Option<String>,
    pub observation_mode: Option<ObservationMode>,
    pub observation_scope: Option<ObservationScope>,
    pub compression_ratio: Option<f64>,
    pub common: GrainCommon,
}

impl Observation {
    pub fn new(observer_id: &str, observer_type: &str) -> Self {
        Observation {
            observer_id: observer_id.to_string(),
            observer_type: observer_type.to_string(),
            subject: None,
            object: None,
            observer_model: None,
            frame_id: None,
            sync_group: None,
            observation_mode: None,
            observation_scope: None,
            compression_ratio: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn subject(mut self, s: &str) -> Self {
        self.subject = Some(s.to_string());
        self
    }

    pub fn object(mut self, o: &str) -> Self {
        self.object = Some(o.to_string());
        self
    }

    pub fn observer_model(mut self, m: &str) -> Self {
        self.observer_model = Some(m.to_string());
        self
    }

    pub fn mode(mut self, m: ObservationMode) -> Self {
        self.observation_mode = Some(m);
        self
    }

    pub fn scope(mut self, s: ObservationScope) -> Self {
        self.observation_scope = Some(s);
        self
    }
}

impl Grain for Observation {
    fn grain_type(&self) -> GrainType {
        GrainType::Observation
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        let mut parts: Vec<&str> = vec![&self.observer_id, &self.observer_type];
        if let Some(ref s) = self.subject {
            parts.push(s.as_str());
        }
        if let Some(ref o) = self.object {
            parts.push(o.as_str());
        }
        parts.join(" ")
    }
}
