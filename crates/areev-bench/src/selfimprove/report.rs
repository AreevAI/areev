//! report.json / report.md / transcript JSONL emit — the honesty surface of
//! every selfimprove bin (SELFIMPROVE.md): transcripts carry every model
//! call, the governance ledger ships verbatim with its failures, and the
//! markdown states plainly what was measured.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{EvalSummary, Ledger};

/// Writes every artifact of one bench run under a single output directory.
pub struct Reporter {
    dir: PathBuf,
}

/// One JSONL stream: one JSON value per line, flushed on drop.
pub struct TranscriptWriter {
    w: BufWriter<File>,
}

impl TranscriptWriter {
    /// Append one row. Panics on IO failure — a silently dropped transcript
    /// row would break the honesty contract, so the run dies instead.
    pub fn row(&mut self, v: &Value) {
        serde_json::to_writer(&mut self.w, v).expect("transcript row");
        self.w.write_all(b"\n").expect("transcript row");
    }

    /// Explicit mid-run flush for callers that want durability per task.
    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

impl Drop for TranscriptWriter {
    fn drop(&mut self) {
        let _ = self.w.flush();
    }
}

impl Reporter {
    /// Creates the output directory (and parents) if needed.
    pub fn new(dir: impl AsRef<Path>) -> io::Result<Reporter> {
        fs::create_dir_all(dir.as_ref())?;
        Ok(Reporter { dir: dir.as_ref().to_path_buf() })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Open a transcript stream; `name` is the file name verbatim
    /// (e.g. "transcripts.jsonl"). Truncates any previous run's file.
    pub fn transcript(&self, name: &str) -> io::Result<TranscriptWriter> {
        let f = File::create(self.dir.join(name))?;
        Ok(TranscriptWriter { w: BufWriter::new(f) })
    }

    /// Emit report.json (config + ledger + per-state summaries) and
    /// report.md (the three tables).
    pub fn write_report(
        &self,
        config: &Value,
        ledger: &Ledger,
        evals: &[EvalSummary],
    ) -> io::Result<()> {
        let mut js = serde_json::to_string_pretty(&report_json(config, ledger, evals))?;
        js.push('\n');
        fs::write(self.dir.join("report.json"), js)?;
        fs::write(self.dir.join("report.md"), report_md(config, ledger, evals))
    }
}

fn report_json(config: &Value, ledger: &Ledger, evals: &[EvalSummary]) -> Value {
    let ledger_rows: Vec<Value> = ledger
        .entries
        .iter()
        .map(|e| {
            json!({
                "hash": e.hash,
                "source": e.source,
                "summary": e.summary,
                "disposition": e.disposition,
                "because": e.because,
            })
        })
        .collect();
    let eval_rows: Vec<Value> = evals
        .iter()
        .map(|s| {
            let per_rule: Vec<Value> = s
                .per_rule
                .iter()
                .map(|r| json!({ "rule": r.rule, "opportunities": r.opportunities, "failures": r.failures }))
                .collect();
            json!({
                "state": s.state,
                "n": s.n,
                "successes": s.successes,
                "success_rate": s.success_rate(),
                "tool_errors": s.tool_errors,
                "total_steps": s.total_steps,
                "mean_steps": if s.n == 0 { 0.0 } else { s.total_steps as f64 / s.n as f64 },
                "per_rule": per_rule,
                "usage": {
                    "prompt_tokens": s.usage.prompt_tokens,
                    "completion_tokens": s.usage.completion_tokens,
                },
            })
        })
        .collect();
    json!({
        "config": config,
        "ledger": ledger_rows,
        "ledger_counts": {
            "proposed": ledger.entries.len(),
            "applied": ledger.count("applied"),
            "rejected": ledger.count("rejected"),
            "advisory": ledger.count("advisory"),
            "apply_failed": ledger.count("apply_failed"),
        },
        "evals": eval_rows,
    })
}

/// Markdown table cells may not carry pipes or newlines.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn report_md(config: &Value, ledger: &Ledger, evals: &[EvalSummary]) -> String {
    let mut md = String::new();
    md.push_str("# selfimprove report\n\n");
    md.push_str(&format!("*config:* `{config}`\n\n"));

