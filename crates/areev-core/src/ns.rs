//! Namespace scopes — the `"org.*"` wildcard convention shared by every
//! query surface (store recall, CAL `WHERE namespace`, MCP, CLI `--ns`).
//!
//! A namespace value ending in `*` is a PREFIX SCOPE rather than an exact
//! name: `"org.*"` selects the namespace `org` itself plus every descendant
//! reached through the separator the caller wrote (`org.sales`,
//! `org.sales.emea` — but never `organization`, and never `org:x`, whose
//! hierarchy uses a different separator). The `*` must be the trailing
//! character and must follow a non-alphanumeric separator, which is what
//! keeps `org*` (ambiguous with `organization`) unspellable. Bare `*` is
//! refused too — "every namespace" stays an explicit authorization concept
//! (grants), not a recall convenience.
//!
//! Because `*` carries this meaning on the read side, it is RESERVED in
//! namespace names on the write side: the store refuses to add a grain whose
//! namespace contains `*` (`VAL-E001`). Replication replay deliberately does
//! not enforce this, so files written before the reservation stay importable.
//!
//! Scopes select **reads only**. Destruction (`FORGET SUBJECT`,
//! `PURGE OLDER THAN … IN`), grants, retention/anonymization policy, and
//! point reads (`latest`, `thread_tail`, graph traversals) all take exact
//! namespaces and refuse patterns loudly — a wildcard must never widen a
//! destructive or policy surface (root invariant 3).

use crate::error::{AreevError, Result};

/// A parsed namespace scope: exactly one namespace, or a prefix family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NsScope {
    /// An exact namespace name (contains no `*`).
    Exact(String),
    /// `<base><sep>*`: the namespace `base` itself plus every namespace
    /// starting with `base` followed by `sep` — parent + descendants.
    Prefix {
        /// The hierarchy root, without the trailing separator (`org`).
        base: String,
        /// The separator the caller wrote (`.` in `org.*`, `:` in `agent:*`).
        sep: char,
    },
}

impl NsScope {
    /// Parse a namespace value from any query surface. `"org"` → `Exact`;
    /// `"org.*"` → `Prefix{base: "org", sep: '.'}`. Any other placement of
    /// `*` is a validation error, never a silent exact-match miss.
    pub fn parse(value: &str) -> Result<NsScope> {
        if !value.contains('*') {
            return Ok(NsScope::Exact(value.to_string()));
        }
        let Some(head) = value.strip_suffix('*') else {
            return Err(AreevError::Validation(format!(
                "namespace pattern \"{value}\": '*' is only valid as the trailing character \
                 (e.g. \"org.*\")"
            )));
        };
        if head.contains('*') {
            return Err(AreevError::Validation(format!(
                "namespace pattern \"{value}\": only one '*' is allowed, as the trailing \
                 character (e.g. \"org.*\")"
            )));
        }
        let Some(sep) = head.chars().last() else {
            return Err(AreevError::Validation(
                "namespace pattern \"*\": a prefix scope needs a base namespace \
                 (e.g. \"org.*\"); \"every namespace\" is not a recall scope"
                    .into(),
            ));
        };
        if sep.is_alphanumeric() {
            return Err(AreevError::Validation(format!(
                "namespace pattern \"{value}\": '*' must follow a separator — write \
                 \"{head}.*\" to select \"{head}\" and its descendants ({head}.x, {head}.y.z); \
                 \"{value}\" would ambiguously match unrelated names sharing the spelling"
            )));
        }
        let base: String = head[..head.len() - sep.len_utf8()].to_string();
        if base.is_empty() {
            return Err(AreevError::Validation(format!(
                "namespace pattern \"{value}\": a prefix scope needs a base namespace before \
                 the separator (e.g. \"org{sep}*\")"
            )));
        }
        Ok(NsScope::Prefix { base, sep })
    }

    /// Whether this value even looks like a pattern (contains `*`). Cheap
    /// pre-check that keeps exact-namespace hot paths at one byte scan.
    #[inline]
    pub fn is_pattern(value: &str) -> bool {
        value.contains('*')
    }

    /// Does `ns` fall inside this scope? Parent + descendants for a prefix:
    /// `org.*` matches `org` and `org.sales`, never `organization` or `org:x`.
    pub fn matches(&self, ns: &str) -> bool {
        match self {
            NsScope::Exact(e) => ns == e,
            NsScope::Prefix { base, sep } => {
                ns == base
                    || (ns.len() > base.len()
                        && ns.starts_with(base.as_str())
                        && ns[base.len()..].starts_with(*sep))
            }
        }
    }
}

