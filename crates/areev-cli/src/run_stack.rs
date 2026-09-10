//! The host-side runtime stack that every surface starting a run assembles.
//!
//! `run start` and `trigger run` both drive an [`areev_run::Runner`], and a
//! firing *is* a run — so a plan that executes from one has to execute from
//! the other. Until 1.5.2 it did not (#90): the trigger path built a bare
//! `CommandExecutor` with `llm: None`, so a code-carrying node refused with
//! `RUN-E018` and an abstract node with `RUN-E006` — on the one path that
//! fires unattended, and after the operator had passed exactly the flags that
//! would have worked from `run start`. The refusal even named three surfaces
//! to pin on, none of them the one being used.
//!
//! The fix is structural rather than a second copy of the same construction:
//! one builder here, called from both, so a stack that grows a component
//! cannot grow it on only one path. `areev-mcp` reads the same `$AREEV_RUN_*`
//! variables for the same reason from its own process (it takes no flags at
//! all), and the Python and Node bindings take the pin as parameters.

use std::collections::HashMap;
use std::sync::Arc;

use areev_run::{CommandExecutor, ExecResult, HostToolExecutor};

use crate::flag;

/// A flag, or the environment variable that stands in for it.
///
/// A trigger heartbeat is a cron line, a launchd plist or a k8s CronJob: the
/// operator writes it once and forgets where it lives, so the executor pin and
/// the sandbox have to be settable out of band — which is what #90 asked for
/// and what `areev-mcp` has always done. The flag wins when both are set,
/// because the argument in front of you should never lose to a variable
/// inherited from a shell you cannot see. An empty value counts as unset:
/// `AREEV_RUN_SANDBOX_CMD=""` in a systemd unit means "not configured", not
/// "dispatch to the empty command".
pub fn flag_or_env(flags: &HashMap<String, String>, key: &str, var: &str) -> Option<String> {
    flag(flags, key)
        .or_else(|| std::env::var(var).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The no-command fallback: refuse to fake tool execution.
pub struct NoExecutor;

impl HostToolExecutor for NoExecutor {
    fn execute(
        &self,
        tool_name: &str,
        _hash: &str,
        _input: &serde_json::Value,
        _idem: &str,
    ) -> ExecResult {
        ExecResult::Err {
            cause: areev_run_core::FailCause::ExecutorError,
            detail: format!("no --tool-cmd configured; cannot execute host tool '{tool_name}'"),
        }
    }
}

/// `--credential`, `--allow-host`, `--tool-egress`, `--credential-ttl` and
/// `--resolver-env` — or their `$AREEV_RUN_*` variables — as one spec. The
/// parser lives in `areev-run` (#201) so the bindings and `areev serve` read
/// exactly this grammar; a flag wins over its variable, as everywhere here.
pub fn egress_spec(flags: &HashMap<String, String>) -> Result<areev_run::EgressSpec, String> {
    let ttl = match flag(flags, "credential-ttl") {
        None => None,
        Some(v) => Some(
            v.trim()
                .parse::<u64>()
                .map_err(|_| format!("--credential-ttl: expected whole seconds, got {v:?}"))?,
        ),
    };
    areev_run::EgressSpec {
        credentials: flag(flags, "credential").filter(|v| !v.trim().is_empty()),
        allow_hosts: flag(flags, "allow-host").filter(|v| !v.trim().is_empty()),
        tool_egress: flag(flags, "tool-egress").filter(|v| !v.trim().is_empty()),
        credential_ttl_secs: ttl,
        resolver_env: flag(flags, "resolver-env").filter(|v| !v.trim().is_empty()),
    }
    .with_env_fallback()
}

/// The broker [`egress_spec`] describes, or None when it describes none —
/// which leaves tools exactly as they were.
pub fn build_egress(
    flags: &HashMap<String, String>,
) -> Result<Option<areev_run::Broker>, String> {
    egress_spec(flags)?.build()
}

/// `--executor-timeout`/`$AREEV_RUN_EXECUTOR_TIMEOUT`: the host override for
/// the fixed 300s wall-clock ceiling every host-executed tool otherwise runs
/// under (#133) — a document-analysis leg that makes a dozen model calls
/// needs longer than the `pdftotext`-shaped tool that ceiling was sized for.
/// `0` means wait forever (the pre-1.3 behaviour, restored on request rather
/// than by omission, which is how every other zero-means-default flag here
/// would read it). An unparseable value is silently ignored, same as every
/// other numeric flag `run_options` reads — the default stands rather than
/// refusing to start a run over a typo.
fn executor_timeout(flags: &HashMap<String, String>) -> Option<Option<std::time::Duration>> {
    flag_or_env(flags, "executor-timeout", "AREEV_RUN_EXECUTOR_TIMEOUT")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| if secs == 0 { None } else { Some(std::time::Duration::from_secs(secs)) })
}

/// The environment a host tool is spawned with, when the operator named one.
///
/// `None` keeps the inherit-minus-secrets default every deployed `--tool-cmd`
/// was written against — the answer for a host that would rather enumerate
/// what a tool sees than what it must not. `parse_args` records a valueless
/// long flag as `"true"`, so a bare `--tool-env` clears to the minimal set.
///
/// This deliberately does NOT use [`flag_or_env`], which treats an empty
/// value as unset. That rule is right for every other knob — an unset
/// `AREEV_RUN_SANDBOX_CMD=""` means "no sandbox" — but here it inverts the
/// operator's intent: for this flag, empty means *clear to the minimal set*,
/// which is the STRICTEST setting, and reading it as "unset" silently selects
/// the weakest one. `AREEV_RUN_TOOL_ENV=""` in a systemd unit, or
/// `--tool-env "$VARS"` with an empty `VARS` in a wrapper script, both say
/// "clear" and would otherwise get "inherit everything" — a downgrade with
/// nothing to notice, on exactly the unattended path this flag exists for.
/// Presence is therefore the signal, and the value only decides what is
/// re-admitted on top of the minimal set.
pub fn tool_env_policy(flags: &HashMap<String, String>) -> Option<areev_core::proc::EnvPolicy> {
    let raw = match flag(flags, "tool-env") {
        Some(v) => v,
        // Present-but-empty is a real setting here, so only an ABSENT
        // variable falls through to the inherit default.
        None => std::env::var("AREEV_RUN_TOOL_ENV").ok()?,
    };
    let raw = raw.trim();
    let (policy, dropped) = areev_run::env_allow_policy(if raw == "true" { "" } else { raw });
    if !dropped.is_empty() {
        eprintln!(
            "areev: --tool-env dropped {} — already registered as holding a secret \
             (--passphrase-env/--token-env/--credential). A tool never receives one.",
            dropped.join(", ")
        );
    }
    Some(policy)
}

/// The executor a run's nodes dispatch through: the `--tool-cmd` subprocess,
/// wrapped in the pinned code executor when the host authorized one.
///
/// Nothing runs from the file's own say-so. A `Definition` may name its
/// executor by content address and the blob travels with the memory, so the
/// grant has to come from the host — a grant in the file would arrive in the
/// same bundle as the code it authorizes. The broker handle reaches the code
/// executor too (#87): a pinned blob gets the SAME credential story as a
/// `--tool-cmd`, whether or not one is configured.
/// The host config a connector that is a GRAIN runs under (#185).
///
/// `None` when the operator pinned nothing: a trigger naming a connector
/// Definition then refuses with `TRG-E012` naming the address to pin, rather
/// than silently polling with whatever `--connector-cmd` happens to be. The
/// flags are deliberately the run path's own — a connector is a tool, and a
/// second spelling for "this host will execute this address" would be a second
/// place for an operator to grant more than they meant to.
pub fn connector_code(
    flags: &HashMap<String, String>,
    db: &str,
) -> Option<areev_trigger::ConnectorCode> {
    let list = flag_or_env(flags, "allow-executor", "AREEV_RUN_ALLOW_EXECUTOR")?;
    let allow: Vec<String> = list
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    if allow.is_empty() {
        return None;
    }
    Some(areev_trigger::ConnectorCode {
        allow,
        cache_dir: flag_or_env(flags, "executor-cache", "AREEV_RUN_EXECUTOR_CACHE")
            .map(std::path::PathBuf::from),
        sandbox_cmd: flag_or_env(flags, "sandbox-cmd", "AREEV_RUN_SANDBOX_CMD"),
        timeout: executor_timeout(flags),
        env: tool_env_policy(flags),
        db_locator: Some(db.to_string()),
    })
}

pub fn tool_executor(
    flags: &HashMap<String, String>,
    egress: Option<&areev_run::EgressHandle>,
) -> Arc<dyn HostToolExecutor> {
    let timeout = executor_timeout(flags);
    let env = tool_env_policy(flags);
    let base: Arc<dyn HostToolExecutor> = match flag_or_env(flags, "tool-cmd", "AREEV_RUN_TOOL_CMD")
    {
        Some(cmd) => {
            let mut ce = CommandExecutor::new(&cmd);
            if let Some(t) = timeout {
                ce = ce.with_timeout(t);
            }
            if let Some(p) = env.clone() {
                ce = ce.with_env_policy(p);
            }
            Arc::new(match egress {
                Some(h) => ce.with_egress(h.clone()),
                None => ce,
            })
        }
        None => Arc::new(NoExecutor),
    };
    match flag_or_env(flags, "allow-executor", "AREEV_RUN_ALLOW_EXECUTOR") {
        None => base,
        Some(list) => {
            let mut ce = areev_run::CodeExecutor::new(base);
            for addr in list.split(',').map(str::trim).filter(|a| !a.is_empty()) {
                ce = ce.allow(addr);
            }
            if let Some(dir) = flag_or_env(flags, "executor-cache", "AREEV_RUN_EXECUTOR_CACHE") {
                ce = ce.cache_dir(dir);
            }
            if let Some(cmd) = flag_or_env(flags, "sandbox-cmd", "AREEV_RUN_SANDBOX_CMD") {
                ce = ce.sandbox_cmd(&cmd);
            }
            if let Some(t) = timeout {
                ce = ce.with_timeout(t);
            }
            if let Some(p) = env {
                ce = ce.with_env_policy(p);
            }
            if let Some(h) = egress {
                ce = ce.with_egress(h.clone());
            }
            Arc::new(ce)
        }
    }
}

/// Whether this invocation can execute a plan's nodes at all.
///
/// `trigger run` used to gate starting runs on `--tool-cmd` alone, which was
/// the same reduction #90 is about one level up: a plan whose nodes are all
/// pinned code, or all abstract, needs no subprocess — and gating on the
/// subprocess meant such a plan was ingested, recorded as fired, and never
/// started. With none of the three the pass still ingests without executing,
/// which stays a useful mode; the condition widened rather than moved.
pub fn can_execute(flags: &HashMap<String, String>) -> bool {
    flag_or_env(flags, "tool-cmd", "AREEV_RUN_TOOL_CMD").is_some()
        || flag_or_env(flags, "allow-executor", "AREEV_RUN_ALLOW_EXECUTOR").is_some()
        || flag_or_env(flags, "model", "AREEV_RUN_MODEL").is_some()
}

/// The tool-calling model abstract nodes need (`--model`, the same spec
/// grammar and env-key discipline as `areev loop run --model`). Without one,
/// abstract nodes refuse at resolve with `RUN-E006`; bound and named plans run
/// either way.
pub fn toolcall_llm(
    flags: &HashMap<String, String>,
) -> Result<Option<Arc<dyn areev_llm::ToolCallLlm>>, String> {
    match flag_or_env(flags, "model", "AREEV_RUN_MODEL") {
        None => Ok(None),
        Some(spec) => areev_llm::resolve_toolcall(
            &spec,
            flag_or_env(flags, "base-url", "AREEV_RUN_BASE_URL").as_deref(),
            flag_or_env(flags, "key-env", "AREEV_RUN_KEY_ENV").as_deref(),
        )
        .map(Some)
        .map_err(|e| e.to_string()),
    }
}

/// `--events` prints each §6.10 run event to stderr as one JSON line (stdout
/// stays the machine surface); `--otel-endpoint` exports OTLP/HTTP JSON spans
/// to a collector. Both compose through one fan-out observer.
pub fn observer(
    flags: &HashMap<String, String>,
) -> Result<Option<Arc<dyn areev_run::RunObserver>>, String> {
    let mut observers: Vec<Arc<dyn areev_run::RunObserver>> = Vec::new();
    if flag(flags, "events").is_some_and(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no")) {
        struct StderrEvents;
        impl areev_run::RunObserver for StderrEvents {
            fn event(&self, ev: &areev_run::RunEvent) {
                if let Ok(line) = serde_json::to_string(ev) {
                    eprintln!("{line}");
                }
            }
        }
        observers.push(Arc::new(StderrEvents));
    }
    if let Some(endpoint) = flag(flags, "otel-endpoint") {
        observers.push(Arc::new(areev_run::OtelObserver::new(&endpoint)?));
    }
    Ok(match observers.len() {
        0 => None,
        1 => observers.pop(),
        _ => {
            struct FanOut(Vec<Arc<dyn areev_run::RunObserver>>);
            impl areev_run::RunObserver for FanOut {
                fn event(&self, ev: &areev_run::RunEvent) {
                    for o in &self.0 {
                        o.event(ev);
                    }
                }
            }
            Some(Arc::new(FanOut(observers)))
        }
    })
}

/// Print every egress refusal the broker recorded.
///
/// A refusal the tool saw as a 403 and swallowed is a refusal the operator
/// would otherwise have to guess at from a failed node — and on a heartbeat
/// there is no operator watching at all, which is why `trigger run` reports
/// them too.
pub fn report_refusals(broker: &Option<Arc<areev_run::Broker>>) {
    if let Some(b) = broker {
        for r in b.refusals() {
            eprintln!(
                "areev: {} ({})",
                areev_run_core::RunError::EgressRefused { destination: r.destination },
                r.reason
            );
        }
    }
}

/// The run ceilings and worker count, from the flags `run start` takes.
///
/// Budgets matter most on the trigger path, which is why they are here rather
/// than duplicated: a standing rule fires unattended, so an unbounded run has
/// nobody watching it and an ask with no TTL parks forever.
pub fn run_options(flags: &HashMap<String, String>) -> areev_run::RunOptions {
    areev_run::RunOptions {
        budgets: areev_run::BudgetsSpec {
            max_supersteps: flag(flags, "max-supersteps").and_then(|v| v.parse().ok()),
            max_tokens: flag(flags, "max-tokens").and_then(|v| v.parse().ok()),
            max_usd_micros: flag(flags, "max-usd")
                .and_then(|v| v.parse::<f64>().ok())
                .map(|usd| (usd * 1_000_000.0) as u64),
            max_wall_ms: flag(flags, "max-wall-ms").and_then(|v| v.parse().ok()),
            max_storage_bytes: flag(flags, "max-storage").and_then(|v| v.parse().ok()),
        },
        ask_ttl_sec: flag(flags, "ask-ttl").and_then(|v| v.parse().ok()),
        workers: flag(flags, "workers").and_then(|v| v.parse().ok()).unwrap_or(4),
        on_dangling: Default::default(),
        llm_max_tokens: flag(flags, "llm-max-tokens").and_then(|v| v.parse().ok()),
        inject_crash: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `std::env` is process-global and Rust runs tests as threads, so every
    /// test that reads or writes `AREEV_RUN_*` has to take this first or it
    /// races the others. The failure is not hypothetical: one test setting
    /// `AREEV_RUN_TOOL_ENV` made a sibling asserting the inherit default see
    /// a cleared environment instead.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take [`ENV_LOCK`], surviving a sibling test that panicked while holding
    /// it — the mutex protects ordering, not data, so poisoning is not a
    /// reason to fail a second test.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn flags(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn budget_flags_reach_the_run_a_firing_starts() {
        // `trigger run` built RunOptions::default(), so moving a workflow
        // behind a trigger silently dropped every ceiling.
        let o = run_options(&flags(&[
            ("max-tokens", "5000"),
            ("max-usd", "0.25"),
            ("max-wall-ms", "60000"),
            ("ask-ttl", "3600"),
        ]));
        assert_eq!(o.budgets.max_tokens, Some(5000));
        assert_eq!(
            o.budgets.max_usd_micros,
            Some(250_000),
            "--max-usd is dollars, stored as micros"
        );
        assert_eq!(o.budgets.max_wall_ms, Some(60_000));
        assert_eq!(o.ask_ttl_sec, Some(3600));
    }

    #[test]
    fn no_budget_flags_means_no_ceiling_not_a_surprise_one() {
        let o = run_options(&flags(&[]));
        assert_eq!(o.budgets.max_tokens, None);
        assert_eq!(o.budgets.max_usd_micros, None);
        assert_eq!(o.ask_ttl_sec, None);
        assert_eq!(o.workers, 4, "the documented default");
    }

    /// The environment fallback, in ONE test on purpose: `set_var` is
    /// process-global and the test harness is threaded, so two tests mutating
    /// the same variable would race each other rather than test anything.
    #[test]
    fn the_environment_stands_in_for_a_flag_a_heartbeat_cannot_carry() {
        let var = "AREEV_TEST_STACK_SANDBOX";

        // A flag beats the variable it falls back to: the argument in front
        // of the operator must not lose to a shell they cannot see.
        std::env::set_var(var, "from-env");
        assert_eq!(
            flag_or_env(&flags(&[("sandbox-cmd", "from-flag")]), "sandbox-cmd", var).as_deref(),
            Some("from-flag")
        );
        assert_eq!(flag_or_env(&flags(&[]), "sandbox-cmd", var).as_deref(), Some("from-env"));

        // `AREEV_RUN_SANDBOX_CMD=""` in a unit file means "not configured".
        // Reading it as a command would dispatch to nothing and blame the plan.
        std::env::set_var(var, "   ");
        assert_eq!(flag_or_env(&flags(&[]), "sandbox-cmd", var), None);
        std::env::remove_var(var);

        // The acceptance criterion from #90: a heartbeat is a cron line, so
        // the pin has to arrive without a flag. `code_allowed` is what
        // `Runner::start` consults before admitting a code-carrying node.
        let addr = "1671652297b93a6a";
        std::env::set_var("AREEV_RUN_ALLOW_EXECUTOR", addr);
        let exec = tool_executor(&flags(&[]), None);
        assert!(exec.code_allowed("tool-hash", &format!("cas://sha256:{addr}")));
        std::env::remove_var("AREEV_RUN_ALLOW_EXECUTOR");

        // And without it the same node is refused — the pin IS the grant.
        let exec = tool_executor(&flags(&[]), None);
        assert!(!exec.code_allowed("tool-hash", &format!("cas://sha256:{addr}")));
    }

    #[test]
    fn executor_timeout_parses_seconds_and_zero_means_wait_forever() {
        assert_eq!(
            executor_timeout(&flags(&[])),
            None,
            "unset means: the executor's own default stands"
        );
        assert_eq!(
            executor_timeout(&flags(&[("executor-timeout", "45")])),
            Some(Some(std::time::Duration::from_secs(45)))
        );
        assert_eq!(
            executor_timeout(&flags(&[("executor-timeout", "0")])),
            Some(None),
            "0 restores the pre-1.3 wait-forever behaviour, on request rather than by omission"
        );
        assert_eq!(
            executor_timeout(&flags(&[("executor-timeout", "not-a-number")])),
            None,
            "an unparseable value is ignored, like every other numeric flag run_options reads"
        );
    }

    /// #133: `--allow-executor`'s fixed 300s ceiling had no host override at
    /// all. A 1s `--executor-timeout` on a `--tool-cmd` that never exits
    /// must fail well inside this test's own timeout, not the old default —
    /// proof the flag actually reaches the constructed executor, not just
    /// that it parses.
    ///
    /// The command busy-loops on shell builtins (`:`, `while`) rather than
    /// calling an external command like `sleep`: `/bin/sh -c` runs a
    /// compound command like this entirely inside its own process (nothing
    /// to exec into), so the DIRECT child IS the looping process on every
    /// shell — unlike `sh -c "sleep 30"`, whose tail-call-into-`sleep`
    /// optimization is shell-specific (observed on bash, not on Linux CI's
    /// `/bin/sh`) and left the previous version of this test relying on a
    /// grandchild-outlives-its-parent gap `SpawnOutput::timed_out` does not
    /// close (see `areev-core::proc`'s module doc on process-group kill
    /// being separate, not-yet-done work).
    #[cfg(unix)]
    #[test]
    fn executor_timeout_flag_shortens_the_ceiling_a_tool_cmd_runs_under() {
        let exec = tool_executor(
            &flags(&[("tool-cmd", "while :; do :; done"), ("executor-timeout", "1")]),
            None,
        );
        let started = std::time::Instant::now();
        let result = exec.execute("work", "h", &serde_json::json!({}), "k");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the override must apply — the 300s default would still be sleeping"
        );
        match result {
            ExecResult::Err { cause, detail } => {
                assert_eq!(cause, areev_run::FailCause::Timeout, "{detail}");
            }
            ExecResult::Ok(v) => panic!("expected a timeout, got {v}"),
        }
    }

    #[test]
    fn tool_env_policy_reads_the_flag_and_treats_a_bare_one_as_clear_only() {
        use areev_core::proc::EnvPolicy;
        let _env = env_guard();
        assert_eq!(tool_env_policy(&flags(&[])), None, "unset keeps the inherit default");

        let minimal = EnvPolicy::minimal_allow();
        match tool_env_policy(&flags(&[("tool-env", "true")])) {
            Some(EnvPolicy::ClearExcept { allow }) => assert_eq!(allow, minimal),
            other => panic!("a valueless --tool-env must still clear, got {other:?}"),
        }
        match tool_env_policy(&flags(&[("tool-env", "AWS_REGION, HTTPS_PROXY")])) {
            Some(EnvPolicy::ClearExcept { allow }) => {
                let mut want = minimal.clone();
                want.extend(["AWS_REGION".to_string(), "HTTPS_PROXY".to_string()]);
                assert_eq!(allow, want);
            }
            other => panic!("expected a cleared environment, got {other:?}"),
        }
        // An EMPTY value is a real setting here — "clear to the minimal set" —
        // and must never be read as "unset", which would silently pick the
        // weaker inherit posture. `flag_or_env`'s empty-means-unset rule is
        // right everywhere else and wrong here, which is why this function
        // does its own presence check.
        match tool_env_policy(&flags(&[("tool-env", "")])) {
            Some(EnvPolicy::ClearExcept { allow }) => assert_eq!(allow, minimal),
            other => panic!("an empty --tool-env must clear, not inherit, got {other:?}"),
        }
        match tool_env_policy(&flags(&[("tool-env", "   ")])) {
            Some(EnvPolicy::ClearExcept { allow }) => assert_eq!(allow, minimal),
            other => panic!("a whitespace --tool-env must clear, not inherit, got {other:?}"),
        }
    }

    /// The same trap one layer out: a systemd unit or cron block that writes
    /// `AREEV_RUN_TOOL_ENV=""` is asking for the strictest environment, and
    /// must not be handed the loosest one.
    #[test]
    fn an_empty_tool_env_variable_still_clears() {
        use areev_core::proc::EnvPolicy;
        let _env = env_guard();
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("AREEV_RUN_TOOL_ENV", v),
                    None => std::env::remove_var("AREEV_RUN_TOOL_ENV"),
                }
            }
        }
        let _restore = Restore(std::env::var("AREEV_RUN_TOOL_ENV").ok());

        std::env::set_var("AREEV_RUN_TOOL_ENV", "");
        match tool_env_policy(&flags(&[])) {
            Some(EnvPolicy::ClearExcept { allow }) => {
                assert_eq!(allow, EnvPolicy::minimal_allow());
            }
            other => panic!("AREEV_RUN_TOOL_ENV=\"\" must clear, not inherit, got {other:?}"),
        }

        std::env::remove_var("AREEV_RUN_TOOL_ENV");
        assert_eq!(
            tool_env_policy(&flags(&[])),
            None,
            "an ABSENT variable is what keeps the inherit default"
        );
    }

    /// #188: proof the flag reaches the constructed executor. `PATH` is
    /// asserted alongside the planted variable because without it a bare
    /// command name in a cleared environment resolves to nothing.
    #[cfg(unix)]
    #[test]
    fn tool_env_clears_the_environment_and_passes_only_what_it_names() {
        let _env = env_guard();
        const PLANTED: &str = "AREEV_TEST_TOOL_ENV_PLANTED";
        const NAMED: &str = "AREEV_TEST_TOOL_ENV_NAMED";
        // Removed on unwind too: a failing assertion must not leak these into
        // sibling tests sharing this process.
        struct Planted;
        impl Drop for Planted {
            fn drop(&mut self) {
                std::env::remove_var(PLANTED);
                std::env::remove_var(NAMED);
            }
        }
        let _planted = Planted;
        std::env::set_var(PLANTED, "leaked");
        std::env::set_var(NAMED, "kept");
        let cmd = format!(
            r#"printf '{{"planted":"%s","named":"%s","path":"%s"}}' "${PLANTED}" "${NAMED}" "${{PATH:+set}}""#
        );

        let seen = |extra: &[(&str, &str)]| {
            let mut f = vec![("tool-cmd", cmd.as_str())];
            f.extend_from_slice(extra);
            match tool_executor(&flags(&f), None).execute("work", "h", &serde_json::json!({}), "k")
            {
                ExecResult::Ok(v) => v,
                ExecResult::Err { detail, .. } => panic!("{detail}"),
            }
        };

        let inherited = seen(&[]);
        assert_eq!(inherited["planted"], "leaked", "the default still inherits");

        let cleared = seen(&[("tool-env", NAMED)]);
        assert_eq!(cleared["planted"], "", "an unnamed variable must not survive the clear");
        assert_eq!(cleared["named"], "kept", "a named one must");
        assert_eq!(cleared["path"], "set", "PATH is load-bearing — without it nothing resolves");
    }

    /// An allow list may not re-admit what the operator told Areev is a
    /// secret — otherwise `--passphrase-env X --tool-env X` hands the
    /// passphrase to every tool, and #100's invariant becomes conditional.
    #[cfg(unix)]
    #[test]
    fn tool_env_refuses_to_re_admit_a_registered_secret() {
        const SECRET: &str = "AREEV_TEST_TOOL_ENV_SECRET";
        struct Planted;
        impl Drop for Planted {
            fn drop(&mut self) {
                std::env::remove_var(SECRET);
            }
        }
        let _planted = Planted;
        std::env::set_var(SECRET, "hunter2");
        areev_core::proc::deny_env_var(SECRET);

        let cmd = format!(r#"printf '{{"seen":"%s"}}' "${SECRET}""#);
        let out = tool_executor(
            &flags(&[("tool-cmd", cmd.as_str()), ("tool-env", SECRET)]),
            None,
        )
        .execute("work", "h", &serde_json::json!({}), "k");
        match out {
            ExecResult::Ok(v) => assert_eq!(
                v["seen"], "",
                "a registered secret named in --tool-env must still be withheld"
            ),
            ExecResult::Err { detail, .. } => panic!("{detail}"),
        }
    }
}
