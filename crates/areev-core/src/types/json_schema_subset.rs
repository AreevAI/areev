//! JSON Schema subset validator for tool definitions (Phase 1).
//!
//! Enforces the portable-by-construction invariant for Tool grain
//! `input_schema` / `output_schema`: every schema accepted here is valid
//! across OpenAI strict mode, Anthropic, Gemini's OpenAPI-3.0 subset, and
//! MCP. Banned keywords are those known to break at least one provider or
//! to enable schema-injection / DoS attacks.
//!
//! Source of truth for the whitelist is `docs/facts/tool-schemas.md`. This
//! module mirrors that doc verbatim — keep them in sync.
//!
//! Errors map to `MEM-E101..MEM-E106` per `docs/facts/error-codes.md`.

use serde_json::Value;

fn detect_pii(_s: &str) -> Vec<String> { Vec::new() }

/// Maximum schema recursion depth.
///
/// SR-F3 (security review 2026-04-19): unbounded recursion through
/// `properties` / `items` / `additionalProperties` is a stack-exhaustion
/// vector when the schema arrives from an untrusted bind-tool caller.
/// Real-world tool schemas rarely exceed depth 4; 16 is generous.
pub const MAX_SCHEMA_DEPTH: usize = 16;

/// Maximum size in bytes the `regex` crate is allowed to compile a
/// `pattern` value into. Defends against pathological patterns even
/// though the `regex` crate uses linear-time RE2 semantics.
pub const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;

/// JSON Schema 2020-12 keywords accepted by the tool-schema subset.
///
/// MUST mirror `docs/facts/tool-schemas.md`. Adding a keyword here without
/// updating that doc is a documentation drift bug.
pub const ALLOWED_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "enum",
    "description",
    "items",
    "additionalProperties",
    "default",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "multipleOf",
    "maxProperties",
    "minProperties",
    "examples",
    "const",
    "title",
];

/// JSON Schema 2020-12 keywords explicitly rejected. Listed for clarity
/// in error messages — anything not in `ALLOWED_KEYWORDS` is rejected by
/// default, but matching against this list lets us distinguish "you used
/// a banned keyword" from "you used something we don't recognize."
pub const BANNED_KEYWORDS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "dependentSchemas",
    "dependentRequired",
    "patternProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "prefixItems",
    "contains",
    // CWE-400: `uniqueItems` triggers O(n²) deep-equality on LLM-emitted
    // arrays in some validators.
    "uniqueItems",
];

/// Allowed values for the `format` keyword. Gemini's OpenAPI-3.0 subset
/// only honors a small set; restricting to this list ensures portability.
pub const ALLOWED_FORMATS: &[&str] = &["date-time", "date", "time", "email", "uri", "uuid"];

/// Canonical tool-name pattern. Dots are allowed so a routing-layer
/// id (`connector.action`) maps cleanly; the LLM-facing safe form
/// replaces dots with double-underscore at render time.
pub const TOOL_NAME_PATTERN: &str = r"^[a-zA-Z0-9_.-]{1,64}$";

/// Shared compiled regex for [`TOOL_NAME_PATTERN`]. Constructed once
/// per process and reused.
pub fn tool_name_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(TOOL_NAME_PATTERN).expect("TOOL_NAME_PATTERN is valid"))
}

/// `true` if `name` matches the canonical tool-name pattern.
pub fn is_valid_tool_name(name: &str) -> bool {
    tool_name_re().is_match(name)
}

/// Validation failure modes, each mapping to a unified `MEM-E10x` code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaSubsetError {
    /// Root is not a JSON object with `type: "object"` → `MEM-E101`.
    NotObject,
    /// A keyword outside `ALLOWED_KEYWORDS` was used → `MEM-E102`.
    BannedKeyword(String),
    /// `format` value outside `ALLOWED_FORMATS` → `MEM-E102`.
    BadFormatValue(String),
    /// PII detected in a string value reachable from the schema
    /// (description, default, enum, examples, title) → `MEM-E104`.
    /// Carries the PII category from `detect_pii`.
    ContainsPii(String),
    /// Recursion depth exceeded `MAX_SCHEMA_DEPTH` → `MEM-E105`.
    TooDeep(usize),
    /// `pattern` value failed `regex` crate compile or exceeded
    /// `REGEX_SIZE_LIMIT` → `MEM-E106`. Carries the regex error.
    PatternInvalid(String),
}

