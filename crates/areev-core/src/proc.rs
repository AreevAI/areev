//! The one place Areev spawns a child process.
//!
//! Six seams shell out to host-supplied commands — `--tool-cmd`, `--embed-cmd`,
//! `--anonymize-cmd`, `--llm-cmd`, `--analyzer-cmd`, and `areev eval`'s case
//! runner. Every one of them hand-rolled the same thirty lines, and none of them
//! had a wall-clock ceiling, an output cap, a working-directory control, or an
//! environment scrub. The concrete consequences, all fixed here:
//!
//! * **A hung child wedged the caller forever.** `wait_with_output()` blocks
//!   until EOF, so a tool that never exits parked a run-pool worker and, at the
//!   next wave boundary, the driver itself.
//! * **A chatty child could exhaust memory.** stdout was read to EOF into a
//!   `Vec` with no ceiling.
//! * **Secrets leaked into every child.** No seam called `env_clear` or
//!   `env_remove`, so `--passphrase-env` (the memory's encryption passphrase)
//!   and `--token-env` sat in the environment block of every subprocess Areev
//!   started. The CLI carefully wrapped its own copy in `Zeroizing` and then
//!   handed the raw variable to every child.
//! * **Large stdin could deadlock.** Every seam wrote the whole payload before
//!   reading a byte of output, so a child that wrote enough to fill the pipe
//!   buffer while still reading its input blocked forever, and so did we.
//!   Reading and writing now happen on separate threads.
//!
//! `areev-loop` cannot use this module — its `Cargo.toml` states the engine
//! crate must never depend on an areev-* sibling — so it carries a private
//! `proc` module mirroring this policy. `proc_contract.rs` in this crate's
//! tests pins the two to the same observable behaviour.
//!
//! ## What this module deliberately does NOT do
//!
//! There is no resource limiting (`setrlimit`, cgroups, Job Objects) and no
//! process-*group* kill. Both need a dependency — `libc` on unix, `win32job` on
//! Windows — and both are tracked separately rather than smuggled in here. In
//! particular [`SpawnOutput::timed_out`] means *the direct child was killed*: a
//! `/bin/sh -c` child may have spawned grandchildren that outlive it, because
//! killing a whole tree requires putting it in its own process group first.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How the child's environment is derived from ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvPolicy {
    /// Inherit the parent environment, minus `deny`.
    ///
    /// The default for the pre-existing seams. A blanket `env_clear` would be
    /// stricter but would break every deployed `--llm-cmd` or `--embed-cmd`
    /// that legitimately reads an API key out of the environment — so the
    /// non-breaking fix is to remove the variables Areev *knows* hold secrets.
    /// It knows them by name because the operator names them: `--passphrase-env
    /// VAR` and `--token-env VAR` pass the variable name, not the value.
    InheritExcept { deny: Vec<String> },
    /// Clear the environment, passing through only `allow` (plus the per-call
    /// extras, which are always set).
    ///
    /// Strictly better, and the default for surfaces introduced in 1.3, which
    /// carry no backward-compatibility burden.
    ClearExcept { allow: Vec<String> },
}

impl EnvPolicy {
    /// Variables worth keeping under [`EnvPolicy::ClearExcept`] for a command
    /// to be able to run at all. `PATH` is load-bearing — without it a bare
    /// command name resolves to nothing.
    pub fn minimal_allow() -> Vec<String> {
        let base: &[&str] = if cfg!(windows) {
            &["PATH", "PATHEXT", "SYSTEMROOT", "SYSTEMDRIVE", "COMSPEC", "TEMP", "TMP", "USERPROFILE"]
        } else {
            &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TZ"]
        };
        base.iter().map(|s| s.to_string()).collect()
    }
}

impl Default for EnvPolicy {
    fn default() -> Self {
        EnvPolicy::InheritExcept { deny: secret_env_vars() }
    }
}

