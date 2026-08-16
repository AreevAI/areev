//! JSON wire format for CAL (`application/json+cal`).
//!
//! Provides bidirectional AST <-> JSON conversion.  The text parser
//! produces `CalQuery` from `text/cal`; this module produces `CalQuery`
//! from `application/json+cal` and serializes `CalQuery` back to JSON.
//!
//! ## Security
//!
//! - **S-04:** Input length validated against `MAX_QUERY_LENGTH` (65536 bytes).
//! - **S-04:** JSON nesting depth validated against `MAX_NESTING_DEPTH` (8)
//!   before deserialization to prevent stack-overflow attacks.

use super::ast::CalQuery;
use super::errors::{CalError, CalResult};

/// Maximum allowed byte length for a JSON+CAL input (S-04).
/// 64 KB — matches the text parser limit.
const MAX_QUERY_LENGTH: usize = 65_536;

/// Maximum allowed JSON nesting depth (S-04).
/// Counts `[` and `{` depth.  serde_json does not enforce recursion
/// limits, so we pre-validate with a lightweight scan.
///
/// Bug 10: imported from `parser` so the text and JSON entry points share a
/// single source of truth.
const MAX_NESTING_DEPTH: usize = super::parser::MAX_NESTING_DEPTH;

/// Parse a `application/json+cal` string into a `CalQuery` AST.
///
/// Validates input length (max 65536 bytes) and JSON nesting depth
/// (max 8 levels) before deserialization.
///
/// # Errors
///
/// Returns `CalError::QueryTooLong` if the input exceeds `MAX_QUERY_LENGTH`.
/// Returns `CalError::NestingTooDeep` if JSON nesting exceeds `MAX_NESTING_DEPTH`.
/// Returns `CalError::InvalidJsonCal` if JSON parsing fails.
pub fn parse_json_cal(json: &str) -> CalResult<CalQuery> {
    // S-04: Length validation.
    if json.len() > MAX_QUERY_LENGTH {
        return Err(CalError::QueryTooLong {
            length: json.len(),
            max: MAX_QUERY_LENGTH,
            span: None,
        });
    }

    // S-04: Nesting depth validation before deserialization.
    validate_nesting_depth(json)?;

    serde_json::from_str(json).map_err(|e| CalError::InvalidJsonCal {
        detail: e.to_string(),
        span: None,
    })
}

/// Serialize a `CalQuery` AST to `application/json+cal`.
///
/// # Errors
///
/// Returns `CalError::InvalidJsonCal` if serialization fails (should not
/// happen for well-formed ASTs).
pub fn to_json_cal(query: &CalQuery) -> CalResult<String> {
    serde_json::to_string_pretty(query).map_err(|e| CalError::InvalidJsonCal {
        detail: e.to_string(),
        span: None,
    })
}

