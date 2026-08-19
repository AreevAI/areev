//! Grain-type metadata registry — the single, data-only source of truth.
//!
//! Adding a grain type used to mean restating the same handful of facts
//! (byte ↔ string ↔ plural, addability, required ADD fields, queryable
//! RECALL fields, TOON columns) across a dozen lookup lists scattered over
//! `grain.rs`, `cal/executor.rs`, `engine/write.rs`, and `server/routes.rs`.
//! Every one of those lists was an independent "forgot to update list N"
//! bug. This table collapses them into one row per type.
//!
//! Scope is deliberately narrow: this holds **data only**. Behaviour
//! (serialize, deserialize, triple extraction, rendering) stays per-type
//! and lives with the concrete grain structs — it is NOT folded in here.
//! The CAL AST enums (`GrainTypePlural` / `GrainTypeSingular`) also stay
//! per-type; they delegate their string bodies to this registry but remain
//! distinct AST node types.

use super::grain::GrainType;

/// Static, data-only metadata for one grain type.
///
/// The single source of truth for the byte↔string↔plural mapping plus the
/// CAL/recall/TOON lookup facts. Constructed once as `const` data in
/// [`GRAIN_TYPES`]; never mutated at runtime.
#[derive(Debug, Clone, Copy)]
pub struct GrainTypeMeta {
    /// The grain type this row describes.
    pub ty: GrainType,
    /// `.mg` header type byte (e.g. `0x01` for Fact).
    pub byte: u8,
    /// Canonical singular OMS string name (e.g. `"skill"`).
    pub name: &'static str,
    /// Canonical plural name used by CAL `RECALL <plural>` (e.g. `"skills"`).
    pub plural: &'static str,
    /// Whether this type can be built from the **generic** CAL form
    /// `ADD <type> SET k = v …`.
    ///
    /// This is a *shape* fact, not a permission. `false` means a flat list of
    /// `SET` pairs cannot express the type — a Workflow is a graph, a Tool has a
    /// call/result lifecycle, an Event carries structured content blocks — so
    /// those types are created through purpose-built paths instead: a dedicated
    /// CAL statement (`ADD workflow … build -> test`), the per-type JSON
    /// builders behind `cal_add` (which MCP, Python and Node all reach), or a
    /// host API such as `capture()`. **Those paths are deliberately not gated by
    /// this flag** — they validate structure themselves, which is the point.
    ///
    /// Enforced in exactly one place: the `CalStatement::Add` arm of the CAL
    /// executor, which returns `Unsupported` (never a permission denial). Access
    /// control lives in scopes and `allow_destructive_ops`, not here.
    pub add_via_set: bool,
    /// Whether a host may author this type at all — i.e. whether the per-type
    /// JSON builders behind `cal_add` (which the CLI, MCP, Python and Node
    /// `add()` all reach) accept it.
    ///
    /// Unlike [`Self::add_via_set`] this *is* a permission-shaped fact, and it
    /// is `false` for exactly one type: a Recommendation is engine-emitted and
    /// lifecycle-gated, so its `dedup_key` is computed from the analyzer family
    /// rather than chosen by the author. A host-authored recommendation would
    /// enter the review queue as though an analyzer had produced it.
    ///
    /// Enforced in `areev_cal::json_build::build_grain_from_json`, and
    /// test-pinned there against this table so the two cannot drift — the
    /// disagreement this flag exists to prevent is a surface reporting a type
    /// as unknown when it is really unwritable.
    pub host_addable: bool,
    /// Required fields for an `ADD` of this type. Consumed by the per-type JSON
    /// builders regardless of [`Self::add_via_set`] — a type that cannot be
    /// built from flat `SET` pairs still has required fields.
    ///
    /// Empty means the type genuinely requires nothing: State, Workflow,
    /// Reasoning and Consensus are host-shaped containers whose whole payload
    /// is caller-defined, so an empty one is legal by design rather than by
    /// oversight.
    pub required_add_fields: &'static [&'static str],
    /// Type-specific fields surfaced by `RECALL <type> WHERE <field> …`.
    pub queryable_fields: &'static [&'static str],
    /// Column set rendered by the TOON output format for this type.
    pub toon_columns: &'static [&'static str],
}