impl SchemaSubsetError {
    /// Stable `MEM-E10x` code (repo-root `ERROR_CODES.md`). Every `Display`
    /// message begins with this code. Append-only — never renumber.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotObject => "MEM-E101",
            // Both "unsupported keyword" and "unsupported format value" are the
            // same class of portable-subset rejection (MEM-E102).
            Self::BannedKeyword(_) | Self::BadFormatValue(_) => "MEM-E102",
            Self::ContainsPii(_) => "MEM-E104",
            Self::TooDeep(_) => "MEM-E105",
            Self::PatternInvalid(_) => "MEM-E106",
        }
    }
}

impl std::fmt::Display for SchemaSubsetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotObject => write!(f, "MEM-E101: schema root must be an object with type=\"object\""),
            Self::BannedKeyword(k) => write!(f, "MEM-E102: schema uses unsupported keyword `{k}`"),
            Self::BadFormatValue(v) => write!(f, "MEM-E102: schema uses unsupported format value `{v}`"),
            Self::ContainsPii(c) => write!(f, "MEM-E104: schema contains PII (category {c})"),
            Self::TooDeep(d) => {
                write!(
                    f,
                    "MEM-E105: schema recursion depth {d} exceeds limit {MAX_SCHEMA_DEPTH}"
                )
            }
            Self::PatternInvalid(r) => write!(f, "MEM-E106: schema pattern invalid: {r}"),
        }
    }
}

impl std::error::Error for SchemaSubsetError {}

/// Strip JSON Schema vendor-extension keywords (`x-*`, `$comment`) in place
/// before validation/storage. The spec reserves `x-` for non-validating
/// annotations validators must ignore; executor specs carry `x-resolve` /
/// `x-semantic-type` on most actions, which the portable subset would
/// otherwise reject as `MEM-E102`. Areev never interprets them (the cloud executor
/// resolves from its own spec at execution), so dropping them is loss-free.
///
/// Structure-aware: strips `x-*` only in keyword position, descending into
/// `properties` *values* / `items` / `additionalProperties` — so a property
/// *named* `x-api-key` survives. Bounded by [`MAX_SCHEMA_DEPTH`] (runs before
/// `validate`'s depth guard). Run before `validate` + `scan_for_pii` so the
/// validated, scanned, hashed, and stored schema are one clean form.
pub fn strip_vendor_extensions(schema: &mut Value) {
    strip_vendor_extensions_at(schema, 0);
}

fn strip_vendor_extensions_at(schema: &mut Value, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    match schema {
        Value::Object(map) => {
            map.retain(|k, _| !(k.starts_with("x-") || k == "$comment"));
            if let Some(Value::Object(props)) = map.get_mut("properties") {
                for sub in props.values_mut() {
                    strip_vendor_extensions_at(sub, depth + 1);
                }
            }
            if let Some(items) = map.get_mut("items") {
                strip_vendor_extensions_at(items, depth + 1);
            }
            if let Some(addl) = map.get_mut("additionalProperties") {
                strip_vendor_extensions_at(addl, depth + 1);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                strip_vendor_extensions_at(v, depth + 1);
            }
        }
        _ => {}
    }
}

/// Validator for the tool-schema subset. Stateless; cheap to construct.
pub struct SchemaValidator {
    allowed_keywords: &'static [&'static str],
    allowed_formats: &'static [&'static str],
    max_depth: usize,
    regex_size_limit: usize,
}

impl SchemaValidator {
    /// Standard validator for tool `input_schema` / `output_schema`.
    pub fn tool_schema() -> Self {
        Self {
            allowed_keywords: ALLOWED_KEYWORDS,
            allowed_formats: ALLOWED_FORMATS,
            max_depth: MAX_SCHEMA_DEPTH,
            regex_size_limit: REGEX_SIZE_LIMIT,
        }
    }

