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
//! The write side refuses one more thing ([`require_writable_ns`]): a
//! namespace that cannot be spelled back — whitespace, control characters, or
//! invisible formatting characters. A namespace is otherwise an opaque string
//! and stays one; this rule exists because minting a namespace is the only
//! operation with no way to fail, so a typo in one is accepted everywhere and
//! found nowhere.
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

/// Characters that make a namespace unspellable — invisible on a terminal, in
/// a diff, and in a review, so a name carrying one is not the name anyone
/// meant to write. Not a general Unicode category check (no dependency for
/// one, by workspace policy): the formatting characters that actually collide
/// with an ASCII identifier, named individually so the list is auditable.
const UNSPELLABLE: [char; 7] = [
    '\u{200b}', // ZERO WIDTH SPACE
    '\u{200c}', // ZERO WIDTH NON-JOINER
    '\u{200d}', // ZERO WIDTH JOINER
    '\u{200e}', // LEFT-TO-RIGHT MARK
    '\u{200f}', // RIGHT-TO-LEFT MARK
    '\u{00ad}', // SOFT HYPHEN
    '\u{feff}', // ZERO WIDTH NO-BREAK SPACE (BOM)
];

/// Guard for MINTING a namespace — a locally authored grain write, the one
/// operation that brings a namespace into existence rather than naming one
/// that already does.
///
/// Namespaces stay opaque strings (ARCHITECTURE.md, "Namespace prefix scopes
/// widen reads only"): a host may spell its hierarchy `org.sales.emea`,
/// `agent:authz` or 部門:営業, and none of that is this crate's business. What
/// a namespace may not be is **unspellable** — carrying whitespace, a control
/// character, or an invisible formatting character. Such a name cannot be
/// typed back at `--ns`, read off a diff, or told apart from the name it was
/// meant to be, and a write is not refused for it anywhere downstream: the
/// grain lands, the registry gains a row, and every reader that names the
/// intended namespace sees nothing.
///
/// That is not hypothetical. A bad substitution in a benchmark harness turned
/// `"agent:harness"` into `"age, build_messagesnt:harness"`; twelve hours of
/// held-out evaluations were journaled into it, the loop found no runs under
/// the namespace it reads, recorded no verdict, and proposed no revert for a
/// lesson that had cost the agent every exact match it had. Nothing failed —
/// which is the whole problem, and why this is a refusal and not a warning.
///
/// Read surfaces deliberately do NOT enforce this ([`require_exact_ns`] is
/// unchanged): a file written before the rule must stay readable, erasable
/// and disclosable under whatever name it used, or the rule would strand the
/// very data it exists to keep findable. Replication replay is exempt for the
/// same reason the `*` reservation exempts it.
pub fn require_writable_ns(ns: &str) -> Result<()> {
    require_exact_ns("a grain write", ns)?;
    let bad = ns
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || c.is_control() || UNSPELLABLE.contains(c));
    if let Some((at, c)) = bad {
        return Err(AreevError::Validation(format!(
            "a grain write takes a spellable namespace (got \"{}\": U+{:04X} at byte {at}): \
             a namespace is an identifier, and whitespace or an invisible character in one is \
             a splice, a quoting accident or a bad paste. It would be accepted everywhere and \
             found nowhere — grains written under it are invisible to every reader that names \
             the namespace you meant",
            ns.escape_debug(),
            c as u32
        )));
    }
    Ok(())
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
    fn writable_ns_accepts_the_names_hosts_actually_use() {
        for ok in [
            "",
            "caller",
            "agent:harness",
            "org.sales.emea",
            "claude-code",
            "deal.energy.42",
            "retention:org.sales",
            "部門:営業",
            "org→x", // an arbitrary separator stays the host's business
        ] {
            assert!(require_writable_ns(ok).is_ok(), "{ok:?} should be writable");
        }
    }

    #[test]
    fn writable_ns_refuses_the_unspellable() {
        // The one that shipped: a bad substitution spliced an import fragment
        // into the constant, and every write under it was accepted in silence.
        let err = require_writable_ns("age, build_messagesnt:harness").unwrap_err();
        assert!(err.to_string().starts_with("VAL-E001"), "{err}");
        assert!(err.to_string().contains("U+0020"), "names the character: {err}");

        for bad in [
            "agent harness",  // space
            "agent\tharness", // tab
            "agent\nharness", // newline
            " caller",        // leading
            "caller ",        // trailing
            " ",              // whitespace only
            "agent\u{200b}harness", // zero width space — looks identical
            "agent\u{feff}harness", // BOM
            "agent\u{00ad}harness", // soft hyphen
            "agent\u{0007}harness", // control
        ] {
            let err = require_writable_ns(bad).unwrap_err();
            assert!(
                matches!(err, AreevError::Validation(_)),
                "{bad:?} should be a validation error, got {err:?}"
            );
        }
    }

    #[test]
    fn writable_ns_still_refuses_the_reserved_star() {
        assert!(require_writable_ns("org.*").is_err());
        assert!(require_writable_ns("o*rg").is_err());
    }

    #[test]
    fn writable_ns_error_does_not_leak_a_raw_control_character() {
        // The message quotes the namespace back; escaped, or a name carrying a
        // newline or an escape sequence would forge lines in the log that
        // records the refusal.
        let msg = require_writable_ns("a\nb\u{1b}[31m").unwrap_err().to_string();
        assert!(!msg.contains('\n'), "no raw newline: {msg:?}");
        assert!(!msg.contains('\u{1b}'), "no raw escape: {msg:?}");
        assert!(msg.contains("\\n"), "escaped instead: {msg:?}");
    }

    #[test]
    fn read_surfaces_still_accept_a_legacy_unspellable_name() {
        // A file written before the rule must stay readable, erasable and
        // disclosable under the name it used — otherwise the rule strands the
        // data it exists to keep findable.
        assert!(require_exact_ns("forget_subject", "age, build_messagesnt:harness").is_ok());
        assert!(require_exact_ns("subject_report", "agent harness").is_ok());
        assert!(NsScope::parse("agent harness").is_ok());
    }

    #[test]
    fn require_exact_refuses_patterns() {
        assert!(require_exact_ns("latest", "org").is_ok());
        let err = require_exact_ns("PURGE", "org.*").unwrap_err();
        assert!(err.to_string().starts_with("VAL-E001"), "{err}");
        assert!(require_exact_ns("forget_subject", "o*rg").is_err());
    }
}
