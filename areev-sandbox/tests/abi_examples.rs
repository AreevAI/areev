//! The non-Rust authoring path (#340), exercised as the bytes that ship.
//!
//! `docs/sandbox-abi.md` is the guest contract written for someone who does
//! not read Rust, and `areev-tools/examples/` holds one reference module per
//! toolchain built from it — C (`zig cc` / `clang --target=wasm32`), Zig
//! (`wasm32-freestanding`) and AssemblyScript (`--runtime stub`). These tests
//! read the COMMITTED `examples/dist/*.wasm`, not a rebuild, so they need no
//! toolchain; CI's `sandbox` job rebuilds them from source separately and
//! asserts the bytes are identical.
//!
//! What each test defends is a sentence in the ABI page:
//!
//!   * each module is accepted under an explicit fuel limit and memory
//!     ceiling, and echoes its input exactly;
//!   * running it twice spends identical fuel and emits identical bytes — the
//!     "pure, re-execution-provable" claim, checked rather than asserted;
//!   * the declared memory maximum is what the ceiling is compared against,
//!     and fuel is a hard stop at the exact budget;
//!   * the import section of each is exactly `areev::emit`;
//!   * the refusal a WASI-built module gets names the ABI page — the one
//!     pointer a toolchain's-defaults author needs.

use areev_sandbox::{run, Limits, SandboxError, ABI_DOC};
use serde_json::{json, Value};

/// Every reference module, by the file name `examples/build.sh` gives it.
const MODULES: [&str; 3] = ["echo-c", "echo-zig", "echo-as"];

/// An explicit budget, far below the default, so a module that quietly spent
/// orders of magnitude more than an echo should would fail here.
const FUEL: u64 = 1_000_000;
/// The ceiling each reference module declares (256 pages = 16 MiB).
const PAGES: u32 = 256;

fn module(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../areev-tools/examples/dist")
        .join(format!("{name}.wasm"));
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e} — run areev-tools/examples/build.sh", path.display()))
}

fn limits() -> Limits {
    Limits { fuel: FUEL, max_pages: PAGES, ..Default::default() }
}

/// Inputs that exercise the framing rather than the logic: non-ASCII UTF-8,
/// nesting, the smallest JSON values, and one payload large enough that the
/// guest's allocator has to grow memory past what the linker laid out.
fn inputs() -> Vec<Value> {
    vec![
        json!({ "invoice": "INV-7", "amount": 1250.5, "tags": ["ap", "q3"] }),
        json!({ "name": "Zoë — naïve café 東京 🚀", "nested": { "a": [1, [2, [3]]] } }),
        json!(null),
        json!(0),
        json!(""),
        json!({ "blob": "x".repeat(300 * 1024) }),
    ]
}

#[test]
fn every_reference_module_echoes_its_input_under_explicit_limits() {
    for name in MODULES {
        let wasm = module(name);
        for input in inputs() {
            let out = run(&wasm, &input, &limits())
                .unwrap_or_else(|e| panic!("{name}: refused or failed: {e}"));
            assert_eq!(out.output, input, "{name} must echo its input exactly");
            assert!(out.fuel_used > 0, "{name}: fuel is metered");
            assert!(out.fuel_used < FUEL, "{name}: within budget");
            assert_eq!((out.fetches, out.blob_reads), (0, 0), "{name} is pure");
        }
    }
}

#[test]
fn each_run_is_deterministic_in_fuel_and_in_bytes() {
    for name in MODULES {
        let wasm = module(name);
        for input in inputs() {
            let a = run(&wasm, &input, &limits()).unwrap();
            let b = run(&wasm, &input, &limits()).unwrap();
            assert_eq!(a.fuel_used, b.fuel_used, "{name}: fuel differs between runs");
            assert_eq!(
                serde_json::to_vec(&a.output).unwrap(),
                serde_json::to_vec(&b.output).unwrap(),
                "{name}: output bytes differ between runs"
            );
        }
    }
}

#[test]
fn fuel_is_a_hard_stop_at_exactly_the_budget() {
    // Spend exactly what a run needs and it completes; one unit less and it
    // does not. A budget that was "roughly" enforced would pass the first and
    // fail neither.
    let input = json!({ "k": "v" });
    for name in MODULES {
        let wasm = module(name);
        let need = run(&wasm, &input, &limits()).unwrap().fuel_used;
        let exact = Limits { fuel: need, ..limits() };
        assert_eq!(run(&wasm, &input, &exact).unwrap().output, input, "{name}");
        let short = Limits { fuel: need - 1, ..limits() };
        let e = run(&wasm, &input, &short).unwrap_err();
        assert!(matches!(e, SandboxError::FuelExhausted), "{name}: got {e}");
    }
}