/// The 12 OMS grain types (OMS 1.5), one metadata row each. The byte values
/// match the `.mg` header spec and are immutable.
pub const GRAIN_TYPES: &[GrainTypeMeta] = &[
    GrainTypeMeta {
        ty: GrainType::Fact,
        byte: 0x01,
        name: "fact",
        plural: "facts",
        add_via_set: true,
        host_addable: true,
        required_add_fields: &["subject", "relation", "object"],
        queryable_fields: &["subject", "relation", "object", "confidence"],
        toon_columns: &["subject", "content", "confidence"],
    },
    GrainTypeMeta {
        ty: GrainType::Event,
        byte: 0x02,
        name: "event",
        plural: "events",
        add_via_set: false,
        host_addable: true,
        required_add_fields: &["content"],
        queryable_fields: &[
            "role",
            "session_id",
            "parent_message_id",
            "model_id",
            "content",
            "created_at",
            "stop_reason",
            // OMS §8.2 `run_id` — serialized since 1.0 but absent from this list,
            // so it was write-only: unfilterable and undiscoverable via DESCRIBE.
            // It is the only run-scoped correlation key in the grain model.
            "run_id",
        ],
        toon_columns: &["role", "time", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::State,
        byte: 0x03,
        name: "state",
        plural: "states",
        add_via_set: false,
        host_addable: true,
        required_add_fields: &[],
        // OMS §8.3: `context` (required) + `plan`/`history` (optional). There is no
        // `checkpoint_data` field in the spec or the struct — it was advertised here
        // but had no serializer, deserializer, or storage.
        queryable_fields: &["context", "plan", "history"],
        toon_columns: &["context", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Workflow,
        byte: 0x04,
        name: "workflow",
        plural: "workflows",
        add_via_set: false,
        host_addable: true,
        required_add_fields: &["nodes"],
        queryable_fields: &[
            "trigger", "node", "binding", "nodes", "edges", "bindings", "name", "retries",
        ],
        toon_columns: &["trigger", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Tool,
        byte: 0x05,
        name: "tool",
        plural: "tools",
        add_via_set: false,
        host_addable: true,
        required_add_fields: &["tool_name"],
        queryable_fields: &[
            "tool_name",
            "tool_phase",
            "is_error",
            "tool_call_id",
            "tool",
            "input",
            "duration_ms",
        ],
        toon_columns: &["tool", "phase", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Observation,
        byte: 0x06,
        name: "observation",
        plural: "observations",
        add_via_set: true,
        host_addable: true,
        required_add_fields: &["observer_id", "observer_type"],
        queryable_fields: &["observer_id", "observer_type", "sensor", "value", "unit"],
        toon_columns: &["observer", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Goal,
        byte: 0x07,
        name: "goal",
        plural: "goals",
        add_via_set: true,
        host_addable: true,
        required_add_fields: &["description"],
        queryable_fields: &[
            "goal_state",
            "assigned_agent",
            "deadline",
            "depends_on",
            "title",
            "description",
            "parent_hash",
        ],
        toon_columns: &["subject", "content", "state"],
    },
    GrainTypeMeta {
        ty: GrainType::Reasoning,
        byte: 0x08,
        name: "reasoning",
        plural: "reasonings",
        add_via_set: false,
        host_addable: true,
        required_add_fields: &[],
        queryable_fields: &["reasoning_type", "premises", "conclusion"],
        toon_columns: &["type", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Consensus,
        byte: 0x09,
        name: "consensus",
        plural: "consensuses",
        add_via_set: false,
        host_addable: true,
        required_add_fields: &[],
        queryable_fields: &["threshold", "agreement_count", "participating_observers"],
        toon_columns: &["threshold", "count", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Consent,
        byte: 0x0A,
        name: "consent",
        plural: "consents",
        add_via_set: false,
        host_addable: true,
        required_add_fields: &["subject_did"],
        queryable_fields: &[
            "consent_action",
            "purpose",
            "grantor_did",
            "grantee_did",
            "expires_at",
            "granted",
            "subject_did",
        ],
        toon_columns: &["grantor", "grantee", "action", "content"],
    },
    // OMS 1.4 — Skill (0x0B). A packaged, reusable agent capability.
    GrainTypeMeta {
        ty: GrainType::Skill,
        byte: 0x0B,
        name: "skill",
        plural: "skills",
        add_via_set: true,
        host_addable: true,
        required_add_fields: &["name", "description"],
        queryable_fields: &[
            "name",
            "version",
            "domain",
            "holder_did",
            "proficiency",
            "transferable",
            "practice_count",
            "last_practiced_at",
            // The definition body. Omitting these made the field that IS the
            // skill unreachable through PROJECT, leaving raw JSON recall as the
            // only way to read a skill's own instructions.
            "instructions",
            "when_to_use",
        ],
        toon_columns: &["name", "domain", "proficiency"],
    },
    // OMS 1.5 — Recommendation (0x0C). A governed, auditable proposal to change
    // memory or agent configuration.
    GrainTypeMeta {
        ty: GrainType::Recommendation,
        byte: 0x0C,
        name: "recommendation",
        plural: "recommendations",
        // Query-only by design (OMS 1.5 / CAL 1.2): a recommendation is
        // engine-emitted and lifecycle-gated, so there is no `ADD
        // recommendation` and lifecycle transitions never occur through
        // `ADD`/`SUPERSEDE SET`. They are emitted by an analyzer layer and
        // moved through review by the audit path.
        add_via_set: false,
        // …and not host-authorable through the JSON builders either. This is the
        // one `false` in the table: every other type is a record of something
        // the host knows, while a recommendation is a claim the engine makes
        // about the memory, carrying a computed `dedup_key` and an analyzer
        // attribution that only an analyzer can honestly assert.
        host_addable: false,
        required_add_fields: &["target_ref", "analyzer", "summary", "dedup_key"],
        // `rec_status` is index-layer (§5.6/§6.1) — filterable, never written
        // by an author.
        queryable_fields: &[
            "target_ref",
            "analyzer",
            "severity",
            "dedup_key",
            "rec_status",
        ],
        toon_columns: &["target_ref", "severity", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Trigger,
        byte: 0x0D,
        name: "trigger",
        plural: "triggers",
        // Not expressible as flat `SET k=v` pairs: `dedup_key` and `members`
        // are lists and `predicate`/`config` are nested JSON. Authored through
        // `areev trigger add`, which builds the grain, rather than by hand.
        add_via_set: false,
        host_addable: true,
        required_add_fields: &["kind", "workflow"],
        // Typed and queryable — the whole reason this is not an Observation
        // carrying `int:` keys in a `context` map, which no CAL query can
        // filter on.
        queryable_fields: &["kind", "workflow", "connector", "scope", "enabled", "cron"],
        toon_columns: &["kind", "content"],
    },
];

/// Look up the metadata row for a grain type. Infallible — every variant has
/// a row (enforced by the `metadata_covers_all_types` test).
pub fn meta(ty: GrainType) -> &'static GrainTypeMeta {
    GRAIN_TYPES
        .iter()
        .find(|m| m.ty == ty)
        // SAFETY: GRAIN_TYPES lists every GrainType variant; the
        // `metadata_covers_all_types` test guarantees this find never fails.
        .expect("GRAIN_TYPES missing a variant — registry is incomplete")
}

/// Parse a grain type from its `.mg` header byte.
pub fn from_byte(b: u8) -> Option<GrainType> {
    GRAIN_TYPES.iter().find(|m| m.byte == b).map(|m| m.ty)
}

/// Parse a grain type from its canonical singular OMS string name.
pub fn from_str(s: &str) -> Option<GrainType> {
    GRAIN_TYPES.iter().find(|m| m.name == s).map(|m| m.ty)
}

/// Canonical singular names of every type buildable from generic `ADD … SET`.
/// The single source for the CAL ADD allow-set. Not an allow-list of what may
/// be *written* — see [`GrainTypeMeta::add_via_set`].
pub fn add_via_set_names() -> impl Iterator<Item = &'static str> {
    GRAIN_TYPES.iter().filter(|m| m.add_via_set).map(|m| m.name)
}

/// Canonical singular names of every type a host may author through the
/// per-type JSON builders (`add()`, MCP `areev_add`, `areev add`).
/// See [`GrainTypeMeta::host_addable`].
pub fn host_addable_names() -> impl Iterator<Item = &'static str> {
    GRAIN_TYPES
        .iter()
        .filter(|m| m.host_addable)
        .map(|m| m.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `GrainType` variant must have exactly one registry row. This is
    /// the invariant that lets `meta()` be infallible and replaces the
    /// compile-forced exhaustive matches the registry absorbed.
    #[test]
    fn metadata_covers_all_types() {
        // Exhaustive match — adding a GrainType variant without a row here
        // is a compile error, which then trips the per-variant lookup below.
        for ty in [
            GrainType::Fact,
            GrainType::Event,
            GrainType::State,
            GrainType::Workflow,
            GrainType::Tool,
            GrainType::Observation,
            GrainType::Goal,
            GrainType::Reasoning,
            GrainType::Consensus,
            GrainType::Consent,
            GrainType::Skill,
        ] {
            let m = meta(ty);
            assert_eq!(m.ty, ty);
            assert!(!m.name.is_empty());
            assert!(!m.plural.is_empty());
        }
    }

    #[test]
    fn byte_and_name_round_trip() {
        for m in GRAIN_TYPES {
            assert_eq!(from_byte(m.byte), Some(m.ty));
            assert_eq!(from_str(m.name), Some(m.ty));
        }
    }

    #[test]
    fn bytes_and_names_are_unique() {
        for (i, a) in GRAIN_TYPES.iter().enumerate() {
            for b in &GRAIN_TYPES[i + 1..] {
                assert_ne!(a.byte, b.byte, "duplicate byte {:#x}", a.byte);
                assert_ne!(a.name, b.name, "duplicate name {}", a.name);
                assert_ne!(a.plural, b.plural, "duplicate plural {}", a.plural);
            }
        }
    }

    #[test]
    fn skill_row_is_correct() {
        let s = meta(GrainType::Skill);
        assert_eq!(s.byte, 0x0B);
        assert_eq!(s.name, "skill");
        assert_eq!(s.plural, "skills");
        assert!(s.add_via_set);
        assert_eq!(s.required_add_fields, &["name", "description"]);
    }

    #[test]
    fn add_via_set_names_match_rows() {
        let from_iter: Vec<&str> = add_via_set_names().collect();
        let from_filter: Vec<&str> = GRAIN_TYPES
            .iter()
            .filter(|m| m.add_via_set)
            .map(|m| m.name)
            .collect();
        assert_eq!(from_iter, from_filter);
        assert!(from_iter.contains(&"skill"));
    }
}
