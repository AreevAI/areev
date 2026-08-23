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

// ---- #101: the capability gate -------------------------------------------

/// The broker address reaches this process through the ENVIRONMENT, which is
/// process-global while `cargo test` runs threads in parallel — so two tests
/// that set it race, and the loser reads the winner's value. Everything that
/// touches `AREEV_EGRESS_*` takes this first.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the lock without inheriting a poisoning from an unrelated failure —
/// the guard protects an ordering, not an invariant.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

/// A guest that asks the host to fetch, then emits whatever came back.
///
/// The `fetch` ABI in one guest: write a request, call `areev::fetch`, and
/// read `[u32 LE len][bytes]` at the returned pointer.
const FETCHER: &str = r#"
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (import "areev" "fetch" (func $fetch (param i32 i32) (result i32)))
  (memory (export "memory") 1 4)
  (global $bump (mut i32) (i32.const 2048))
  (func (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  ;; {"url":"http://127.0.0.1:1/x","method":"GET"}  — 44 bytes
  (data (i32.const 16) "{\22url\22:\22http://127.0.0.1:1/x\22,\22method\22:\22GET\22}")
  (func (export "run") (param $ptr i32) (param $len i32)
    (local $r i32)
    (local.set $r (call $fetch (i32.const 16) (i32.const 44)))
    (if (i32.lt_s (local.get $r) (i32.const 0))
      (then
        ;; negative = the host could not place a response at all
        (call $emit (i32.const 16) (i32.const 0))
        (return)))
    ;; emit the length-prefixed payload's BODY: ptr+4, length at ptr
    (call $emit
      (i32.add (local.get $r) (i32.const 4))
      (i32.load (local.get $r)))))
"#;

/// A guest that asks the host for a stored blob, then emits what came back.
///
/// Same shape as [`FETCHER`] — the `blob_get` ABI is one more function, not
/// one more pattern — so the reply framing here is identical.
const BLOB_READER: &str = r#"
(module
  (import "areev" "emit" (func $emit (param i32 i32)))
  (import "areev" "blob_get" (func $blob_get (param i32 i32) (result i32)))
  (memory (export "memory") 1 4)
  (global $bump (mut i32) (i32.const 2048))
  (func (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  ;; {"uri":"cas://sha256:00…00"}  — 8 + 13 + 64 + 2 = 87 bytes
  (data (i32.const 16) "{\22uri\22:\22cas://sha256:0000000000000000000000000000000000000000000000000000000000000000\22}")
  (func (export "run") (param $ptr i32) (param $len i32)
    (local $r i32)
    (local.set $r (call $blob_get (i32.const 16) (i32.const 87)))
    (if (i32.lt_s (local.get $r) (i32.const 0))
      (then
        (call $emit (i32.const 16) (i32.const 0))
        (return)))
    (call $emit
      (i32.add (local.get $r) (i32.const 4))
      (i32.load (local.get $r)))))
"#;

/// The blob gate is linked, not guarded — the same treatment `fetch` gets.
#[test]
fn a_module_importing_blob_get_is_refused_unless_the_host_allowed_it() {
    // Refused by NAME at instantiation, before one instruction. And note the
    // gate is per-capability: allowing `fetch` does not allow `blob_get`, so
    // the widest existing grant still does not open this door.
    for limits in [Limits::default(), Limits { allow_fetch: true, ..Default::default() }] {
        let e = run(&wasm(BLOB_READER), &serde_json::Value::Null, &limits).unwrap_err();
        match e {
            SandboxError::ForbiddenImport { ref module, ref name } => {
                assert_eq!(module, "areev");
                assert_eq!(name, "blob_get");
            }
            other => panic!("got {other}"),
        }
    }
}

/// And the converse: allowing blobs does not allow the network.
#[test]
fn allowing_blobs_does_not_link_fetch() {
    let limits = Limits { allow_blob: true, ..Default::default() };
    let e = run(&wasm(FETCHER), &serde_json::Value::Null, &limits).unwrap_err();
    match e {
        SandboxError::ForbiddenImport { ref name, .. } => assert_eq!(name, "fetch"),
        other => panic!("got {other}"),
    }
}

/// With the capability allowed but no broker wired, a blob read is a typed
/// error the guest can read — never a silent success, and never a read from
/// somewhere the host did not name.
#[test]
fn a_blob_module_with_no_broker_gets_an_error_it_can_read() {
    let _guard = env_lock();
    std::env::remove_var("AREEV_EGRESS_URL");
    std::env::remove_var("AREEV_EGRESS_TOKEN");
    let limits = Limits { allow_blob: true, ..Default::default() };
    let out = run(&wasm(BLOB_READER), &serde_json::Value::Null, &limits).unwrap();
    let err = out.output["error"].as_str().unwrap_or_default();
    assert!(err.contains("no broker"), "got {}", out.output);
    assert_eq!(out.blob_reads, 1, "the attempt is still counted");
}

/// A pure module is unaffected by the second gate existing, and its outcome
/// stays byte-identical: `blob_reads` is omitted when zero.
#[test]
fn a_pure_module_is_unchanged_by_the_blob_gate() {
    let limits = Limits { allow_blob: true, ..Default::default() };
    let out = run(&wasm(ECHO), &serde_json::json!({ "x": 1 }), &limits).unwrap();
    assert_eq!(out.output, serde_json::json!({ "ok": true }));
    assert_eq!(out.blob_reads, 0);
    let json = serde_json::to_value(&out).unwrap();
    assert!(json.get("blob_reads").is_none(), "omitted when zero: {json}");
    assert!(json.get("fetches").is_none(), "and so is the 1.6 field: {json}");
}

/// The same guest, but the host never opted in.
#[test]
fn a_module_importing_fetch_is_refused_unless_the_host_allowed_it() {
    // The `ForbiddenImport` philosophy extended from "which imports" to "which
    // capabilities": refused BY NAME at instantiation, before one instruction.
    let e = run(&wasm(FETCHER), &serde_json::Value::Null, &Limits::default()).unwrap_err();
    match e {
        SandboxError::ForbiddenImport { ref module, ref name } => {
            assert_eq!(module, "areev");
            assert_eq!(name, "fetch");
        }
        other => panic!("got {other}"),
    }
}

/// And a PURE module is unaffected by the gate existing.
#[test]
fn a_pure_module_still_runs_under_a_capability_host() {
    // `--allow-fetch` links the import; it does not change a module that never
    // asks for it, and `fetches` stays 0 so the outcome is byte-identical.
    let limits = Limits { allow_fetch: true, ..Default::default() };
    let out = run(&wasm(ECHO), &serde_json::json!({ "x": 1 }), &limits).unwrap();
    assert_eq!(out.output, serde_json::json!({ "ok": true }));
    assert_eq!(out.fetches, 0);
}

/// With the capability allowed but no broker wired, a call is a typed error
/// the guest can read — never a silent success and never a hang.
#[test]
fn a_capability_module_with_no_broker_gets_an_error_it_can_read() {
    // Belt and braces: the engine refuses this pairing at dispatch, so getting
    // here means a hand-run sandbox. It still must not pretend to succeed.
    let _guard = env_lock();
    std::env::remove_var("AREEV_EGRESS_URL");
    std::env::remove_var("AREEV_EGRESS_TOKEN");
    let limits = Limits { allow_fetch: true, ..Default::default() };
    let out = run(&wasm(FETCHER), &serde_json::Value::Null, &limits).unwrap();
    let err = out.output["error"].as_str().unwrap_or_default();
    assert!(err.contains("no credential broker"), "got {}", out.output);
    assert_eq!(out.fetches, 1, "the attempt is still counted");
}

/// The whole round trip against a real loopback broker stand-in: the guest's
/// request reaches it, and its answer reaches the guest.
#[test]
fn a_brokered_call_round_trips_through_loopback() {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen_t = std::sync::Arc::clone(&seen);
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut token = String::new();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("x-areev-egress-token:") {
                token = v.trim().to_string();
            }
            if let Some(v) = lower.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).unwrap();
        *seen_t.lock().unwrap() =
            format!("{token}|{}", String::from_utf8_lossy(&body));

        let reply = r#"{"status":200,"body":"hello from upstream"}"#;
        let mut stream = stream;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 \r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            )
            .unwrap();
        stream.flush().unwrap();
    });

    // The guest's hardcoded URL is port 1; point the module at our listener by
    // rewriting the WAT (the request URL is the guest's business, the BROKER
    // address is the host's).
    let _guard = env_lock();
    std::env::set_var("AREEV_EGRESS_URL", format!("http://127.0.0.1:{port}"));
    std::env::set_var("AREEV_EGRESS_TOKEN", "tok-abc123");
    let limits = Limits { allow_fetch: true, ..Default::default() };
    let out = run(&wasm(FETCHER), &serde_json::Value::Null, &limits).unwrap();
    std::env::remove_var("AREEV_EGRESS_URL");
    std::env::remove_var("AREEV_EGRESS_TOKEN");
    handle.join().unwrap();

    assert_eq!(out.fetches, 1);
    assert_eq!(
        out.output,
        serde_json::json!({ "status": 200, "body": "hello from upstream" }),
        "the broker's answer reaches the guest verbatim"
    );
    let seen = seen.lock().unwrap().clone();
    let (token, body) = seen.split_once('|').unwrap();
    assert_eq!(token, "tok-abc123", "the capability token is presented");
    assert!(
        body.contains("\"url\":\"http://127.0.0.1:1/x\""),
        "the guest's request is forwarded verbatim, not translated: {body}"
    );
}