#[test]
fn the_declared_maximum_is_what_the_memory_ceiling_is_held_against() {
    // Each module declares 256 pages. A host ceiling one page lower refuses it
    // at instantiation, before `alloc` is ever called — so the declaration in
    // the build flags is load-bearing, not decoration.
    for name in MODULES {
        let wasm = module(name);
        let tight = Limits { max_pages: PAGES - 1, ..limits() };
        let e = run(&wasm, &json!({}), &tight).unwrap_err();
        match e {
            SandboxError::Module(ref w) => {
                assert!(w.contains("256 memory pages"), "{name}: {w}")
            }
            other => panic!("{name}: expected a ceiling refusal, got {other}"),
        }
    }
}

#[test]
fn each_reference_module_imports_exactly_areev_emit_and_exports_the_contract() {
    // The ABI page's promise, checked on the shipped bytes: one import, three
    // exports. A toolchain upgrade that slipped in `env::abort` or
    // `env::memcpy` would be refused at run time; this says so at test time,
    // and names the import.
    let engine = wasmi::Engine::default();
    for name in MODULES {
        let m = wasmi::Module::new(&engine, module(name)).unwrap();
        let imports: Vec<String> =
            m.imports().map(|i| format!("{}::{}", i.module(), i.name())).collect();
        assert_eq!(imports, ["areev::emit"], "{name}");
        let mut exports: Vec<String> = m.exports().map(|e| e.name().to_string()).collect();
        exports.sort();
        assert_eq!(exports, ["alloc", "memory", "run"], "{name}");
    }
}

#[test]
fn a_wasi_import_is_still_refused_and_the_refusal_names_the_abi_page() {
    // What `clang --target=wasm32-wasi` or a default `zig build -target
    // wasm32-wasi` produces: WASI imports. Refused by name, by default, before
    // one instruction — and told where the contract lives.
    let wasi = wat::parse_str(
        r#"(module
          (import "wasi_snapshot_preview1" "fd_write"
            (func (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1 4)
          (func (export "alloc") (param i32) (result i32) (i32.const 1024))
          (func (export "run") (param i32) (param i32)))"#,
    )
    .unwrap();
    let e = run(&wasi, &Value::Null, &limits()).unwrap_err();
    assert!(
        matches!(e, SandboxError::ForbiddenImport { ref module, .. } if module == "wasi_snapshot_preview1"),
        "got {e}"
    );
    let text = e.to_string();
    assert!(text.contains(ABI_DOC), "must name the ABI page: {text}");
    assert!(text.contains("WASI is not provided"), "and say why: {text}");
    assert_eq!(ABI_DOC, "docs/sandbox-abi.md");
}

#[test]
fn any_other_unlisted_import_names_the_abi_page_too() {
    // AssemblyScript's default `env::abort` — the second most common way a
    // toolchain's defaults produce a module of the wrong shape.
    let abort = wat::parse_str(
        r#"(module
          (import "env" "abort" (func (param i32 i32 i32 i32)))
          (memory (export "memory") 1 4)
          (func (export "alloc") (param i32) (result i32) (i32.const 1024))
          (func (export "run") (param i32) (param i32)))"#,
    )
    .unwrap();
    let e = run(&abort, &Value::Null, &limits()).unwrap_err();
    let text = e.to_string();
    assert!(text.contains("env::abort") && text.contains(ABI_DOC), "{text}");
    assert!(!text.contains("WASI"), "not a WASI import, so no WASI hint: {text}");
}

#[test]
fn the_hand_written_module_in_the_abi_page_runs_as_written() {
    // The page promises "a complete, working module"; hold it to that. The
    // first ```wat fence that is a whole module with an `unreachable` guard is
    // the echo — parse it straight out of the doc and run it.
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(ABI_DOC),
    )
    .expect("the ABI page exists");
    let echo = doc
        .split("```wat\n")
        .skip(1)
        .map(|rest| rest.split("```").next().unwrap_or_default())
        .find(|block| block.starts_with("(module") && block.contains("unreachable"))
        .expect("the ABI page carries a complete WAT module");
    let wasm = wat::parse_str(echo).expect("the page's WAT parses");
    for input in inputs() {
        let a = run(&wasm, &input, &limits()).unwrap();
        let b = run(&wasm, &input, &limits()).unwrap();
        assert_eq!(a.output, input);
        assert_eq!(a.fuel_used, b.fuel_used);
    }
}

#[test]
fn the_cli_accepts_each_reference_module() {
    // The acceptance criterion as the operator sees it: `areev-sandbox
    // --module FILE` with the input on stdin and the output on stdout.
    use std::io::Write;
    let bin = env!("CARGO_BIN_EXE_areev-sandbox");
    let input = r#"{"hello":"wörld","n":[1,2,3]}"#;
    for name in MODULES {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../areev-tools/examples/dist")
            .join(format!("{name}.wasm"));
        let mut child = std::process::Command::new(bin)
            .arg("--module")
            .arg(&path)
            .args(["--fuel", &FUEL.to_string(), "--max-pages", &PAGES.to_string()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{name}: {stderr}");
        let echoed: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(echoed, serde_json::from_str::<Value>(input).unwrap(), "{name}");
        assert!(stderr.contains("fuel used"), "{name}: {stderr}");
    }
}
