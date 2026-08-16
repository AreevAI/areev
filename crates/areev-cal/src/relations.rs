//! mg: relation vocabulary and IS CATEGORY expansion.
//!
//! This module contains the static vocabulary of 24 standard `mg:` relations
//! defined by the OMS specification, grouped into 9 semantic categories.
//!
//! # Category expansion
//!
//! The `expand_category()` function maps an `IS CATEGORY` condition (e.g.
//! `relation IS PREFERENCE`) into the constituent `mg:` relation strings.
//! This expansion happens at desugar time, before execution, so the executor
//! never sees `IsCategory` conditions directly.
//!
//! # Validation
//!
//! `validate_relation()` checks whether an `mg:`-prefixed relation is one of
//! the 24 standard relations.  Unknown `mg:` relations produce a
//! `CalWarning::UnknownRelation` (CAL-W001), not an error.  Non-`mg:` custom
//! relations (e.g. `acme:similar_to`) pass through without warning.

use super::errors::CalWarning;

// ---------------------------------------------------------------------------
// Relation categories
// ---------------------------------------------------------------------------

/// The 7 semantic categories for `mg:` relations.
///
/// Used in `relation IS PREFERENCE` and similar `IS CATEGORY` conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationCategory {
    Preference,
    Knowledge,
    Permission,
    Interaction,
    Agency,
    Lifecycle,
    Observation,
    /// Consensus relations (agreement, endorsement).
    Consensus,
    /// Workflow relations (step sequencing).
    Workflow,
}

impl RelationCategory {
    /// Parse a category name (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "preference" => Some(Self::Preference),
            "knowledge" => Some(Self::Knowledge),
            "permission" => Some(Self::Permission),
            "interaction" => Some(Self::Interaction),
            "agency" => Some(Self::Agency),
            "lifecycle" => Some(Self::Lifecycle),
            "observation" => Some(Self::Observation),
            "consensus" => Some(Self::Consensus),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Static vocabulary: 24 mg: relations
// ---------------------------------------------------------------------------

/// The 24 standard `mg:` relations with their categories.
///
/// Each entry is `(relation_string, category)`.  The relation string includes
/// the `mg:` namespace prefix.
pub const MG_RELATIONS: &[(&str, RelationCategory)] = &[
    // Observation
    ("mg:perceives", RelationCategory::Observation),
    ("mg:state_at", RelationCategory::Observation),
    // Knowledge
    ("mg:knows", RelationCategory::Knowledge),
    ("mg:infers", RelationCategory::Knowledge),
    ("mg:learned", RelationCategory::Knowledge),
    ("mg:owned_by", RelationCategory::Knowledge),
    // Interaction
    ("mg:said", RelationCategory::Interaction),
    ("mg:did", RelationCategory::Interaction),
    ("mg:handed_off_to", RelationCategory::Interaction),
    // Consensus
    ("mg:agrees_with", RelationCategory::Consensus),
    // Workflow. Note the parameterized `mg:step_action:<node_id>` family
    // (OMS §8.4) also belongs to this category but cannot be listed here — see
    // `is_known_relation`.
    ("mg:requires_steps", RelationCategory::Workflow),
    // Lifecycle
    ("mg:intends", RelationCategory::Lifecycle),
    ("mg:depends_on", RelationCategory::Lifecycle),
    // Permission
    ("mg:permits", RelationCategory::Permission),
    ("mg:revokes", RelationCategory::Permission),
    ("mg:prohibits", RelationCategory::Permission),
    // Preference
    ("mg:requires", RelationCategory::Preference),
    ("mg:prefers", RelationCategory::Preference),
    ("mg:avoids", RelationCategory::Preference),
    ("mg:dislikes", RelationCategory::Preference),
    // Agency
    ("mg:delegates_to", RelationCategory::Agency),
    ("mg:has_capability", RelationCategory::Agency),
    ("mg:assigned_to", RelationCategory::Agency),
    // Agency — Skill grain (OMS 1.4): "agent X is capable of skill N".
    ("mg:capable_of", RelationCategory::Agency),
];

// ---------------------------------------------------------------------------
// Category expansion
// ---------------------------------------------------------------------------

/// Expand a category shortcut to its constituent `mg:` relation strings.
///
/// Called during the desugar phase (before execution).  For example:
///
/// ```text
/// relation IS PREFERENCE  -->  relation IN ("mg:prefers", "mg:avoids", "mg:requires", "mg:dislikes")
/// ```
///
/// Returns an empty vec if the category has no registered relations (should
/// not happen for the standard 7+2 categories).
pub fn expand_category(category: &str) -> Vec<&'static str> {
    let Some(cat) = RelationCategory::parse(category) else {
        return Vec::new();
    };
    expand_category_enum(cat)
}

