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
//! egress allowlist and the credential broker are for.
//!
//! ## Two runtimes, because there are two determinism stories
//!
//! Until 1.6.0 there was one, and its rule was absolute: *a Tier C module
//! cannot make a network call at all, so a Gmail connector will never be one*.
//! That was true, and it was also the reason Tier C half-delivered on its own
//! promise — it is the only tier that produces a persistable,
//! content-addressed tool, and it forbade exactly the I/O every real agent
//! needs. So an I/O tool had to be a native blob (persisted, *not sandboxed —
//! it runs as you*) or a host script (sandboxed by nothing, and outside the
//! memory entirely).
//!
//! [`Limits::allow_fetch`] splits the tier in two (#101):
//!
//! | Runtime | Imports | Determinism |
//! |---|---|---|
//! | `wasm32-areev` | `areev::emit` | pure — **re-execution-provable** |
//! | `wasm32-areev-io` | `+ areev::fetch` | deterministic **modulo journaled effects** |
//!
//! The isolation claim is *strengthened*, not weakened. The guest still has no
//! socket, no credential, no clock, no environment: it gets one more
//! unforgeable capability to **ask the host**, and the host enforces policy and
//! records everything. Three trust levels, credentials confined to the
//! innermost:
//!
//! ```text
//! guest wasm ──areev::fetch(req)──▶ THIS BINARY (trusted Rust half)
//!                                        │ POST loopback, with AREEV_EGRESS_TOKEN
//!                                        ▼
//!                                 engine broker (holds credentials)
//!                                   token→caller · grant · declaration ·
//!                                   allowlist · method · attach credential · perform
//!                                        ▼
//!                                   real upstream
//! ```
//!
//! This binary holds a revocable **broker token**, never a credential — the
//! same shape proxy-wasm and Cloudflare Workers use ("the platform performs the
//! fetch, the guest never gets a socket") and the same one Spin's runtime-config
//! and Extism use for secrets ("the guest holds a label, the host resolves it").
//! It needed no new IPC: the engine already injected `AREEV_EGRESS_URL` +
//! `AREEV_EGRESS_TOKEN` into this process for uniformity, inert only because
//! the *guest* could not reach them.
//!
//! ## The blessed format
//!
//! A pure `wasm32` core module. **No WASI.** The import set is frozen, and
//! anything outside it is refused at instantiation rather than trapped at call
//! time:
//!
//! - `areev.emit(ptr: i32, len: i32)` — the guest hands back its JSON result.
//! - `areev.fetch(ptr: i32, len: i32) -> i32` — **only** under
//!   [`Limits::allow_fetch`]. A module that imports it without a capability
//!   declaration is refused by name before one instruction runs.
//! - `alloc(len: i32) -> i32` is a guest **export** (with `run` and
//!   `memory`), not an import: the guest allocates a buffer in its own
//!   linear memory and the host calls it to place input — and, for `fetch`,
//!   to place the response.
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
//! | response byte cap | an upstream ballooning the guest's memory |

use serde::{Deserialize, Serialize};
use wasmi::{Config, Engine, Linker, Module, Store};

mod http;

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

/// Default ceiling on ONE brokered response, mirroring the broker's own.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Limits {
    pub fuel: u64,
    pub max_pages: u32,
    pub max_module_bytes: usize,
    /// Link `areev::fetch` into the guest's import set (#101).
    ///
    /// `false` is pure Tier C and the default, so a module that imports
    /// `fetch` on a host that did not opt in is refused at instantiation, by
    /// name, before one instruction runs. The engine derives this from the
    /// MANIFEST-pinned runtime (`wasm32-areev-io`), never from the module —
    /// a blob cannot talk its way into a gate.
    pub allow_fetch: bool,
    /// Largest single brokered response handed back to the guest. Overruns
    /// are errors, never truncation.
    pub max_response_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            fuel: DEFAULT_FUEL,
            max_pages: DEFAULT_MAX_PAGES,
            max_module_bytes: MAX_MODULE_BYTES,
            allow_fetch: false,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

/// Where this process sends a guest's `areev::fetch`, read from the
/// environment the engine injected.
///
/// Absent when the host configured no broker. A module whose runtime admits
/// `fetch` but whose host wired no broker gets a typed error from every call
/// rather than a silent success — the engine also refuses that pairing at
/// dispatch, so reaching this is a misconfiguration, not a path.
#[derive(Debug, Clone)]
struct BrokerEnv {
    url: String,
    token: String,
}