    // (1) The A/B/A/B state table.
    md.push_str("## Held-out eval by state\n\n");
    md.push_str("| state | n | success | tool errors | mean steps | tokens |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for s in evals {
        let mean = if s.n == 0 { 0.0 } else { s.total_steps as f64 / s.n as f64 };
        md.push_str(&format!(
            "| {} | {} | {:.1}% ({}/{}) | {} | {:.1} | {} |\n",
            cell(&s.state),
            s.n,
            s.success_rate() * 100.0,
            s.successes,
            s.n,
            s.tool_errors,
            mean,
            s.usage.prompt_tokens + s.usage.completion_tokens,
        ));
    }

    // (2) Per-rule recurrence: rule × state → mishandled/opportunities.
    md.push_str("\n## Per-rule mishandling recurrence (mishandled/opportunities)\n\n");
    let mut rules: Vec<&str> = Vec::new();
    for s in evals {
        for r in &s.per_rule {
            if !rules.contains(&r.rule) {
                rules.push(r.rule);
            }
        }
    }
    rules.sort_unstable();
    md.push_str("| rule |");
    for s in evals {
        md.push_str(&format!(" {} |", cell(&s.state)));
    }
    md.push_str("\n|---|");
    md.push_str(&"---|".repeat(evals.len()));
    md.push('\n');
    for rule in rules {
        md.push_str(&format!("| {rule} |"));
        for s in evals {
            match s.per_rule.iter().find(|r| r.rule == rule) {
                Some(r) => md.push_str(&format!(" {}/{} |", r.failures, r.opportunities)),
                None => md.push_str(" — |"),
            }
        }
        md.push('\n');
    }

