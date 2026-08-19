//! Bounded subprocess spawning for the engine's two host-command seams
//! (`--llm-cmd`, `--analyzer-cmd`).
//!
//! **This is a deliberate duplicate of `areev_core::proc`.** This crate's
//! `Cargo.toml` states the engine must never depend on an areev-* sibling, so
//! it cannot use the canonical module. `areev-loop/tests/proc_contract.rs`
//! pins the two to the same observable behaviour; change one and the test tells
//! you to change the other.
//!
//! Scope is trimmed to what these two seams need: argv commands (never a
//! shell), inherited stderr, no working-directory control, and no
//! clear-the-environment mode. Everything load-bearing is identical — a
//! wall-clock ceiling, an output cap that keeps draining, stdin on its own
//! thread, and a registry of variables that must never reach a child.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Matches `areev_core::proc::DEFAULT_TIMEOUT`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Matches `areev_core::proc::DEFAULT_MAX_OUTPUT`.
pub const DEFAULT_MAX_OUTPUT: usize = 64 * 1024 * 1024;

static SECRET_ENV: std::sync::Mutex<Option<BTreeSet<String>>> = std::sync::Mutex::new(None);

/// Register a variable whose value must never reach a child process.
///
/// A host that also uses `areev_core::proc` must register with both — the two
/// registries cannot be shared without the dependency this crate refuses.
pub fn deny_env_var(name: &str) {
    if name.trim().is_empty() {
        return;
    }
    let mut guard = SECRET_ENV.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(BTreeSet::new).insert(name.to_string());
}

/// The registered secret variable names.
pub fn secret_env_vars() -> Vec<String> {
    let guard = SECRET_ENV.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|s| s.iter().cloned().collect()).unwrap_or_default()
}

/// What a spawn produced.
#[derive(Debug)]
pub struct SpawnOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
}

impl SpawnOutput {
    /// The reason this spawn failed, or `None` if it succeeded.
    pub fn failure(&self, what: &str) -> Option<String> {
        if self.timed_out {
            return Some(format!("{what} timed out and was killed"));
        }
        if !self.status.success() {
            return Some(format!("{what} exited with {}", self.status));
        }
        None
    }
}

/// Run `argv` with `stdin`, bounded by `timeout` and the output cap.
///
/// stderr is inherited: these seams are driven from a terminal where the
/// command's own diagnostics are what an operator needs to see.
pub fn run_argv(argv: &[String], stdin: &str, timeout: Option<Duration>) -> io::Result<SpawnOutput> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for var in secret_env_vars() {
        cmd.env_remove(var);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());

    let mut child = cmd.spawn()?;

    // stdin on its own thread: writing the whole payload before reading any
    // output deadlocks once the child's output fills the pipe buffer.
    let payload = stdin.as_bytes().to_vec();
    let stdin_thread = child.stdin.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&payload);
        })
    });
    let out_thread = child.stdout.take().map(|pipe| std::thread::spawn(move || drain(pipe)));

    let (status, timed_out) = wait_bounded(&mut child, timeout)?;

    if let Some(t) = stdin_thread {
        let _ = t.join();
    }
    let (stdout, stdout_truncated) =
        out_thread.and_then(|t| t.join().ok()).unwrap_or((Vec::new(), false));

    Ok(SpawnOutput { status, stdout, timed_out, stdout_truncated })
}

/// Read to EOF keeping at most [`DEFAULT_MAX_OUTPUT`]. Past the cap the bytes
/// are read and dropped — leaving them unread would block the child forever.
fn drain<R: Read>(mut src: R) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if kept.len() < DEFAULT_MAX_OUTPUT {
                    let take = (DEFAULT_MAX_OUTPUT - kept.len()).min(n);
                    kept.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (kept, truncated)
}

fn wait_bounded(child: &mut Child, timeout: Option<Duration>) -> io::Result<(ExitStatus, bool)> {
    let Some(limit) = timeout else {
        return Ok((child.wait()?, false));
    };
    let deadline = Instant::now() + limit;
    let mut nap = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait()?;
            return Ok((status, true));
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(50));
    }
}
