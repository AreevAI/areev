//! Tier C: run a pure wasm32 module with hard limits and a frozen import set.
//!
//! ## What this defends, and what it does not
//!
//! This protects **the host from the tool**: a module cannot open a socket,
//! touch the filesystem, read an environment variable, see a clock, or run
//! forever. It is real isolation for pure-compute work — parsing, extraction,
//! classification, scoring.
//!
//! It is **not credential protection**, and the two must never be described as
//! substitutes. A connector that legitimately holds an OAuth token and makes
//! outbound calls is not made safer by being in a sandbox; that is what the
//! egress allowlist and the credential broker are for. By this module's own
//! design a Tier C module cannot make a network call at all, so a Gmail
//! connector will never be one.
//!
//! ## The blessed format
//!
//! A pure `wasm32` core module. **No WASI.** The import set is frozen at two
//! functions, and anything else in the import section is refused at
//! instantiation rather than trapped at call time:
//!
//! - `areev.alloc(len: i32) -> i32` — the guest allocates a buffer in its own
//!   linear memory and returns the offset. Implemented by the guest; the host
//!   calls it to place input.
//! - `areev.emit(ptr: i32, len: i32)` — the guest hands back its JSON result.
//!
//! JSON in, JSON out, over linear memory. The same shape as every other seam,
//! so a Tier C tool and a subprocess tool look identical from the outside.
//!
//! ## The limits, and why each one exists
//!
//! Fuel alone is not enough, which is the mistake worth not repeating: fuel
//! bounds *execution*, and a module can exhaust a host before executing a
//! single instruction.
//!
//! | Limit | Stops |
//! |---|---|
//! | module byte cap, **before decode** | a decompression/parse bomb |
//! | fuel | an infinite loop, deterministically |
//! | memory pages | a guest ballooning linear memory |
//! | frozen imports | reaching anything not on the list |
//! | no WASI | filesystem, network, clock, environment, randomness |

use serde::{Deserialize, Serialize};
use wasmi::{Config, Engine, Linker, Module, Store};

/// Largest module accepted, checked **before** the decoder sees it. A parse
/// bomb does its damage during decode, so a cap applied afterwards is a cap
/// applied too late.
pub const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;

/// Largest input or output payload.
pub const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Default linear-memory ceiling: 256 pages of 64 KiB = 16 MiB.
pub const DEFAULT_MAX_PAGES: u32 = 256;

/// Default fuel. Generous for parsing or extraction, and bounded.
pub const DEFAULT_FUEL: u64 = 200_000_000;

/// The only module namespace a Tier C guest may import from.
pub const IMPORT_MODULE: &str = "areev";

#[derive(Debug, Clone)]
pub struct Limits {
    pub fuel: u64,
    pub max_pages: u32,
    pub max_module_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            fuel: DEFAULT_FUEL,
            max_pages: DEFAULT_MAX_PAGES,
            max_module_bytes: MAX_MODULE_BYTES,
        }
    }
}

