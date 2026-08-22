//! The `--context-query` declaration spelling (#92).
//!
//! A trigger's declared context is a saved query, and since 1.5.1 the
//! declaration may bind the query's parameters from the FIRING ITEM using
//! the same JSON pointers `--dedup-key` already understands:
//!
//! ```text
//! triage_ctx                                   # parameterless (1.5.0 form)
//! triage_ctx($session = /session)              # one binding
//! triage_ctx($session = /session, $from = /email/from)
//! ```
//!
//! The whole spelling is stored verbatim in the Trigger grain's
//! `context_query` field, so *what a fired run gets to see* stays on the
//! declaration — auditable and replicating — rather than in host config.
//! At fire time the evaluator resolves each pointer against the item's
//! payload and runs the saved query with those bindings. Resolution is
//! fail-closed like `--dedup-key`'s unidentifiable path: an unresolvable
//! pointer, or a pointer landing on a non-scalar, refuses the firing
//! rather than running the query with a hole in it.

/// A parsed `--context-query` spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextQuerySpec {
    /// The saved-query name (`qry:<name>` in the file's meta registry).
    pub name: String,
    /// `($param, /json/pointer)` bindings, in declaration order. Parameter
    /// names are stored WITHOUT the `$` (matching CAL's `RunQueryStmt`).
    pub bindings: Vec<(String, String)>,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn is_param_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Parse a `--context-query` spelling: `name` or
/// `name($p = /ptr, $q = /ptr)`.
///
/// The plain-name form parses to an empty binding list, byte-identical in
/// meaning to the 1.5.0 feature. Errors are plain strings — this runs at
/// declaration time (CLI + `schedule::validate`) and at fire time, and both
/// surface the message verbatim.
///
/// Pointers must start with `/` (RFC 6901) and, because `,` and `)` are
/// structural in this spelling, cannot contain either character. Keys that
/// need them can be reached by reshaping the connector payload.
pub fn parse_context_query(spec: &str) -> Result<ContextQuerySpec, String> {
    let spec = spec.trim();
    let (name, rest) = match spec.find('(') {
        None => (spec, None),
        Some(i) => {
            let inner = spec[i + 1..]
                .strip_suffix(')')
                .ok_or_else(|| "missing closing ')' after parameter bindings".to_string())?;
            (spec[..i].trim_end(), Some(inner))
        }
    };
    if name.is_empty() || !name.chars().all(is_name_char) {
        return Err(format!(
            "{name:?} is not a saved-query name (allowed: [A-Za-z0-9_.-])"
        ));
    }
    let mut bindings = Vec::new();
    if let Some(inner) = rest {
        if inner.trim().is_empty() {
            return Err("empty parameter list — drop the parentheses for a parameterless query"
                .to_string());
        }
        for part in inner.split(',') {
            let part = part.trim();
            let Some((param, pointer)) = part.split_once('=') else {
                return Err(format!(
                    "binding {part:?} is not of the form $param = /json/pointer"
                ));
            };
            let param = param.trim();
            let pointer = pointer.trim();
            let Some(param) = param.strip_prefix('$') else {
                return Err(format!("parameter {param:?} must start with '$'"));
            };
            if param.is_empty() || !param.chars().all(is_param_char) {
                return Err(format!(
                    "${param} is not a parameter name (allowed: [A-Za-z0-9_])"
                ));
            }
            if !pointer.starts_with('/') {
                return Err(format!(
                    "pointer {pointer:?} for ${param} must be a JSON pointer starting with '/'"
                ));
            }
            if bindings.iter().any(|(p, _)| p == param) {
                return Err(format!("parameter ${param} is bound twice"));
            }
            bindings.push((param.to_string(), pointer.to_string()));
        }
    }
    Ok(ContextQuerySpec {
        name: name.to_string(),
        bindings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_name_parses_with_no_bindings() {
        let s = parse_context_query("triage_ctx").unwrap();
        assert_eq!(s.name, "triage_ctx");
        assert!(s.bindings.is_empty());
    }

    #[test]
    fn bindings_parse_in_order() {
        let s =
            parse_context_query("triage_ctx($session = /session, $from = /email/from)").unwrap();
        assert_eq!(s.name, "triage_ctx");
        assert_eq!(
            s.bindings,
            vec![
                ("session".to_string(), "/session".to_string()),
                ("from".to_string(), "/email/from".to_string()),
            ]
        );
    }

    #[test]
    fn whitespace_is_forgiven() {
        let s = parse_context_query("  ctx( $a=/x , $b = /y/0 )  ").unwrap();
        assert_eq!(s.bindings.len(), 2);
        assert_eq!(s.bindings[1], ("b".to_string(), "/y/0".to_string()));
    }

    #[test]
    fn malformed_spellings_refuse() {
        for bad in [
            "",
            "not a name!",
            "ctx(",
            "ctx()",
            "ctx($a)",
            "ctx(a = /x)",
            "ctx($ = /x)",
            "ctx($a = x)",
            "ctx($a = /x, $a = /y)",
            "ctx($a-b = /x)",
        ] {
            assert!(parse_context_query(bad).is_err(), "{bad:?} must refuse");
        }
    }

    #[test]
    fn the_1_5_0_char_class_still_holds_for_names() {
        assert!(parse_context_query("a.b-c_9").is_ok());
        assert!(parse_context_query("a b").is_err());
    }
}