impl BrokerEnv {
    fn from_env() -> Option<BrokerEnv> {
        let url = std::env::var("AREEV_EGRESS_URL").ok().filter(|s| !s.is_empty())?;
        let token = std::env::var("AREEV_EGRESS_TOKEN").ok().filter(|s| !s.is_empty())?;
        Some(BrokerEnv { url, token })
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
                 a Tier C module may import only {IMPORT_MODULE}::emit ({IMPORT_MODULE}::alloc \
                 is a guest EXPORT the host calls, not an import)"
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
    /// Fuel actually consumed. Deterministic for a given module and input —
    /// which is what makes a PURE Tier C tool re-execution-provable. A module
    /// that made brokered calls is deterministic only modulo those calls, and
    /// `fetches` is how you tell the two apart from the outcome alone.
    pub fuel_used: u64,
    /// Brokered calls the guest made. `0` for every pure module, so this
    /// serializes identically to a pre-1.6 outcome.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fetches: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Host state: the buffer the guest emitted, and its cap.
#[derive(Default)]
struct HostState {
    emitted: Option<Vec<u8>>,
    too_large: bool,
    /// Where `areev::fetch` goes. `None` when the host wired no broker.
    broker: Option<BrokerEnv>,
    max_response_bytes: usize,
    /// How many brokered calls the guest has made. Reported alongside fuel so
    /// an operator can see the shape of a module's I/O, not just its compute.
    fetches: u32,
    /// A `fetch` is in flight. "One outstanding call, synchronous" is the
    /// contract, and this is what ENFORCES it: placing the response calls the
    /// guest's `alloc`, which is guest code, which could call `fetch` again —
    /// and each nesting level is a native host frame plus a broker round trip,
    /// so unchecked reentrancy is a stack-exhaustion primitive with I/O
    /// amplification. A reentrant call returns `-1` immediately, before any
    /// broker traffic and before any `alloc`.
    fetching: bool,
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
    //
    // `fetch` extends that philosophy from "which imports" to "which
    // capabilities": on a host that did not opt in, a module importing it is
    // refused by NAME before one instruction runs, rather than instantiating
    // and discovering at the first call that it can reach nothing.
    for import in module.imports() {
        let (m, n) = (import.module(), import.name());
        let permitted = m == IMPORT_MODULE
            && (n == "emit" || (n == "fetch" && limits.allow_fetch));
        if !permitted {
            return Err(SandboxError::ForbiddenImport {
                module: m.to_string(),
                name: n.to_string(),
            });
        }
    }

    let mut store = Store::new(
        &engine,
        HostState {
            // Read once, here, so the guest's calls cannot observe a mutating
            // environment — and so a module with no `fetch` import never
            // touches these at all.
            broker: if limits.allow_fetch { BrokerEnv::from_env() } else { None },
            max_response_bytes: limits.max_response_bytes,
            ..HostState::default()
        },
    );
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

    // Linked only when the host opted in. Absent otherwise, which is what
    // makes the `ForbiddenImport` above the *only* way a pure module can be
    // told about `fetch` — there is no second path where it exists but is
    // inert.
    if limits.allow_fetch {
        linker
            .func_wrap(
                IMPORT_MODULE,
                "fetch",
                |caller: wasmi::Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                    host_fetch(caller, ptr, len)
                },
            )
            .map_err(|e| SandboxError::Module(format!("linker: {e}")))?;
    }

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
    let fetches = store.data().fetches;
    Ok(Outcome { output, fuel_used, fetches })
}

/// `areev::fetch(ptr, len) -> i32` — the guest's one gate to the network.
///
/// ## The ABI
///
/// **In**: `[ptr, ptr+len)` in guest memory is a UTF-8 JSON request, exactly
/// the shape the broker takes — this binary forwards it rather than
/// translating, so the guest ABI *is* the broker ABI and there is no place for
/// a translation bug:
///
/// ```json
/// { "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
///   "method": "GET", "credential": "gmail", "body": null }
/// ```
///
/// The guest names a credential; it can never name a *value* it was not given,
/// and no value ever crosses back.
///
/// **Out**: a non-negative return is a pointer into guest memory to
/// `[u32 little-endian length][JSON bytes]`. One `i32` cannot carry both a
/// pointer and a length, and a second import to fetch the length would be a
/// second gate — so the response is self-describing instead. The buffer comes
/// from the guest's own `alloc` export, so the guest owns and frees it.
///
/// A **negative** return means the host could not place a response at all
/// (no `alloc` export, allocation refused, memory unwritable). Everything
/// else — a refusal, a broker error, an upstream 500 — arrives as ordinary
/// JSON with an `error` key, so the guest has one shape to handle:
///
/// ```json
/// { "status": 200, "body": "…" }          // it worked
/// { "error": "…", "code": "RUN-E022" }    // policy said no
/// ```
///
/// ## Synchronous, one call at a time
///
/// No concurrency in v1, deliberately. Completion-order nondeterminism is
/// exactly what a durable-execution engine spends enormous machinery taming,
/// and it would add an ordering side channel to a boundary whose whole point
/// is that it leaks nothing. One outstanding call keeps framing trivial: the
/// guest is never running while a response is in flight.
fn host_fetch(mut caller: wasmi::Caller<'_, HostState>, ptr: i32, len: i32) -> i32 {
    // Reentrancy gate FIRST, before any work and before any path that could
    // call back into guest code. `reply` runs the guest's `alloc`, and a guest
    // whose `alloc` calls `fetch` would otherwise recurse this native frame —
    // plus one broker round trip per level — until the stack dies. The flag
    // stays set through `reply` for exactly that reason, and the reentrant
    // call gets a bare `-1`: an error the guest can see, produced without
    // touching its allocator.
    if caller.data().fetching {
        return -1;
    }
    caller.data_mut().fetching = true;
    let code = host_fetch_inner(&mut caller, ptr, len);
    caller.data_mut().fetching = false;
    code
}

fn host_fetch_inner(caller: &mut wasmi::Caller<'_, HostState>, ptr: i32, len: i32) -> i32 {
    let Some(wasmi::Extern::Memory(mem)) = caller.get_export("memory") else {
        return -1;
    };
    let len = len.max(0) as usize;
    if len > MAX_PAYLOAD_BYTES {
        return reply(
            caller,
            &json_error(&format!(
                "request of {len} bytes exceeds the {MAX_PAYLOAD_BYTES}-byte payload cap"
            )),
        );
    }
    let mut request = vec![0u8; len];
    if mem.read(&*caller, ptr.max(0) as usize, &mut request).is_err() {
        return -1;
    }

    let (broker, max_response_bytes) = {
        let st = caller.data();
        (st.broker.clone(), st.max_response_bytes)
    };
    caller.data_mut().fetches = caller.data().fetches.saturating_add(1);

    let body = match broker {
        None => json_error(
            "this module's runtime admits areev::fetch, but the host wired no credential \
             broker — configure --allow-host/--credential/--tool-egress",
        ),
        Some(b) => match http::post_json(&b.url, &b.token, &request, max_response_bytes) {
            Ok(bytes) => bytes,
            Err(e) => json_error(&e),
        },
    };
    reply(caller, &body)
}

/// Place `body` in guest memory as `[u32 LE len][bytes]` and return the
/// pointer, or a negative code if it cannot be placed.
fn reply(caller: &mut wasmi::Caller<'_, HostState>, body: &[u8]) -> i32 {
    // The guest allocates: the host writing into a buffer the guest did not
    // hand out would be the host corrupting the guest's heap. This is a
    // reentrant call into guest code and it spends the SAME fuel budget, so a
    // module cannot use its allocator as a free-execution channel.
    let Some(wasmi::Extern::Func(alloc)) = caller.get_export("alloc") else {
        return -1;
    };
    let Ok(alloc) = alloc.typed::<i32, i32>(&caller) else {
        return -1;
    };
    let total = match i32::try_from(body.len() + 4) {
        Ok(n) => n,
        Err(_) => return -1,
    };
    let Ok(ptr) = alloc.call(&mut *caller, total) else {
        return -1;
    };
    if ptr <= 0 {
        return -1;
    }
    let Some(wasmi::Extern::Memory(mem)) = caller.get_export("memory") else {
        return -1;
    };
    let at = ptr as usize;
    if mem.write(&mut *caller, at, &(body.len() as u32).to_le_bytes()).is_err() {
        return -1;
    }
    if mem.write(&mut *caller, at + 4, body).is_err() {
        return -1;
    }
    ptr
}

/// The one error shape the guest sees, so a policy refusal, a broker failure
/// and a malformed request are all handled the same way.
fn json_error(message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "error": message }))
        .unwrap_or_else(|_| br#"{"error":"unprintable"}"#.to_vec())
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
