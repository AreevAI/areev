use std::collections::HashMap;

use rmpv::Value;

use crate::error::{AreevError, Hash, Result};
use crate::format::field_map::{
    expand_context_field, expand_field, expand_related_to_field, expand_workflow_edge_field,
};
use crate::format::header::MgHeader;
#[allow(clippy::wildcard_imports)]
use crate::types::*;

/// Unwrap a COSE Sign1 envelope to get the inner .mg blob, if the bytes start with 0x84.
///
/// COSE Sign1 is encoded as a CBOR 4-element array, which always starts with 0x84.
/// Raw .mg blobs start with 0x01 (version byte).
/// If the input is not COSE-wrapped, returns `None` (caller uses raw bytes directly).
fn unwrap_if_cose(raw_bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if raw_bytes.first() != Some(&0x84) {
        return Ok(None);
    }
    #[cfg(feature = "signing")]
    {
        use coset::{CborSerializable, CoseSign1};
        let cose = CoseSign1::from_slice(raw_bytes)
            .map_err(|e| AreevError::Format(format!("COSE Sign1 decode: {}", e)))?;
        let inner = cose
            .payload
            .ok_or_else(|| AreevError::Format("COSE Sign1 envelope has no payload".into()))?;
        Ok(Some(inner))
    }
    #[cfg(not(feature = "signing"))]
    {
        Err(AreevError::Format(
            "blob is a COSE Sign1 envelope but the 'signing' feature is not enabled".into(),
        ))
    }
}

/// Deserialized grain data from an .mg blob.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeserializedGrain {
    pub header: MgHeader,
    pub grain_type: GrainType,
    pub fields: HashMap<String, serde_json::Value>,
    pub hash: Hash,
}

/// Deserialize an .mg blob into a DeserializedGrain.
///
/// Transparently handles COSE Sign1 envelopes: if the bytes start with 0x84,
/// the inner .mg blob is extracted first (no signature verification — use
/// `crypto::signing::verify_grain` separately if verification is required).
pub fn deserialize_blob(blob: &[u8]) -> Result<DeserializedGrain> {
    // Unwrap COSE Sign1 envelope if present (0x84 = CBOR 4-element array header).
    // The inner blob is owned; we keep a reference to either the unwrapped or original bytes.
    let unwrapped: Option<Vec<u8>> = unwrap_if_cose(blob)?;
    let inner: &[u8] = match unwrapped.as_deref() {
        Some(b) => b,
        None => blob,
    };

    if inner.len() < 10 {
        return Err(AreevError::Format(
            "blob too short (minimum 10 bytes)".into(),
        ));
    }
    if inner.len() > MAX_GRAIN_BYTES {
        return Err(AreevError::Format(format!(
            "blob too large ({} bytes, maximum {MAX_GRAIN_BYTES})",
            inner.len()
        )));
    }

    // Parse header
    let header = MgHeader::from_bytes(&inner[..9])?;

    // Parse grain type
    let grain_type = GrainType::from_byte(header.grain_type).ok_or_else(|| {
        AreevError::Format(format!("unknown grain type byte: {:#x}", header.grain_type))
    })?;

    // Parse msgpack payload. Guard the untrusted framing first: this rejects
    // hostile grains that would otherwise stack-overflow the recursive decoder
    // (deep nesting) or trigger a giant pre-allocation (a short header claiming
    // a huge container/string length) before `read_value` ever touches them.
    let payload = &inner[9..];
    guard_msgpack_shape(payload)?;
    let mut cursor: &[u8] = payload;
    let value: Value = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| AreevError::Serialization(format!("msgpack decode error: {}", e)))?;
    // Canonical blobs are exactly one msgpack value — no trailing bytes. Reject
    // padding: otherwise the same logical grain has unlimited distinct content
    // addresses (a content-address malleability / dedup bypass), since the hash
    // is taken over the whole padded blob.
    if !cursor.is_empty() {
        return Err(AreevError::Format(format!(
            "non-canonical blob: {} trailing byte(s) after the grain payload",
            cursor.len()
        )));
    }

    // Convert to expanded field names
    let fields = msgpack_to_json_expanded(&value)?;
    let fields_map = match fields {
        serde_json::Value::Object(m) => {
            let mut map = HashMap::with_capacity(m.len().max(20));
            map.extend(m);
            map
        }
        _ => return Err(AreevError::Format("payload must be a map".into())),
    };

    // Content hash is always SHA-256(inner_blob), not SHA-256(cose_bytes)
    let hash = crate::format::header::content_address(inner);

    Ok(DeserializedGrain {
        header,
        grain_type,
        fields: fields_map,
        hash,
    })
}

/// Maximum accepted size of a single `.mg` grain payload — defense against
/// memory exhaustion from a crafted bundle/segment. Grains are small records.
/// Enforced symmetrically at serialize time so a grain that can be written can
/// always be read back.
pub(crate) const MAX_GRAIN_BYTES: usize = 16 * 1024 * 1024;

/// Maximum msgpack container nesting depth accepted from an untrusted blob —
/// defense against stack overflow in the recursive decoder. Generous relative
/// to real grains (which are shallow), so it does not reject legitimate data;
/// enforced symmetrically at serialize time.
pub(crate) const MAX_MSGPACK_DEPTH: usize = 256;

