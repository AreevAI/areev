//! The guest half of the Tier C contract, and the JSON shaping every blessed
//! tool needs — `no_std`, no dependencies, no allocator but our own.
//!
//! ## Why hand-rolled JSON
//!
//! A blessed blob is reviewed as bytes and pinned by address, so what it
//! carries is part of its security story. `serde_json` would pull an ecosystem
//! into a module whose whole job is to move four strings into a different
//! shape, and would triple an artifact a human is expected to read the
//! disassembly of. What is here instead is a *slicer*: it finds a member of a
//! top-level object and hands back the raw bytes of its value, so a caller's
//! `arguments` object travels into the request verbatim — never reserialized,
//! so there is no round-trip that could change it.
//!
//! It is not a validating parser and does not pretend to be. A malformed input
//! yields a missing member, and the tool then sends a request the broker (or
//! the upstream) refuses — the authorities that were going to decide anyway.
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod abi;
pub mod json;
pub mod jsonrpc;