#[derive(Debug)]
pub enum SandboxError {
    /// The module is larger than the cap, or malformed.
    Module(String),
    /// The module imports something outside the frozen set.
    ForbiddenImport { module: String, name: String },
    /// The guest ran out of fuel.
    FuelExhausted,
    /// The guest exceeded its memory ceiling.
    MemoryExhausted,
    /// The guest trapped, or the contract was not met.
    Trap(String),
    /// A payload exceeded its cap.
    PayloadTooLarge(usize),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Module(w) => write!(f, "module rejected: {w}"),
            SandboxError::ForbiddenImport { module, name } => write!(
                f,
                "module imports {module}::{name}, which is not in the frozen import set — \
                 a Tier C module may import only {IMPORT_MODULE}::alloc and {IMPORT_MODULE}::emit"
            ),
            SandboxError::FuelExhausted => write!(f, "guest exhausted its fuel"),
            SandboxError::MemoryExhausted => write!(f, "guest exceeded its memory ceiling"),
            SandboxError::Trap(w) => write!(f, "guest trapped: {w}"),
            SandboxError::PayloadTooLarge(n) => write!(f, "payload of {n} bytes exceeds the cap"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// What the guest produced.
#[derive(Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub output: serde_json::Value,
    /// Fuel actually consumed. Deterministic for a given module and input,
    /// which is what makes a Tier C tool re-execution-provable.
    pub fuel_used: u64,
}

/// Host state: the buffer the guest emitted, and its cap.
#[derive(Default)]
struct HostState {
    emitted: Option<Vec<u8>>,
    too_large: bool,
}

/// Run `wasm` against `input`.
pub fn run(
    wasm: &[u8],
    input: &serde_json::Value,
    limits: &Limits,
) -> Result<Outcome, SandboxError> {
    // Before decode, on purpose: a parse bomb does its damage inside the
    // decoder, so a cap checked afterwards has already lost.
    if wasm.len() > limits.max_module_bytes {
        return Err(SandboxError::Module(format!(
            "{} bytes exceeds the {} byte cap",
            wasm.len(),
            limits.max_module_bytes
        )));
    }
    let payload = serde_json::to_vec(input)
        .map_err(|e| SandboxError::Module(format!("input is not serializable: {e}")))?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(SandboxError::PayloadTooLarge(payload.len()));
    }

    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm)
        .map_err(|e| SandboxError::Module(format!("will not decode: {e}")))?;

    // Refuse an unexpected import at instantiation rather than trapping at call
    // time. A module that asks for `wasi_snapshot_preview1::fd_write` should be
    // told it is the wrong shape, not allowed to start and fail later where the
    // reason is harder to see.
    for import in module.imports() {
        let (m, n) = (import.module(), import.name());
        if m != IMPORT_MODULE || !matches!(n, "emit") {
            return Err(SandboxError::ForbiddenImport {
                module: m.to_string(),
                name: n.to_string(),
            });
        }
    }

    let mut store = Store::new(&engine, HostState::default());
    store
        .set_fuel(limits.fuel)
        .map_err(|e| SandboxError::Module(format!("fuel: {e}")))?;

    let mut linker = <Linker<HostState>>::new(&engine);
    linker
        .func_wrap(
            IMPORT_MODULE,
            "emit",
            |mut caller: wasmi::Caller<'_, HostState>, ptr: i32, len: i32| {
                let Some(wasmi::Extern::Memory(mem)) = caller.get_export("memory") else {
                    return;
                };
                let len = len.max(0) as usize;
                if len > MAX_PAYLOAD_BYTES {
                    caller.data_mut().too_large = true;
                    return;
                }
                let mut buf = vec![0u8; len];
                if mem.read(&caller, ptr.max(0) as usize, &mut buf).is_ok() {
                    caller.data_mut().emitted = Some(buf);
                }
            },
        )
        .map_err(|e| SandboxError::Module(format!("linker: {e}")))?;

    // `instantiate_and_start` rather than the deprecated two-step: a module's
    // `start` function runs at instantiation, so it is guest code executing
    // before anything the host called. Fuel is already set above, which is what
    // bounds it — a start function is not a free pass to spin.
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .map_err(|e| match classify_ref(&e, &store) {
            SandboxError::Trap(_) => SandboxError::Module(format!("will not instantiate: {e}")),
            other => other,
        })?;

    // Memory ceiling: wasmi grows on demand, so the check is on the declared
    // maximum rather than on current use.
    if let Some(wasmi::Extern::Memory(mem)) = instance.get_export(&store, "memory") {
        let ty = mem.ty(&store);
        let declared_max = ty.maximum().unwrap_or(u64::MAX);
        if declared_max > u64::from(limits.max_pages) {
            return Err(SandboxError::Module(format!(
                "module declares up to {declared_max} memory pages, above the {} page ceiling — \
                 declare a maximum at or below it",
                limits.max_pages
            )));
        }
    }

    // Place the input where the guest asked for it.
    let alloc = instance
        .get_typed_func::<i32, i32>(&store, "alloc")
        .map_err(|_| SandboxError::Module("module exports no `alloc(i32) -> i32`".into()))?;
    let ptr = alloc
        .call(&mut store, payload.len() as i32)
        .map_err(|e| classify(e, &store))?;

    let Some(wasmi::Extern::Memory(mem)) = instance.get_export(&store, "memory") else {
        return Err(SandboxError::Module("module exports no linear memory".into()));
    };
    mem.write(&mut store, ptr.max(0) as usize, &payload)
        .map_err(|e| SandboxError::Trap(format!("writing input: {e}")))?;

    let run = instance
        .get_typed_func::<(i32, i32), ()>(&store, "run")
        .map_err(|_| SandboxError::Module("module exports no `run(i32, i32)`".into()))?;
    run.call(&mut store, (ptr, payload.len() as i32))
        .map_err(|e| classify(e, &store))?;

    if store.data().too_large {
        return Err(SandboxError::PayloadTooLarge(MAX_PAYLOAD_BYTES));
    }
    let emitted = store
        .data()
        .emitted
        .clone()
        .ok_or_else(|| SandboxError::Trap("guest never called areev::emit".into()))?;
    let output: serde_json::Value = serde_json::from_slice(&emitted)
        .map_err(|e| SandboxError::Trap(format!("guest output is not JSON: {e}")))?;

    let fuel_used = limits.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
    Ok(Outcome { output, fuel_used })
}

fn classify_ref(e: &wasmi::Error, store: &Store<HostState>) -> SandboxError {
    if store.get_fuel().unwrap_or(1) == 0 || e.to_string().contains("fuel") {
        return SandboxError::FuelExhausted;
    }
    SandboxError::Trap(e.to_string())
}

/// Distinguish "ran out of fuel" from a general trap.
///
/// Worth separating because they mean different things operationally: fuel
/// exhaustion is a budget the host set and may raise, a trap is the guest
/// being wrong.
fn classify(e: wasmi::Error, store: &Store<HostState>) -> SandboxError {
    if store.get_fuel().unwrap_or(1) == 0 {
        return SandboxError::FuelExhausted;
    }
    let text = e.to_string();
    if text.contains("fuel") {
        SandboxError::FuelExhausted
    } else if text.contains("memory") && text.contains("grow") {
        SandboxError::MemoryExhausted
    } else {
        SandboxError::Trap(text)
    }
}
