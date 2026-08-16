//! Tool executor kind — the harness executor URI scheme so
//! Workflow/Tool grain blobs stay byte-identical across implementations.

/// Which side executes a bound tool. Wire strings are stable — renaming
/// breaks chain-replay verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ExecutorKind {
    /// Host-side inline execution (default).
    #[default]
    Host,
    /// The external caller executes the tool.
    Client,
}

impl ExecutorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Client => "client",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "host" => Some(Self::Host),
            "client" => Some(Self::Client),
            _ => None,
        }
    }
}
