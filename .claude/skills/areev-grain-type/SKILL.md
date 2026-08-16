---
name: areev-grain-type
description: Checklist for adding a new grain type or changing an existing one's fields in areev-core — registry row, serialize/deserialize downcast arms, compact field-map keys, omit-default rules, and the conformance/roundtrip tests. Use whenever you touch anything under crates/areev-core/src/types/ or add a grain type byte, because serialization dispatches on downcast chains and a registry row alone silently half-works.
---

# Adding or changing a grain type

There are **12 grain types** (Fact 0x01 … Recommendation 0x0C). Serialization dispatches
on `downcast_ref` chains, so a `registry.rs` row is **necessary but not
sufficient** — miss an arm and the type serializes to garbage or fails to
reconstruct, with tests catching only some of it.

## First: which case are you in?

- **Adding a new type** (new type byte, e.g. 0x0D) is **additive** — it does not
  change any existing grain's content address. Safe to do without a spec gate,
  but coordinate with OMS: the type byte and name are conformance surface.
- **Adding/renaming a field on an existing type**, or changing its default/omit
  behavior, **changes that type's content address for future writes** and can
  break byte-identical replication of legacy blobs. This is a
  canonical-serialization change → **STOP** and confirm intent (see the
  `areev-invariants` skill and `crates/areev-core/CLAUDE.md`).

## Touch in this order (all mandatory)

1. **`types/<name>.rs`** — the grain struct (+ `types/mod.rs` module decl).
   Follow an existing type; implement the `Grain` trait / `GrainCommon`.
2. **`types/registry.rs`** — the `GRAIN_TYPES` metadata row (byte, name,
   plural, add_via_set, queryable). A test forces coverage of every type here.
3. **`format/serialize.rs`** — the `add_type_specific_fields` downcast arm.
4. **`format/deserialize.rs`** — the reconstruction arm + the typed
   reconstructor (`to_<type>()`), and update `embedding_text()` / `base_text()`
   if the new fields are searchable (they are deliberately decoupled — both are
   pinned by tests).
5. **`format/field_map.rs`** — compact short keys for every new field (writers
   MUST emit short forms; a collision test exists). Respect the uncompacted
   exceptions (`content`, `nodes`, `edges`, `trigger`, `retries`) and the
   `Tool.content → cnt` rename that avoids colliding with `Event.content`.

## Canonical-serialization rules the new fields must obey

- **NFC-normalize** every string before hashing (`serialize.rs` `nfc_string`) —
  **keys as well as values** (a map key stored un-normalized gives composition
  variants distinct content addresses).
- **Sorted map keys** — build as `BTreeMap`, emit sorted.
- **Omit-when-default** — `None`/empty omitted; default enum variants omitted so
  legacy blobs stay byte-identical.
- Timestamps: epoch **ms** in the payload, epoch **sec** in the header.
- **Anything you write must read back** (the serialize ⇒ deserialize symmetry
  invariant). If a new field holds a float, reject non-finite values on write
  (the reader refuses NaN/±Inf); if it holds nested user JSON, store keys
  verbatim and do **not** run them through `expand_field` on read (that rewrites
  a user key colliding with an OMS short code, e.g. `"o"`→`"object"`); integers
  above `i64::MAX` stay integers, not lossy floats. See the "serialize ⇒
  deserialize symmetry" axis in [[areev-testing]] and add a round-trip test.

## Verify

```bash
cargo test -p areev-core
cargo test -p areev-core --test oms_conformance   # §21 vectors + per-type roundtrip determinism
```

- `tests/oms_conformance.rs` reconstructs a byte-exact vector (address
  `3288d0d4…`) — if it moves, you changed the wire format. `tests/proptest_roundtrip.rs`
  fuzzes roundtrip. Extend both with the new type.
- If you added an error variant, keep `error_code_tests` green (codes are
  append-only — see `ERROR_CODES.md`).
- Fuzz the deserializer if you touched it: `cargo +nightly fuzz run deserialize_blob`.
- Then run the **areev-invariants** gate before committing.

Gotcha: the `signing` feature is dormant scaffolding — don't try to build a
crypto module for a signed grain. `Skill` has no `proficiency` field (it aliases
`common.confidence`).
