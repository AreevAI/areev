//! The connector contract.
//!
//! A connector is a Tool: JSON in, JSON out, one process per invocation. It
//! reuses `areev_run::HostToolExecutor` rather than inventing a second
//! subprocess contract, so a host configures one seam and learns one shape —
//! and a connector inherits the spawn hardening (timeout, output cap, secret
//! scrub) that every other host command already gets.
//!
//! **stdin**
//! ```json
//! { "trigger": "<hash>", "connector": "gmail", "scope": "mailbox:…",
//!   "cursor": "1802529", "max_items": 100, "config": { "int:cursor_field": "since" } }
//! ```
//!
//! **stdout**
//! ```json
//! { "items": [ { "id": "<dedup value>", "payload": { … } } ],
//!   "cursor": "1802611", "more": false }
//! ```
//!
//! `more: true` means "there is a backlog": the next invocation runs
//! immediately instead of waiting out the interval, so a cold start drains
//! without hammering.

use serde::{Deserialize, Serialize};

/// What the evaluator hands a connector.
#[derive(Debug, Clone, Serialize)]
pub struct PollRequest<'a> {
    pub trigger: &'a str,
    pub connector: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<&'a str>,
    /// Opaque and connector-defined. Absent on the very first poll.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<&'a str>,
    pub max_items: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<&'a serde_json::Value>,
}

/// One thing the connector found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PollItem {
    /// The connector's own identity for this item. Used only when the
    /// declaration names no `dedup_key`; a declared pointer wins, because the
    /// memory's idea of identity should not depend on a connector's field
    /// naming.
    pub id: String,
    pub payload: serde_json::Value,
}

/// What the connector answered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PollResponse {
    pub items: Vec<PollItem>,
    /// The position to resume from. Absent leaves the stored cursor alone,
    /// which is the correct reading of "I have nothing new to tell you" — the
    /// alternative, treating absent as null, would silently rewind the source.
    pub cursor: Option<String>,
    /// There is more beyond this page.
    pub more: bool,
}

/// Resolve an item's dedup value.
///
/// A declared `dedup_key` is a list of JSON pointers joined in order, which is
/// how a "changed" semantic is expressed over a source that only reports
/// "created": `/id` alone identifies the entity, `/id` + `/updated_at`
/// identifies the *occurrence*. Zapier's `id + "-" + updatedAt` idiom, made
/// declarative.
pub fn dedup_value(item: &PollItem, pointers: &[String]) -> Option<String> {
    if pointers.is_empty() {
        return if item.id.trim().is_empty() { None } else { Some(item.id.clone()) };
    }
    let mut parts = Vec::with_capacity(pointers.len());
    for p in pointers {
        let v = item.payload.pointer(p)?;
        parts.push(match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }
    Some(parts.join("\u{1f}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, payload: serde_json::Value) -> PollItem {
        PollItem { id: id.into(), payload }
    }

    #[test]
    fn falls_back_to_the_connectors_own_id() {
        let i = item("abc", serde_json::json!({}));
        assert_eq!(dedup_value(&i, &[]), Some("abc".into()));
    }

    #[test]
    fn an_item_with_no_identity_at_all_yields_none() {
        // Better to report the item as unidentifiable than to invent an
        // identity, which would make every poll look like new work.
        let i = item("", serde_json::json!({}));
        assert_eq!(dedup_value(&i, &[]), None);
        assert_eq!(dedup_value(&i, &["/missing".into()]), None);
    }

    #[test]
    fn a_declared_pointer_beats_the_connectors_id() {
        let i = item("connector-chosen", serde_json::json!({ "message_id": "m1" }));
        assert_eq!(dedup_value(&i, &["/message_id".into()]), Some("m1".into()));
    }

    #[test]
    fn composite_pointers_identify_the_occurrence_not_the_entity() {
        let pointers = vec!["/id".to_string(), "/updated_at".to_string()];
        let first = item("x", serde_json::json!({ "id": "42", "updated_at": 100 }));
        let edited = item("x", serde_json::json!({ "id": "42", "updated_at": 200 }));
        let a = dedup_value(&first, &pointers).unwrap();
        let b = dedup_value(&edited, &pointers).unwrap();
        assert_ne!(a, b, "an edit is a new occurrence of the same entity");
    }

    #[test]
    fn the_joiner_cannot_be_forged_from_field_contents() {
        // Joining on a printable character would let {"a":"1-2","b":"3"} and
        // {"a":"1","b":"2-3"} collide, so one item could mask another.
        let pointers = vec!["/a".to_string(), "/b".to_string()];
        let x = item("", serde_json::json!({ "a": "1-2", "b": "3" }));
        let y = item("", serde_json::json!({ "a": "1", "b": "2-3" }));
        assert_ne!(dedup_value(&x, &pointers), dedup_value(&y, &pointers));
    }

    #[test]
    fn absent_cursor_in_a_response_means_leave_it_alone() {
        let r: PollResponse = serde_json::from_str(r#"{"items":[]}"#).unwrap();
        assert!(r.cursor.is_none(), "not Some(\"null\") and not an error");
        assert!(!r.more);
    }

    #[test]
    fn unknown_response_fields_are_ignored() {
        // A connector written against a later version must not break this one.
        let r: PollResponse =
            serde_json::from_str(r#"{"items":[],"cursor":"c","more":true,"future":1}"#).unwrap();
        assert_eq!(r.cursor.as_deref(), Some("c"));
        assert!(r.more);
    }
}