/// The guest never sees the broker's address or token, even when it can call
/// through it — there is no WASI, so there is no way to read the environment.
#[test]
fn the_guest_cannot_read_the_broker_env_it_calls_through() {
    // A guest importing anything that could read the environment is refused,
    // which is what makes the token unreachable rather than merely unread.
    const WANTS_ENV: &str = r#"
    (module
      (import "areev" "emit" (func $emit (param i32 i32)))
      (import "wasi_snapshot_preview1" "environ_get"
        (func $environ_get (param i32 i32) (result i32)))
      (memory (export "memory") 1 4)
      (func (export "alloc") (param i32) (result i32) (i32.const 1024))
      (func (export "run") (param i32) (param i32)))
    "#;
    let limits = Limits { allow_fetch: true, ..Default::default() };
    let e = run(&wasm(WANTS_ENV), &serde_json::Value::Null, &limits).unwrap_err();
    assert!(
        matches!(e, SandboxError::ForbiddenImport { ref module, .. } if module == "wasi_snapshot_preview1"),
        "got {e}"
    );
}

/// A hostile `alloc` that calls `fetch` reentrantly must be refused, not
/// recursed. `reply` runs the guest's allocator, so without the in-flight
/// guard each nesting level is a native host frame plus a broker round trip —
/// a stack-exhaustion primitive with I/O amplification.
#[test]
fn a_reentrant_fetch_from_inside_alloc_is_refused_not_recursed() {
    const HOSTILE_ALLOC: &str = r#"
    (module
      (import "areev" "emit" (func $emit (param i32 i32)))
      (import "areev" "fetch" (func $fetch (param i32 i32) (result i32)))
      (memory (export "memory") 1 4)
      (global $bump (mut i32) (i32.const 4096))
      (data (i32.const 16) "{\22url\22:\22http://127.0.0.1:1/x\22,\22method\22:\22GET\22}")
      (data (i32.const 200) "{\22ok\22:true}")
      (func (export "alloc") (param $n i32) (result i32)
        (local $p i32)
        ;; the attack: every allocation tries to fetch again
        (drop (call $fetch (i32.const 16) (i32.const 44)))
        (local.set $p (global.get $bump))
        (global.set $bump (i32.add (global.get $bump) (local.get $n)))
        (local.get $p))
      (func (export "run") (param i32) (param i32)
        (drop (call $fetch (i32.const 16) (i32.const 44)))
        (call $emit (i32.const 200) (i32.const 11))))
    "#;
    let _guard = env_lock();
    std::env::remove_var("AREEV_EGRESS_URL");
    std::env::remove_var("AREEV_EGRESS_TOKEN");
    let limits = Limits { allow_fetch: true, ..Default::default() };
    let out = run(&wasm(HOSTILE_ALLOC), &serde_json::Value::Null, &limits).unwrap();
    assert_eq!(out.output, serde_json::json!({ "ok": true }), "the run still completes");
    // Two NON-reentrant attempts go through (the input-placement alloc's, and
    // run's own); every reentrant one from inside a reply's alloc is refused
    // with -1 before it is even counted.
    assert_eq!(out.fetches, 2, "reentrant attempts are refused, not performed");
}
