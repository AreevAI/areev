//! areev-core — OMS grain types, the .mg binary format, canonical
//! serialization, and content addressing. Licensed under MIT OR Apache-2.0.

pub mod anon;
pub mod authz;
pub mod b64;
pub mod error;
pub mod format;
pub mod ns;
pub mod proc;
pub mod time;
pub mod types;

pub use error::{Hash, AreevError, Result};
pub use format::*;
pub use types::*;
