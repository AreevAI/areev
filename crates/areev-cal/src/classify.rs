//! The single source of truth for what kind of statement a piece of CAL is.
//!
//! Before this module, `areev-server` kept its own keyword-based
//! read-only sniff (`cal_body_is_read_only`) — a second, independent
//! classifier that any grammar growth would silently desynchronize. Every
//! consumer now classifies through [`classify`], whose match is exhaustive
//! **with no wildcard arm**: adding a `CalStatement` variant fails this
//! module's build until someone decides its class. That compile error is the
//! drift guard.
//!
//! The classes mirror the CAL 1.3 tier model (Tier 0/1 read/evolve, Tier 2
//! destroy, Tier 3 control) without claiming tier vocabulary for statements
//! that predate it.

use crate::ast::{BatchStmt, CalStatement};

/// What executing a statement can do to the memory, coarsest first.
/// Ordered so a mixed `BATCH` classifies as its most privileged entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatementClass {
    /// Pure reads: no grain written, nothing changed.
    Read,
    /// Append-only evolution: new grains, supersessions — never removal.
    Evolve,
    /// Host-config artifacts (templates, saved queries) — admin surface,
    /// but never memory grains.
    Control,
    /// Tombstones / erasure. The only class that removes anything.
    Destructive,
}

/// Classify one statement. Exhaustive by construction — never add a
/// wildcard arm here.
pub fn classify(stmt: &CalStatement) -> StatementClass {
    match stmt {
        CalStatement::Recall(_)
        | CalStatement::SetOp(_)
        | CalStatement::Exists(_)
        | CalStatement::Assemble(_)
        | CalStatement::History(_)
        | CalStatement::Explain(_)
        | CalStatement::Describe(_)
        | CalStatement::Coalesce(_) => StatementClass::Read,
        // A saved query's body is structurally read-only (enforced at
        // definition and execution), so running one is a read.
        CalStatement::RunQuery(_) => StatementClass::Read,
        // Wave-2 reads: as-of, the run↔memory join, reverse provenance,
        // fork listing.
        CalStatement::EntityAt(_)
        | CalStatement::RunTrace(_)
        | CalStatement::RunsTouching(_)
        | CalStatement::DerivedFrom(_)
        | CalStatement::ShowForks(_)
        | CalStatement::Related(_)
        | CalStatement::Novelty(_) => StatementClass::Read,
        // The DSAR read: the erasure selector in show-me mode — a pure
        // read, deliberately available wherever RECALL is.
        CalStatement::ReportSubject(_) => StatementClass::Read,
        // A merge supersedes every open tip — evolution, never removal.
        CalStatement::Merge(_) => StatementClass::Evolve,
        CalStatement::Add(_)
        | CalStatement::AddWorkflow(_)
        | CalStatement::Supersede(_)
        | CalStatement::SupersedeWorkflow(_)
        | CalStatement::Accumulate(_)
        | CalStatement::Revert(_)
        | CalStatement::Remember(_) => StatementClass::Evolve,
        CalStatement::DefineTemplate(_)
        | CalStatement::DropTemplate(_)
        | CalStatement::DefineQuery(_)
        | CalStatement::DropQuery(_) => StatementClass::Control,
        // Tier-3 DCL: append-only writes to the authz namespace, admin-gated
        // — control, never destructive. SHOW GRANTS is a read.
        CalStatement::Grant(_) | CalStatement::Revoke(_) => StatementClass::Control,
        CalStatement::ShowGrants(_) => StatementClass::Read,
        // Governance: engine-gated lifecycle transitions and the analysis
        // trigger — control. (An APPLY may execute a destructive proposal,
        // but that path re-checks delete/erase inside the engine — the
        // statement itself is control.)
        CalStatement::Approve(_)
        | CalStatement::Reject(_)
        | CalStatement::ApplyRec(_)
        | CalStatement::RollbackRec(_)
        | CalStatement::RunLoop(_) => StatementClass::Control,
        CalStatement::Forget(_) | CalStatement::Purge(_) => StatementClass::Destructive,
        CalStatement::Batch(b) => classify_batch(b),
    }
}

/// A batch is as privileged as its most privileged entry.
fn classify_batch(b: &BatchStmt) -> StatementClass {
    let positional = b.statements.iter().map(|e| classify(&e.statement));
    let labeled = b
        .labeled
        .iter()
        .flatten()
        .map(|(_, e)| classify(&e.statement));
    positional
        .chain(labeled)
        .max()
        .unwrap_or(StatementClass::Read)
}

/// Parse-and-classify convenience for hosts gating raw query text (the ui's
/// token-less read-only mode). **Fail closed**: text that does not parse is
/// not read-only.
pub fn query_is_read_only(input: &str) -> bool {
    match crate::parser::parse(input) {
        Ok(q) => classify(&q.statement) == StatementClass::Read,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_of(q: &str) -> StatementClass {
        classify(&crate::parser::parse(q).expect(q).statement)
    }

    #[test]
    fn reads_classify_as_read() {
        assert_eq!(class_of(r#"RECALL facts WHERE subject = "john""#), StatementClass::Read);
        assert_eq!(
            class_of("HISTORY OF sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d"),
            StatementClass::Read
        );
        assert_eq!(class_of("DESCRIBE CAPABILITIES"), StatementClass::Read);
    }

    #[test]
    fn evolve_and_control_and_destroy_classify_correctly() {
        assert_eq!(
            class_of(r#"ADD fact SET subject = "j" SET relation = "likes" SET object = "rust" REASON "t""#),
            StatementClass::Evolve
        );
        assert_eq!(
            class_of("FORGET sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d"),
            StatementClass::Destructive
        );
        assert_eq!(
            class_of(r#"DEFINE TEMPLATE brief AS "{{content}}""#),
            StatementClass::Control
        );
        assert_eq!(class_of(r#"DROP TEMPLATE "brief""#), StatementClass::Control);
    }

    #[test]
    fn batch_takes_its_most_privileged_entry() {
        assert_eq!(
            class_of(r#"BATCH { RECALL facts WHERE subject = "a" ; RECALL facts WHERE subject = "b" }"#),
            StatementClass::Read
        );
        assert_eq!(
            class_of(
                r#"BATCH { RECALL facts WHERE subject = "a" ; ADD fact SET subject = "j" SET relation = "r" SET object = "o" REASON "t" }"#
            ),
            StatementClass::Evolve
        );
    }

    #[test]
    fn query_is_read_only_fails_closed() {
        assert!(query_is_read_only(r#"RECALL facts WHERE subject = "john""#));
        assert!(!query_is_read_only(
            r#"ADD fact SET subject = "j" SET relation = "r" SET object = "o" REASON "t""#
        ));
        // Unparseable text is not read-only.
        assert!(!query_is_read_only("DELETE FROM facts"));
        assert!(!query_is_read_only(""));
    }

    #[test]
    fn class_ordering_puts_destructive_on_top() {
        assert!(StatementClass::Read < StatementClass::Evolve);
        assert!(StatementClass::Evolve < StatementClass::Control);
        assert!(StatementClass::Control < StatementClass::Destructive);
    }
}
