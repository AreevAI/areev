//! The selfimprove track against a PROVISIONED POSTGRES SCHEMA (#250).
//!
//! The A/B/A/B bench is the one track with no external dataset, a keyless
//! deterministic floor and a programmatic scorer — which makes it the natural
//! case for validating that a Cloud-provisioned memory answers like an
//! embedded file. It could not target one until `--db` / `$AREEV_BENCH_DB`
//! reached the Rust binary, and this is the gate on that path.
//!
//! Two halves, so `cargo test -p areev-bench` covers the DSN path either way:
//!   - without the `postgres` feature, a DSN must be REFUSED by name — never
//!     quietly written into the workdir as a filename;
//!   - with it, the real thing, against `$DATABASE_URL` / `$AREEV_PG_URL`.
//!     Absent a server the postgres half skips with a named reason, and under
//!     `CI=true` it panics instead, so a broken job cannot look like a skipped
//!     one (the rule `areev-conformance`'s runners follow).
//!
//!   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16
//!   export DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres
//!   cargo test -p areev-bench --features postgres --test selfimprove_pg

use areev_bench::selfimprove::memory::{BenchDb, Memory};
#[cfg(feature = "postgres")]
use areev_bench::selfimprove::memory::LearnOutcome;

/// A DSN is a memory locator on every surface that takes one — but only when
/// the binary was built with the backend. Without it the refusal has to NAME
/// the reason: a bench that created `./postgres:` in the workdir and reported
/// a clean run would be the worst of both answers.
#[test]
#[cfg(not(feature = "postgres"))]
fn without_the_postgres_feature_a_dsn_is_refused_by_name() {
    let db = BenchDb::Postgres("postgres://h/db?schema=mem_x".into());
    let err = match Memory::create(&db) {
        Ok(_) => panic!("a DSN must not open without the postgres backend"),
        Err(e) => e,
    };
    assert!(err.contains("postgres"), "{err}");
    assert!(err.contains("--features postgres"), "the refusal names the fix: {err}");
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use areev_bench::selfimprove::{RecordedCall, TaskRunRecord, Usage};

    /// Pinned engine clocks — never the wall clock when the value decides
    /// behavior (the determinism rule), and well inside the 1-day
    /// `outcome_review` horizon so no revert fires mid-test.
    const T1: i64 = 1_700_000_000_000;
    const T2: i64 = T1 + 3_600_000;

    fn pg_url() -> Option<String> {
        std::env::var("AREEV_PG_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
            .filter(|u| u.starts_with("postgres"))
    }

    /// The server, or a loud skip. Under `CI=true` a missing database is a
    /// hard failure: "never ran" and "ran and passed" must not look alike.
    fn url() -> Option<String> {
        match pg_url() {
            Some(u) => Some(u),
            None => {
                if std::env::var("CI").as_deref() == Ok("true") {
                    panic!("CI=true but no DATABASE_URL — the postgres job must not silently skip");
                }
                eprintln!(
                    "skipping: no DATABASE_URL/AREEV_PG_URL (start one: docker run --rm -d \
                     -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16)"
                );
                None
            }
        }
    }

    /// A schema named after this process, dropped on `Drop` — the
    /// schema-per-test analogue of the tempdir rule. No clock or RNG in the
    /// name (determinism), and a leak from a killed run is prefix-recognizable.
    struct Schema {
        url: String,
        name: String,
    }

    impl Schema {
        /// Provision it the way an operator would (`areev provision`), then
        /// release the handle: everything after this runs as a memory role
        /// that needs no `CREATE`.
        fn provision(url: &str, tag: &str) -> Schema {
            let name = format!("aba_{}_{tag}", std::process::id());
            let _ = areev_store::pg::drop_postgres_schema(url, &name);
            drop(areev_store::Areev::open_postgres(url, &name).expect("provision schema"));
            Schema { url: url.to_string(), name }
        }

        /// The locator the bench is handed: `provision=never`, so the open
        /// below issues no DDL at all and a role without `CREATE` still works.
        fn dsn(&self) -> String {
            let sep = if self.url.contains('?') { '&' } else { '?' };
            format!("{}{sep}schema={}&provision=never", self.url, self.name)
        }
    }

    impl Drop for Schema {
        fn drop(&mut self) {
            let _ = areev_store::pg::drop_postgres_schema(&self.url, &self.name);
        }
    }

    fn failing_task(tool: &str, code: &str, failures: usize) -> TaskRunRecord {
        let mut calls: Vec<RecordedCall> = (0..failures)
            .map(|i| RecordedCall {
                call_id: format!("call_{i}"),
                tool: tool.to_string(),
                input_json: r#"{"customer_id":"cus_123","amount":250}"#.to_string(),
                output_json: format!(
                    r#"{{"error":{{"code":"{code}","message":"blocked by hidden rule"}}}}"#
                ),
                is_error: true,
                rule: Some("R3"),
            })
            .collect();
        calls.push(RecordedCall {
            call_id: "call_ok".to_string(),
            tool: tool.to_string(),
            input_json: r#"{"customer_id":"cus_123","amount":50}"#.to_string(),
            output_json: r#"{"refund_id":"rf_1"}"#.to_string(),
            is_error: false,
            rule: None,
        });
        TaskRunRecord {
            task_id: "task-exp-1".to_string(),
            success: false,
            steps: (failures + 1) as u32,
            tool_errors: failures as u32,
            rule_failures: vec![("R3", failures as u32)],
            calls,
            final_answer: "could not complete the refund".to_string(),
            usage: Usage::default(),
            failure_reason: "refund not recorded".to_string(),
        }
    }

    /// The governed lever the whole bench measures — record → learn → apply →
    /// rollback — driven against a Postgres schema the memory role cannot
    /// create. This is the claim #250 asks for: the same arms, the same
    /// artifacts, a Cloud-provisioned memory underneath.
    #[test]
    fn the_governed_lever_runs_against_a_provisioned_schema() {
        let Some(url) = url() else { return };
        let schema = Schema::provision(&url, "lever");
        let db = BenchDb::Postgres(schema.dsn());
        assert!(db.is_postgres());

        let mem = Memory::create(&db).expect("open the provisioned schema");
        mem.record_task(&failing_task("refund", "approval_required", 6)).unwrap();
        assert_eq!(mem.lessons_markdown().unwrap(), "", "A0: nothing applied yet");

        let LearnOutcome { ledger, applied, .. } = mem.learn(None, None, T1).unwrap();
        assert!(!applied.is_empty(), "tool_failure lesson must apply; ledger: {:?}", ledger.entries);
        let lessons = mem.lessons_markdown().unwrap();
        assert!(lessons.contains("approval_required"), "B: {lessons:?}");

        mem.rollback(&applied, T2).unwrap();
        assert_eq!(
            mem.lessons_markdown().unwrap(),
            "",
            "A1: rollback must empty the prompt on this backend too"
        );
    }

    /// A schema that already holds grains is refused for the same reason a
    /// leftover `bench.db` is: stale lessons would silently poison A0, and on
    /// this backend "the file exists" has no meaning — what it HOLDS does.
    #[test]
    fn a_schema_that_already_holds_grains_is_refused() {
        let Some(url) = url() else { return };
        let schema = Schema::provision(&url, "stale");
        let db = BenchDb::Postgres(schema.dsn());

        let first = Memory::create(&db).expect("a freshly provisioned schema is empty");
        first.record_task(&failing_task("refund", "approval_required", 2)).unwrap();
        drop(first);

        let err = match Memory::create(&db) {
            Ok(_) => panic!("a schema carrying an earlier run must refuse"),
            Err(e) => e,
        };
        assert!(err.contains("already holds"), "{err}");
        // The DSN is named so the operator knows WHICH schema — redacted,
        // because an error message travels into logs and tickets.
        assert!(err.contains(&schema.name), "the refusal names the schema: {err}");
        assert!(!err.contains(":areev@"), "the password must not ride along: {err}");
    }

    /// The bench issues no DDL of its own: under `?provision=never` a schema
    /// that was never provisioned is REFUSED, and — the part that makes it a
    /// proof rather than an assertion — refused again on a second attempt,
    /// which it would not be if the first had quietly created it.
    #[test]
    fn an_unprovisioned_schema_is_refused_not_created() {
        let Some(url) = url() else { return };
        let name = format!("aba_{}_absent", std::process::id());
        let _ = areev_store::pg::drop_postgres_schema(&url, &name);
        let sep = if url.contains('?') { '&' } else { '?' };
        let db = BenchDb::Postgres(format!("{url}{sep}schema={name}&provision=never"));

        let first = Memory::create(&db).err().expect("an absent schema must refuse");
        assert!(first.contains("provision=never"), "{first}");
        let second = Memory::create(&db).err().expect("and must still be absent");
        assert!(
            second.contains("provision=never"),
            "the refused open created the schema after all: {second}"
        );
        let _ = areev_store::pg::drop_postgres_schema(&url, &name);
    }

    /// End to end through the binary, exactly as the acceptance states it:
    /// the arms run against the schema, the artifacts land in `--workdir`,
    /// and NO `bench.db` is created there.
    #[test]
    fn selfimprove_aba_targets_the_schema_and_leaves_no_file_behind() {
        let Some(url) = url() else { return };
        let schema = Schema::provision(&url, "aba");
        let workdir = tempfile::TempDir::new().unwrap();

        let out = std::process::Command::new(env!("CARGO_BIN_EXE_selfimprove_aba"))
            .args(["--workdir", workdir.path().to_str().unwrap()])
            .args(["--db", &schema.dsn()])
            .args(["--mock", "--experience", "24", "--eval", "12", "--workers", "2"])
            // The env override must not decide a run the flag already did.
            .env_remove("AREEV_BENCH_DB")
            .output()
            .expect("run selfimprove_aba");
        assert!(
            out.status.success(),
            "bench failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        assert!(
            !workdir.path().join("bench.db").exists(),
            "a DSN-backed run must create no memory file in the workdir"
        );
        let report: serde_json::Value = serde_json::from_slice(
            &std::fs::read(workdir.path().join("report.json")).expect("report.json"),
        )
        .unwrap();
        assert_eq!(report["config"]["db_backend"], "postgres");
        let recorded = report["config"]["db"].as_str().unwrap();
        assert!(recorded.contains(&schema.name), "the report names the schema: {recorded}");
        // Every governed state was measured, on this backend.
        let states: Vec<&str> = report["evals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["state"].as_str().unwrap())
            .collect();
        for want in ["A0", "B", "A1", "B2"] {
            assert!(states.contains(&want), "missing {want} in {states:?}");
        }
    }
}