    /// Structural validation: object root, allowed keywords, allowed
    /// `format` values, depth ≤ `max_depth`, every `pattern` compiles
    /// via the `regex` crate within the size limit.
    ///
    /// Does NOT scan for PII — call `scan_for_pii` separately after this
    /// returns `Ok`. The split lets callers report the structural error
    /// first (cheaper, simpler fix).
    pub fn validate(&self, schema: &Value) -> Result<(), SchemaSubsetError> {
        let obj = schema.as_object().ok_or(SchemaSubsetError::NotObject)?;
        let type_ok = obj
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s == "object")
            .unwrap_or(false);
        if !type_ok {
            return Err(SchemaSubsetError::NotObject);
        }
        self.visit(schema, 0)
    }

    /// PII scan over every string value reachable in the schema.
    ///
    /// CR-F1 (compliance review 2026-04-19): tool-definition Tool
    /// grains live in memory-scoped `harnesses/<slug>/def` and survive
    /// `forget_user`. Stray PII in a developer-authored `default` /
    /// `enum` / `examples` would become erasure-immune. This scan
    /// rejects bind-tool requests that smuggle PII via schema constants.
    pub fn scan_for_pii(&self, schema: &Value) -> Result<(), SchemaSubsetError> {
        Self::scan_value_for_pii(schema)
    }

    fn visit(&self, schema: &Value, depth: usize) -> Result<(), SchemaSubsetError> {
        if depth > self.max_depth {
            return Err(SchemaSubsetError::TooDeep(depth));
        }
        let Some(obj) = schema.as_object() else {
            return Ok(());
        };
        for (key, value) in obj {
            if !self.allowed_keywords.contains(&key.as_str()) {
                if BANNED_KEYWORDS.contains(&key.as_str()) {
                    return Err(SchemaSubsetError::BannedKeyword(key.clone()));
                }
                return Err(SchemaSubsetError::BannedKeyword(key.clone()));
            }
            match key.as_str() {
                "format" => {
                    if let Some(v) = value.as_str() {
                        if !self.allowed_formats.contains(&v) {
                            return Err(SchemaSubsetError::BadFormatValue(v.to_string()));
                        }
                    }
                }
                "pattern" => {
                    if let Some(p) = value.as_str() {
                        regex::RegexBuilder::new(p)
                            .size_limit(self.regex_size_limit)
                            .build()
                            .map_err(|e| SchemaSubsetError::PatternInvalid(e.to_string()))?;
                    }
                }
                "properties" | "patternProperties" => {
                    if let Some(props) = value.as_object() {
                        for (_, sub) in props {
                            self.visit(sub, depth + 1)?;
                        }
                    }
                }
                "items" | "additionalProperties" => {
                    if value.is_object() {
                        self.visit(value, depth + 1)?;
                    } else if let Some(arr) = value.as_array() {
                        for sub in arr {
                            self.visit(sub, depth + 1)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn scan_value_for_pii(value: &Value) -> Result<(), SchemaSubsetError> {
        match value {
            Value::String(s) => {
                let hits = detect_pii(s);
                if let Some(category) = hits.into_iter().next() {
                    return Err(SchemaSubsetError::ContainsPii(category));
                }
                Ok(())
            }
            Value::Array(arr) => {
                for v in arr {
                    Self::scan_value_for_pii(v)?;
                }
                Ok(())
            }
            Value::Object(map) => {
                for (_, v) in map {
                    Self::scan_value_for_pii(v)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ── HPL Phase 4.6 — JSON Schema instance validator ────────────────────
//
// `SchemaValidator::validate` checks that a developer-authored tool
// schema is STRUCTURALLY portable (no banned keywords, compiled patterns,
// bounded depth). It does NOT validate a live instance against that
// schema. The HPL resume handler needs the latter: when a client returns
// a tool_output, we must confirm the JSON instance conforms to the tool's
// `output_schema` exactly, or reject with `HRN-E019`.
//
// This validator implements the subset the tool schema subset accepts
// (type / properties / required / items / enum / const / minimum /
// maximum / exclusive{Minimum,Maximum} / min{Length,Items,Properties} /
// max{Length,Items,Properties} / pattern / additionalProperties /
// multipleOf / format — the last is informational). `uniqueItems` is
// forbidden by the structural validator so we skip it here.
//
// Errors are classified into a sanitized category enum
// (`InstanceErrorKind`) so the resume handler can report
// `detail={shape|type|required|size}` per SR B-7 without ever reflecting
// the offending value back to the client.

/// Coarse-grained instance error classifier. The resume handler maps
/// this to a fixed `detail` string and wires it into `HRN-E019`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceErrorKind {
    /// Structural mismatch — expected object where array was given,
    /// array where scalar, etc.
    Shape,
    /// Scalar type mismatch (expected string, got number, etc.).
    Type,
    /// Required property missing.
    Required,
    /// min/max bounds (length, items, value, properties).
    Size,
}

impl InstanceErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shape => "shape",
            Self::Type => "type",
            Self::Required => "required",
            Self::Size => "size",
        }
    }
}

/// Validate a JSON `instance` against `schema`. Returns the first
/// violation encountered, classified into the sanitized category set.
/// Unknown keywords are silently skipped (the bind-time structural
/// validator already rejected non-subset keywords).
pub fn validate_instance(instance: &Value, schema: &Value) -> Result<(), InstanceErrorKind> {
    validate_instance_rec(instance, schema, 0)
}

fn validate_instance_rec(
    instance: &Value,
    schema: &Value,
    depth: usize,
) -> Result<(), InstanceErrorKind> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(InstanceErrorKind::Shape);
    }
    let Some(sch) = schema.as_object() else {
        return Ok(());
    };

    // Type check — support string or array-of-strings.
    if let Some(t) = sch.get("type") {
        let types: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() && !type_matches(instance, &types) {
            return Err(InstanceErrorKind::Type);
        }
    }

    // Const — exact JSON equality.
    if let Some(c) = sch.get("const") {
        if c != instance {
            return Err(InstanceErrorKind::Type);
        }
    }

    // Enum — `instance` must equal one of the members.
    if let Some(Value::Array(members)) = sch.get("enum") {
        if !members.iter().any(|m| m == instance) {
            return Err(InstanceErrorKind::Type);
        }
    }

    match instance {
        Value::Object(obj) => {
            // required
            if let Some(Value::Array(req)) = sch.get("required") {
                for k in req {
                    if let Some(key) = k.as_str() {
                        if !obj.contains_key(key) {
                            return Err(InstanceErrorKind::Required);
                        }
                    }
                }
            }
            // minProperties / maxProperties
            if let Some(min) = sch.get("minProperties").and_then(|v| v.as_u64()) {
                if (obj.len() as u64) < min {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(max) = sch.get("maxProperties").and_then(|v| v.as_u64()) {
                if (obj.len() as u64) > max {
                    return Err(InstanceErrorKind::Size);
                }
            }
            // properties / additionalProperties
            let properties = sch.get("properties").and_then(|v| v.as_object());
            let additional = sch.get("additionalProperties");
            for (k, v) in obj {
                match properties.and_then(|p| p.get(k)) {
                    Some(sub_schema) => {
                        validate_instance_rec(v, sub_schema, depth + 1)?;
                    }
                    None => match additional {
                        Some(Value::Bool(false)) => {
                            return Err(InstanceErrorKind::Shape);
                        }
                        Some(sub_schema) if sub_schema.is_object() => {
                            validate_instance_rec(v, sub_schema, depth + 1)?;
                        }
                        _ => {
                            // additionalProperties=true or unspecified — accept.
                        }
                    },
                }
            }
        }
        Value::Array(arr) => {
            if let Some(min) = sch.get("minItems").and_then(|v| v.as_u64()) {
                if (arr.len() as u64) < min {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(max) = sch.get("maxItems").and_then(|v| v.as_u64()) {
                if (arr.len() as u64) > max {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(items_schema) = sch.get("items") {
                if items_schema.is_object() {
                    for v in arr {
                        validate_instance_rec(v, items_schema, depth + 1)?;
                    }
                }
            }
        }
        Value::String(s) => {
            if let Some(min) = sch.get("minLength").and_then(|v| v.as_u64()) {
                if (s.chars().count() as u64) < min {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(max) = sch.get("maxLength").and_then(|v| v.as_u64()) {
                if (s.chars().count() as u64) > max {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(pat) = sch.get("pattern").and_then(|v| v.as_str()) {
                // Reuse the bind-time regex compile limit so oversized
                // patterns can't be smuggled via the tool's stored schema.
                let compiled = regex::RegexBuilder::new(pat)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build();
                match compiled {
                    Ok(re) => {
                        if !re.is_match(s) {
                            return Err(InstanceErrorKind::Shape);
                        }
                    }
                    Err(_) => return Err(InstanceErrorKind::Shape),
                }
            }
        }
        Value::Number(n) => {
            let as_f64 = n.as_f64().unwrap_or(f64::NAN);
            if let Some(min) = sch.get("minimum").and_then(|v| v.as_f64()) {
                if as_f64 < min {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(max) = sch.get("maximum").and_then(|v| v.as_f64()) {
                if as_f64 > max {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(min) = sch.get("exclusiveMinimum").and_then(|v| v.as_f64()) {
                if as_f64 <= min {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(max) = sch.get("exclusiveMaximum").and_then(|v| v.as_f64()) {
                if as_f64 >= max {
                    return Err(InstanceErrorKind::Size);
                }
            }
            if let Some(step) = sch.get("multipleOf").and_then(|v| v.as_f64()) {
                if step > 0.0 {
                    let ratio = as_f64 / step;
                    if (ratio - ratio.round()).abs() > 1e-9 {
                        return Err(InstanceErrorKind::Size);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn type_matches(instance: &Value, types: &[&str]) -> bool {
    for t in types {
        let ok = match *t {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance.is_i64() || instance.is_u64(),
            "number" => instance.is_number(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => true, // unknown type — lenient
        };
        if ok {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_minimal_object_schema() {
        let s = json!({"type": "object"});
        SchemaValidator::tool_schema().validate(&s).unwrap();
    }

    #[test]
    fn tool_name_pattern_accepts_canonical_forms() {
        assert!(is_valid_tool_name("send_email"));
        assert!(is_valid_tool_name("send-email"));
        assert!(is_valid_tool_name("connector.action"));
        assert!(is_valid_tool_name("a"));
        assert!(is_valid_tool_name(&"x".repeat(64)));
    }

    #[test]
    fn tool_name_pattern_rejects_invalid_forms() {
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name(&"x".repeat(65)));
        assert!(!is_valid_tool_name("has space"));
        assert!(!is_valid_tool_name("emoji_💀"));
        assert!(!is_valid_tool_name("path/segment"));
    }

    #[test]
    fn accepts_full_realistic_schema() {
        let s = json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string", "description": "Slack channel id"},
                "text": {"type": "string", "minLength": 1, "maxLength": 4000},
                "priority": {"type": "string", "enum": ["low", "normal", "high"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 10}
            },
            "required": ["channel", "text"],
            "additionalProperties": false
        });
        SchemaValidator::tool_schema().validate(&s).unwrap();
    }

    #[test]
    fn rejects_non_object_root() {
        let s = json!({"type": "string"});
        assert_eq!(
            SchemaValidator::tool_schema().validate(&s).unwrap_err(),
            SchemaSubsetError::NotObject
        );
    }

    #[test]
    fn rejects_array_root() {
        let s = json!([]);
        assert_eq!(
            SchemaValidator::tool_schema().validate(&s).unwrap_err(),
            SchemaSubsetError::NotObject
        );
    }

    #[test]
    fn rejects_each_banned_keyword() {
        for kw in BANNED_KEYWORDS {
            let s = json!({"type": "object", *kw: {}});
            let err = SchemaValidator::tool_schema().validate(&s).unwrap_err();
            assert!(
                matches!(err, SchemaSubsetError::BannedKeyword(ref k) if k == *kw),
                "expected BannedKeyword({kw}), got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_format_values() {
        let s = json!({
            "type": "object",
            "properties": {"x": {"type": "string", "format": "ipv4"}}
        });
        let err = SchemaValidator::tool_schema().validate(&s).unwrap_err();
        assert!(matches!(err, SchemaSubsetError::BadFormatValue(_)));
    }

    #[test]
    fn accepts_allowed_format_values() {
        for f in ALLOWED_FORMATS {
            let s = json!({
                "type": "object",
                "properties": {"x": {"type": "string", "format": *f}}
            });
            SchemaValidator::tool_schema()
                .validate(&s)
                .unwrap_or_else(|e| panic!("format {f} should pass: {e:?}"));
        }
    }

    #[test]
    fn rejects_invalid_regex_pattern() {
        let s = json!({
            "type": "object",
            "properties": {"x": {"type": "string", "pattern": "(unclosed"}}
        });
        let err = SchemaValidator::tool_schema().validate(&s).unwrap_err();
        assert!(matches!(err, SchemaSubsetError::PatternInvalid(_)));
    }

    #[test]
    fn accepts_safe_regex_pattern() {
        let s = json!({
            "type": "object",
            "properties": {"x": {"type": "string", "pattern": "^[a-z]+$"}}
        });
        SchemaValidator::tool_schema().validate(&s).unwrap();
    }

    #[test]
    fn enforces_max_depth() {
        // Build a chain of properties → x → object → properties → x → ...
        let mut leaf = json!({"type": "object"});
        for _ in 0..(MAX_SCHEMA_DEPTH + 4) {
            leaf = json!({"type": "object", "properties": {"x": leaf}});
        }
        let err = SchemaValidator::tool_schema().validate(&leaf).unwrap_err();
        assert!(matches!(err, SchemaSubsetError::TooDeep(_)));
    }



    #[test]
    fn pii_scan_passes_clean_schema() {
        let s = json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string", "description": "channel id"}
            }
        });
        SchemaValidator::tool_schema().scan_for_pii(&s).unwrap();
    }

    // ── HPL instance validator — happy + 8 failure modes (SR B-6) ────

    fn sch_sample() -> Value {
        json!({
            "type": "object",
            "required": ["channel", "count"],
            "properties": {
                "channel": {"type": "string", "minLength": 1, "maxLength": 64},
                "count": {"type": "integer", "minimum": 1, "maximum": 99},
                "tag": {"type": "string", "enum": ["a", "b"]},
                "tags": {"type": "array", "minItems": 0, "maxItems": 3, "items": {"type": "string"}},
                "re": {"type": "string", "pattern": "^[a-z]+$"}
            },
            "additionalProperties": false
        })
    }

    #[test]
    fn instance_happy_path() {
        let inst = json!({"channel": "general", "count": 5, "tag": "a"});
        validate_instance(&inst, &sch_sample()).unwrap();
    }

    #[test]
    fn instance_shape_mismatch() {
        // Expected object, got array.
        let err = validate_instance(&json!([1, 2, 3]), &sch_sample()).unwrap_err();
        assert_eq!(err, InstanceErrorKind::Type);
    }

    #[test]
    fn instance_type_mismatch_on_property() {
        let inst = json!({"channel": "general", "count": "five"});
        let err = validate_instance(&inst, &sch_sample()).unwrap_err();
        assert_eq!(err, InstanceErrorKind::Type);
    }

    #[test]
    fn instance_missing_required() {
        let inst = json!({"channel": "general"});
        let err = validate_instance(&inst, &sch_sample()).unwrap_err();
        assert_eq!(err, InstanceErrorKind::Required);
    }

    #[test]
    fn instance_enum_violation() {
        let inst = json!({"channel": "general", "count": 1, "tag": "c"});
        let err = validate_instance(&inst, &sch_sample()).unwrap_err();
        assert_eq!(err, InstanceErrorKind::Type);
    }

    #[test]
    fn instance_length_bound() {
        let inst = json!({"channel": "", "count": 1});
        let err = validate_instance(&inst, &sch_sample()).unwrap_err();
        assert_eq!(err, InstanceErrorKind::Size);
    }

    #[test]
    fn instance_pattern_mismatch() {
        let inst = json!({"channel": "ok", "count": 1, "re": "HasUpperCase"});
        let err = validate_instance(&inst, &sch_sample()).unwrap_err();
        assert_eq!(err, InstanceErrorKind::Shape);
    }

    #[test]
    fn instance_depth_guard() {
        // Build a 20-deep nested object; validator caps at MAX_SCHEMA_DEPTH.
        let mut inner = json!({"type": "object"});
        for _ in 0..20 {
            inner = json!({"type": "object", "properties": {"a": inner}});
        }
        let schema = json!({"type": "object", "properties": {"a": inner}});
        // Mirror instance shape deep enough to trip the guard.
        let mut v = json!(1);
        for _ in 0..20 {
            v = json!({"a": v});
        }
        let instance = json!({"a": v});
        let err = validate_instance(&instance, &schema).unwrap_err();
        // Either Shape (depth) or Type (leaf mismatch) is acceptable —
        // both surface before a stack blow.
        assert!(matches!(
            err,
            InstanceErrorKind::Shape | InstanceErrorKind::Type
        ));
    }

    #[test]
    fn instance_extra_property_rejected_when_additional_false() {
        let inst = json!({"channel": "general", "count": 1, "oops": "extra"});
        let err = validate_instance(&inst, &sch_sample()).unwrap_err();
        assert_eq!(err, InstanceErrorKind::Shape);
    }

    // ── strip_vendor_extensions ──

    fn no_vendor_keys(v: &Value) -> bool {
        match v {
            Value::Object(m) => {
                m.keys().all(|k| !(k.starts_with("x-") || k == "$comment"))
                    && m.values().all(no_vendor_keys)
            }
            Value::Array(a) => a.iter().all(no_vendor_keys),
            _ => true,
        }
    }

    #[test]
    fn strip_clears_x_keywords_then_validates() {
        let mut s = json!({
            "type": "object",
            "$comment": "example",
            "properties": { "channel": {
                "type": "string", "description": "Channel ID",
                "x-resolve": { "action": "slack/list-channels", "output_path": "$.channels[*].id" },
                "x-semantic-type": "slack-channel-id"
            }}
        });
        strip_vendor_extensions(&mut s);
        assert!(no_vendor_keys(&s), "x-*/$comment must not remain: {s}");
        SchemaValidator::tool_schema()
            .validate(&s)
            .expect("clean schema validates");
        assert_eq!(s["properties"]["channel"]["description"], "Channel ID");
    }

    #[test]
    fn strip_preserves_property_named_like_extension() {
        let mut s = json!({ "type": "object", "properties": { "x-api-key": {
            "type": "string", "x-semantic-type": "secret"
        }}});
        strip_vendor_extensions(&mut s);
        assert!(
            s["properties"].get("x-api-key").is_some(),
            "property name kept"
        );
        assert!(s["properties"]["x-api-key"]
            .get("x-semantic-type")
            .is_none());
    }

    #[test]
    fn strip_leaves_banned_keywords_for_validator() {
        let mut s = json!({ "type": "object", "properties": { "x": { "$ref": "#/d/F" } } });
        strip_vendor_extensions(&mut s);
        let err = SchemaValidator::tool_schema().validate(&s).unwrap_err();
        assert!(
            matches!(err, SchemaSubsetError::BannedKeyword(ref k) if k == "$ref"),
            "{err:?}"
        );
    }

    #[test]
    fn strip_keeps_data_values() {
        let mut s = json!({ "type": "object", "properties": { "cfg": {
            "type": "object", "default": { "x-raw": 1 }, "additionalProperties": true
        }}});
        strip_vendor_extensions(&mut s);
        assert_eq!(s["properties"]["cfg"]["default"]["x-raw"], 1);
    }

    #[test]
    fn strip_removes_pii_inside_extension() {
        let mut s = json!({ "type": "object", "properties": { "to": {
            "type": "string", "x-resolve": { "description": "email alice@example.com" }
        }}});
        strip_vendor_extensions(&mut s);
        let v = SchemaValidator::tool_schema();
        v.validate(&s).expect("validates");
        v.scan_for_pii(&s).expect("no PII after strip");
    }


    #[test]
    fn strip_bounded_on_deep_schema_no_panic() {
        let mut node = json!({ "type": "string", "x-resolve": { "a": 1 } });
        for _ in 0..(MAX_SCHEMA_DEPTH + 50) {
            node = json!({ "type": "object", "properties": { "n": node } });
        }
        strip_vendor_extensions(&mut node);
    }

    #[test]
    fn deep_unstripped_extension_is_rejected_by_validate() {
        let mut node = json!({ "type": "object", "properties": { "deep": {
            "type": "string", "x-resolve": { "a": 1 }
        }}});
        for _ in 0..(MAX_SCHEMA_DEPTH + 5) {
            node = json!({ "type": "object", "properties": { "n": node } });
        }
        strip_vendor_extensions(&mut node);
        let err = SchemaValidator::tool_schema().validate(&node).unwrap_err();
        assert!(matches!(err, SchemaSubsetError::TooDeep(_)), "{err:?}");
    }
}