/// Expand a `RelationCategory` enum value to its constituent relation strings.
/// Returns both `mg:`-prefixed and plain versions so that data stored with
/// either format is matched (e.g. both `"mg:prefers"` and `"prefers"`).
pub fn expand_category_enum(category: RelationCategory) -> Vec<&'static str> {
    let mut result = Vec::new();
    for (r, c) in MG_RELATIONS.iter() {
        if *c == category {
            result.push(*r); // mg:prefers
            if let Some(plain) = r.strip_prefix("mg:") {
                result.push(plain); // prefers
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate an `mg:` relation string.
///
/// Returns `Some(CalWarning::UnknownRelation)` if the relation starts with
/// `mg:` but is not one of the 24 standard relations.  Returns `None` if:
/// - The relation IS a known `mg:` relation, or
/// - The relation does NOT start with `mg:` (custom relations are allowed).
pub fn validate_relation(relation: &str) -> Option<CalWarning> {
    if !relation.starts_with("mg:") {
        return None;
    }
    let known = is_known_relation(relation);
    if known {
        None
    } else {
        Some(CalWarning::UnknownRelation {
            relation: relation.to_string(),
            span: None,
        })
    }
}

/// Check if a relation string is a known `mg:` relation.
///
/// Covers the 24 fixed entries plus the one *parameterized* family the spec
/// defines: `mg:step_action:<node_id>` (OMS §8.4), the execution-record link
/// from a Tool grain to the Workflow node it ran. That family cannot live in
/// the static table — its tail is a node id — so it is matched by prefix.
pub fn is_known_relation(relation: &str) -> bool {
    MG_RELATIONS.iter().any(|(r, _)| *r == relation)
        || areev_core::types::step_action_node(relation).is_some()
}

/// Look up the category for a given `mg:` relation.
pub fn relation_category(relation: &str) -> Option<RelationCategory> {
    MG_RELATIONS
        .iter()
        .find(|(r, _)| *r == relation)
        .map(|(_, c)| *c)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_preference_category() {
        let relations = expand_category("preference");
        assert!(relations.contains(&"mg:prefers"));
        assert!(relations.contains(&"prefers"));
        assert!(relations.contains(&"mg:avoids"));
        assert!(relations.contains(&"avoids"));
        assert!(relations.contains(&"mg:requires"));
        assert!(relations.contains(&"requires"));
        assert!(relations.contains(&"mg:dislikes"));
        assert!(relations.contains(&"dislikes"));
        assert_eq!(relations.len(), 8);
    }

    #[test]
    fn test_expand_knowledge_category() {
        let relations = expand_category("knowledge");
        assert!(relations.contains(&"mg:knows"));
        assert!(relations.contains(&"knows"));
        assert!(relations.contains(&"mg:infers"));
        assert!(relations.contains(&"infers"));
        assert!(relations.contains(&"mg:learned"));
        assert!(relations.contains(&"learned"));
        assert!(relations.contains(&"mg:owned_by"));
        assert!(relations.contains(&"owned_by"));
        assert_eq!(relations.len(), 8);
    }

    #[test]
    fn test_expand_permission_category() {
        let relations = expand_category("permission");
        assert!(relations.contains(&"mg:permits"));
        assert!(relations.contains(&"permits"));
        assert!(relations.contains(&"mg:revokes"));
        assert!(relations.contains(&"revokes"));
        assert!(relations.contains(&"mg:prohibits"));
        assert!(relations.contains(&"prohibits"));
        assert_eq!(relations.len(), 6);
    }

    #[test]
    fn test_expand_interaction_category() {
        let relations = expand_category("interaction");
        assert!(relations.contains(&"mg:said"));
        assert!(relations.contains(&"said"));
        assert!(relations.contains(&"mg:did"));
        assert!(relations.contains(&"did"));
        assert!(relations.contains(&"mg:handed_off_to"));
        assert!(relations.contains(&"handed_off_to"));
        assert_eq!(relations.len(), 6);
    }

    #[test]
    fn test_expand_agency_category() {
        let relations = expand_category("agency");
        assert!(relations.contains(&"mg:delegates_to"));
        assert!(relations.contains(&"delegates_to"));
        assert!(relations.contains(&"mg:has_capability"));
        assert!(relations.contains(&"has_capability"));
        assert!(relations.contains(&"mg:assigned_to"));
        assert!(relations.contains(&"assigned_to"));
        assert!(relations.contains(&"mg:capable_of"));
        assert!(relations.contains(&"capable_of"));
        assert_eq!(relations.len(), 8);
    }

    #[test]
    fn test_expand_lifecycle_category() {
        let relations = expand_category("lifecycle");
        assert!(relations.contains(&"mg:intends"));
        assert!(relations.contains(&"intends"));
        assert!(relations.contains(&"mg:depends_on"));
        assert!(relations.contains(&"depends_on"));
        assert_eq!(relations.len(), 4);
    }

    #[test]
    fn test_expand_observation_category() {
        let relations = expand_category("observation");
        assert!(relations.contains(&"mg:perceives"));
        assert!(relations.contains(&"perceives"));
        assert!(relations.contains(&"mg:state_at"));
        assert!(relations.contains(&"state_at"));
        assert_eq!(relations.len(), 4);
    }

    #[test]
    fn test_expand_consensus_category() {
        let relations = expand_category("consensus");
        assert!(relations.contains(&"mg:agrees_with"));
        assert!(relations.contains(&"agrees_with"));
        assert_eq!(relations.len(), 2);
    }

    #[test]
    fn test_expand_workflow_category() {
        let relations = expand_category("workflow");
        assert!(relations.contains(&"mg:requires_steps"));
        assert!(relations.contains(&"requires_steps"));
        assert_eq!(relations.len(), 2);
    }

    #[test]
    fn test_expand_unknown_category() {
        let relations = expand_category("nonexistent");
        assert!(relations.is_empty());
    }

    #[test]
    fn test_expand_case_insensitive() {
        let relations = expand_category("PREFERENCE");
        assert_eq!(relations.len(), 8);
    }

    #[test]
    fn test_total_relations_count() {
        assert_eq!(MG_RELATIONS.len(), 24);
    }

    #[test]
    fn test_validate_known_relation() {
        assert!(validate_relation("mg:prefers").is_none());
        assert!(validate_relation("mg:knows").is_none());
        assert!(validate_relation("mg:delegates_to").is_none());
    }

    #[test]
    fn test_validate_unknown_mg_relation() {
        let warning = validate_relation("mg:unknown_thing");
        assert!(warning.is_some());
        match warning.unwrap() {
            CalWarning::UnknownRelation { relation, .. } => {
                assert_eq!(relation, "mg:unknown_thing");
            }
            _ => panic!("expected UnknownRelation warning"),
        }
    }

    #[test]
    fn test_validate_custom_relation_no_warning() {
        // Non-mg: relations should not produce a warning.
        assert!(validate_relation("acme:similar_to").is_none());
        assert!(validate_relation("custom_relation").is_none());
        assert!(validate_relation("prefers").is_none());
    }

    #[test]
    fn test_is_known_relation() {
        assert!(is_known_relation("mg:prefers"));
        assert!(is_known_relation("mg:agrees_with"));
        assert!(!is_known_relation("mg:unknown"));
        assert!(!is_known_relation("prefers"));
    }

    #[test]
    fn test_relation_category_lookup() {
        assert_eq!(
            relation_category("mg:prefers"),
            Some(RelationCategory::Preference)
        );
        assert_eq!(
            relation_category("mg:knows"),
            Some(RelationCategory::Knowledge)
        );
        assert_eq!(relation_category("mg:unknown"), None);
        assert_eq!(relation_category("prefers"), None);
    }

    #[test]
    fn test_relation_category_parse() {
        assert_eq!(
            RelationCategory::parse("preference"),
            Some(RelationCategory::Preference)
        );
        assert_eq!(
            RelationCategory::parse("KNOWLEDGE"),
            Some(RelationCategory::Knowledge)
        );
        assert_eq!(RelationCategory::parse("bogus"), None);
    }
}