/// Guard for surfaces that take exactly one namespace (writes, destruction,
/// policy, point reads): refuse a `*`-bearing value loudly instead of letting
/// it exact-match nothing (silent empty) or select a family (silent widening).
/// `what` names the operation for the error message.
pub fn require_exact_ns(what: &str, ns: &str) -> Result<()> {
    if NsScope::is_pattern(ns) {
        return Err(AreevError::Validation(format!(
            "{what} takes an exact namespace, not a pattern (got \"{ns}\"): '*' is reserved \
             for read scoping (e.g. RECALL … WHERE namespace = \"org.*\")"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_when_no_star() {
        assert_eq!(NsScope::parse("org").unwrap(), NsScope::Exact("org".into()));
        assert_eq!(
            NsScope::parse("org.sales").unwrap(),
            NsScope::Exact("org.sales".into())
        );
        assert_eq!(NsScope::parse("").unwrap(), NsScope::Exact("".into()));
    }

    #[test]
    fn prefix_forms_parse() {
        assert_eq!(
            NsScope::parse("org.*").unwrap(),
            NsScope::Prefix { base: "org".into(), sep: '.' }
        );
        assert_eq!(
            NsScope::parse("agent:*").unwrap(),
            NsScope::Prefix { base: "agent".into(), sep: ':' }
        );
        assert_eq!(
            NsScope::parse("org.sales.*").unwrap(),
            NsScope::Prefix { base: "org.sales".into(), sep: '.' }
        );
        // `-` and `_` are non-alphanumeric, hence valid separators — the same
        // rule the erasure identity selector uses.
        assert_eq!(
            NsScope::parse("areev-*").unwrap(),
            NsScope::Prefix { base: "areev".into(), sep: '-' }
        );
    }

    #[test]
    fn malformed_patterns_refuse() {
        for bad in ["*", "org*", "*.org", "org.*x", "o*.sales.*", "**", "org.**"] {
            let err = NsScope::parse(bad).unwrap_err();
            assert!(
                matches!(err, AreevError::Validation(_)),
                "{bad} should be a validation error, got {err:?}"
            );
        }
    }

    #[test]
    fn separator_only_pattern_needs_a_base() {
        assert!(NsScope::parse(".*").is_err());
        assert!(NsScope::parse(":*").is_err());
    }

    #[test]
    fn matches_parent_and_descendants_only() {
        let s = NsScope::parse("org.*").unwrap();
        assert!(s.matches("org"), "parent is included");
        assert!(s.matches("org.sales"));
        assert!(s.matches("org.sales.emea"));
        assert!(!s.matches("organization"), "separator required");
        assert!(!s.matches("org:x"), "the caller chose '.' as the hierarchy");
        assert!(!s.matches("orgs"));
        assert!(!s.matches(""));
        assert!(!s.matches("xorg.sales"));
    }

    #[test]
    fn matches_with_colon_separator() {
        let s = NsScope::parse("agent:*").unwrap();
        assert!(s.matches("agent"));
        assert!(s.matches("agent:authz"));
        assert!(!s.matches("agent.authz"));
        assert!(!s.matches("agents"));
    }

    #[test]
    fn unicode_separator_boundary_is_char_correct() {
        // A multi-byte separator must slice on the char boundary, not byte len 1.
        let s = NsScope::parse("org→*").unwrap();
        assert_eq!(s, NsScope::Prefix { base: "org".into(), sep: '→' });
        assert!(s.matches("org"));
        assert!(s.matches("org→x"));
        assert!(!s.matches("org.x"));
    }

    #[test]
    fn exact_matches_exactly() {
        let s = NsScope::parse("org").unwrap();
        assert!(s.matches("org"));
        assert!(!s.matches("org.sales"));
        assert!(!s.matches("or"));
    }

    #[test]
    fn require_exact_refuses_patterns() {
        assert!(require_exact_ns("latest", "org").is_ok());
        let err = require_exact_ns("PURGE", "org.*").unwrap_err();
        assert!(err.to_string().starts_with("VAL-E001"), "{err}");
        assert!(require_exact_ns("forget_subject", "o*rg").is_err());
    }
}
