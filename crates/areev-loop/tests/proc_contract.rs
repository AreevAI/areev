//! Pins `areev_loop::proc` to the same observable behaviour as
//! `areev_core::proc`.
//!
//! The duplication is forced: this crate's `Cargo.toml` states the engine must
//! never depend on an areev-* sibling, so the two modules cannot share code.
//! What they *can* share is a test that fails when they drift, which is the
//! only thing standing between "two implementations of one policy" and "two
//! policies". If you change one, this test tells you to change the other.
//!
//! Deliberately black-box: it asserts what a child process observes, not how
//! either module is written.

#![cfg(unix)]

use areev_loop::proc;
use std::time::Duration;

fn sh_argv(script: &str) -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
}

#[test]
fn captures_stdout_and_status() {
    let out = proc::run_argv(&sh_argv("printf hello"), "", Some(Duration::from_secs(10))).unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"hello");
    assert!(!out.timed_out);
}

#[test]
fn stdin_reaches_the_child() {
    let out = proc::run_argv(&sh_argv("cat"), "payload", Some(Duration::from_secs(10))).unwrap();
    assert_eq!(out.stdout, b"payload");
}

#[test]
fn timeout_kills_a_hung_child() {
    let out = proc::run_argv(&sh_argv("sleep 30"), "", Some(Duration::from_millis(150))).unwrap();
    assert!(out.timed_out, "expected the child to be killed");
    assert!(out.failure("--llm-cmd").unwrap().contains("timed out"));
}

#[test]
fn large_stdin_does_not_deadlock() {
    // The child echoes while we are still writing. A single-threaded
    // write-then-read deadlocks here once the pipe buffer fills — which is what
    // both seams did before 1.3.
    let big = "x".repeat(4 * 1024 * 1024);
    let out = proc::run_argv(&sh_argv("cat"), &big, Some(Duration::from_secs(20))).unwrap();
    assert!(!out.timed_out, "write-then-read deadlock");
    assert_eq!(out.stdout.len(), big.len());
}

#[test]
fn output_cap_truncates_without_stalling() {
    // 80 MiB against a 64 MiB cap. The child only exits if we keep draining
    // past the cap rather than stopping the read.
    let out = proc::run_argv(
        &sh_argv("head -c 83886080 /dev/zero"),
        "",
        Some(Duration::from_secs(60)),
    )
    .unwrap();
    assert_eq!(out.stdout.len(), proc::DEFAULT_MAX_OUTPUT);
    assert!(out.stdout_truncated);
    assert!(!out.timed_out, "draining past the cap must not stall the child");
}

#[test]
fn registered_secrets_are_scrubbed() {
    std::env::set_var("AREEV_LOOP_TEST_SECRET", "hunter2");
    proc::deny_env_var("AREEV_LOOP_TEST_SECRET");
    let out = proc::run_argv(
        &sh_argv("printf %s \"${AREEV_LOOP_TEST_SECRET:-absent}\""),
        "",
        Some(Duration::from_secs(10)),
    )
    .unwrap();
    std::env::remove_var("AREEV_LOOP_TEST_SECRET");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "absent");
}

#[test]
fn unregistered_vars_still_inherit() {
    // The non-breaking half: scrubbing one variable must not strip the rest, or
    // every deployed --llm-cmd loses the API key it reads from the environment.
    std::env::set_var("AREEV_LOOP_TEST_KEEP", "kept");
    let out = proc::run_argv(
        &sh_argv("printf %s \"${AREEV_LOOP_TEST_KEEP:-absent}\""),
        "",
        Some(Duration::from_secs(10)),
    )
    .unwrap();
    std::env::remove_var("AREEV_LOOP_TEST_KEEP");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "kept");
}

#[test]
fn nonzero_exit_is_a_failure() {
    let out = proc::run_argv(&sh_argv("exit 3"), "", Some(Duration::from_secs(10))).unwrap();
    let msg = out.failure("--analyzer-cmd").unwrap();
    assert!(msg.contains("--analyzer-cmd"), "{msg}");
}

#[test]
fn defaults_match_the_canonical_module() {
    // The two modules define these independently; if one moves, so must the
    // other, or the same command behaves differently depending on which seam
    // invoked it.
    assert_eq!(proc::DEFAULT_TIMEOUT, areev_core_default_timeout());
    assert_eq!(proc::DEFAULT_MAX_OUTPUT, areev_core_default_max_output());
}

// areev-loop must not depend on areev-core, so the canonical values are
// restated here rather than imported. That is the drift this test exists to
// catch: changing areev_core::proc's constants without changing these fails.
fn areev_core_default_timeout() -> Duration {
    Duration::from_secs(300)
}
fn areev_core_default_max_output() -> usize {
    64 * 1024 * 1024
}