/// Validate that JSON nesting depth does not exceed `MAX_NESTING_DEPTH`.
///
/// Performs a single-pass scan counting `{` and `[` depth, skipping
/// characters inside JSON string literals.  This is O(n) and does not
/// allocate.
fn validate_nesting_depth(json: &str) -> CalResult<()> {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escape = false;

    for byte in json.bytes() {
        if escape {
            escape = false;
            continue;
        }
        match byte {
            b'\\' if in_string => {
                escape = true;
            }
            b'"' => {
                in_string = !in_string;
            }
            b'{' | b'[' if !in_string => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Err(CalError::NestingTooDeep {
                        depth,
                        max: MAX_NESTING_DEPTH,
                        span: None,
                    });
                }
            }
            b'}' | b']' if !in_string => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;

    #[test]
    fn test_roundtrip_simple_recall() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: Some(AboutClause {
                    text: "john preferences".into(),
                    span: None,
                }),
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: Some(10),
                as_format: None,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).expect("serialization should succeed");
        let parsed = parse_json_cal(&json).expect("deserialization should succeed");

        assert_eq!(query, parsed);
    }

    #[test]
    fn test_roundtrip_with_pipeline() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Events,
                about: None,
                where_clause: None,
                recent: Some(RecentClause {
                    count: 5,
                    span: None,
                }),
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: vec![
                PipelineStage::OrderBy {
                    field: "created_at".into(),
                    descending: true,
                    span: None,
                },
                PipelineStage::Limit {
                    value: 5,
                    span: None,
                },
            ],
            with_options: vec![WithOption::ScoreBreakdown],
            format: Some(FormatClause::Single(FormatSpec::Json)),
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).expect("serialization");
        let parsed = parse_json_cal(&json).expect("deserialization");
        assert_eq!(query, parsed);
    }

    #[test]
    fn test_roundtrip_exists() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Exists(ExistsStmt {
                grain_type: GrainTypePlural::Facts,
                where_clause: Some(WhereClause {
                    condition: Condition::Comparison {
                        field: "subject".into(),
                        comparator: Comparator::Eq,
                        value: Value::String {
                            value: "john".into(),
                        },
                        span: None,
                    },
                    span: None,
                }),
                about: None,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).expect("serialization");
        let parsed = parse_json_cal(&json).expect("deserialization");
        assert_eq!(query, parsed);
    }

    #[test]
    fn test_input_too_long() {
        let long_input = "x".repeat(MAX_QUERY_LENGTH + 1);
        let err = parse_json_cal(&long_input).unwrap_err();
        assert_eq!(err.code(), "CAL-E001");
    }

    #[test]
    fn test_nesting_too_deep() {
        // 9 levels of nesting (exceeds max of 8)
        let deep_json = "{".repeat(9) + &"}".repeat(9);
        let err = parse_json_cal(&deep_json).unwrap_err();
        assert_eq!(err.code(), "CAL-E007");
    }

    #[test]
    fn test_nesting_at_limit() {
        // Exactly 8 levels — should be accepted (though content is invalid JSON)
        let json = "{".repeat(8) + &"}".repeat(8);
        // This will fail at the serde parse step, not the depth check
        let err = parse_json_cal(&json).unwrap_err();
        // Parse error (CAL-E120 InvalidJsonCal), not a nesting error.
        assert_eq!(err.code(), "CAL-E120");
    }

    #[test]
    fn test_nesting_in_strings_ignored() {
        // Braces inside JSON string values should not count toward depth
        let json = r#"{"key": "{{{{{{{{{{{"}"#;
        // This will fail at serde parse (not a CalQuery), but depth check passes
        let err = parse_json_cal(json).unwrap_err();
        assert_eq!(err.code(), "CAL-E120"); // CAL-E120: InvalidJsonCal
    }

    #[test]
    fn test_invalid_json() {
        let err = parse_json_cal("not valid json").unwrap_err();
        assert_eq!(err.code(), "CAL-E120"); // CAL-E120: InvalidJsonCal
    }

    #[test]
    fn test_roundtrip_nested_condition() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: Some(WhereClause {
                    condition: Condition::And {
                        left: Box::new(Condition::Comparison {
                            field: "subject".into(),
                            comparator: Comparator::Eq,
                            value: Value::String {
                                value: "john".into(),
                            },
                            span: None,
                        }),
                        right: Box::new(Condition::Or {
                            left: Box::new(Condition::Comparison {
                                field: "confidence".into(),
                                comparator: Comparator::Gte,
                                value: Value::Number { value: 0.8 },
                                span: None,
                            }),
                            right: Box::new(Condition::IsNotNull {
                                field: "tags".into(),
                                span: None,
                            }),
                            span: None,
                        }),
                        span: None,
                    },
                    span: None,
                }),
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).unwrap();
        let parsed = parse_json_cal(&json).unwrap();
        assert_eq!(query, parsed);
    }

    /// JSON round-trip: BATCH with two sub-queries.
    #[test]
    fn test_roundtrip_batch_statement() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Batch(BatchStmt {
                statements: vec![
                    crate::ast::BatchEntry {
                        statement: CalStatement::Recall(RecallStmt {
                            grain_type: GrainTypePlural::Facts,
                            about: None,
                            where_clause: None,
                            recent: None,
                            since: None,
                            until: None,
                            like: None,
                            between: None,
                            contradictions: None,
                            limit: Some(5),
                            as_format: None,
                            span: None,
                        }),
                        pipeline: Vec::new(),
                        with_options: Vec::new(),
                        format: None,
                        user_vars: std::collections::HashMap::new(),
                    },
                    crate::ast::BatchEntry {
                        statement: CalStatement::Recall(RecallStmt {
                            grain_type: GrainTypePlural::Events,
                            about: None,
                            where_clause: None,
                            recent: Some(RecentClause {
                                count: 3,
                                span: None,
                            }),
                            since: None,
                            until: None,
                            like: None,
                            between: None,
                            contradictions: None,
                            limit: None,
                            as_format: None,
                            span: None,
                        }),
                        pipeline: Vec::new(),
                        with_options: Vec::new(),
                        format: None,
                        user_vars: std::collections::HashMap::new(),
                    },
                ],
                labeled: None,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).expect("BATCH serialization");
        let parsed = parse_json_cal(&json).expect("BATCH deserialization");
        assert_eq!(query, parsed);
    }

    /// JSON round-trip: LET binding with SUBJECTS extractor.
    #[test]
    fn test_roundtrip_let_binding() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: Some(AboutClause {
                    text: "john preferences".into(),
                    span: None,
                }),
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_values: Default::default(),
            let_bindings: vec![LetBinding {
                name: "people".to_string(),
                extractor: Extractor::Subjects,
                source: Box::new(CalStatement::Recall(RecallStmt {
                    grain_type: GrainTypePlural::Facts,
                    about: None,
                    where_clause: None,
                    recent: Some(RecentClause {
                        count: 10,
                        span: None,
                    }),
                    since: None,
                    until: None,
                    like: None,
                    between: None,
                    contradictions: None,
                    limit: None,
                    as_format: None,
                    span: None,
                })),
                span: None,
            }],
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).expect("LET serialization");
        let parsed = parse_json_cal(&json).expect("LET deserialization");
        assert_eq!(query, parsed);
    }

    /// JSON round-trip: DESCRIBE SERVER.
    #[test]
    fn test_roundtrip_describe_server() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Describe(DescribeStmt {
                target: DescribeTarget::Server,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).expect("DESCRIBE serialization");
        let parsed = parse_json_cal(&json).expect("DESCRIBE deserialization");
        assert_eq!(query, parsed);
    }

    /// Parse → serialize → parse round-trip produces identical AST.
    #[test]
    fn test_full_roundtrip_parse_serialize_parse() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: Some(AboutClause {
                    text: "john preferences".into(),
                    span: None,
                }),
                where_clause: Some(WhereClause {
                    condition: Condition::Comparison {
                        field: "subject".into(),
                        comparator: Comparator::Eq,
                        value: Value::String {
                            value: "john".into(),
                        },
                        span: None,
                    },
                    span: None,
                }),
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: Some(10),
                as_format: None,
                span: None,
            }),
            pipeline: vec![PipelineStage::Limit {
                value: 5,
                span: None,
            }],
            with_options: vec![WithOption::Superseded, WithOption::ScoreBreakdown],
            format: Some(FormatClause::Single(FormatSpec::Json)),
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        // Round-trip 1: serialize → parse
        let json1 = to_json_cal(&query).unwrap();
        let parsed1 = parse_json_cal(&json1).unwrap();
        assert_eq!(query, parsed1);

        // Round-trip 2: serialize again → parse again
        let json2 = to_json_cal(&parsed1).unwrap();
        let parsed2 = parse_json_cal(&json2).unwrap();
        assert_eq!(parsed1, parsed2);

        // JSON output should be identical (deterministic serialization)
        assert_eq!(
            json1, json2,
            "double round-trip should produce identical JSON"
        );
    }

    #[test]
    fn test_to_json_cal_produces_valid_json() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Describe(DescribeStmt {
                target: DescribeTarget::Schema,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: std::collections::HashMap::new(),
            warnings: vec![],
        };

        let json = to_json_cal(&query).unwrap();
        // Verify it's valid JSON
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }

    // ── WITH VARS — JSON round-trip ────────────────────────────────────

    #[test]
    fn test_roundtrip_with_user_vars() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("app_name".into(), "TestApp".into());
        vars.insert("theme".into(), "dark".into());

        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: Some(AboutClause {
                    text: "john".into(),
                    span: None,
                }),
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: vars,
            warnings: vec![],
        };

        // Round-trip: serialize → deserialize
        let json = to_json_cal(&query).unwrap();
        let parsed = parse_json_cal(&json).unwrap();
        assert_eq!(query.user_vars, parsed.user_vars);
        assert_eq!(parsed.user_vars.get("app_name").unwrap(), "TestApp");
        assert_eq!(parsed.user_vars.get("theme").unwrap(), "dark");

        // Double round-trip: structural equality
        let json2 = to_json_cal(&parsed).unwrap();
        let parsed2 = parse_json_cal(&json2).unwrap();
        assert_eq!(parsed.user_vars, parsed2.user_vars);
    }

    #[test]
    fn test_json_missing_user_vars_defaults_to_empty() {
        // Serialize a query with user_vars, then strip the field from JSON,
        // and verify deserialization defaults to empty HashMap.
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: vec![],
            with_options: vec![],
            format: None,
            let_bindings: vec![],
            let_values: Default::default(),
            user_vars: {
                let mut v = std::collections::HashMap::new();
                v.insert("x".into(), "y".into());
                v
            },
            warnings: vec![],
        };

        let json = to_json_cal(&query).unwrap();
        // Strip user_vars from the JSON to simulate an older client.
        let mut val: serde_json::Value = serde_json::from_str(&json).unwrap();
        val.as_object_mut().unwrap().remove("user_vars");
        let stripped = serde_json::to_string_pretty(&val).unwrap();

        let parsed = parse_json_cal(&stripped).unwrap();
        assert!(
            parsed.user_vars.is_empty(),
            "missing user_vars should default to empty HashMap"
        );
    }
}