/// Environment variables this process has been told hold secrets.
///
/// Host config, never a file truth, and process-wide by nature: the operator
/// names the variables once on the command line (`--passphrase-env VAR`,
/// `--token-env VAR`) long before any seam spawns anything, and no seam has a
/// path to that flag. A registry is what lets [`SpawnPolicy::default`] scrub
/// them everywhere without every call site remembering to.
static SECRET_ENV: std::sync::Mutex<Option<BTreeSet<String>>> = std::sync::Mutex::new(None);

/// Register a variable whose value must never reach a child process.
///
/// Idempotent, and safe to call before or after other setup. Registering after
/// a spawn does not retroactively protect that spawn, so hosts should call this
/// while parsing arguments — which is also the only moment they know the name.
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

/// Where the child's stderr goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StderrMode {
    /// Capture it, subject to the output cap. Use when stderr is part of the
    /// error message the caller reports.
    #[default]
    Pipe,
    /// Let it flow to the parent's stderr. Use where an operator is watching a
    /// terminal and the child's diagnostics are the point.
    Inherit,
}

/// The policy a spawn runs under.
#[derive(Debug, Clone)]
pub struct SpawnPolicy {
    /// Wall-clock ceiling. `None` waits forever — the pre-1.3 behaviour, kept
    /// expressible so a caller has to opt into it rather than get it by
    /// forgetting.
    pub timeout: Option<Duration>,
    /// Maximum bytes retained per captured stream. Output past the cap is
    /// drained and discarded — never left unread, which would block the child
    /// on a full pipe — and the corresponding `*_truncated` flag is set.
    pub max_output_bytes: usize,
    pub env: EnvPolicy,
    pub stderr: StderrMode,
    /// Working directory for the child. `None` inherits ours.
    pub current_dir: Option<PathBuf>,
}

/// 300s: long enough for a slow model call or a cold-start container, short
/// enough that a wedged child surfaces the same day. Deliberately not
/// optimistic.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// 64 MiB per stream. Four times the 16 MiB maximum grain, so a legitimate
/// payload cannot hit it, while a runaway `yes` is stopped well short of
/// exhausting memory.
pub const DEFAULT_MAX_OUTPUT: usize = 64 * 1024 * 1024;

impl Default for SpawnPolicy {
    fn default() -> Self {
        SpawnPolicy {
            timeout: Some(DEFAULT_TIMEOUT),
            max_output_bytes: DEFAULT_MAX_OUTPUT,
            env: EnvPolicy::default(),
            stderr: StderrMode::default(),
            current_dir: None,
        }
    }
}

impl SpawnPolicy {
    /// Deny the named variables, ignoring any that are `None`. The shape the
    /// hosts use: they hold `Option<String>` for `--passphrase-env` and
    /// `--token-env` and want both scrubbed when set.
    pub fn deny_vars<I: IntoIterator<Item = String>>(mut self, vars: I) -> Self {
        let mut denied: BTreeSet<String> = match self.env {
            EnvPolicy::InheritExcept { deny } => deny.into_iter().collect(),
            EnvPolicy::ClearExcept { .. } => BTreeSet::new(),
        };
        denied.extend(vars);
        self.env = EnvPolicy::InheritExcept { deny: denied.into_iter().collect() };
        self
    }

    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn stderr(mut self, mode: StderrMode) -> Self {
        self.stderr = mode;
        self
    }
}

/// What a spawn produced.
#[derive(Debug)]
pub struct SpawnOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The child exceeded the policy's timeout and was killed. `status` then
    /// reflects the kill, not the child's own exit.
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl SpawnOutput {
    /// Trimmed stderr, for error messages.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }

    /// The reason this spawn failed, or `None` if it succeeded. Centralised so
    /// every seam words a timeout and a non-zero exit the same way.
    pub fn failure(&self, what: &str) -> Option<String> {
        if self.timed_out {
            return Some(format!("{what} timed out and was killed"));
        }
        if !self.status.success() {
            let err = self.stderr_text();
            return Some(if err.is_empty() {
                format!("{what} exited with {}", self.status)
            } else {
                format!("{what} exited with {}: {err}", self.status)
            });
        }
        None
    }
}

