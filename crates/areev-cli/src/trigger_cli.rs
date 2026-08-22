//! `areev trigger` — declare, inspect, and evaluate triggers.
//!
//! Structurally this follows `areev retention`: a declarative policy that
//! travels with the memory, enforced by a separate explicit command. `add`
//! prints the same kind of "declared, not enforced" nudge, because a trigger
//! that nobody evaluates looks exactly like a trigger that has nothing to do.

use std::collections::HashMap;
use std::sync::Arc;

use areev_cal::AreevFacade;
use areev_core::types::{Catchup, Concurrency, Grain, Trigger, TriggerKind};
use areev_store::Areev;
use areev_trigger::{
    predicate, schedule, EvalOptions, Evaluator, RunStarter, StartResult, SystemClock,
};

use crate::{flag, need};

/// `10m`, `2h`, `90s`, `1d` -> milliseconds.
///
/// The unit suffix is mandatory. A bare number is ambiguous between the seconds
/// `--interval` takes and the milliseconds the field stores, and guessing wrong
/// moves a correlation window by three orders of magnitude without saying so.
fn parse_window_ms(spec: &str) -> Result<i64, String> {
    let spec = spec.trim();
    let bad = || {
        format!("--window: {spec:?} must be a positive number with a unit -- 90s, 10m, 2h, 1d")
    };
    let mult = match spec.as_bytes().last() {
        Some(b's') => 1_000i64,
        Some(b'm') => 60_000,
        Some(b'h') => 3_600_000,
        Some(b'd') => 86_400_000,
        _ => return Err(bad()),
    };
    // The suffix matched an ASCII byte, so trimming one byte stays on a
    // character boundary.
    let n: i64 = spec[..spec.len() - 1].trim().parse().map_err(|_| bad())?;
    if n <= 0 {
        return Err(bad());
    }
    n.checked_mul(mult).ok_or_else(bad)
}

