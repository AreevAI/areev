//! `areev tune` — the tuning seam. Hands a governed corpus to a
//! HOST-supplied trainer command and registers the returned adapter as an
//! `mg:adapter` Fact with full lineage (`derived_from` → the corpus export
//! manifest, the Rule E1 evalset pin embedded). Areev never trains, ships
//! no trainer, and takes no training dependency: this verb is the same
//! posture as `--embed-cmd`/`--llm-cmd` — a process boundary, JSON on
//! stdio.
//!
//! Contract with the trainer command:
//! - stdin: `{"corpus_path", "manifest_hash", "evalset_hash", "recipient"?}`
//!   (also in env as `AREEV_CORPUS_PATH`, `AREEV_CORPUS_MANIFEST`,
//!   `AREEV_EVALSET`, `AREEV_RECIPIENT`)
//! - stdout: the adapter reference JSON —
//!   `{"adapter": {"uri", "sha256"}, "base_model", "serves_as",
//!     "base_build"?, "quantization"?, "serving_runtime"?, "metrics"?}` —
//!   base + adapter + quantization pinned as ONE unit, because an adapter
//!   trained against one base build and served on another silently drifts.
//! - stderr: inherited, so hours of training progress stay visible.
//!
//! Promotion is NOT this verb's job: `areev loop run` proposes the
//! registered candidate (the `adapter_intake` analyzer), `areev eval run`
//! grades it against the pinned evalset, and `areev loop approve` / `apply
//! --gating-run` / `rollback` govern it — tune is the seam, the loop is the
//! gate.

use std::collections::HashMap;

use areev_cal::AreevFacade;
use serde_json::json;

use crate::corpus;
use crate::{flag, need, now_ms};

const USAGE: &str = "usage: areev tune --cmd 'TRAINER' --evalset HASH \
                     (--select '<READ CAL>' --out FILE | --corpus FILE --manifest HASH) \
                     [--recipient ID] [--timeout-secs N]";

