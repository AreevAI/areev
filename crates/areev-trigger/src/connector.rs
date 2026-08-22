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
//!
//! **Blobs (#93).** An item may carry binary attachments without inlining
//! them into the Event grain: a per-item `blobs` array of
//! `{ "filename", "mime", "b64" }`, referenced from the payload by index
//! (`"blob": "@0"`). The EVALUATOR — the party that already holds the
//! writer — stores each entry in the CAS (`put_blob`, idempotent on
//! content), rewrites every `"blob": "@N"` reference to the resulting
//! `cas://sha256:…` address, and attaches a matching `content_refs` entry
//! to the Event it writes. The run's tools then use `blob get` exactly as
//! on the host-driven path, and erasure's sole-reference reclamation covers
//! these blobs with no special case. Budgets are enforced per item
//! ([`MAX_BLOB_BYTES_PER_ITEM`]) and per response
//! ([`MAX_BLOB_BYTES_PER_RESPONSE`]), decoded size, and a violation —
//! oversize, undecodable `b64`, or a dangling `"@N"` — fails the WHOLE
//! poll (`TRG-E011`) with the cursor unmoved: a silently dropped attachment
//! is an invoice posting without evidence, and a lost item is worse.

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
    /// Binary attachments for this item (#93). The evaluator stores each in
    /// the CAS and rewrites `"blob": "@<index>"` payload references to the
    /// `cas://` address; the bytes themselves never enter the Event grain.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blobs: Vec<PollBlob>,
}

/// One attachment riding with a [`PollItem`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PollBlob {
    /// Original filename, carried into the Event's `content_refs` metadata.
    pub filename: Option<String>,
    /// MIME type, carried into `content_refs.mime_type`.
    pub mime: Option<String>,
    /// The bytes, standard base64 (RFC 4648). Wasteful over the pipe but
    /// consistent with the one JSON-on-stdio contract; a temp-file side
    /// channel would be faster and was rejected for breaking it.
    pub b64: String,
}

/// Decoded-size budget for one item's blobs: 16 MiB, the maximum grain
/// size. Refused loudly (`TRG-E011`), never truncated.
pub const MAX_BLOB_BYTES_PER_ITEM: usize = 16 * 1024 * 1024;

/// Decoded-size budget for one poll response: 48 MiB — what the 64 MiB
/// stdout cap can carry once base64's 4/3 inflation is paid.
pub const MAX_BLOB_BYTES_PER_RESPONSE: usize = 48 * 1024 * 1024;

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

/// Rewrite `"blob": "@<index>"` payload references to the CAS addresses of
/// the item's stored blobs (#93).
///
/// Only string values under a key named `blob` are eligible, and only when
/// they start with `@` — anything else (a `cas://` address from a
/// re-delivered payload, ordinary strings) passes through untouched. An
/// `@`-value that is not a valid in-range index is an error: a dangling
/// reference silently left in place would hand the run a payload that
/// points at nothing.
pub fn rewrite_blob_refs(payload: &mut serde_json::Value, uris: &[String]) -> Result<(), String> {
    match payload {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "blob" {
                    if let serde_json::Value::String(s) = v {
                        if let Some(n) = s.strip_prefix('@') {
                            let uri = n
                                .parse::<usize>()
                                .ok()
                                .and_then(|i| uris.get(i))
                                .ok_or_else(|| {
                                    format!(
                                        "payload references blob \"@{n}\" but the item \
                                         carries {} blob(s)",
                                        uris.len()
                                    )
                                })?;
                            *v = serde_json::Value::String(uri.clone());
                            continue;
                        }
                    }
                }
                rewrite_blob_refs(v, uris)?;
            }
            Ok(())
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_blob_refs(v, uris)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, payload: serde_json::Value) -> PollItem {
        PollItem { id: id.into(), payload, blobs: Vec::new() }
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
    fn blob_refs_rewrite_to_cas_addresses() {
        let uris = vec!["cas://sha256:aa".to_string(), "cas://sha256:bb".to_string()];
        let mut payload = serde_json::json!({
            "email": { "from": "a@b.c" },
            "attachments": [
                { "filename": "inv.pdf", "blob": "@0" },
                { "filename": "po.pdf", "blob": "@1" },
            ],
            // Not under a `blob` key: untouched even though it looks like one.
            "note": "@0",
            // Already an address (re-delivery): untouched.
            "prior": { "blob": "cas://sha256:cc" },
        });
        rewrite_blob_refs(&mut payload, &uris).unwrap();
        assert_eq!(payload["attachments"][0]["blob"], "cas://sha256:aa");
        assert_eq!(payload["attachments"][1]["blob"], "cas://sha256:bb");
        assert_eq!(payload["note"], "@0");
        assert_eq!(payload["prior"]["blob"], "cas://sha256:cc");
    }

    #[test]
    fn a_dangling_blob_ref_is_refused_not_left_in_place() {
        let uris = vec!["cas://sha256:aa".to_string()];
        let mut payload = serde_json::json!({ "a": { "blob": "@7" } });
        assert!(rewrite_blob_refs(&mut payload, &uris).is_err());
        let mut payload = serde_json::json!({ "a": { "blob": "@zero" } });
        assert!(rewrite_blob_refs(&mut payload, &uris).is_err());
    }

    #[test]
    fn blobs_parse_and_default_to_empty() {
        let r: PollResponse = serde_json::from_str(
            r#"{"items":[{"id":"m1","payload":{"a":{"blob":"@0"}},
                 "blobs":[{"filename":"inv.pdf","mime":"application/pdf","b64":"Zm9v"}]}]}"#,
        )
        .unwrap();
        assert_eq!(r.items[0].blobs.len(), 1);
        assert_eq!(r.items[0].blobs[0].filename.as_deref(), Some("inv.pdf"));
        // An item without the field stays valid — the 1.5.0 contract.
        let r: PollResponse =
            serde_json::from_str(r#"{"items":[{"id":"m1","payload":{}}]}"#).unwrap();
        assert!(r.items[0].blobs.is_empty());
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
