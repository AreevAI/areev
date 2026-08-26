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

/// `--credential name=ENV_VAR`, `--allow-host URL`, `--tool-egress
/// tool:cred+cred:METHOD+METHOD`. Returns None when no egress is configured,
/// which leaves tools exactly as they were.
pub fn build_egress(
    flags: &HashMap<String, String>,
) -> Result<Option<areev_run::Broker>, String> {
    let (creds, hosts, tools) =
        (flag(flags, "credential"), flag(flags, "allow-host"), flag(flags, "tool-egress"));
    if creds.is_none() && hosts.is_none() && tools.is_none() {
        return Ok(None);
    }
    // How long a MINTED credential may be reused, and which variables a
    // resolver may see (#113). Both are host config and apply to every
    // dynamic source this process configures.
    let ttl_secs = match flag(flags, "credential-ttl") {
        None => None,
        Some(v) => Some(
            v.trim()
                .parse::<u64>()
                .map_err(|_| format!("--credential-ttl: expected whole seconds, got {v:?}"))?,
        ),
    };
    let resolver_env: Vec<String> = flag(flags, "resolver-env")
        .iter()
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut credentials = std::collections::BTreeMap::new();
    // name -> owning run principal, for `--credential name=VAR@principal` and
    // its source-agnostic sibling `--credential name@principal=…`.
    let mut owners: Vec<(String, String)> = Vec::new();
    for pair in creds.iter().flat_map(|v| v.split(',')) {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (lhs, spec) = pair.split_once('=').ok_or_else(|| {
            format!(
                "--credential: expected name=ENV_VAR[@principal], name[@principal]=cmd:COMMAND, \
                 or name[@principal]=vault:PATH#FIELD, got {pair:?} — note that a comma \
                 separates credentials, so a resolver command containing one belongs in a script"
            )
        })?;
        // The principal may be named on the NAME side, which is the only
        // unambiguous place for a `cmd:` source: a command can contain '@'
        // anywhere, so splitting one on it would bind the credential to a
        // principal the operator never wrote (#113).
        let (name, name_owner) = match lhs.trim().split_once('@') {
            Some((n, p)) if !p.trim().is_empty() => (n.trim(), Some(p.trim().to_string())),
            Some(_) => {
                return Err(format!(
                    "--credential {pair:?}: empty principal after '@' — write name=SOURCE for an \
                     unbound credential, or name@principal=SOURCE to bind one"
                ))
            }
            None => (lhs.trim(), None),
        };
        if name.is_empty() {
            return Err(format!("--credential {pair:?}: the credential has no name"));
        }
        // An env-var value is READ here from a variable the host named — a
        // secret on a command line is a secret in shell history and in `ps`.
        // A `cmd:`/`vault:` source reads nothing yet; it is minted per TTL
        // window at call time, inside the broker.
        let (source, spec_owner) = areev_run::CredentialSource::from_spec(spec.trim())?;
        let owner = match (name_owner, spec_owner) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "--credential {pair:?}: names a principal on both sides of '=' — pick one"
                ))
            }
            (a, b) => a.or(b),
        };
        let name = name.to_string();
        if let Some(o) = owner {
            owners.push((name.clone(), o));
        }
        credentials.insert(name, source.with_resolver_config(ttl_secs, &resolver_env));
    }
    let policy = match &hosts {
        // Absent means unrestricted, and is reported as such rather than
        // silently reading as a policy.
        None => areev_run::EgressPolicy::unrestricted(),
        Some(list) => {
            let entries: Vec<serde_json::Value> = list
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(|h| serde_json::json!(h))
                .collect();
            areev_run::EgressPolicy::from_config(Some(&serde_json::json!({
                "int:allowed_outbound_hosts": entries
            })))?
        }
    };
    let mut grants = areev_run::EgressGrants::new();
    for spec in tools.iter().flat_map(|v| v.split(',')) {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        // A URL anywhere in the spec is an operator writing `cred@https://host`
        // for the #112 pairing. Split on ':' that would leave `https` sitting
        // in the host position — a pairing that matches nothing while reading
        // as a restriction. Caught here, where the whole spec is still intact
        // and the message can say what to write instead.
        if spec.contains("://") {
            return Err(format!(
                "--tool-egress {spec:?}: pair a credential with a BARE hostname \
                 (cred@api.example.com), not a URL — this spec is colon-delimited, so a scheme \
                 or port would tear it apart; scheme and port are narrowed by --allow-host"
            ));
        }
        let mut parts = spec.split(':');
        let tool = parts.next().unwrap_or("").trim();
        if tool.is_empty() {
            return Err(format!(
                "--tool-egress: expected tool:cred[@host]+cred[@host]:METHOD+METHOD, got {spec:?}"
            ));
        }
        let mut g = areev_run::CallerGrant::new();
        for c in parts.next().unwrap_or("").split('+').map(str::trim) {
            if c.is_empty() {
                continue;
            }
            // `cred@host` pairs the credential with where it may be sent
            // (#112); a bare `cred` keeps the older any-host meaning.
            g = match c.split_once('@') {
                Some((name, host)) => {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(format!(
                            "--tool-egress {spec:?}: {c:?} has no credential name before '@'"
                        ));
                    }
                    let host = areev_run::AllowedHost::parse_host_pattern(host, "--tool-egress")?;
                    g.credential_for(name, vec![host])
                }
                None => g.credential(c),
            };
        }
        for m in parts.next().unwrap_or("").split('+').map(str::trim) {
            if m.is_empty() {
                continue;
            }
            // Validated rather than accepted verbatim: an unrecognized token
            // here used to become a method nothing would ever match, so a
            // typo — or a port that survived the colon split — produced a
            // grant that silently refused every write at runtime.
            let upper = m.to_ascii_uppercase();
            if !matches!(
                upper.as_str(),
                "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
            ) {
                // A field of digits is almost always a port that survived the
                // colon split (`cred@host:8443`), so say that rather than
                // leaving the operator to work back from "8443 is not a method".
                let hint = if m.chars().all(|c| c.is_ascii_digit()) {
                    " — if you meant a port, drop it: a credential↔host pairing \
                     names the host, and the port is narrowed by --allow-host"
                } else {
                    ""
                };
                return Err(format!(
                    "--tool-egress {spec:?}: {m:?} is not an HTTP method; accepted: \
                     GET, HEAD, POST, PUT, PATCH, DELETE{hint}"
                ));
            }
            g = g.method(&upper);
        }
        grants = grants.grant(tool, g);
    }
    let broker = areev_run::Broker::start(policy, credentials, grants, "RUN-E022")?;
    for (name, owner) in owners {
        broker.bind_credential_owner(&name, &owner);
    }
    Ok(Some(broker))
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

/// The executor a run's nodes dispatch through: the `--tool-cmd` subprocess,
/// wrapped in the pinned code executor when the host authorized one.
///
/// Nothing runs from the file's own say-so. A `Definition` may name its
/// executor by content address and the blob travels with the memory, so the
/// grant has to come from the host — a grant in the file would arrive in the
/// same bundle as the code it authorizes. The broker handle reaches the code
/// executor too (#87): a pinned blob gets the SAME credential story as a
/// `--tool-cmd`, whether or not one is configured.
pub fn tool_executor(
    flags: &HashMap<String, String>,
    egress: Option<&areev_run::EgressHandle>,
) -> Arc<dyn HostToolExecutor> {
    let timeout = executor_timeout(flags);
    let base: Arc<dyn HostToolExecutor> = match flag_or_env(flags, "tool-cmd", "AREEV_RUN_TOOL_CMD")
    {
        Some(cmd) => {
            let mut ce = CommandExecutor::new(&cmd);
            if let Some(t) = timeout {
                ce = ce.with_timeout(t);
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
}
