//! `areev-sandbox` — run one Tier C module and exit.
//!
//! The same subprocess contract as every other Areev seam: JSON on stdin, JSON
//! on stdout, one process per invocation. That means it drops straight into
//! `--tool-cmd` without anything new to learn, and it inherits the spawn
//! hardening the host already applies.
//!
//! ```text
//! areev-sandbox --module extract.wasm [--fuel N] [--max-pages N] < input.json
//! ```

use std::collections::HashMap;
use std::io::Read;

fn main() {
    match run() {
        Ok(text) => println!("{text}"),
        Err(e) => {
            // stderr for the human, non-zero for the caller — the tool seam
            // reads a non-zero exit as a failed effect and reports stderr.
            eprintln!("areev-sandbox: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<String, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut flags: HashMap<String, String> = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(name) = args[i].strip_prefix("--") {
            if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                flags.insert(name.to_string(), args[i + 1].clone());
                i += 2;
                continue;
            }
            flags.insert(name.to_string(), "true".into());
        }
        i += 1;
    }

    let path = flags
        .get("module")
        .ok_or("usage: areev-sandbox --module <FILE.wasm> [--fuel N] [--max-pages N]")?;
    let wasm = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;

    let mut limits = areev_sandbox::Limits::default();
    if let Some(f) = flags.get("fuel") {
        limits.fuel = f.parse().map_err(|_| format!("--fuel: not a number: {f}"))?;
    }
    if let Some(p) = flags.get("max-pages") {
        limits.max_pages = p.parse().map_err(|_| format!("--max-pages: not a number: {p}"))?;
    }

    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).map_err(|e| format!("reading stdin: {e}"))?;
    let input: serde_json::Value = if raw.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(raw.trim()).map_err(|e| format!("stdin is not JSON: {e}"))?
    };

    let outcome = areev_sandbox::run(&wasm, &input, &limits).map_err(|e| e.to_string())?;
    // Fuel used goes to stderr, not stdout: stdout is the tool's result and
    // must stay exactly what the guest emitted.
    eprintln!("areev-sandbox: fuel used {}", outcome.fuel_used);
    serde_json::to_string(&outcome.output).map_err(|e| e.to_string())
}
