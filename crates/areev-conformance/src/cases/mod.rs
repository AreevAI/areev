//! The conformance cases, grouped by contract. Each is a plain `pub fn` over
//! `&dyn Backend` that panics on violation — the per-backend runners macro
//! them into `#[test]`s.

mod add_recall;
mod blobs_hybrid;
mod erasure;
mod heads_forks;
mod meta_registry;
mod ns_scope;
mod oplog_import;
mod read_only;
mod recall_purity;
mod run_journal;
mod supersede_forget;

pub use add_recall::*;
pub use blobs_hybrid::*;
pub use erasure::*;
pub use heads_forks::*;
pub use meta_registry::*;
pub use ns_scope::*;
pub use oplog_import::*;
pub use read_only::*;
pub use recall_purity::*;
pub use run_journal::*;
pub use supersede_forget::*;
