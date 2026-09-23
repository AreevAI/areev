//! The sandbox guest ABI (#340), proven through the shipped binary.
//!
//! `docs/sandbox-abi.md` is the contract a non-Rust author builds a Tier C
//! module from; `areev-tools/examples/dist/` holds one committed module per
//! toolchain (C, Zig, AssemblyScript). This runner hands each one to the REAL
//! `areev-sandbox` binary — the process the engine spawns — with an explicit
//! fuel limit and memory ceiling, asserts the output, and runs it twice to
//! compare the fuel spent and the output bytes.
//!
//! Why a subprocess and not a library call: `areev-sandbox` is deliberately
//! not a workspace member (it carries `wasmi`, whose tree and MSRV must not
//! reach this workspace), so the only way this crate can reach it is the way
//! the engine does. Its in-package tests (`areev-sandbox/tests/abi_examples.rs`)
//! cover the same modules through the library, plus the exact-fuel and
//! import-set checks.
//!
//! Needs the binary: `cargo build --manifest-path areev-sandbox/Cargo.toml`,
//! or `AREEV_SANDBOX=/path/to/areev-sandbox`. Skips (loudly) without one —
//! except under `CI=true`, where a missing binary is a hard failure so a
//! broken job can never look like a skipped one.
//!
//! ```text
//! cargo test -p areev-conformance --features sandbox --test sandbox_abi
//! ```
#![cfg(feature = "sandbox")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MODULES: [&str; 3] = ["echo-c", "echo-zig", "echo-as"];
const FUEL: &str = "1000000";
const PAGES: &str = "256";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sandbox() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AREEV_SANDBOX").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        assert!(p.is_file(), "AREEV_SANDBOX={} is not a file", p.display());
        return Some(p);
    }
    let exe = format!("areev-sandbox{}", std::env::consts::EXE_SUFFIX);
    let found = ["debug", "release"]
        .iter()
        .map(|profile| repo().join("areev-sandbox/target").join(profile).join(&exe))
        .find(|p| p.is_file());
    if found.is_none() {
        if std::env::var("CI").as_deref() == Ok("true") {
            panic!(
                "CI=true but no areev-sandbox binary — the sandbox job must not silently skip \
                 (set AREEV_SANDBOX or build areev-sandbox first)"
            );
        }
        eprintln!(
            "skipping: no areev-sandbox binary (cargo build --manifest-path \
             areev-sandbox/Cargo.toml, or set AREEV_SANDBOX)"
        );
    }
    found
}

/// One invocation: `(exit ok, stdout bytes, stderr text)`.
fn invoke(bin: &Path, module: &Path, input: &[u8], extra: &[&str]) -> (bool, Vec<u8>, String) {
    let mut child = Command::new(bin)
        .arg("--module")
        .arg(module)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {}: {e}", bin.display()));
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.success(), out.stdout, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The `fuel used N` the binary reports on stderr.
fn fuel_used(stderr: &str) -> u64 {
    stderr
        .split("fuel used ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no fuel report in stderr: {stderr}"))
}

#[test]
fn each_non_rust_module_runs_through_the_sandbox_deterministically() {
    let Some(bin) = sandbox() else { return };
    let input = r#"{"invoice":"INV-7","memo":"Zoë — café 東京","lines":[1,[2,[3]]]}"#;
    let want: serde_json::Value = serde_json::from_str(input).unwrap();
    for name in MODULES {
        let module = repo().join("areev-tools/examples/dist").join(format!("{name}.wasm"));
        let limits = ["--fuel", FUEL, "--max-pages", PAGES];
        let (ok_a, out_a, err_a) = invoke(&bin, &module, input.as_bytes(), &limits);
        assert!(ok_a, "{name}: {err_a}");
        let got: serde_json::Value = serde_json::from_slice(&out_a)
            .unwrap_or_else(|e| panic!("{name}: stdout is not JSON ({e}): {out_a:?}"));
        assert_eq!(got, want, "{name} must echo its input");

        // Twice: identical fuel, identical bytes. That is what "pure,
        // re-execution-provable" means for a tool a host pins by address.
        let (ok_b, out_b, err_b) = invoke(&bin, &module, input.as_bytes(), &limits);
        assert!(ok_b, "{name}: {err_b}");
        assert_eq!(out_a, out_b, "{name}: output bytes differ between runs");
        let (fa, fb) = (fuel_used(&err_a), fuel_used(&err_b));
        assert_eq!(fa, fb, "{name}: fuel differs between runs");
        assert!(fa > 0 && fa < FUEL.parse().unwrap(), "{name}: fuel {fa}");

        // The limits are live, not advisory: one unit short of what the run
        // needs fails, and a ceiling below the declared maximum refuses it.
        let short = (fa - 1).to_string();
        let (ok, _, err) =
            invoke(&bin, &module, input.as_bytes(), &["--fuel", &short, "--max-pages", PAGES]);
        assert!(!ok && err.contains("fuel"), "{name}: {err}");
        let (ok, _, err) =
            invoke(&bin, &module, input.as_bytes(), &["--fuel", FUEL, "--max-pages", "255"]);
        assert!(!ok && err.contains("page ceiling"), "{name}: {err}");
    }
}

/// The smallest module that imports WASI: a type section and one import,
/// `wasi_snapshot_preview1::fd_write` — the import any wasi-libc `printf`
/// produces. Written as bytes because this crate carries no WAT parser.
const WANTS_WASI: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic, version 1
    0x01, 0x09, 0x01, 0x60, 0x04, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f, // (i32 x4) -> i32
    0x02, 0x23, 0x01, 0x16, // one import, 22-byte module name:
    b'w', b'a', b's', b'i', b'_', b's', b'n', b'a', b'p', b's', b'h', b'o', b't', b'_', b'p',
    b'r', b'e', b'v', b'i', b'e', b'w', b'1', 0x08, // 8-byte field name:
    b'f', b'd', b'_', b'w', b'r', b'i', b't', b'e', 0x00, 0x00, // func, type 0
];

#[test]
fn a_wasi_module_is_refused_by_default_and_told_where_the_abi_is() {
    let Some(bin) = sandbox() else { return };
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("wants-wasi.wasm");
    std::fs::write(&module, WANTS_WASI).unwrap();
    let (ok, out, err) = invoke(&bin, &module, b"null", &["--fuel", FUEL, "--max-pages", PAGES]);
    assert!(!ok, "a WASI import must be refused");
    assert!(out.is_empty(), "nothing on stdout: {out:?}");
    assert!(err.contains("wasi_snapshot_preview1::fd_write"), "names the import: {err}");
    assert!(err.contains("docs/sandbox-abi.md"), "names the ABI page: {err}");
}
