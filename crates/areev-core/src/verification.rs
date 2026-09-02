#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Unverified,
    Verified,
    Contested,
    Retracted,
}

impl Trust {
    pub fn from_field(value: Option<&str>) -> Self {
        match value {
            Some("verified") => Trust::Verified,
            Some("contested") => Trust::Contested,
            Some("retracted") | Some("rejected") => Trust::Retracted,
            _ => Trust::Unverified,
        }
    }

    pub fn is_actionable(self) -> bool {
        !matches!(self, Trust::Retracted)
    }

    pub fn is_disputed(self) -> bool {
        matches!(self, Trust::Contested | Trust::Retracted)
    }

    pub fn priority_delta(self) -> f32 {
        match self {
            Trust::Retracted => -0.3,
            Trust::Contested => -0.15,
            Trust::Unverified | Trust::Verified => 0.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Unverified => "unverified",
            Trust::Verified => "verified",
            Trust::Contested => "contested",
            Trust::Retracted => "retracted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_oms_value_maps_to_its_own_state() {
        assert_eq!(Trust::from_field(Some("unverified")), Trust::Unverified);
        assert_eq!(Trust::from_field(Some("verified")), Trust::Verified);
        assert_eq!(Trust::from_field(Some("contested")), Trust::Contested);
        assert_eq!(Trust::from_field(Some("retracted")), Trust::Retracted);
    }

    #[test]
    fn absent_and_unknown_are_unverified() {
        assert_eq!(Trust::from_field(None), Trust::Unverified);
        assert_eq!(Trust::from_field(Some("")), Trust::Unverified);
        assert_eq!(Trust::from_field(Some("banana")), Trust::Unverified);
    }

    #[test]
    fn rejected_is_an_alias_for_retracted() {
        assert_eq!(Trust::from_field(Some("rejected")), Trust::Retracted);
    }

    #[test]
    fn only_retraction_withholds() {
        assert!(!Trust::Retracted.is_actionable());
        assert!(Trust::Contested.is_actionable());
        assert!(Trust::Verified.is_actionable());
        assert!(Trust::Unverified.is_actionable());
    }

    #[test]
    fn retraction_outranks_contest_as_a_penalty() {
        assert!(Trust::Retracted.priority_delta() < Trust::Contested.priority_delta());
        assert_eq!(Trust::Verified.priority_delta(), 0.0);
        assert_eq!(Trust::Unverified.priority_delta(), 0.0);
    }

    #[test]
    fn as_str_round_trips_through_from_field() {
        for t in [Trust::Unverified, Trust::Verified, Trust::Contested, Trust::Retracted] {
            assert_eq!(Trust::from_field(Some(t.as_str())), t);
        }
    }
}