/// Run `cmd` to completion under `policy`, writing `stdin` to it.
///
/// The caller supplies a `Command` with its program and arguments already set —
/// the shell-vs-argv choice belongs to the seam, and only the seam knows
/// whether the Windows path needs `raw_arg`. Everything after that (stdio,
/// environment, working directory, timeout, caps) is decided here.
///
/// `extra_env` is applied *after* the environment policy, so a seam's own
/// variables (`AREEV_TOOL_NAME` and friends) survive `ClearExcept`.
pub fn run(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    extra_env: &[(&str, &str)],
    policy: &SpawnPolicy,
) -> io::Result<SpawnOutput> {
    match &policy.env {
        EnvPolicy::InheritExcept { deny } => {
            for var in deny {
                cmd.env_remove(var);
            }
        }
        EnvPolicy::ClearExcept { allow } => {
            cmd.env_clear();
            for var in allow {
                if let Ok(val) = std::env::var(var) {
                    cmd.env(var, val);
                }
            }
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    if let Some(dir) = &policy.current_dir {
        cmd.current_dir(dir);
    }

    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(match policy.stderr {
            StderrMode::Pipe => Stdio::piped(),
            StderrMode::Inherit => Stdio::inherit(),
        });

    let mut child = cmd.spawn()?;

    // stdin on its own thread: writing the whole payload before reading a byte
    // of output deadlocks as soon as the child's output fills the pipe buffer
    // while it is still reading its input.
    let stdin_thread = child.stdin.take().map(|mut pipe| {
        let payload = stdin.unwrap_or_default().to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&payload);
            // Dropping closes the pipe so the child sees EOF.
        })
    });

    let cap = policy.max_output_bytes;
    let out_thread = child.stdout.take().map(|pipe| std::thread::spawn(move || drain(pipe, cap)));
    let err_thread = child.stderr.take().map(|pipe| std::thread::spawn(move || drain(pipe, cap)));

    let (status, timed_out) = wait_bounded(&mut child, policy.timeout)?;

    if let Some(t) = stdin_thread {
        let _ = t.join();
    }
    let (stdout, stdout_truncated) = out_thread.and_then(|t| t.join().ok()).unwrap_or((Vec::new(), false));
    let (stderr, stderr_truncated) = err_thread.and_then(|t| t.join().ok()).unwrap_or((Vec::new(), false));

    Ok(SpawnOutput { status, stdout, stderr, timed_out, stdout_truncated, stderr_truncated })
}

