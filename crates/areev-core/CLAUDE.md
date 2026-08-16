# areev-core

OMS grain model + the `.mg` binary format: canonical serialization, content
addressing, the 12 grain types, and tool-schema rendering. Storage-agnostic —
depends on no other workspace crate; everything else depends on it. OMS
conformance is the compatibility contract with other implementations.

## The `.mg` blob

`blob = 9-byte header ++ canonical MessagePack payload`.
Content address = `SHA-256(entire blob, header included)` → `Hash([u8;32])`.

Header (`src/format/header.rs`): `version(0x01) | flags | grain_type_byte |
ns_hash(u16 BE) | created_at_sec(u32 BE)`. `ns_hash` = first 2 bytes of
SHA-256(namespace). Flags bits: signed 0x01, encrypted 0x02, compressed 0x04,
has_content_refs 0x08, has_embedding_refs 0x10, ai_generated 0x20, bits 6–7
sensitivity. Byte-exact header vector pinned in `header.rs` tests
(`01 00 01 a4 d2 …`).

## Canonical serialization invariants — DO NOT BREAK

Any change here silently changes content addresses of every grain ever written
and breaks OMS §21 conformance. Treat as frozen unless the spec moves:

- **NFC-normalize every string before hashing** (`serialize.rs` `nfc_string`).
  Unicode composition variants deliberately collapse to one content hash.
- **Sorted map keys** — maps are built as `BTreeMap`, emitted in sorted order.
- **Compact keys mandatory** — writers MUST emit the short forms in
  `field_map.rs`. Exceptions that stay uncompacted: `content`, `nodes`,
  `edges`, `trigger`, `retries`. Tool's content compacts to `cnt` to avoid
  colliding with Event `content`.
- **Omit-when-default** — `None`/empty fields omitted; `ToolKind::Execution`
  and `ExecutorKind::Axtion` omitted, to keep legacy blobs byte-identical.
- Timestamps: epoch **ms** in the payload, epoch **sec** in the header.
- Signed grains: the content hash is computed over the *inner* blob (with the
  signed flag set), not over the COSE envelope.

## Module map

- `error.rs` — `AreevError`, `Result<T>`, `Hash` newtype (hex display/serde).
- `format/header.rs` — `MgHeader`, flag bits, `content_address()`.
- `format/field_map.rs` — long↔short key tables.
- `format/serialize.rs` — `serialize_grain()` → `(blob, Hash)`.
- `format/deserialize.rs` — `deserialize_blob()` → `DeserializedGrain`; typed
  reconstructors (`to_fact`/`to_event`/…), `embedding_text()`, `base_text()`.
- `format/tool_schema/` — render Tool-definition grains to 9 provider formats.
- `types/grain.rs` — `Grain` trait, `GrainCommon`, `GrainType`, `GrainData`.
- `types/registry.rs` — `GRAIN_TYPES` table: **source of truth** for
  byte/name/plural/add_via_set/queryable per type.
- `types/<type>.rs` — the 12 grain structs: Fact 0x01, Event 0x02, State 0x03,
  Workflow 0x04, Tool 0x05, Observation 0x06, Goal 0x07, Reasoning 0x08,
  Consensus 0x09, Consent 0x0A, Skill 0x0B,
  Recommendation 0x0C.

## Adding / changing a grain type

A registry row is necessary but not sufficient. Serialization dispatches on
`downcast_ref` chains, so you must also touch:
1. `types/registry.rs` — the metadata row (a test forces coverage of all types).
2. `format/serialize.rs` — `add_type_specific_fields` downcast arm.
3. `format/deserialize.rs` — reconstruction arm.
4. `field_map.rs` — compact keys for new fields (collision test exists).

## tool_schema/

9 `ProviderKind`s: openai-tools, openai-responses, anthropic-tools,
gemini-tools, mcp-tools (JSON) + hermes, llama31, markdown-tools, sml-tools
(text). Adding a format: new variant + `parse`/`as_str`/`ALL`/`is_text`, an
adapter file with `render(&Tool)`, wired into `render_json`/`render_text`.
Outputs must be deterministic. Text adapters must sanitize via `escape.rs`.
`parse.rs` is the inverse for 5 formats (ReDoS-guarded regexes).

## Tests

`cargo test -p areev-core`. `tests/oms_conformance.rs` = OMS §21 vectors
(Vector 1 byte-exact, content address `3288d0d4…` reproduced) + roundtrip
determinism per type. Grains are built inline — no fixture files. Heavy inline
`#[cfg(test)]` suites in serialize/deserialize/field_map/registry.

## Gotchas

- The `signing` feature is referenced in code (`serialize_grain_signed`,
  `crate::crypto::signing`, `coset`) but there is no crypto module, dep, or
  feature declaration — dormant scaffolding for a future `areev-crypto`.
  Don't try to build it.
- Deserialize is forward-compatible: unknown enum wire strings are ignored,
  not errors.
- `base_text()` (reranker input) ≠ `embedding_text()` (embedder input) —
  deliberately decoupled, pinned by tests.
- Skill has no `proficiency` field — it aliases `common.confidence`.
- Naming quirk: the Tool grain is called "action"/"axtion"
  in older comments and field names (`axtion_uri`).
