//! Tier C limits, exercised with real modules.
//!
//! Written in WAT so each test says what shape of guest it defends against,
//! rather than hiding it in a byte array.

use areev_sandbox::{run, Limits, SandboxError};

/// A guest that emits a fixed JSON object.
const ECHO: &str = r#"
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 4)
  (global $bump (mut i32) (i32.const 1024))
  (func (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  (data (i32.const 16) "{\22ok\22:true}")
  (func (export "run") (param $ptr i32) (param $len i32)
    (call $emit (i32.const 16) (i32.const 11))))
"#;

/// A guest that loops forever.
const SPIN: &str = r#"
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 4)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "run") (param i32) (param i32)
    (loop $forever (br $forever))))
"#;

/// A guest that asks for WASI.
const WANTS_WASI: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1 4)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "run") (param i32) (param i32)))
"#;

/// A guest that returns without emitting.
const SILENT: &str = r#"
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 4)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "run") (param i32) (param i32)))
"#;

/// A guest declaring far more linear memory than the ceiling allows.
const GREEDY_MEMORY: &str = r#"
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 30000)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "run") (param i32) (param i32)))
"#;

fn wasm(src: &str) -> Vec<u8> {
    wat::parse_str(src).expect("valid WAT")
}

#[test]
fn a_well_formed_guest_runs_and_reports_its_fuel() {
    let out = run(&wasm(ECHO), &serde_json::json!({ "x": 1 }), &Limits::default()).unwrap();
    assert_eq!(out.output, serde_json::json!({ "ok": true }));
    assert!(out.fuel_used > 0, "fuel accounting must actually count");
}

#[test]
fn fuel_use_is_deterministic() {
    // The property that makes a Tier C tool re-execution-provable: same module,
    // same input, same cost.
    let m = wasm(ECHO);
    let a = run(&m, &serde_json::json!({ "x": 1 }), &Limits::default()).unwrap();
    let b = run(&m, &serde_json::json!({ "x": 1 }), &Limits::default()).unwrap();
    assert_eq!(a.fuel_used, b.fuel_used);
}

#[test]
fn an_infinite_loop_is_stopped_by_fuel_rather_than_hanging() {
    let limits = Limits { fuel: 100_000, ..Default::default() };
    let e = run(&wasm(SPIN), &serde_json::Value::Null, &limits).unwrap_err();
    assert!(matches!(e, SandboxError::FuelExhausted), "got {e}");
}

#[test]
fn a_module_that_asks_for_wasi_is_refused_at_instantiation() {
    // Refused by NAME before it starts, not trapped mid-call: a module of the
    // wrong shape should be told so, not allowed to begin and fail somewhere
    // the reason is harder to see.
    let e = run(&wasm(WANTS_WASI), &serde_json::Value::Null, &Limits::default()).unwrap_err();
    match e {
        SandboxError::ForbiddenImport { module, name } => {
            assert_eq!(module, "wasi_snapshot_preview1");
            assert_eq!(name, "fd_write");
        }
        other => panic!("expected ForbiddenImport, got {other}"),
    }
}

#[test]
fn a_guest_that_never_emits_is_an_error_not_an_empty_result() {
    // Silence must not read as "no result": that is how a broken extractor
    // looks exactly like a document with nothing in it.
    let e = run(&wasm(SILENT), &serde_json::Value::Null, &Limits::default()).unwrap_err();
    assert!(matches!(e, SandboxError::Trap(ref w) if w.contains("emit")), "got {e}");
}

#[test]
fn a_module_declaring_more_memory_than_the_ceiling_is_refused() {
    // 30000 pages is ~1.9 GiB. Refused on the DECLARED maximum, because wasmi
    // grows on demand and a check on current use would come too late.
    let e = run(&wasm(GREEDY_MEMORY), &serde_json::Value::Null, &Limits::default()).unwrap_err();
    assert!(matches!(e, SandboxError::Module(ref w) if w.contains("page")), "got {e}");
}

#[test]
fn an_oversized_module_is_refused_before_the_decoder_sees_it() {
    // A parse bomb does its damage inside the decoder, so a cap applied
    // afterwards has already lost.
    let limits = Limits { max_module_bytes: 64, ..Default::default() };
    let e = run(&wasm(ECHO), &serde_json::Value::Null, &limits).unwrap_err();
    assert!(matches!(e, SandboxError::Module(ref w) if w.contains("cap")), "got {e}");
}

#[test]
fn garbage_is_rejected_rather_than_panicking() {
    let e =
        run(b"not a wasm module at all", &serde_json::Value::Null, &Limits::default()).unwrap_err();
    assert!(matches!(e, SandboxError::Module(_)), "got {e}");
}

#[test]
fn a_raised_fuel_budget_lets_a_heavier_guest_finish() {
    // Fuel exhaustion is a budget the host set, not the guest being wrong — so
    // raising it must be the fix, and it must work.
    let m = wasm(ECHO);
    let stingy = Limits { fuel: 1, ..Default::default() };
    assert!(matches!(run(&m, &serde_json::Value::Null, &stingy), Err(SandboxError::FuelExhausted)));
    assert!(run(&m, &serde_json::Value::Null, &Limits::default()).is_ok());
}

/// `areev::alloc` is a guest EXPORT, not an import — a module that tries to
/// IMPORT it is refused at instantiation, and the refusal message says which
/// way round the contract goes (#86: the old message claimed alloc was
/// importable, which walked authors straight into this refusal).
#[test]
fn a_module_that_imports_alloc_is_refused_and_the_message_names_the_contract() {
    const IMPORTS_ALLOC: &str = r#"
(module
  (import "areev" "alloc" (func $alloc (param i32) (result i32)))
  (import "areev" "emit" (func $emit (param i32 i32)))
  (memory (export "memory") 1 4)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "run") (param i32) (param i32)))
"#;
    let e = run(&wasm(IMPORTS_ALLOC), &serde_json::Value::Null, &Limits::default()).unwrap_err();
    match &e {
        SandboxError::ForbiddenImport { module, name } => {
            assert_eq!(module, "areev");
            assert_eq!(name, "alloc");
        }
        other => panic!("expected ForbiddenImport, got {other}"),
    }
    let msg = e.to_string();
    assert!(msg.contains("EXPORT"), "the message must say alloc is an export: {msg}");
    assert!(!msg.contains("only areev::alloc and"), "the stale claim must be gone: {msg}");
}