/// Read to EOF, retaining at most `cap` bytes. Everything past the cap is read
/// and dropped rather than left in the pipe — an unread pipe blocks the child
/// forever, which would turn an output cap into a hang.
fn drain<R: Read>(mut src: R, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if kept.len() < cap {
                    let room = cap - kept.len();
                    let take = room.min(n);
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

/// Wait for `child`, killing it if `timeout` elapses first.
///
/// Polls rather than blocking because `wait()` has no timeout in std. The
/// interval ramps from 1ms to 50ms so a fast command still returns promptly
/// while a long one costs almost nothing to watch.
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
            // Reap, so the child does not linger as a zombie.
            let status = child.wait()?;
            return Ok((status, true));
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn captures_stdout_and_exit_status() {
        let out = run(sh("printf hello"), None, &[], &SpawnPolicy::default()).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
        assert!(!out.timed_out);
        assert!(!out.stdout_truncated);
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn stdin_reaches_the_child() {
        let out = run(sh("cat"), Some(b"payload"), &[], &SpawnPolicy::default()).unwrap();
        assert_eq!(out.stdout, b"payload");
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn timeout_kills_a_hung_child() {
        let policy = SpawnPolicy::default().timeout(Some(Duration::from_millis(150)));
        let out = run(sh("sleep 30"), None, &[], &policy).unwrap();
        assert!(out.timed_out, "expected the child to be killed");
        assert!(!out.status.success());
        assert!(out.failure("tool").unwrap().contains("timed out"));
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn output_cap_truncates_without_hanging() {
        // Far more than the cap, and the child only exits if we keep draining.
        let policy = SpawnPolicy { max_output_bytes: 1024, ..SpawnPolicy::default() };
        let out = run(sh("head -c 200000 /dev/zero"), None, &[], &policy).unwrap();
        assert_eq!(out.stdout.len(), 1024);
        assert!(out.stdout_truncated);
        assert!(!out.timed_out, "draining past the cap must not stall the child");
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn large_stdin_does_not_deadlock() {
        // The child echoes while we are still writing: with a single-threaded
        // write-then-read this deadlocks once the pipe buffer fills.
        let big = vec![b'x'; 4 * 1024 * 1024];
        let policy = SpawnPolicy::default().timeout(Some(Duration::from_secs(20)));
        let out = run(sh("cat"), Some(&big), &[], &policy).unwrap();
        assert!(!out.timed_out, "write-then-read deadlock");
        assert_eq!(out.stdout.len(), big.len());
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn denied_vars_do_not_reach_the_child() {
        std::env::set_var("AREEV_TEST_SECRET", "hunter2");
        let policy = SpawnPolicy::default().deny_vars(["AREEV_TEST_SECRET".to_string()]);
        let out = run(sh("printf %s \"${AREEV_TEST_SECRET:-absent}\""), None, &[], &policy).unwrap();
        std::env::remove_var("AREEV_TEST_SECRET");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "absent");
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn inherited_vars_still_reach_the_child() {
        // The non-breaking half of the contract: denying one variable must not
        // strip the rest, or every deployed --llm-cmd loses its API key.
        std::env::set_var("AREEV_TEST_KEEP", "kept");
        std::env::set_var("AREEV_TEST_DROP", "dropped");
        let policy = SpawnPolicy::default().deny_vars(["AREEV_TEST_DROP".to_string()]);
        let out = run(sh("printf %s \"${AREEV_TEST_KEEP:-absent}\""), None, &[], &policy).unwrap();
        std::env::remove_var("AREEV_TEST_KEEP");
        std::env::remove_var("AREEV_TEST_DROP");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "kept");
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn clear_except_drops_everything_unlisted_but_keeps_extras() {
        std::env::set_var("AREEV_TEST_AMBIENT", "ambient");
        let policy = SpawnPolicy {
            env: EnvPolicy::ClearExcept { allow: EnvPolicy::minimal_allow() },
            ..SpawnPolicy::default()
        };
        let out = run(
            sh("printf %s \"${AREEV_TEST_AMBIENT:-absent}/${AREEV_EXTRA:-none}\""),
            None,
            &[("AREEV_EXTRA", "set")],
            &policy,
        )
        .unwrap();
        std::env::remove_var("AREEV_TEST_AMBIENT");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "absent/set");
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn nonzero_exit_reports_stderr() {
        let out = run(sh("echo boom >&2; exit 3"), None, &[], &SpawnPolicy::default()).unwrap();
        let msg = out.failure("embed command").unwrap();
        assert!(msg.contains("embed command"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    #[cfg_attr(windows, ignore = "uses /bin/sh")]
    fn registered_secrets_are_scrubbed_without_the_seam_asking() {
        // The whole point of the registry: a seam that just uses the default
        // policy still must not leak a registered secret.
        std::env::set_var("AREEV_TEST_REGISTERED", "hunter2");
        deny_env_var("AREEV_TEST_REGISTERED");
        let out = run(
            sh("printf %s \"${AREEV_TEST_REGISTERED:-absent}\""),
            None,
            &[],
            &SpawnPolicy::default(),
        )
        .unwrap();
        std::env::remove_var("AREEV_TEST_REGISTERED");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "absent");
    }

    #[test]
    fn deny_env_var_ignores_blanks() {
        deny_env_var("   ");
        deny_env_var("");
        assert!(!secret_env_vars().iter().any(|v| v.trim().is_empty()));
    }

    #[test]
    fn spawn_failure_is_an_error_not_a_panic() {
        let cmd = Command::new("areev-no-such-binary-eaf1");
        assert!(run(cmd, None, &[], &SpawnPolicy::default()).is_err());
    }
}