/// First `n` characters, safely.
///
/// `&s[..12]` panics on a string shorter than 12 bytes and on a multi-byte
/// boundary. Hashes we author are 64 hex, but a declaration can arrive by
/// bundle import from an implementation we did not write, and a display helper
/// must not be the thing that takes the process down.
fn short(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Bridges the evaluator to the real runtime.
///
/// The duplicate rule is the whole idempotency story, so it is worth being
/// exact about where it comes from: `Runner::start` refuses an existing run id
/// with `RUN-E016`, and that refusal — not a lease, not a lock — is what makes
/// a re-delivered item a skip instead of a second run.
struct RunnerStarter {
    runner: areev_run::Runner,
    opts: areev_run::RunOptions,
}

impl RunStarter for RunnerStarter {
    fn start(&self, workflow: &str, run_id: &str, input: serde_json::Value) -> StartResult {
        let hash = match areev_core::error::Hash::from_hex(workflow) {
            Ok(h) => h,
            Err(e) => return StartResult::Failed(format!("workflow {workflow}: {e}")),
        };
        match self.runner.start(&hash, run_id, input, &self.opts) {
            Ok(_) => StartResult::Started,
            // The idempotency guarantee, surfacing as the runtime's own refusal.
            Err(areev_run_core::RunError::Tainted { why }) if why.contains("already exists") => {
                StartResult::Duplicate
            }
            Err(e) => StartResult::Failed(e.to_string()),
        }
    }
}

/// The run ceilings a firing passes on, from the same flags `run start` takes.
fn run_budgets(flags: &HashMap<String, String>) -> areev_run::RunOptions {
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

pub fn run_trigger(
    m: Areev,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub = positional.first().map(|s| s.as_str()).unwrap_or("status");
    let facade = Arc::new(AreevFacade::with_session(m, Some(ns.to_string()), None));
    let json_out = flag(flags, "format").as_deref() == Some("json");

    match sub {
        "add" => add(&facade, ns, flags, json_out),
        "list" => list(&facade, ns, json_out),
        "show" => show(&facade, ns, positional.get(1), json_out),
        "run" => evaluate(facade, ns, flags, json_out),
        "pause" => set_paused(&facade, ns, positional.get(1), flags, true, json_out),
        "resume" => set_paused(&facade, ns, positional.get(1), flags, false, json_out),
        "status" => status(&facade, ns, json_out),
        "render" => render_target(&facade, ns, flags, positional.get(1)),
        "deliver" => deliver(facade, ns, flags, json_out),
        other => Err(format!(
            "unknown trigger subcommand '{other}' \
             (add|list|show|status|run|render|deliver|pause|resume)"
        )),
    }
}

fn evaluator(facade: Arc<AreevFacade>, ns: &str, flags: &HashMap<String, String>) -> Evaluator {
    // The same seam host tools use. A connector IS a tool — JSON in, JSON out,
    // one process per invocation — so there is one subprocess contract to learn
    // and connectors inherit its timeout, output cap and secret scrub.
    let connector: Option<Arc<dyn areev_run::HostToolExecutor>> =
        flag(flags, "connector-cmd").or_else(|| flag(flags, "tool-cmd")).map(|cmd| {
            Arc::new(areev_run::CommandExecutor::new(&cmd)) as Arc<dyn areev_run::HostToolExecutor>
        });

    // Starting runs needs a tool command of its own: the workflow's nodes have
    // to execute. Without one, the pass ingests items and records firings but
    // starts nothing, which is a useful mode rather than a broken one.
    let starter: Option<Arc<dyn RunStarter>> = flag(flags, "tool-cmd").map(|cmd| {
        let runner = areev_run::Runner {
            facade: Arc::clone(&facade),
            clock: Arc::new(areev_run::SystemClock),
            executor: Arc::new(areev_run::CommandExecutor::new(&cmd)),
            llm: None,
            observer: None,
            ns: ns.to_string(),
            principal: flag(flags, "as").unwrap_or_else(|| "user:local".into()),
        };
        Arc::new(RunnerStarter { runner, opts: run_budgets(flags) }) as Arc<dyn RunStarter>
    });

    // Credentials are named on the command line and READ here, so a value
    // never appears in a grain, in shell history, or in the connector's
    // environment: `--credential gmail=GMAIL_TOKEN_VAR` names the variable.
    let mut credentials = std::collections::BTreeMap::new();
    if let Some(spec) = flag(flags, "credential") {
        for pair in spec.split(',') {
            if let Some((name, var)) = pair.split_once('=') {
                if let Ok(c) = areev_run::Credential::bearer_from_env(var.trim()) {
                    credentials.insert(name.trim().to_string(), c);
                }
            }
        }
    }

    Evaluator {
        facade,
        clock: Arc::new(SystemClock),
        connector,
        starter,
        credentials,
        ns: ns.to_string(),
        principal: flag(flags, "as").unwrap_or_else(|| "user:local".into()),
    }
}

fn add(
    facade: &Arc<AreevFacade>,
    ns: &str,
    flags: &HashMap<String, String>,
    json_out: bool,
) -> Result<(), String> {
    let kind_s = need(flags, "type")?;
    let kind = TriggerKind::parse(&kind_s).ok_or_else(|| {
        format!(
            "unknown trigger type '{kind_s}' \
             (interval|schedule|once|polling|memory|webhook|manual|composite)"
        )
    })?;
    let workflow = need(flags, "workflow")?;
    // A rationale is not optional in spirit: a trigger with no recorded reason
    // is not auditable, the same argument retention policies make.
    let because = need(flags, "because")?;

    let mut t = Trigger::new(kind, &workflow).namespace(ns);
    if let Some(c) = flag(flags, "observer").or_else(|| flag(flags, "connector")) {
        t = t.connector(&c);
    }
    if let Some(s) = flag(flags, "scope") {
        t = t.scope(&s);
    }
    if let Some(i) = flag(flags, "interval") {
        t = t.interval_secs(i.parse().map_err(|_| format!("--interval: not a number: {i}"))?);
    }
    if let Some(c) = flag(flags, "cron") {
        t = t.cron(&c);
    }
    if let Some(a) = flag(flags, "at") {
        t = t.at_ms(a.parse().map_err(|_| format!("--at: not an epoch-ms integer: {a}"))?);
    }
    for k in flag(flags, "dedup-key").iter().flat_map(|v| v.split(',')) {
        t = t.dedup_key(k.trim());
    }
    if let Some(c) = flag(flags, "concurrency") {
        t = t.concurrency(
            Concurrency::parse(&c).ok_or_else(|| format!("--concurrency: {c} (forbid|allow|replace)"))?,
        );
    }
    if let Some(c) = flag(flags, "catchup") {
        t = t.catchup(Catchup::parse(&c).ok_or_else(|| format!("--catchup: {c} (last|none|all)"))?);
    }
    // `memory` and `composite` select with a predicate, written in CAL `WHERE`
    // syntax and stored as a Condition tree. A data structure rather than an
    // expression string is what keeps this off the frozen edge grammar and out
    // of an OMS syntax decision -- see `predicate`'s module doc.
    if let Some(w) = flag(flags, "where") {
        let cond = predicate::parse_predicate(&w).map_err(|e| e.to_string())?;
        t = t.predicate(predicate::to_value(&cond).map_err(|e| e.to_string())?);
    }
    // `alias=hash` pairs. A gate names its members by alias because a 64-hex
    // content address is not a legal identifier in any expression grammar --
    // CAL's lexer reads it as a number.
    for pair in flag(flags, "members").iter().flat_map(|v| v.split(',')) {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (alias, hash) = pair
            .split_once('=')
            .ok_or_else(|| format!("--members: expected alias=hash pairs, got {pair:?}"))?;
        let (alias, hash) = (alias.trim(), hash.trim());
        if alias.is_empty() || hash.is_empty() {
            return Err(format!("--members: both sides of {pair:?} must be non-empty"));
        }
        t = t.member(alias, hash);
    }
    if let Some(c) = flag(flags, "correlate") {
        t = t.correlate(&c);
    }
    if let Some(w) = flag(flags, "window") {
        t = t.window_ms(parse_window_ms(&w)?);
    }
    if let Some(tz) = flag(flags, "timezone") {
        t = t.config(serde_json::json!({ "int:timezone": tz }));
    }
    t = t.extra_field("because", serde_json::json!(because));

    // Refuse here rather than storing something that can never fire — the
    // failure nobody notices, because its symptom is nothing happening.
    schedule::validate(&t).map_err(|e| e.to_string())?;

    let hash = facade.with_store(|m| m.add(&t)).map_err(|e| e.to_string())?;
    if json_out {
        println!("{}", serde_json::json!({ "ok": true, "trigger": hash.to_hex() }));
    } else {
        println!("declared trigger {}", hash.to_hex());
        eprintln!(
            "areev: declared, not enforced — run `areev trigger run` on a heartbeat \
             (cron, launchd, systemd, or a k8s CronJob)"
        );
    }
    Ok(())
}

fn list(facade: &Arc<AreevFacade>, ns: &str, json_out: bool) -> Result<(), String> {
    let ev = Evaluator::read_only(Arc::clone(facade), Arc::new(SystemClock), ns);
    let declarations = ev.declarations().map_err(|e| e.to_string())?;
    if json_out {
        let rows: Vec<_> = declarations
            .iter()
            .map(|(h, t)| {
                serde_json::json!({
                    "trigger": h, "kind": t.kind.as_str(), "workflow": t.workflow,
                    "connector": t.connector, "scope": t.scope, "enabled": t.enabled,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "ok": true, "triggers": rows }));
        return Ok(());
    }
    if declarations.is_empty() {
        println!("no triggers declared in {ns}");
        return Ok(());
    }
    for (h, t) in declarations {
        let what = t.scope.as_deref().unwrap_or("-");
        let off = if t.enabled { "" } else { "  [disabled]" };
        println!(
            "{}  {:<9} {:<28} -> {}{off}",
            short(&h, 12),
            t.kind.as_str(),
            what,
            short(&t.workflow, 12)
        );
    }
    Ok(())
}

fn show(
    facade: &Arc<AreevFacade>,
    ns: &str,
    id: Option<&String>,
    json_out: bool,
) -> Result<(), String> {
    let id = id.ok_or("usage: areev trigger show <TRIGGER>")?;
    let ev = Evaluator::read_only(Arc::clone(facade), Arc::new(SystemClock), ns);
    let found = ev
        .status()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|s| s.trigger.starts_with(id.as_str()))
        .ok_or_else(|| format!("no trigger matching '{id}' in {ns}"))?;
    if json_out {
        println!("{}", serde_json::to_string(&found).map_err(|e| e.to_string())?);
    } else {
        println!("trigger      {}", found.trigger);
        println!("kind         {}", found.kind);
        println!("workflow     {}", found.workflow);
        println!("enabled      {}", found.enabled);
        println!("paused       {}", found.paused);
        println!("due          {}", found.due);
        if let Some(by) = &found.leased_by {
            println!("leased by    {by}");
        }
        match found.last_fired_at {
            Some(t) => println!("last fired   {t}"),
            None => println!("last fired   never"),
        }
        if found.exhausted {
            println!("exhausted    yes — a one-shot past its instant; it will not fire again");
        }
        if let Some(why) = &found.unusable {
            println!("cannot fire  {why}");
        }
        if found.consecutive_failures > 0 {
            println!("failures     {}", found.consecutive_failures);
        }
        if let Some(e) = &found.last_error {
            println!("last error   {e}");
        }
    }
    Ok(())
}

fn status(facade: &Arc<AreevFacade>, ns: &str, json_out: bool) -> Result<(), String> {
    let ev = Evaluator::read_only(Arc::clone(facade), Arc::new(SystemClock), ns);
    let rows = ev.status().map_err(|e| e.to_string())?;
    if json_out {
        println!(
            "{}",
            serde_json::json!({ "ok": true, "triggers": rows })
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("no triggers declared in {ns}");
        return Ok(());
    }
    for s in &rows {
        let state = if !s.enabled {
            "disabled"
        } else if s.unusable.is_some() {
            // The whole point (#67): a declaration that can never fire must not
            // sit in the same column as one that is merely waiting its turn.
            "unusable"
        } else if s.exhausted {
            // A one-shot that has fired. Reporting it as "waiting" would be a
            // lie it tells forever.
            "done"
        } else if s.paused {
            "paused"
        } else if s.leased_by.is_some() {
            "running"
        } else if s.due {
            "due"
        } else {
            "waiting"
        };
        println!(
            "{}  {:<8} {:<9} {}",
            short(&s.trigger, 12),
            state,
            s.kind,
            short(&s.workflow, 12)
        );
        if let Some(why) = &s.unusable {
            println!("              cannot fire: {why}");
        }
        if let Some(e) = &s.last_error {
            println!("              last error: {e}");
        }
    }
    // An unfired trigger is invisible otherwise — the loop learned this the
    // same way, and self-reports staleness so a forgotten cron does not
    // silently kill it.
    let never = rows.iter().filter(|s| s.never_fired && s.enabled && !s.paused).count();
    if never > 0 {
        eprintln!(
            "⚠ {never} enabled trigger(s) have never fired — is `areev trigger run` \
             on a heartbeat?"
        );
    }
    Ok(())
}

fn evaluate(
    facade: Arc<AreevFacade>,
    ns: &str,
    flags: &HashMap<String, String>,
    json_out: bool,
) -> Result<(), String> {
    let ev = evaluator(facade, ns, flags);
    let mut opts = EvalOptions { dry_run: flag(flags, "dry-run").is_some(), ..Default::default() };
    if let Some(id) = flag(flags, "id") {
        opts.only = Some(id);
    }
    if let Some(l) = flag(flags, "lease") {
        opts.lease = std::time::Duration::from_secs(
            l.parse().map_err(|_| format!("--lease: not a number of seconds: {l}"))?,
        );
    }
    if let Some(n) = flag(flags, "max-items") {
        opts.max_items = n.parse().map_err(|_| format!("--max-items: not a number: {n}"))?;
    }

    let report = ev.run(&opts).map_err(|e| e.to_string())?;
    if json_out {
        println!("{}", serde_json::to_string(&report).map_err(|e| e.to_string())?);
    } else {
        let started = if report.runs_started > 0 || report.ingested == 0 {
            format!("runs {}", report.runs_started)
        } else {
            // Say what actually happened rather than implying runs executed.
            format!("ingested {} (no --tool-cmd, so nothing was executed)", report.ingested)
        };
        // `unusable` is printed unconditionally when non-zero: burying it
        // would recreate the silence this counter exists to break.
        let unusable =
            if report.unusable > 0 { format!(" · unusable {}", report.unusable) } else { String::new() };
        println!(
            "claimed {} · items {} · {started} · duplicates {} · not due {} · locked {}{unusable}",
            report.claimed,
            report.items,
            report.duplicates,
            report.skipped_not_due,
            report.skipped_locked
        );
        for e in &report.errors {
            eprintln!("error: {e}");
        }
    }
    // Exit 0 on a clean pass with nothing to do, so a heartbeat never pages on
    // a healthy no-op — the same discipline `loop run` uses.
    if report.errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{} trigger(s) failed", report.errors.len()))
    }
}

/// `areev trigger render` — emit scheduler config, never create it.
fn render_target(
    facade: &Arc<AreevFacade>,
    ns: &str,
    flags: &HashMap<String, String>,
    positional_target: Option<&String>,
) -> Result<(), String> {
    let target = flag(flags, "target")
        .or_else(|| positional_target.cloned())
        .ok_or_else(|| {
            format!(
                "usage: areev trigger render --target <{}>",
                areev_trigger::render::TARGETS.join("|")
            )
        })?;

    let ev = Evaluator::read_only(Arc::clone(facade), Arc::new(SystemClock), ns);
    let declarations: Vec<_> =
        ev.declarations().map_err(|e| e.to_string())?.into_iter().map(|(_, t)| t).collect();
    let heartbeat = areev_trigger::render::heartbeat_secs(&declarations);

    let db = flag(flags, "db").unwrap_or_else(|| "memory.db".into());
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "areev".into());
    let extra = flag(flags, "extra-args").unwrap_or_default();

    let ctx = areev_trigger::render::RenderContext {
        exe: &exe,
        db: &db,
        ns,
        heartbeat_secs: heartbeat,
        extra_args: &extra,
    };
    print!("{}", areev_trigger::render::render(&target, &ctx).map_err(|e| e.to_string())?);
    eprintln!(
        "areev: heartbeat {heartbeat}s — the memory owns the real cadence, so this is \
         deliberately coarser than your shortest interval"
    );
    Ok(())
}

/// `areev trigger deliver` — the host received something; hand it over.
fn deliver(
    facade: Arc<AreevFacade>,
    ns: &str,
    flags: &HashMap<String, String>,
    json_out: bool,
) -> Result<(), String> {
    let id = need(flags, "id")?;
    let raw = match flag(flags, "payload").as_deref() {
        // Read stdin when no literal was given. Note `--payload -` cannot mean
        // stdin here the way it does in other tools: `parse_args` treats a
        // following token that starts with `-` as another flag, so `--payload -`
        // arrives as the valueless-flag sentinel `"true"`. Both spellings, and
        // omitting the flag entirely, mean stdin.
        Some("-") | Some("true") | None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| format!("reading payload from stdin: {e}"))?;
            buf
        }
        Some(literal) => literal.to_string(),
    };
    let payload: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|e| format!("payload is not JSON: {e}"))?;

    let ev = evaluator(facade, ns, flags);
    let report = ev.deliver(&id, payload).map_err(|e| e.to_string())?;
    if json_out {
        println!("{}", serde_json::to_string(&report).map_err(|e| e.to_string())?);
    } else if report.duplicates > 0 {
        println!("already delivered — no new run started");
    } else if report.unidentifiable > 0 {
        println!("payload carries no value at the declared dedup key — nothing started");
    } else {
        println!("delivered · runs {} · ingested {}", report.runs_started, report.ingested);
    }
    Ok(())
}

