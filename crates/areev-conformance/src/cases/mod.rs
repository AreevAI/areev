//! The conformance cases, grouped by contract. Each is a plain `pub fn` over
//! `&dyn Backend` that panics on violation — the per-backend runners macro
//! them into `#[test]`s.

mod add_recall;
mod blobs_hybrid;
mod erasure;
mod heads_forks;
mod oplog_import;
mod run_journal;
mod supersede_forget;

pub use add_recall::*;
pub use blobs_hybrid::*;
pub use erasure::*;
pub use heads_forks::*;
pub use oplog_import::*;
pub use run_journal::*;
pub use supersede_forget::*;
