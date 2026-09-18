//! The `areev` binary's host-side library surface.
//!
//! The binary is the primary product; this library exists for the parts a
//! Rust HOST needs to call directly rather than by spawning a subprocess.
//!
//! Today that is [`pack`] (#315): a service that provisions a memory per
//! tenant and installs a versioned agent pack into it had to ship the
//! `areev` binary into its image, spawn `areev pack install … --format
//! json`, parse the stdout, and map string errors back to causes. `areev
//! pack` is now a PRINTER over these functions, so the CLI and the library
//! cannot drift on what a pack contains.

use std::collections::HashMap;

pub mod pack;

/// A flag's value, or `None`.
///
/// Duplicated from the binary rather than shared the other way round: the
/// library must not depend on the binary, and this is four lines.
pub(crate) fn flag(args: &HashMap<String, String>, k: &str) -> Option<String> {
    args.get(k).cloned()
}

/// A required flag, or a usage error naming it.
pub(crate) fn need(args: &HashMap<String, String>, k: &str) -> Result<String, String> {
    flag(args, k).ok_or_else(|| format!("--{k} is required"))
}