/// Validate the framing of an untrusted msgpack payload *before* decoding it,
/// iteratively (no recursion). Rejects two classes of hostile input:
///
/// 1. **Nesting deeper than [`MAX_MSGPACK_DEPTH`]** — the decoder
///    (`rmpv::read_value`) and our own conversion are recursive, so a deeply
///    nested value could overflow the stack. This walk is iterative and caps
///    depth before that can happen.
/// 2. **A container/string/binary whose declared length exceeds the bytes that
///    remain** — e.g. a 5-byte `array32` header claiming four billion elements,
///    which would make the decoder pre-allocate a huge buffer. Every declared
///    length is checked against the actual remaining bytes.
///
/// Only the framing (markers + length prefixes) is examined; values are
/// skipped, so this is O(n) in the payload size with no allocation.
pub(crate) fn guard_msgpack_shape(buf: &[u8]) -> Result<()> {
    fn need_bytes(buf: &[u8], pos: usize, n: usize) -> Result<()> {
        match pos.checked_add(n) {
            Some(end) if end <= buf.len() => Ok(()),
            _ => Err(AreevError::Format("truncated msgpack payload".into())),
        }
    }
    fn read_be(buf: &[u8], pos: usize, n: usize) -> u64 {
        let mut v = 0u64;
        for &b in &buf[pos..pos + n] {
            v = (v << 8) | b as u64;
        }
        v
    }
    // Read an `n`-byte big-endian length prefix and advance past it.
    fn len_prefixed(buf: &[u8], pos: &mut usize, n: usize) -> Result<usize> {
        need_bytes(buf, *pos, n)?;
        let v = read_be(buf, *pos, n);
        *pos += n;
        Ok(v as usize)
    }
    // Advance past `n` payload bytes, checking they are present.
    fn skip(buf: &[u8], pos: &mut usize, n: usize) -> Result<()> {
        need_bytes(buf, *pos, n)?;
        *pos += n;
        Ok(())
    }
    // Push a container of `count` child elements, enforcing depth and rejecting
    // counts that cannot fit in the remaining bytes (each element needs >= 1).
    fn push_container(stack: &mut Vec<u64>, buf: &[u8], pos: usize, count: u64) -> Result<()> {
        if stack.len() >= MAX_MSGPACK_DEPTH {
            return Err(AreevError::Format("msgpack nesting too deep".into()));
        }
        if count > (buf.len() - pos) as u64 {
            return Err(AreevError::Format(
                "msgpack container length exceeds payload".into(),
            ));
        }
        stack.push(count);
        Ok(())
    }

    let mut pos = 0usize;
    // Each stack entry is the number of values still to read at that level.
    let mut stack: Vec<u64> = vec![1]; // top level: exactly one value
    while let Some(&top) = stack.last() {
        if top == 0 {
            stack.pop();
            continue;
        }
        *stack.last_mut().unwrap() -= 1;

        need_bytes(buf, pos, 1)?;
        let marker = buf[pos];
        pos += 1;

        match marker {
            // fixint (pos/neg), nil, false, true — no payload
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => {}
            0xc1 => return Err(AreevError::Format("invalid msgpack marker 0xc1".into())),
            // fixstr
            0xa0..=0xbf => {
                let len = (marker & 0x1f) as usize;
                skip(buf, &mut pos, len)?;
            }
            // fixmap (0x80..=0x8f, pairs) / fixarray (0x90..=0x9f, elems)
            0x80..=0x9f => {
                let n = (marker & 0x0f) as u64;
                let count = if marker < 0x90 { n * 2 } else { n };
                push_container(&mut stack, buf, pos, count)?;
            }
            // fixed-width scalars
            0xcc | 0xd0 => skip(buf, &mut pos, 1)?,
            0xcd | 0xd1 => skip(buf, &mut pos, 2)?,
            0xca | 0xce | 0xd2 => skip(buf, &mut pos, 4)?,
            0xcb | 0xcf | 0xd3 => skip(buf, &mut pos, 8)?,
            // str8/bin8, str16/bin16, str32/bin32
            0xd9 | 0xc4 => {
                let l = len_prefixed(buf, &mut pos, 1)?;
                skip(buf, &mut pos, l)?;
            }
            0xda | 0xc5 => {
                let l = len_prefixed(buf, &mut pos, 2)?;
                skip(buf, &mut pos, l)?;
            }
            0xdb | 0xc6 => {
                let l = len_prefixed(buf, &mut pos, 4)?;
                skip(buf, &mut pos, l)?;
            }
            // ext8/16/32 (length prefix + 1 type byte + data)
            0xc7 => {
                let l = len_prefixed(buf, &mut pos, 1)?;
                skip(buf, &mut pos, l + 1)?;
            }
            0xc8 => {
                let l = len_prefixed(buf, &mut pos, 2)?;
                skip(buf, &mut pos, l + 1)?;
            }
            0xc9 => {
                let l = len_prefixed(buf, &mut pos, 4)?;
                skip(buf, &mut pos, l + 1)?;
            }
            // fixext1/2/4/8/16 (1 type byte + N data)
            0xd4 => skip(buf, &mut pos, 1 + 1)?,
            0xd5 => skip(buf, &mut pos, 1 + 2)?,
            0xd6 => skip(buf, &mut pos, 1 + 4)?,
            0xd7 => skip(buf, &mut pos, 1 + 8)?,
            0xd8 => skip(buf, &mut pos, 1 + 16)?,
            // array16 / array32
            0xdc => {
                let c = len_prefixed(buf, &mut pos, 2)? as u64;
                push_container(&mut stack, buf, pos, c)?;
            }
            0xdd => {
                let c = len_prefixed(buf, &mut pos, 4)? as u64;
                push_container(&mut stack, buf, pos, c)?;
            }
            // map16 / map32 (pairs -> 2x elements)
            0xde => {
                let c = len_prefixed(buf, &mut pos, 2)? as u64;
                push_container(&mut stack, buf, pos, c * 2)?;
            }
            0xdf => {
                let c = len_prefixed(buf, &mut pos, 4)? as u64;
                push_container(&mut stack, buf, pos, c * 2)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod msgpack_guard_tests {
    use super::guard_msgpack_shape;

    #[test]
    fn accepts_small_valid_values() {
        assert!(guard_msgpack_shape(&[0x80]).is_ok()); // empty map
        assert!(guard_msgpack_shape(&[0x90]).is_ok()); // empty array
        assert!(guard_msgpack_shape(&[0xc0]).is_ok()); // nil
        assert!(guard_msgpack_shape(&[0x81, 0xa1, b'a', 0xa1, b'b']).is_ok()); // {"a":"b"}
    }

    #[test]
    fn rejects_excessive_depth() {
        // Nesting beyond MAX_MSGPACK_DEPTH (256) is rejected.
        let mut deep = vec![0x91u8; 300];
        deep.push(0xc0);
        assert!(guard_msgpack_shape(&deep).is_err());
    }

    #[test]
    fn rejects_oversized_container_count() {
        // array32 / map32 claiming ~4.29e9 elements in a 5-byte header.
        assert!(guard_msgpack_shape(&[0xdd, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(guard_msgpack_shape(&[0xdf, 0xff, 0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(guard_msgpack_shape(&[0x92]).is_err()); // array of 2, no elements
        assert!(guard_msgpack_shape(&[0xa5, b'h', b'i']).is_err()); // str len 5, 2 bytes
        assert!(guard_msgpack_shape(&[]).is_err()); // needs exactly one value
    }

    #[test]
    fn rejects_reserved_marker() {
        assert!(guard_msgpack_shape(&[0xc1]).is_err());
    }

    #[test]
    fn map_element_count_is_pairs_times_two() {
        // map16 claiming 3 pairs = 6 children but only 5 present: the correct
        // `pairs * 2` count must reject; pins the container-count arithmetic.
        assert!(guard_msgpack_shape(&[0xde, 0x00, 0x03, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0]).is_err());
        // map32 form.
        assert!(guard_msgpack_shape(
            &[0xdf, 0x00, 0x00, 0x00, 0x03, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0]
        )
        .is_err());
        // Exactly 2 pairs (4 children) present → accepted.
        assert!(
            guard_msgpack_shape(&[0xde, 0x00, 0x02, 0xa1, b'k', 0xc0, 0xa1, b'j', 0xc3]).is_ok()
        );
    }

    #[test]
    fn array_count_is_exact() {
        // array16 with exactly 3 elements → accepted; one fewer byte → rejected.
        assert!(guard_msgpack_shape(&[0xdc, 0x00, 0x03, 0xc0, 0xc0, 0xc0]).is_ok());
        assert!(guard_msgpack_shape(&[0xdc, 0x00, 0x03, 0xc0, 0xc0]).is_err());
    }

    #[test]
    fn multibyte_string_lengths_are_read_exactly() {
        // str16 length 3 as the first of two array elements, then nil: the
        // length prefix must be read exactly or the second element misaligns.
        assert!(guard_msgpack_shape(&[0x92, 0xda, 0x00, 0x03, b'a', b'b', b'c', 0xc0]).is_ok());
        // str16 claiming length 5 with only 3 bytes present → rejected.
        assert!(guard_msgpack_shape(&[0xda, 0x00, 0x05, b'a', b'b', b'c']).is_err());
    }
}

/// Which key table (if any) applies to the map currently being decoded.
#[derive(Clone, Copy)]
enum KeyMode {
    /// The grain's top-level map — keys are OMS field names, expand via FIELD_MAP.
    OmsTop,
    /// The immediate keys of a `context` map — only the OMS int:* profile keys
    /// were compacted there; reverse just those, keep user keys verbatim.
    ContextTop,
    /// A user-controlled / already-verbatim nested map — never rewrite keys.
    Verbatim,
    /// The `edges` array of a Workflow grain. Its elements are OMS-defined edge
    /// maps (§8.4), which carry a compacted key.
    WorkflowEdgeArray,
    /// One element of that array.
    WorkflowEdgeMap,
    /// The `related_to` array (§6.1). Its elements are OMS-defined link maps
    /// with compacted keys.
    RelatedToArray,
    /// One element of that array.
    RelatedToMap,
}

/// Convert a msgpack Value to JSON, expanding field names ONLY where the
/// serializer compacted them: the grain's top-level OMS fields and (restricted
/// to int:* profile keys) the immediate keys of a `context` map. Every deeper,
/// user-controlled map is converted verbatim — applying the general FIELD_MAP
/// to nested user JSON rewrites keys that collide with an OMS short code (e.g.
/// a State `context` key `"o"` → `"object"`), corrupting the data and
/// destabilizing the content address on re-serialize.
fn msgpack_to_json_expanded(value: &Value) -> Result<serde_json::Value> {
    msgpack_to_json(value, KeyMode::OmsTop)
}

fn msgpack_to_json(value: &Value, mode: KeyMode) -> Result<serde_json::Value> {
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Integer(i) => {
            if let Some(n) = i.as_i64() {
                Ok(serde_json::Value::Number(n.into()))
            } else if let Some(n) = i.as_u64() {
                Ok(serde_json::Value::Number(n.into()))
            } else {
                Err(AreevError::Format("integer out of range".into()))
            }
        }
        Value::F32(f) => Ok(serde_json::Value::Number(
            serde_json::Number::from_f64(*f as f64)
                .ok_or_else(|| AreevError::Format("invalid float".into()))?,
        )),
        Value::F64(f) => Ok(serde_json::Value::Number(
            serde_json::Number::from_f64(*f)
                .ok_or_else(|| AreevError::Format("invalid float (NaN/Inf)".into()))?,
        )),
        Value::String(s) => {
            let str_val = s.as_str().unwrap_or("");
            Ok(serde_json::Value::String(str_val.to_string()))
        }
        Value::Binary(b) => Ok(serde_json::Value::String(hex::encode(b))),
        Value::Array(arr) => {
            // Array elements are values, not OMS field names — verbatim keys.
            // The sole exception is a Workflow `edges` array, whose elements are
            // spec-defined maps carrying the compacted `mxc` key.
            let child = match mode {
                KeyMode::WorkflowEdgeArray => KeyMode::WorkflowEdgeMap,
                KeyMode::RelatedToArray => KeyMode::RelatedToMap,
                _ => KeyMode::Verbatim,
            };
            let items: Result<Vec<serde_json::Value>> =
                arr.iter().map(|x| msgpack_to_json(x, child)).collect();
            Ok(serde_json::Value::Array(items?))
        }
        Value::Map(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                let raw = match k {
                    Value::String(s) => s.as_str().unwrap_or(""),
                    _ => return Err(AreevError::Format("map key must be string".into())),
                };
                let (key, child) = match mode {
                    KeyMode::OmsTop => {
                        let expanded = expand_field(raw).to_string();
                        // Only the `context` field's immediate keys carry the
                        // restricted int:* reversal; everything else is verbatim.
                        let child = match expanded.as_str() {
                            // `common_context` holds a §6.1 common context that
                            // yielded `ctx` to a type-specific field, so its keys
                            // were compacted the same way and reverse the same way.
                            "context" | "common_context" => KeyMode::ContextTop,
                            // A Trigger's `config` carries the same A.7 `int:`
                            // vocabulary as a `context` map and is compacted
                            // with it on write, so it reverses the same way.
                            // Connector-specific keys inside it stay verbatim.
                            "config" => KeyMode::ContextTop,
                            "edges" => KeyMode::WorkflowEdgeArray,
                            "related_to" => KeyMode::RelatedToArray,
                            _ => KeyMode::Verbatim,
                        };
                        (expanded, child)
                    }
                    KeyMode::ContextTop => (expand_context_field(raw).to_string(), KeyMode::Verbatim),
                    KeyMode::WorkflowEdgeMap => {
                        (expand_workflow_edge_field(raw).to_string(), KeyMode::Verbatim)
                    }
                    KeyMode::RelatedToMap => {
                        (expand_related_to_field(raw).to_string(), KeyMode::Verbatim)
                    }
                    // An `edges`/`related_to` value that is a map rather than an
                    // array is not spec-shaped; treat its keys as data.
                    KeyMode::WorkflowEdgeArray
                    | KeyMode::RelatedToArray
                    | KeyMode::Verbatim => (raw.to_string(), KeyMode::Verbatim),
                };
                map.insert(key, msgpack_to_json(v, child)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        Value::Ext(_, _) => Err(AreevError::Format("ext type not supported".into())),
    }
}

impl DeserializedGrain {
    /// Get a string field value.
    pub fn get_str(&self, field: &str) -> Option<&str> {
        self.fields.get(field)?.as_str()
    }

    /// Base text used by the recall **reranker** (`query::extract_grain_text`
    /// delegates here). This is a cross-encoder scoring projection, NOT the
    /// embedding projection — it intentionally differs from the typed
    /// [`crate::types::Grain::text`] for non-Fact types (it scans a fixed
    /// content-field list rather than reconstructing each type's `text()`).
    ///
    /// `Fact` → `"subject relation object"`; every other type → the first
    /// non-empty of a fixed content-field list, falling back to the grain-type
    /// name. Do NOT route the embedding backfill through this — use
    /// [`Self::embedding_text`], which reconstructs the typed grain and calls
    /// the real [`crate::types::Grain::embedding_text`] for per-type parity
    /// with the inline write path.
    pub fn base_text(&self) -> String {
        match self.grain_type {
            GrainType::Fact => {
                let s = self.get_str("subject").unwrap_or("");
                let r = self.get_str("relation").unwrap_or("");
                let o = self.get_str("object").unwrap_or("");
                format!("{} {} {}", s, r, o).trim().to_string()
            }
            _ => {
                for field in &[
                    "content",
                    "description",
                    "title",
                    "goal",
                    "query",
                    "result",
                    "output",
                ] {
                    if let Some(v) = self.get_str(field) {
                        if !v.trim().is_empty() {
                            return v.to_string();
                        }
                    }
                }
                self.grain_type.as_str().to_string()
            }
        }
    }

    /// Reconstruct the embedding-input text for this stored grain, matching
    /// [`crate::types::Grain::embedding_text`] of the live typed grain EXACTLY
    /// so the backfill embeds the SAME text the inline write path embeds
    /// (`write.rs` calls `grain.embedding_text()`; design §PR-3b step 4 / §R-6).
    ///
    /// Parity is guaranteed *by construction*: instead of hand-replicating each
    /// type's `text()` projection (which silently drifts as types evolve), we
    /// reconstruct the concrete typed grain from the deserialized fields and
    /// delegate to its real `Grain::embedding_text()`. That trait method already
    /// handles the explicit `embedding_text` override and the `text() + [tags] +
    /// (namespace)` auto-enrichment uniformly for all 11 grain types.
    ///
    /// Returns an empty string when there is no text to embed (the caller then
    /// skips the grain — an empty projection must never be embedded).
    pub fn embedding_text(&self) -> String {
        self.reconstruct_for_embedding().embedding_text()
    }

    /// Populate the three [`GrainCommon`] fields that
    /// [`crate::types::Grain::embedding_text`] reads — `embedding_text`
    /// override, `tags`, `namespace` — onto a freshly reconstructed grain.
    ///
    /// `GrainCommon.tags` serializes under the long name `structural_tags` (it
    /// compacts to the wire key `tags`, then expands back to `structural_tags`
    /// on deserialize — NOT `tags`). Reading the wrong key would silently drop
    /// the tag enrichment, so the embedding projection diverges from the write
    /// path. The other GrainCommon fields are irrelevant to `embedding_text()`.
    fn fill_embedding_common(&self, common: &mut GrainCommon) {
        if let Some(et) = self.get_str("embedding_text") {
            common.embedding_text = Some(et.to_string());
        }
        if let Some(ns) = self.get_str("namespace") {
            common.namespace = Some(ns.to_string());
        }
        if let Some(arr) = self
            .fields
            .get("structural_tags")
            .and_then(|v| v.as_array())
        {
            common.tags = arr
                .iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
        }
    }

    /// Reconstruct a concrete typed grain carrying exactly the fields that feed
    /// [`crate::types::Grain::embedding_text`]: each type's `text()` inputs plus
    /// the three common fields populated by [`Self::fill_embedding_common`].
    /// Used only to derive embedding text — not a full round-trip reconstruction
    /// (see `to_fact`/`to_event`/`to_tool`/`to_skill` for those).
    fn reconstruct_for_embedding(&self) -> Box<dyn Grain> {
        let s = |f: &str| self.get_str(f).unwrap_or("").to_string();
        let mut grain: Box<dyn Grain> = match self.grain_type {
            GrainType::Fact => Box::new(Fact::new(&s("subject"), &s("relation"), &s("object"))),
            GrainType::Event => Box::new(Event::new(&s("content"))),
            GrainType::State => {
                let context = self
                    .fields
                    .get("context")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Box::new(State::new(context))
            }
            GrainType::Workflow => {
                let nodes = self
                    .fields
                    .get("nodes")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|n| n.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut wf = Workflow::new(nodes);
                if let Some(t) = self.get_str("trigger") {
                    wf = wf.trigger(t);
                }
                Box::new(wf)
            }
            GrainType::Trigger => {
                let kind = self
                    .get_str("kind")
                    .and_then(TriggerKind::parse)
                    .unwrap_or_default();
                let mut t = Trigger::new(kind, &s("workflow"));
                if let Some(c) = self.get_str("connector") {
                    t = t.connector(c);
                }
                if let Some(sc) = self.get_str("scope") {
                    t = t.scope(sc);
                }
                if let Some(c) = self.get_str("cron") {
                    t = t.cron(c);
                }
                Box::new(t)
            }
            GrainType::Tool => {
                let mut tool = Tool::new(&s("tool_name"));
                // Tool's content compacts to "cnt" → expands to "tool_content".
                if let Some(c) = self
                    .get_str("tool_content")
                    .or_else(|| self.get_str("content"))
                {
                    tool.content = Some(c.to_string());
                }
                Box::new(tool)
            }
            GrainType::Observation => {
                let mut obs = Observation::new(&s("observer_id"), &s("observer_type"));
                if let Some(subj) = self.get_str("subject") {
                    obs = obs.subject(subj);
                }
                if let Some(obj) = self.get_str("object") {
                    obs = obs.object(obj);
                }
                Box::new(obs)
            }
            GrainType::Goal => {
                let mut goal = Goal::new(&s("description"));
                if let Some(c) = self.get_str("criteria") {
                    goal.criteria = Some(c.to_string());
                }
                Box::new(goal)
            }
            GrainType::Reasoning => {
                let mut r = Reasoning::new();
                if let Some(c) = self.get_str("conclusion") {
                    r.conclusion = Some(c.to_string());
                }
                if let Some(t) = self.get_str("thinking_content") {
                    r.thinking_content = Some(t.to_string());
                }
                Box::new(r)
            }
            GrainType::Consensus => {
                let mut c = Consensus::new();
                if let Some(ac) = self.get_str("agreed_content") {
                    c.agreed_content = Some(ac.to_string());
                }
                c.agreement_count = self.get_i64("agreement_count");
                c.threshold = self.get_f64("threshold");
                Box::new(c)
            }
            GrainType::Consent => {
                let mut c = Consent::new(&s("subject_did"));
                if let Some(g) = self.get_str("grantee_did") {
                    c.grantee_did = Some(g.to_string());
                }
                c.is_withdrawal = self.get_bool("is_withdrawal");
                Box::new(c)
            }
            GrainType::Recommendation => {
                let sm = self.fields.get("summary");
                let summary = Summary {
                    template_id: sm
                        .and_then(|v| v.get("template_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    args: sm
                        .and_then(|v| v.get("args"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                };
                Box::new(Recommendation::new(
                    self.get_str("target_ref").unwrap_or(""),
                    Analyzer::new(
                        self.fields
                            .get("analyzer")
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    ),
                    summary,
                    Proposal::Cal(self.get_str("proposal_cal").unwrap_or("").to_string()),
                ))
            }
            GrainType::Skill => {
                let mut sk = Skill::new(&s("name"), &s("description"));
                if let Some(w) = self.get_str("when_to_use") {
                    sk.when_to_use = Some(w.to_string());
                }
                if let Some(d) = self.get_str("domain") {
                    sk.domain = Some(d.to_string());
                }
                Box::new(sk)
            }
        };
        self.fill_embedding_common(grain.common_mut());
        grain
    }

    /// Get an i64 field value.
    pub fn get_i64(&self, field: &str) -> Option<i64> {
        self.fields.get(field)?.as_i64()
    }

    /// Get an f64 field value.
    pub fn get_f64(&self, field: &str) -> Option<f64> {
        self.fields.get(field)?.as_f64()
    }

    /// Get a bool field value.
    pub fn get_bool(&self, field: &str) -> Option<bool> {
        self.fields.get(field)?.as_bool()
    }

    /// Get a u64 field value.
    pub fn get_u64(&self, field: &str) -> Option<u64> {
        self.fields.get(field)?.as_u64()
    }

    /// Reconstruct a Fact from deserialized fields.
    pub fn to_fact(&self) -> Result<Fact> {
        if self.grain_type != GrainType::Fact {
            return Err(AreevError::Validation("not a Fact grain".into()));
        }
        let subject = self
            .get_str("subject")
            .ok_or_else(|| AreevError::Validation("missing subject".into()))?;
        let relation = self
            .get_str("relation")
            .ok_or_else(|| AreevError::Validation("missing relation".into()))?;
        let object = self
            .get_str("object")
            .ok_or_else(|| AreevError::Validation("missing object".into()))?;

        let mut fact = Fact::new(subject, relation, object);

        if let Some(c) = self.get_f64("confidence") {
            fact.common.confidence = c;
        }
        if let Some(ns) = self.get_str("namespace") {
            fact.common.namespace = Some(ns.to_string());
        }
        if let Some(uid) = self.get_str("user_id") {
            fact.common.user_id = Some(uid.to_string());
        }
        if let Some(st) = self.get_str("source_type") {
            fact.common.source_type = Some(st.to_string());
        }
        if let Some(ca) = self.get_i64("created_at") {
            fact.common.created_at = Some(ca);
        }
        if let Some(adid) = self.get_str("author_did") {
            fact.common.author_did = Some(adid.to_string());
        }
        if let Some(et) = self.get_str("embedding_text") {
            fact.common.embedding_text = Some(et.to_string());
        }

        Ok(fact)
    }

    /// Reconstruct an Tool from deserialized fields. Phase 1 (2026-04-19):
    /// reads typed definition fields (`input_schema`, `executor_uri`,
    /// `locked_params`, `examples`, `annotations`, `spec_hash`,
    /// `tool_description`, `strict`, `tool_call_id`, `call_batch_id`,
    /// `kind`) plus existing execution-record fields (`tool_name`, `input`,
    /// `content`/`tool_content`, `is_error`, `error`, `duration_ms`,
    /// `parent_task_id`, `output_schema`).
    ///
    /// `kind` defaults to `Execution` when absent — preserves
    /// backward-compatibility for any pre-Phase-1 grain that lacks the
    /// discriminator.
    pub fn to_tool(&self) -> Result<Tool> {
        if self.grain_type != GrainType::Tool {
            return Err(AreevError::Validation("not an Tool grain".into()));
        }
        let tool_name = self.get_str("tool_name").unwrap_or("").to_string();
        let mut a = Tool::new(&tool_name);

        if let Some(k) = self.get_str("kind").and_then(ToolKind::parse) {
            a.kind = k;
        }
        if let Some(v) = self.fields.get("input") {
            a.input = Some(v.clone());
        }
        // Tool's content compacts to "cnt" but expands back to "tool_content".
        if let Some(s) = self
            .get_str("tool_content")
            .or_else(|| self.get_str("content"))
        {
            a.content = Some(s.to_string());
        }
        if let Some(b) = self.get_bool("is_error") {
            a.is_error = Some(b);
        }
        if let Some(s) = self.get_str("error") {
            a.error = Some(s.to_string());
        }
        if let Some(n) = self.get_u64("duration_ms") {
            a.duration_ms = Some(n);
        }
        if let Some(s) = self.get_str("parent_task_id") {
            a.parent_task_id = Some(s.to_string());
        }
        if let Some(s) = self.get_str("tool_call_id") {
            a.tool_call_id = Some(s.to_string());
        }
        if let Some(s) = self.get_str("call_batch_id") {
            a.call_batch_id = Some(s.to_string());
        }
        if let Some(s) = self.get_str("tool_description") {
            a.tool_description = Some(s.to_string());
        }
        if let Some(v) = self.fields.get("input_schema") {
            a.input_schema = Some(v.clone());
        }
        if let Some(v) = self.fields.get("output_schema") {
            a.output_schema = Some(v.clone());
        }
        if let Some(b) = self.get_bool("strict") {
            a.strict = Some(b);
        }
        if let Some(b) = self.get_bool("async_mode") {
            a.async_mode = Some(b);
        }
        if let Some(s) = self.get_str("executor_uri") {
            a.executor_uri = Some(s.to_string());
        }
        if let Some(v) = self.fields.get("locked_params") {
            a.locked_params = Some(v.clone());
        }
        if let Some(arr) = self.fields.get("examples").and_then(|v| v.as_array()) {
            a.examples = Some(arr.clone());
        }
        if let Some(v) = self.fields.get("annotations") {
            if let Ok(anno) = serde_json::from_value::<ToolAnnotations>(v.clone()) {
                a.annotations = Some(anno);
            }
        }
        if let Some(s) = self.get_str("spec_hash") {
            a.spec_hash = Some(s.to_string());
        }
        // HPL Phase 4.1: executor_kind. Absent → None; deserialize
        // defaults to None, dispatch treats None as Host. Unknown wire
        // strings are ignored (rather than erroring) so blobs authored
        // by a newer code version remain readable by older cells.
        if let Some(k) = self
            .get_str("executor_kind")
            .and_then(crate::types::executor_kind::ExecutorKind::parse)
        {
            a.executor_kind = Some(k);
        }
        // Async exec lifecycle. Absent fields stay None;
        // `status=None` is treated as `Completed` at every read site.
        // Unknown wire strings are ignored (forward-compat with newer
        // cells that may emit additional variants).
        if let Some(s) = self.get_str("status").and_then(ExecutionStatus::parse) {
            a.status = Some(s);
        }
        if let Some(s) = self.get_str("correlation_id") {
            a.correlation_id = Some(s.to_string());
        }
        if let Some(n) = self.get_i64("expires_at_sec") {
            a.expires_at_sec = Some(n);
        }
        if let Some(hex_str) = self.get_str("transient_definition_hash") {
            if let Ok(bytes) = hex::decode(hex_str) {
                if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                    a.transient_definition_hash = Some(arr);
                }
            }
        }
        if let Some(fc) = self.get_str("failure_cause").and_then(FailureCause::parse) {
            a.failure_cause = Some(fc);
        }
        if let Some(s) = self.get_str("failure_detail") {
            a.failure_detail = Some(s.to_string());
        }
        if let Some(aex) = self
            .get_str("actor_execution_environment")
            .and_then(ActorExecutionEnvironment::parse)
        {
            a.actor_execution_environment = Some(aex);
        }

        // Common fields.
        if let Some(c) = self.get_f64("confidence") {
            a.common.confidence = c;
        }
        if let Some(ns) = self.get_str("namespace") {
            a.common.namespace = Some(ns.to_string());
        }
        if let Some(uid) = self.get_str("user_id") {
            a.common.user_id = Some(uid.to_string());
        }
        if let Some(st) = self.get_str("source_type") {
            a.common.source_type = Some(st.to_string());
        }
        if let Some(ca) = self.get_i64("created_at") {
            a.common.created_at = Some(ca);
        }
        if let Some(adid) = self.get_str("author_did") {
            a.common.author_did = Some(adid.to_string());
        }

        Ok(a)
    }

    /// Reconstruct an Event from deserialized fields, including the
    /// OMS 1.2 §8.2 chat-extension fields (role, session_id,
    /// parent_message_id, content_blocks, model_id, stop_reason,
    /// token_usage, run_id).
    pub fn to_event(&self) -> Result<Event> {
        if self.grain_type != GrainType::Event {
            return Err(AreevError::Validation("not an Event grain".into()));
        }
        let content = self.get_str("content").unwrap_or("").to_string();
        let mut ev = Event::new(&content);

        if let Some(s) = self.get_str("subject") {
            ev.subject = Some(s.to_string());
        }
        if let Some(o) = self.get_str("object") {
            ev.object = Some(o.to_string());
        }
        if let Some(r) = self.get_str("role").and_then(Role::from_str) {
            ev.role = Some(r);
        }
        if let Some(s) = self.get_str("session_id") {
            ev.session_id = Some(s.to_string());
        }
        if let Some(p) = self.get_str("parent_message_id") {
            ev.parent_message_id = Some(p.to_string());
        }
        if let Some(m) = self.get_str("model_id") {
            ev.model_id = Some(m.to_string());
        }
        if let Some(sr) = self.get_str("stop_reason") {
            ev.stop_reason = Some(sr.to_string());
        }
        if let Some(rid) = self.get_str("run_id") {
            ev.run_id = Some(rid.to_string());
        }
        if let Some(blocks_json) = self.fields.get("content_blocks") {
            if let Ok(blocks) = serde_json::from_value(blocks_json.clone()) {
                ev.content_blocks = Some(blocks);
            }
        }
        if let Some(tu_json) = self.fields.get("token_usage") {
            if let Ok(tu) = serde_json::from_value(tu_json.clone()) {
                ev.token_usage = Some(tu);
            }
        }

        // Common fields (mirror to_fact).
        if let Some(c) = self.get_f64("confidence") {
            ev.common.confidence = c;
        }
        if let Some(ns) = self.get_str("namespace") {
            ev.common.namespace = Some(ns.to_string());
        }
        if let Some(uid) = self.get_str("user_id") {
            ev.common.user_id = Some(uid.to_string());
        }
        if let Some(st) = self.get_str("source_type") {
            ev.common.source_type = Some(st.to_string());
        }
        if let Some(ca) = self.get_i64("created_at") {
            ev.common.created_at = Some(ca);
        }
        if let Some(adid) = self.get_str("author_did") {
            ev.common.author_did = Some(adid.to_string());
        }
        if let Some(et) = self.get_str("embedding_text") {
            ev.common.embedding_text = Some(et.to_string());
        }

        Ok(ev)
    }

    /// Reconstruct a Skill from deserialized fields (OMS 1.4 §8.11).
    ///
    /// `proficiency` aliases `common.confidence` (D3): we read `prof` if
    /// present, else fall back to `cf` (`confidence`), and write the result
    /// into `common.confidence`. A writer that emits `prof ≠ cf`
    /// is taken at its `prof` word for a Skill (spec is SHOULD, not MUST)
    /// with a debug log — the only place the two could disagree.
    pub fn to_skill(&self) -> Result<Skill> {
        if self.grain_type != GrainType::Skill {
            return Err(AreevError::Validation("not a Skill grain".into()));
        }
        let name = self.get_str("name").unwrap_or("").to_string();
        let description = self.get_str("description").unwrap_or("").to_string();
        let mut s = Skill::new(&name, &description);

        if let Some(instr) = self.get_str("instructions") {
            s.instructions = Some(instr.to_string());
        }
        if let Some(wtu) = self.get_str("when_to_use") {
            s.when_to_use = Some(wtu.to_string());
        }
        if let Some(v) = self.get_str("version") {
            s.version = Some(v.to_string());
        }
        if let Some(arr) = self.fields.get("allowed_tools").and_then(|v| v.as_array()) {
            s.allowed_tools = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(arr) = self.fields.get("resources").and_then(|v| v.as_array()) {
            s.resources = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(arr) = self.fields.get("dependencies").and_then(|v| v.as_array()) {
            s.dependencies = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(arr) = self
            .fields
            .get("input_modalities")
            .and_then(|v| v.as_array())
        {
            s.input_modalities = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(arr) = self
            .fields
            .get("output_modalities")
            .and_then(|v| v.as_array())
        {
            s.output_modalities = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(dom) = self.get_str("domain") {
            s.domain = Some(dom.to_string());
        }
        if let Some(hdid) = self.get_str("holder_did") {
            s.holder_did = Some(hdid.to_string());
        }
        if let Some(pc) = self.get_u64("practice_count") {
            s.practice_count = Some(pc as u32);
        }
        if let Some(lpa) = self.get_i64("last_practiced_at") {
            s.last_practiced_at = Some(lpa);
        }
        if let Some(strat_json) = self.fields.get("strategies") {
            if let Ok(strats) = serde_json::from_value(strat_json.clone()) {
                s.strategies = strats;
            }
        }
        if let Some(xfer) = self.get_bool("transferable") {
            s.transferable = Some(xfer);
        }

        // Common fields.
        if let Some(ns) = self.get_str("namespace") {
            s.common.namespace = Some(ns.to_string());
        }
        if let Some(uid) = self.get_str("user_id") {
            s.common.user_id = Some(uid.to_string());
        }
        if let Some(st) = self.get_str("source_type") {
            s.common.source_type = Some(st.to_string());
        }
        if let Some(ca) = self.get_i64("created_at") {
            s.common.created_at = Some(ca);
        }
        if let Some(adid) = self.get_str("author_did") {
            s.common.author_did = Some(adid.to_string());
        }
        if let Some(odid) = self.get_str("origin_did") {
            s.common.origin_did = Some(odid.to_string());
        }
        if let Some(df) = self.get_str("derived_from") {
            s.common.derived_from = Some(df.to_string());
        }
        if let Some(et) = self.get_str("embedding_text") {
            s.common.embedding_text = Some(et.to_string());
        }

        // proficiency aliases confidence (D3). `prof` wins over `cf` when
        // both are present and disagree (inbound-interop edge only).
        let cf = self.get_f64("confidence");
        let prof = self.get_f64("proficiency");
        match (prof, cf) {
            (Some(p), Some(c)) if (p - c).abs() > f64::EPSILON => {
                tracing::debug!(
                    proficiency = p,
                    confidence = c,
                    "skill prof != cf on deserialize; taking prof as authoritative (D3)"
                );
                s.common.confidence = p;
            }
            (Some(p), _) => s.common.confidence = p,
            (None, Some(c)) => s.common.confidence = c,
            (None, None) => {}
        }

        Ok(s)
    }

    /// Reconstruct a [`Recommendation`] (OMS 1.5 §8.12).
    ///
    /// `rec_status` is never restored: it is index-layer state rebuilt from the
    /// audit chain, and a blob that carried one would be a forged status.
    pub fn to_recommendation(&self) -> Result<Recommendation> {
        if self.grain_type != GrainType::Recommendation {
            return Err(AreevError::Validation("not a Recommendation grain".into()));
        }
        let target_ref = self
            .get_str("target_ref")
            .ok_or_else(|| AreevError::Validation("missing target_ref".into()))?
            .to_string();

        let am = self.fields.get("analyzer");
        let analyzer = Analyzer {
            id: am
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| AreevError::Validation("missing analyzer.id".into()))?
                .to_string(),
            params: am.and_then(|v| v.get("params")).cloned(),
        };

        let sm = self.fields.get("summary");
        let summary = Summary {
            template_id: sm
                .and_then(|v| v.get("template_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| AreevError::Validation("missing summary.template_id".into()))?
                .to_string(),
            args: sm
                .and_then(|v| v.get("args"))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        };

        // Rule 1: exactly one proposal. Zero is malformed; more than one is
        // ambiguous about which change was actually approved, so both are
        // rejected rather than resolved by precedence.
        let cal = self.get_str("proposal_cal").map(str::to_string);
        let edit = self.fields.get("proposal_edit").cloned();
        let data = self.fields.get("proposal_data").cloned();
        let present = cal.is_some() as u8 + edit.is_some() as u8 + data.is_some() as u8;
        if present != 1 {
            return Err(AreevError::Validation(format!(
                "a recommendation must carry exactly one proposal, found {present}"
            )));
        }
        let proposal = match (cal, edit, data) {
            (Some(c), _, _) => Proposal::Cal(c),
            (_, Some(e), _) => Proposal::Edit(e),
            (_, _, Some(d)) => Proposal::Data(d),
            _ => unreachable!("count checked above"),
        };

        let mut rec = Recommendation::new(&target_ref, analyzer, summary, proposal);
        // The stored key is authoritative — recomputing would silently rewrite
        // a key a peer derived under a different (possibly newer) rule.
        if let Some(k) = self.get_str("dedup_key") {
            rec.dedup_key = k.to_string();
        }
        rec.severity = self.get_str("severity").and_then(Severity::parse);
        rec.metric_snapshot = self.fields.get("metric_snapshot").cloned();
        rec.evidence_query = self.get_str("evidence_query").map(str::to_string);

        self.fill_common_scalars(&mut rec.common);
        Ok(rec)
    }

    /// Fill the common fields every typed reconstructor restores.
    fn fill_common_scalars(&self, common: &mut GrainCommon) {
        if let Some(c) = self.get_f64("confidence") {
            common.confidence = c;
        }
        if let Some(ns) = self.get_str("namespace") {
            common.namespace = Some(ns.to_string());
        }
        if let Some(uid) = self.get_str("user_id") {
            common.user_id = Some(uid.to_string());
        }
        if let Some(st) = self.get_str("source_type") {
            common.source_type = Some(st.to_string());
        }
        if let Some(ca) = self.get_i64("created_at") {
            common.created_at = Some(ca);
        }
        if let Some(adid) = self.get_str("author_did") {
            common.author_did = Some(adid.to_string());
        }
        if let Some(odid) = self.get_str("origin_did") {
            common.origin_did = Some(odid.to_string());
        }
        if let Some(df) = self.get_str("derived_from") {
            common.derived_from = Some(df.to_string());
        }
        if let Some(et) = self.get_str("embedding_text") {
            common.embedding_text = Some(et.to_string());
        }
        // The §6.1 common context. For a State it rides in `cctx`, because the
        // §8.3 snapshot owns `ctx`; for every other type `ctx` *is* the common
        // context. Reading the wrong one for a State would hand the caller the
        // agent's snapshot as if it were grain metadata.
        common.context = match self.fields.get("common_context") {
            Some(c) => Some(c.clone()),
            None if self.grain_type != GrainType::State => self.fields.get("context").cloned(),
            None => None,
        };
    }

    /// Reconstruct a [`State`] (OMS §8.3).
    ///
    /// `context` is the snapshot; its inner keys were never compacted, so they
    /// come back verbatim. For a State the snapshot owns the `ctx` wire key, so
    /// the §6.1 common context rides in `cctx` and is restored from there.
    pub fn to_state(&self) -> Result<State> {
        if self.grain_type != GrainType::State {
            return Err(AreevError::Validation("not a State grain".into()));
        }
        let context = self
            .fields
            .get("context")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let mut st = State::new(context);

        if let Some(arr) = self.fields.get("plan").and_then(|v| v.as_array()) {
            st.plan = Some(
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            );
        }
        if let Some(arr) = self.fields.get("history").and_then(|v| v.as_array()) {
            st.history = Some(arr.clone());
        }

        self.fill_common_scalars(&mut st.common);
        Ok(st)
    }

    /// Reconstruct a [`Workflow`] (OMS §8.4) with its full topology — nodes,
    /// edges (including conditions and cycle bounds), tool bindings and retries.
    ///
    /// Node order is load-bearing: the first element is the graph's entry point.
    /// Reconstruct a [`Trigger`] grain.
    pub fn to_trigger(&self) -> Result<Trigger> {
        if self.grain_type != GrainType::Trigger {
            return Err(AreevError::Validation("not a Trigger grain".into()));
        }
        let kind = self
            .get_str("kind")
            .and_then(TriggerKind::parse)
            .unwrap_or_default();
        let workflow = self.get_str("workflow").unwrap_or("").to_string();
        let strings = |field: &str| -> Vec<String> {
            self.fields
                .get(field)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        let mut t = Trigger::new(kind, &workflow);
        t.connector = self.get_str("connector").map(str::to_string);
        t.scope = self.get_str("scope").map(str::to_string);
        // Omitted means true — the write side omits the flag when live.
        t.enabled = self.fields.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        t.dedup_key = strings("dedup_key");
        t.interval_secs = self.get_i64("interval_secs").and_then(|v| u32::try_from(v).ok());
        t.cron = self.get_str("cron").map(str::to_string);
        t.at_ms = self.get_i64("at_ms");
        t.predicate = self.fields.get("predicate").cloned();
        t.members = self
            .fields
            .get("members")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        t.correlate = self.get_str("correlate").map(str::to_string);
        t.window_ms = self.get_i64("window_ms");
        t.concurrency = self
            .get_str("concurrency")
            .and_then(Concurrency::parse)
            .unwrap_or_default();
        t.catchup = self.get_str("catchup").and_then(Catchup::parse).unwrap_or_default();
        t.config = self.fields.get("config").cloned();
        self.fill_common_scalars(&mut t.common);
        Ok(t)
    }

    pub fn to_workflow(&self) -> Result<Workflow> {
        if self.grain_type != GrainType::Workflow {
            return Err(AreevError::Validation("not a Workflow grain".into()));
        }
        let nodes: Vec<String> = self
            .fields
            .get("nodes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut wf = Workflow::new(nodes);

        if let Some(t) = self.get_str("trigger") {
            wf.trigger = Some(t.to_string());
        }
        if let Some(arr) = self.fields.get("edges").and_then(|v| v.as_array()) {
            for e in arr {
                let (Some(src), Some(dst)) = (
                    e.get("src").and_then(|v| v.as_str()),
                    e.get("dst").and_then(|v| v.as_str()),
                ) else {
                    continue; // forward-compatible: skip malformed edges, never fail
                };
                wf.edges.push(WorkflowEdge {
                    src: src.to_string(),
                    dst: dst.to_string(),
                    cond: e
                        .get("cond")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    max_cycles: e.get("max_cycles").and_then(|v| v.as_u64()).map(|v| v as u32),
                });
            }
        }
        if let Some(obj) = self.fields.get("bindings").and_then(|v| v.as_object()) {
            for (node, hash) in obj {
                if let Some(h) = hash.as_str() {
                    wf.bindings.insert(node.clone(), h.to_string());
                }
            }
        }
        if let Some(obj) = self.fields.get("retries").and_then(|v| v.as_object()) {
            for (node, max) in obj {
                if let Some(m) = max.as_u64() {
                    wf.retries.insert(node.clone(), m as u32);
                }
            }
        }

        self.fill_common_scalars(&mut wf.common);
        Ok(wf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::serialize::serialize_grain;

    /// R-6 / AC-2c (actual behavior, NOT as the design literally states) —
    /// PIN the true Unicode-composition contract of the existing serializer so a
    /// future change to NFC placement is caught.
    ///
    /// The design doc (§R-6, AC-2c) claims two grains differing only by Unicode
    /// composition form "still hash differently (different bytes → different
    /// grains)". That is FALSE against the current code: `serialize_grain`
    /// applies `nfc_string()` to the canonical field bytes BEFORE the hash, so
    /// two composition forms of the same logical text collapse to identical
    /// canonical bytes → identical content hash → they dedup as ONE grain. This
    /// test documents the real behavior. (Flagged to architect/PM — see the
    /// tester report. PR-3's own NFC projection at the embedding/query
    /// consumption sites is unaffected and correct either way.)
    #[test]
    fn test_nfc_composition_forms_collapse_to_same_hash() {
        // Same created_at + namespace so the 9-byte header is identical and only
        // the payload bytes (the object field) could differ.
        let composed = Fact::new("u", "likes", "caf\u{00e9}").created_at(1_700_000_000_000); // U+00E9
        let decomposed = Fact::new("u", "likes", "cafe\u{0301}").created_at(1_700_000_000_000); // e + U+0301
        let (b1, h1) = serialize_grain(&composed).unwrap();
        let (b2, h2) = serialize_grain(&decomposed).unwrap();
        assert_eq!(
            h1, h2,
            "serializer NFC-normalizes field bytes pre-hash, so composition \
             variants collapse to one content address (design §R-6 claims the \
             opposite — flagged)"
        );
        assert_eq!(b1, b2, "canonical bytes are byte-identical after NFC");
    }

    #[test]
    fn test_roundtrip_fact() {
        let fact = Fact::new("john", "likes", "coffee")
            .confidence(0.95)
            .namespace("test")
            .source_type("user_explicit")
            .created_at(1768471200000);

        let (blob, _hash) = serialize_grain(&fact).unwrap();
        let deserialized = deserialize_blob(&blob).unwrap();

        assert_eq!(deserialized.grain_type, GrainType::Fact);
        assert_eq!(deserialized.get_str("subject"), Some("john"));
        assert_eq!(deserialized.get_str("relation"), Some("likes"));
        assert_eq!(deserialized.get_str("object"), Some("coffee"));
        assert_eq!(deserialized.get_f64("confidence"), Some(0.95));
        assert_eq!(deserialized.get_str("namespace"), Some("test"));
    }

    /// PR-3b [C-R BLOCKING fix] — `DeserializedGrain::embedding_text()` must
    /// reproduce EXACTLY what the live `Grain::embedding_text()` projects for
    /// ALL 11 grain types, so the backfill embeds the same text the inline write
    /// path (`write.rs`) would have. Build one grain of each type, serialize via
    /// the real write projection, deserialize, and compare round-trip.
    #[test]
    fn test_embedding_text_matches_grain_trait_all_types() {
        use crate::types::{
            Consensus, Consent, Event, Goal, Grain, Observation, Reasoning, Skill, State, Tool,
            Workflow,
        };

        /// Assert the deserialized embedding text equals the typed grain's
        /// `embedding_text()` (parity by construction). Also guards against the
        /// empty-projection trap where both sides are "" (which would pass
        /// trivially but means nothing was embedded).
        fn assert_parity<G: Grain + 'static>(g: G) {
            let expected = g.embedding_text();
            let (blob, _h) = serialize_grain(&g).unwrap();
            let dg = deserialize_blob(&blob).unwrap();
            assert_eq!(
                dg.embedding_text(),
                expected,
                "embedding_text parity failed for {:?}",
                g.grain_type()
            );
            assert!(
                !expected.trim().is_empty(),
                "test grain for {:?} produced empty embedding text",
                g.grain_type()
            );
        }

        // 1. Fact — "subject relation object".
        assert_parity(Fact::new("john", "likes", "coffee").namespace("prefs"));
        // 2. Event — content.
        assert_parity(Event::new("user discussed vacation plans").tags(["travel"]));
        // 3. State — text() reads context_data keys (label/description/title/name).
        assert_parity(State::new(
            serde_json::json!({ "label": "checkpoint-7", "extra": 42 }),
        ));
        // 4. Workflow — "trigger | n1 -> n2".
        assert_parity(Workflow::new(vec!["plan".into(), "execute".into()]).trigger("on_request"));
        // 5. Tool — "tool_name content" (current base_text drops tool_name).
        assert_parity(Tool::new("calculator").content("computed 42"));
        // 6. Observation — "observer_id observer_type subject object".
        assert_parity(
            Observation::new("agent-1", "vision")
                .subject("scene")
                .object("cat"),
        );
        // 7. Goal — description + criteria (current base_text drops criteria).
        let mut goal = Goal::new("ship trilingual recall");
        goal.criteria = Some("all 11 types embed".into());
        assert_parity(goal);
        // 8. Reasoning — conclusion (falls back to thinking_content).
        assert_parity(Reasoning::new().conclusion("therefore X holds"));
        // 9. Consensus — agreed_content.
        let mut consensus = Consensus::new();
        consensus.agreed_content = Some("ratified the plan".into());
        assert_parity(consensus);
        // 10. Consent — "subj grants/withdraws grantee".
        let mut consent = Consent::new("did:key:subjectA");
        consent.grantee_did = Some("did:key:granteeB".into());
        assert_parity(consent);
        // 11. Skill — "name: description — when_to_use [domain]".
        assert_parity(
            Skill::new("code_review", "review code for defects")
                .when_to_use("before merge")
                .domain("swe"),
        );
    }

    /// PR-3b — the enriched composed SHAPE and the explicit-override branch.
    /// Pins the exact string so a future refactor cannot silently change the
    /// `text() + [tags] + (namespace)` join.
    #[test]
    fn test_embedding_text_enrichment_and_override() {
        use crate::types::Grain;

        // (a) Enriched path: Fact base text + tags + namespace.
        let fact = Fact::new("john", "likes", "coffee")
            .namespace("prefs")
            .tags(["a", "b"]);
        let (blob, _h) = serialize_grain(&fact).unwrap();
        let dg = deserialize_blob(&blob).unwrap();
        assert_eq!(dg.embedding_text(), fact.embedding_text());
        assert_eq!(dg.embedding_text(), "john likes coffee [a, b] (prefs)");

        // (b) Explicit override wins over the enriched base text.
        let mut fact2 = Fact::new("a", "b", "c");
        fact2.common_mut().embedding_text = Some("rich custom context".to_string());
        let (blob2, _h2) = serialize_grain(&fact2).unwrap();
        let dg2 = deserialize_blob(&blob2).unwrap();
        assert_eq!(dg2.embedding_text(), "rich custom context");
        assert_eq!(dg2.embedding_text(), fact2.embedding_text());
    }

    /// PR-3b [C-R BLOCKING] — the reranker `base_text()` projection (consumed by
    /// `query::extract_grain_text`) is DECOUPLED from `embedding_text()` and must
    /// keep its current `text()`-only-via-content-field-list behavior. This pins
    /// the divergence so the embed-parity fix did NOT regress the reranker input.
    #[test]
    fn test_base_text_reranker_projection_unchanged() {
        // Tool: the reranker scans the fixed content-field list, which does NOT
        // include Tool's `tool_content` key (Tool's content serializes under
        // `cnt`/`tool_content`, not `content`), so base_text() falls back to the
        // grain-type name "tool". The embedding projection, by contrast, routes
        // through Tool::text() = "tool_name content" and includes "calculator".
        // The two MUST differ — confirms base_text() was not retargeted onto the
        // typed text().
        let tool = Tool::new("calculator").content("computed 42");
        let (blob, _h) = serialize_grain(&tool).unwrap();
        let dg = deserialize_blob(&blob).unwrap();
        assert_eq!(dg.base_text(), "tool");
        assert!(dg.embedding_text().contains("calculator"));
        assert!(dg.embedding_text().contains("computed 42"));
        assert_ne!(dg.base_text(), dg.embedding_text());

        // State: reranker falls back to the grain-type name (its content lives
        // under `context`, not a scanned top-level field) — embedding reads the
        // context label. Confirms base_text() was NOT changed to read context.
        let state = State::new(serde_json::json!({ "label": "checkpoint-7" }));
        let (sblob, _sh) = serialize_grain(&state).unwrap();
        let sdg = deserialize_blob(&sblob).unwrap();
        assert_eq!(sdg.base_text(), "state");
        assert_eq!(sdg.embedding_text(), "checkpoint-7");

        // Fact: base_text() == "subject relation object" (unchanged).
        let fact = Fact::new("john", "likes", "coffee");
        let (fblob, _fh) = serialize_grain(&fact).unwrap();
        let fdg = deserialize_blob(&fblob).unwrap();
        assert_eq!(fdg.base_text(), "john likes coffee");
    }

    /// HPL Phase 4.1 — `executor_kind` survives the .mg blob round-trip.
    /// `Client` is the non-default variant, so this pins the compact
    /// key `exk` + lowercase wire string in both serialize + deserialize.
    #[test]
    fn test_tool_executor_kind_round_trip() {
        use crate::types::executor_kind::ExecutorKind;
        let tool = Tool::new("slack.post").executor_kind(ExecutorKind::Client);
        let (blob, _hash) = serialize_grain(&tool).unwrap();
        let dg = deserialize_blob(&blob).unwrap();
        assert_eq!(dg.get_str("executor_kind"), Some("client"));
        let back = dg.to_tool().unwrap();
        assert_eq!(back.executor_kind, Some(ExecutorKind::Client));
    }

    /// Legacy grains without `executor_kind` default to `None` on
    /// deserialize (dispatch treats that as Host). The default
    /// Host variant is also skipped at serialize time, so a grain
    /// authored with `ExecutorKind::Host` still has no `exk` key in
    /// the blob — legacy callers see identical bytes.
    #[test]
    fn test_action_executor_kind_default_host_omits_field() {
        use crate::types::executor_kind::ExecutorKind;
        let tool = Tool::new("slack.post").executor_kind(ExecutorKind::Host);
        let (blob, _hash) = serialize_grain(&tool).unwrap();
        let dg = deserialize_blob(&blob).unwrap();
        assert_eq!(dg.get_str("executor_kind"), None);
        let back = dg.to_tool().unwrap();
        assert_eq!(back.executor_kind, None);
    }

    #[test]
    fn test_get_bool_roundtrip_tool() {
        let tool = Tool::new("calculator")
            .is_error(true)
            .duration_ms(250)
            .error("divide by zero");

        let (blob, _hash) = serialize_grain(&tool).unwrap();
        let deserialized = deserialize_blob(&blob).unwrap();

        assert_eq!(deserialized.grain_type, GrainType::Tool);
        assert_eq!(deserialized.get_bool("is_error"), Some(true));
        assert_eq!(deserialized.get_u64("duration_ms"), Some(250));
        assert_eq!(deserialized.get_str("error"), Some("divide by zero"));
        assert_eq!(deserialized.get_str("tool_name"), Some("calculator"));
    }

    #[test]
    fn test_get_bool_returns_none_for_missing_field() {
        let fact = Fact::new("john", "likes", "coffee");
        let (blob, _hash) = serialize_grain(&fact).unwrap();
        let deserialized = deserialize_blob(&blob).unwrap();

        // Facts don't have is_error — should return None, not panic
        assert_eq!(deserialized.get_bool("is_error"), None);
        assert_eq!(deserialized.get_u64("duration_ms"), None);
    }

    #[test]
    fn test_get_bool_false_roundtrip() {
        let tool = Tool::new("browser").is_error(false);
        let (blob, _hash) = serialize_grain(&tool).unwrap();
        let deserialized = deserialize_blob(&blob).unwrap();

        assert_eq!(deserialized.get_bool("is_error"), Some(false));
    }
}