    // (3) The governance ledger, verbatim — the failures are the evidence.
    md.push_str("\n## Governance ledger\n\n");
    md.push_str("| hash | source | disposition | summary | because |\n");
    md.push_str("|---|---|---|---|---|\n");
    for e in &ledger.entries {
        let prefix: String = e.hash.chars().take(8).collect();
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            cell(&prefix),
            cell(&e.source),
            cell(&e.disposition),
            cell(&e.summary),
            cell(&e.because),
        ));
    }
    md.push_str(&format!(
        "\n*{} proposed — {} applied, {} rejected, {} advisory, {} apply_failed.*\n",
        ledger.entries.len(),
        ledger.count("applied"),
        ledger.count("rejected"),
        ledger.count("advisory"),
        ledger.count("apply_failed"),
    ));
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selfimprove::{LedgerEntry, RuleStat, Usage};

    fn sample_ledger() -> Ledger {
        Ledger {
            entries: vec![
                LedgerEntry {
                    hash: "abcdef0123456789".to_string(),
                    source: "tool_failure".to_string(),
                    summary: "refunds over $100 need approval | token first".to_string(),
                    disposition: "applied".to_string(),
                    because: "recurring approval_required cluster".to_string(),
                },
                LedgerEntry {
                    hash: "1234".to_string(),
                    source: "llm".to_string(),
                    summary: "always cancel first".to_string(),
                    disposition: "rejected".to_string(),
                    because: "contradicts journal evidence".to_string(),
                },
                LedgerEntry {
                    hash: "feedface00112233".to_string(),
                    source: "staleness".to_string(),
                    summary: "advisory only".to_string(),
                    disposition: "advisory".to_string(),
                    because: "not executable".to_string(),
                },
            ],
        }
    }

    fn sample_evals() -> Vec<EvalSummary> {
        vec![
            EvalSummary {
                state: "A0".to_string(),
                n: 10,
                successes: 4,
                tool_errors: 17,
                total_steps: 83,
                per_rule: vec![
                    RuleStat { rule: "R1", opportunities: 5, failures: 3 },
                    RuleStat { rule: "R3", opportunities: 4, failures: 4 },
                ],
                usage: Usage { prompt_tokens: 1000, completion_tokens: 200 },
            },
            EvalSummary {
                state: "B".to_string(),
                n: 10,
                successes: 8,
                tool_errors: 3,
                total_steps: 61,
                per_rule: vec![RuleStat { rule: "R1", opportunities: 5, failures: 1 }],
                usage: Usage { prompt_tokens: 900, completion_tokens: 150 },
            },
        ]
    }

    #[test]
    fn transcript_rows_are_one_json_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let rep = Reporter::new(dir.path().join("out")).unwrap();
        {
            let mut tw = rep.transcript("transcripts.jsonl").unwrap();
            tw.row(&serde_json::json!({"task": "exp-0001", "step": 1}));
            tw.row(&serde_json::json!({"task": "exp-0001", "note": "unicode — ok"}));
        } // drop flushes
        let text = fs::read_to_string(rep.dir().join("transcripts.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in &lines {
            let v: Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["task"], serde_json::json!("exp-0001"));
        }
    }

    #[test]
    fn report_json_is_wellformed_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let rep = Reporter::new(dir.path()).unwrap();
        let config = serde_json::json!({"seed": 1, "mode": "mock"});
        rep.write_report(&config, &sample_ledger(), &sample_evals()).unwrap();

        let v: Value =
            serde_json::from_str(&fs::read_to_string(rep.dir().join("report.json")).unwrap())
                .unwrap();
        assert_eq!(v["config"]["mode"], serde_json::json!("mock"));
        assert_eq!(v["ledger"].as_array().unwrap().len(), 3);
        assert_eq!(v["ledger_counts"]["applied"], serde_json::json!(1));
        assert_eq!(v["ledger_counts"]["rejected"], serde_json::json!(1));
        let evals = v["evals"].as_array().unwrap();
        assert_eq!(evals.len(), 2);
        assert!((evals[0]["success_rate"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        assert!((evals[0]["mean_steps"].as_f64().unwrap() - 8.3).abs() < 1e-9);
        assert_eq!(evals[0]["per_rule"][1]["failures"], serde_json::json!(4));
        assert_eq!(evals[1]["usage"]["prompt_tokens"], serde_json::json!(900));
    }

    #[test]
    fn report_md_has_the_three_tables() {
        let dir = tempfile::tempdir().unwrap();
        let rep = Reporter::new(dir.path()).unwrap();
        rep.write_report(&serde_json::json!({}), &sample_ledger(), &sample_evals()).unwrap();
        let md = fs::read_to_string(rep.dir().join("report.md")).unwrap();

        // (1) state table rows
        assert!(md.contains("| state | n | success | tool errors | mean steps | tokens |"));
        assert!(md.contains("| A0 | 10 | 40.0% (4/10) | 17 | 8.3 | 1200 |"));
        assert!(md.contains("| B | 10 | 80.0% (8/10) | 3 | 6.1 | 1050 |"));
        // (2) recurrence: rule × state, absent cells dashed
        assert!(md.contains("| rule | A0 | B |"));
        assert!(md.contains("| R1 | 3/5 | 1/5 |"));
        assert!(md.contains("| R3 | 4/4 | — |"));
        // (3) ledger: 8-char hash prefix, pipes escaped, honest footer
        assert!(md.contains("| abcdef01 | tool_failure | applied |"));
        assert!(md.contains("approval \\| token first"));
        assert!(md.contains("| 1234 | llm | rejected |"));
        assert!(md.contains("*3 proposed — 1 applied, 1 rejected, 1 advisory, 0 apply_failed.*"));
    }

    #[test]
    fn empty_inputs_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let rep = Reporter::new(dir.path().join("a/b/c")).unwrap(); // nested creation
        rep.write_report(&serde_json::json!(null), &Ledger::default(), &[]).unwrap();
        let md = fs::read_to_string(rep.dir().join("report.md")).unwrap();
        assert!(md.contains("*0 proposed — 0 applied, 0 rejected, 0 advisory, 0 apply_failed.*"));
        let v: Value =
            serde_json::from_str(&fs::read_to_string(rep.dir().join("report.json")).unwrap())
                .unwrap();
        assert_eq!(v["evals"].as_array().unwrap().len(), 0);
    }
}