pub fn run_tune(facade: &AreevFacade, flags: &HashMap<String, String>) -> Result<(), String> {
    let cmd = need(flags, "cmd").map_err(|_| USAGE.to_string())?;
    let evalset = need(flags, "evalset").map_err(|_| USAGE.to_string())?;
    let recipient = flag(flags, "recipient");

    // Validate the pin up front, mirroring the apply gate's liveness read —
    // a doomed pin (unknown, not an evalset, already superseded) fails at
    // tune time, before hours of training, not at draft or apply time.
    let evalset_hash = areev_core::Hash::from_hex(&evalset)
        .map_err(|_| format!("--evalset {evalset:?} is not a grain hash"))?;
    let evalset_hex = evalset_hash.to_hex();
    {
        let grain = facade
            .with_store(|m| m.get(&evalset_hash))
            .map_err(|_| format!("evalset {evalset_hex} not found in this memory"))?;
        if grain.get_str("relation") != Some("mg:evalset") {
            return Err(format!(
                "{evalset_hex} is not an evalset (create one with `areev eval create`)"
            ));
        }
        let superseded = facade
            .with_store(|m| m.supersession_map(&[evalset_hash]))
            .map_err(|e| e.to_string())?;
        if superseded.contains_key(&evalset_hash) {
            return Err(format!(
                "evalset {evalset_hex} was superseded — pin the current version \
                 (Rule E1: a stale pin cannot gate)"
            ));
        }
    }

    // The corpus: integrated export or bring-your-own — exactly one.
    let select = flag(flags, "select");
    let corpus_file = flag(flags, "corpus");
    let (corpus_path, manifest_hex) = match (select, corpus_file) {
        (Some(_), Some(_)) => {
            return Err("give either --select (integrated export) or --corpus (bring \
                        your own), not both"
                .into());
        }
        (Some(selector), None) => {
            // The corpus is lineage: the export registry's `destination` must
            // name a file that persists, so the integrated mode requires a
            // real path — no stdout, no temp files.
            let out = need(flags, "out")
                .map_err(|_| "--select requires --out FILE (the corpus is lineage; the \
                              manifest records where it went)".to_string())?;
            if out == "stdout" {
                return Err("--out stdout cannot feed a trainer — give a file path".into());
            }
            let (summary, manifest) =
                corpus::export(facade, &selector, &out, recipient.as_deref(), now_ms())?;
            eprintln!(
                "corpus export: {} row(s), {} source grain(s), manifest {}",
                summary.rows,
                summary.source_hashes.len(),
                manifest
            );
            (out, manifest.to_hex())
        }
        (None, Some(path)) => {
            let manifest = need(flags, "manifest").map_err(|_| {
                "--corpus requires --manifest HASH (from the `areev corpus` receipt)"
                    .to_string()
            })?;
            if !std::path::Path::new(&path).is_file() {
                return Err(format!("corpus file {path} does not exist"));
            }
            // The hash must name a recorded export of THIS memory — the one
            // reader (`corpus_exports`) decides, so lineage cannot be
            // asserted from the command line.
            let exports = facade
                .with_store(|m| m.corpus_exports())
                .map_err(|e| e.to_string())?;
            let known = exports.iter().any(|e| e.manifest_hash == manifest);
            if !known {
                return Err(format!(
                    "--manifest {manifest} is not a recorded corpus export of this \
                     memory (run `areev corpus` first and use the manifest hash it \
                     prints)"
                ));
            }
            (path, manifest)
        }
        (None, None) => return Err(USAGE.into()),
    };

    // Hand the job to the host's trainer. Training runs for hours: the
    // default is wait-forever (explicit, per SpawnPolicy's design);
    // --timeout-secs opts into a ceiling, and kills only the direct child —
    // a wrapper script's grandchildren may survive it.
    let job = json!({
        "corpus_path": corpus_path,
        "manifest_hash": manifest_hex,
        "evalset_hash": evalset_hex,
        "recipient": recipient,
    })
    .to_string();
    let reply = run_trainer(&cmd, &job, &corpus_path, &manifest_hex, &evalset_hex,
                            recipient.as_deref(), flags)?;

    // Validate + register — a bad reply writes nothing (record_adapter is
    // the gate). The receipt is machine-readable stdout; guidance is stderr.
    let adapter_grain = facade
        .record_adapter(&reply, &manifest_hex, &evalset_hex, now_ms())
        .map_err(|e| format!("trainer reply rejected: {e}"))?;
    let serves_as = serde_json::from_str::<serde_json::Value>(&reply)
        .ok()
        .and_then(|v| v.get("serves_as").and_then(|s| s.as_str()).map(str::to_string))
        .unwrap_or_default();
    println!(
        "{}",
        json!({
            "adapter_grain": adapter_grain.to_hex(),
            "manifest": manifest_hex,
            "evalset": evalset_hex,
            "serves_as": serves_as,
        })
    );
    eprintln!(
        "adapter registered. next:\n  \
         areev loop run                                   # propose the promotion\n  \
         areev eval run --evalset {evalset_hex} --model <runtime>:{serves_as}\n  \
         areev loop approve <rec> --because \"...\"\n  \
         areev loop apply <rec> --gating-run <eval-run-id>"
    );
    Ok(())
}

fn run_trainer(
    cmd: &str,
    job: &str,
    corpus_path: &str,
    manifest: &str,
    evalset: &str,
    recipient: Option<&str>,
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    use areev_core::proc::{self, SpawnPolicy, StderrMode};
    use std::process::Command;
    // The platform shell: /bin/sh -c on unix, cmd /C on Windows (raw_arg —
    // Command::arg MSVC-quotes embedded quotes, which cmd.exe does not parse).
    #[cfg(not(windows))]
    let mut shell = Command::new("/bin/sh");
    #[cfg(not(windows))]
    shell.arg("-c").arg(cmd);
    #[cfg(windows)]
    let mut shell = Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        shell.raw_arg("/C").raw_arg(cmd);
    }
    let timeout = match flag(flags, "timeout-secs") {
        Some(raw) => Some(std::time::Duration::from_secs(
            raw.parse::<u64>()
                .map_err(|_| "--timeout-secs must be a whole number of seconds".to_string())?,
        )),
        None => None,
    };
    let recipient_env = recipient.unwrap_or_default();
    let mut env = vec![
        ("AREEV_CORPUS_PATH", corpus_path),
        ("AREEV_CORPUS_MANIFEST", manifest),
        ("AREEV_EVALSET", evalset),
    ];
    if recipient.is_some() {
        env.push(("AREEV_RECIPIENT", recipient_env));
    }
    let policy = SpawnPolicy {
        timeout,
        // The operator is watching a terminal and the trainer's diagnostics
        // are the point — captured-only would mean hours of silence.
        stderr: StderrMode::Inherit,
        ..SpawnPolicy::default()
    };
    let out = proc::run(shell, Some(job.as_bytes()), &env, &policy)
        .map_err(|e| format!("trainer spawn failed: {e}"))?;
    if let Some(why) = out.failure("trainer") {
        return Err(why);
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err("trainer printed no adapter reference on stdout".into());
    }
    Ok(stdout)
}
