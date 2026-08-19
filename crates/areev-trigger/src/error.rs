//! `TRG-Ennn` error codes.
//!
//! Every user-facing error carries a stable code as the **leading token of its
//! `Display` string**, plus a `code()` method, so a reported code alone locates
//! the variant. Codes are **append-only** — never renumber or reuse one.

use std::fmt;

#[derive(Debug)]
pub enum TriggerError {
    /// TRG-E001 — the declaration cannot fire as written.
    Malformed { what: String },
    /// TRG-E002 — `workflow` does not resolve to a Workflow grain.
    UnresolvedWorkflow { hash: String, why: String },
    /// TRG-E003 — a trigger is due but the host configured no connector command.
    ///
    /// Loud on purpose, matching how a host tool with no `--tool-cmd` refuses
    /// rather than quietly succeeding: a poll that silently does nothing looks
    /// exactly like a source with no new items.
    NoConnector { trigger: String, connector: String },
    /// TRG-E004 — the connector failed: non-zero exit, timeout, or non-JSON.
    ConnectorFailed { trigger: String, detail: String },
    /// TRG-E005 — claim lost: the lease expired and another evaluator took over.
    ClaimLost { trigger: String },
    /// TRG-E006 — the cron expression is invalid, or names a timezone this
    /// release refuses.
    BadSchedule { expr: String, why: String },
    /// TRG-E007 — unknown `trigger render` target.
    UnknownTarget { target: String },
    /// TRG-E008 — a composite predicate references a member it does not declare.
    UnknownMember { trigger: String, member: String },
    /// TRG-E009 — the connector tried to reach a host outside its allowlist.
    EgressRefused { trigger: String, host: String },
    /// TRG-E010 — the store refused or failed underneath us.
    Storage { detail: String },
}

impl TriggerError {
    pub fn code(&self) -> &'static str {
        match self {
            TriggerError::Malformed { .. } => "TRG-E001",
            TriggerError::UnresolvedWorkflow { .. } => "TRG-E002",
            TriggerError::NoConnector { .. } => "TRG-E003",
            TriggerError::ConnectorFailed { .. } => "TRG-E004",
            TriggerError::ClaimLost { .. } => "TRG-E005",
            TriggerError::BadSchedule { .. } => "TRG-E006",
            TriggerError::UnknownTarget { .. } => "TRG-E007",
            TriggerError::UnknownMember { .. } => "TRG-E008",
            TriggerError::EgressRefused { .. } => "TRG-E009",
            TriggerError::Storage { .. } => "TRG-E010",
        }
    }
}

impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TriggerError::Malformed { what } => {
                write!(f, "TRG-E001: malformed trigger declaration: {what}")
            }
            TriggerError::UnresolvedWorkflow { hash, why } => {
                write!(f, "TRG-E002: workflow {hash} does not resolve: {why}")
            }
            TriggerError::NoConnector { trigger, connector } => write!(
                f,
                "TRG-E003: trigger {trigger} is due but no connector command is configured \
                 for '{connector}' — pass --connector-cmd, or the poll would silently do nothing"
            ),
            TriggerError::ConnectorFailed { trigger, detail } => {
                write!(f, "TRG-E004: connector for trigger {trigger} failed: {detail}")
            }
            TriggerError::ClaimLost { trigger } => write!(
                f,
                "TRG-E005: claim on trigger {trigger} was lost — its lease expired and another \
                 evaluator took over; this firing's release was refused"
            ),
            TriggerError::BadSchedule { expr, why } => {
                write!(f, "TRG-E006: schedule {expr:?} is unusable: {why}")
            }
            TriggerError::UnknownTarget { target } => {
                write!(f, "TRG-E007: unknown render target '{target}'")
            }
            TriggerError::UnknownMember { trigger, member } => write!(
                f,
                "TRG-E008: composite trigger {trigger} references member '{member}' \
                 which it does not declare"
            ),
            TriggerError::EgressRefused { trigger, host } => write!(
                f,
                "TRG-E009: connector for trigger {trigger} tried to reach '{host}', \
                 which is outside its declared allowed_outbound_hosts"
            ),
            TriggerError::Storage { detail } => write!(f, "TRG-E010: store: {detail}"),
        }
    }
}

impl std::error::Error for TriggerError {}

pub type Result<T> = std::result::Result<T, TriggerError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<TriggerError> {
        vec![
            TriggerError::Malformed { what: "x".into() },
            TriggerError::UnresolvedWorkflow { hash: "h".into(), why: "w".into() },
            TriggerError::NoConnector { trigger: "t".into(), connector: "c".into() },
            TriggerError::ConnectorFailed { trigger: "t".into(), detail: "d".into() },
            TriggerError::ClaimLost { trigger: "t".into() },
            TriggerError::BadSchedule { expr: "e".into(), why: "w".into() },
            TriggerError::UnknownTarget { target: "t".into() },
            TriggerError::UnknownMember { trigger: "t".into(), member: "m".into() },
            TriggerError::EgressRefused { trigger: "t".into(), host: "h".into() },
            TriggerError::Storage { detail: "d".into() },
        ]
    }

    #[test]
    fn the_code_leads_the_display_string() {
        // A reported code alone has to locate the variant, so it must be the
        // first token — not buried mid-sentence where a log grep misses it.
        for e in all() {
            let s = e.to_string();
            assert!(s.starts_with(e.code()), "{s} should start with {}", e.code());
        }
    }

    #[test]
    fn codes_are_unique_and_contiguous() {
        let codes: Vec<&str> = all().iter().map(|e| e.code()).collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "codes must be unique");
        for (i, c) in codes.iter().enumerate() {
            assert_eq!(*c, format!("TRG-E{:03}", i + 1), "codes are append-only and dense");
        }
    }
}