fn set_paused(
    facade: &Arc<AreevFacade>,
    ns: &str,
    id: Option<&String>,
    flags: &HashMap<String, String>,
    paused: bool,
    json_out: bool,
) -> Result<(), String> {
    let verb = if paused { "pause" } else { "resume" };
    let id = id.ok_or_else(|| format!("usage: areev trigger {verb} <TRIGGER> --because \"...\""))?;
    need(flags, "because")?;

    let ev = Evaluator::read_only(Arc::clone(facade), Arc::new(SystemClock), ns);
    let target = ev
        .declarations()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|(h, _)| h.starts_with(id.as_str()))
        .ok_or_else(|| format!("no trigger matching '{id}' in {ns}"))?
        .0;

    // Read-modify-write against the exact prior row: pausing must not clobber a
    // cursor a concurrent firing just advanced.
    let (mut state, raw) = facade
        .with_store(|m| m.trigger_state(&target))
        .map_err(|e| e.to_string())?
        .map(|(s, r)| (s, Some(r)))
        .unwrap_or_default();
    state.paused = paused;
    let ok = facade
        .with_store(|m| m.put_trigger_state(&target, raw.as_deref(), &state))
        .map_err(|e| e.to_string())?;
    if !ok {
        return Err(format!(
            "trigger {} changed underneath this command (a firing is in progress) — retry",
            short(&target, 12)
        ));
    }
    if json_out {
        println!("{}", serde_json::json!({ "ok": true, "trigger": target, "paused": paused }));
    } else {
        println!("{verb}d trigger {}", short(&target, 12));
    }
    Ok(())
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    fn flags(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn budget_flags_reach_the_run_a_firing_starts() {
        // `trigger run` built RunOptions::default(), so moving a workflow
        // behind a trigger silently dropped every ceiling.
        let o = run_budgets(&flags(&[
            ("max-tokens", "5000"),
            ("max-usd", "0.25"),
            ("max-wall-ms", "60000"),
            ("ask-ttl", "3600"),
        ]));
        assert_eq!(o.budgets.max_tokens, Some(5000));
        assert_eq!(o.budgets.max_usd_micros, Some(250_000), "--max-usd is dollars, stored as micros");
        assert_eq!(o.budgets.max_wall_ms, Some(60_000));
        assert_eq!(o.ask_ttl_sec, Some(3600));
    }

    #[test]
    fn no_budget_flags_means_no_ceiling_not_a_surprise_one() {
        let o = run_budgets(&flags(&[]));
        assert_eq!(o.budgets.max_tokens, None);
        assert_eq!(o.budgets.max_usd_micros, None);
        assert_eq!(o.ask_ttl_sec, None);
        assert_eq!(o.workers, 4, "the documented default");
    }
}
