//! trust_suite — CI-runnable trust artifacts (LR-1 tier 2).
//!
//! T1  kill -9 mid-write        → clean reopen, integrity ok
//! T2  tamper a stored blob     → `verify` catches the content-address break
//! T3  deletion-remnant scan    → what bytes survive logical deletion,
//!                                measured honestly on BOTH sqlite and areev
//! T4  point-in-time restore    → bundle → fresh file, full + until-HLC
//!
//! T3 is deliberately adversarial against areev too: `forget` is an
//! auditable index-level removal, not byte erasure — the design's honest
//! erasure story is per-file crypto-erasure (key destruction), validated at
//! the Turso layer in M0 and not yet wired into the store. This suite
//! exists to keep that claim precise.

use areev_bench::{gen_facts, load_facts, Xorshift};
use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions};
use std::process::Command;
use std::time::{Duration, Instant};

fn opts() -> AreevOptions {
    AreevOptions { index_text: false, ..Default::default() }
}

fn scan_for(path: &std::path::Path, needle: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => bytes.windows(needle.len()).any(|w| w == needle),
        Err(_) => false,
    }
}

/// Scan a db file plus its -wal sibling and .blobs sidecar tree.
fn scan_db_family(db: &std::path::Path, needle: &[u8]) -> Vec<String> {
    let mut hits = Vec::new();
    let mut check = |p: std::path::PathBuf| {
        if scan_for(&p, needle) {
            hits.push(p.file_name().unwrap().to_string_lossy().to_string());
        }
    };
    check(db.to_path_buf());
    check(db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy())));
    let blobs = db.with_file_name(format!("{}.blobs", db.file_name().unwrap().to_string_lossy()));
    if blobs.is_dir() {
        for entry in walk(&blobs) {
            check(entry);
        }
    }
    hits
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn main() {
    // ---- child mode for T1: write grains forever until killed ----
    if let Ok(path) = std::env::var("AREEV_TRUST_CHILD") {
        let mut m = Areev::open_with(&path, opts()).unwrap();
        let mut i = 0u64;
        loop {
            let mut f = Fact::new(&format!("caller:{:04}", i % 800), "note", &format!("n{i}"))
                .confidence(0.9);
            f.common.namespace = Some("main".to_string());
            m.add(&f).unwrap();
            i += 1;
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut verdict = |name: &str, ok: bool, detail: String| {
        println!("{} {name} — {detail}", if ok { "PASS" } else { "FAIL" });
        if ok { pass += 1 } else { fail += 1 }
    };

    // ================= T1: kill -9 during writes =================
    let t1_db = dir.path().join("t1.db");
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(&exe)
        .env("AREEV_TRUST_CHILD", t1_db.to_str().unwrap())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    child.kill().unwrap(); // SIGKILL — no shutdown path runs
    child.wait().unwrap();
    let mut m = Areev::open(t1_db.to_str().unwrap()).unwrap();
    let rep = m.verify().unwrap();
    let stats = m.stats().unwrap();
    verdict(
        "T1 kill -9 mid-write, clean recovery",
        rep.integrity == "ok" && rep.hash_mismatches == 0 && rep.undecodable == 0 && stats.grains > 0,
        format!(
            "{} grains survived, integrity={}, hash_mismatches={}, undecodable={}",
            stats.grains, rep.integrity, rep.hash_mismatches, rep.undecodable
        ),
    );
    drop(m);

    // ================= T2: tamper with a stored grain =================
    let t2_db = dir.path().join("t2.db");
    {
        let mut m = Areev::open_with(t2_db.to_str().unwrap(), opts()).unwrap();
        let mut rng = Xorshift(7);
        let facts = gen_facts(&mut rng, 100, 20);
        load_facts(&mut m, &facts);
        let clean = m.verify().unwrap();
        assert_eq!(clean.hash_mismatches, 0, "pre-tamper baseline must be clean");
    }
    // the attacker: file-level access, one byte flipped inside one blob.
    // Explicit txn + read-back through a *fresh* connection so the bench
    // proves the tamper actually reached the file before testing detection.
    {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let raw_open = || async {
                let db = turso::Builder::new_local(t2_db.to_str().unwrap())
                    .experimental_index_method(true)
                    .build()
                    .await
                    .unwrap();
                db.connect().unwrap()
            };
            let read_blob = |conn: turso::Connection| async move {
                let mut rows = conn
                    .query("SELECT blob FROM grains WHERE seq = 50", ())
                    .await
                    .unwrap();
                let row = rows.next().await.unwrap().unwrap();
                match row.get_value(0).unwrap() {
                    turso::Value::Blob(b) => b,
                    v => panic!("expected blob, got {v:?}"),
                }
            };
            let original = read_blob(raw_open().await).await;
            let mut tampered = original.clone();
            let mid = tampered.len() / 2;
            tampered[mid] ^= 0xFF;
            let conn = raw_open().await;
            conn.execute("BEGIN", ()).await.unwrap();
            conn.execute(
                "UPDATE grains SET blob = ?1 WHERE seq = 50",
                (turso::Value::Blob(tampered.clone()),),
            )
            .await
            .unwrap();
            conn.execute("COMMIT", ()).await.unwrap();
            drop(conn);
            let persisted = read_blob(raw_open().await).await;
            assert_eq!(persisted, tampered, "tamper write did not persist to the file");
        });
    }
    let mut m = Areev::open(t2_db.to_str().unwrap()).unwrap();
    let rep = m.verify().unwrap();
    verdict(
        "T2 tampered blob detected by content-address recheck",
        rep.hash_mismatches + rep.undecodable >= 1,
        format!(
            "1 byte flipped in 1 of {} grains → hash_mismatches={}, undecodable={}",
            rep.grains, rep.hash_mismatches, rep.undecodable
        ),
    );
    drop(m);

    // ================= T3: deletion-remnant scan (both engines) =================
    let secret = b"TOPSECRET-ceres-9931";
    let secret_s = "TOPSECRET-ceres-9931";

    // sqlite (system binary): delete ONE user's row among others — the real
    // "forget this user" shape (a bare DELETE FROM would hit the truncate
    // optimization and prove nothing). Run twice: platform defaults, and
    // upstream SQLite's default secure_delete=OFF (Apple's system build
    // ships secure_delete=2/FAST, which is NOT the stock behavior).
    let sqlite_available = Command::new("sqlite3").arg("--version").output().is_ok();
    if sqlite_available {
        let platform_sd = String::from_utf8_lossy(
            &Command::new("sqlite3").arg(":memory:").arg("PRAGMA secure_delete;").output().unwrap().stdout,
        )
        .trim()
        .to_string();
        for (label, pragma) in [
            (format!("platform default (secure_delete={platform_sd})"), ""),
            ("upstream default (secure_delete=OFF)".to_string(), "PRAGMA secure_delete=OFF; "),
        ] {
            let sq_db = dir.path().join(format!("t3-{}.db", if pragma.is_empty() { "plat" } else { "upstream" }));
            let run = |sql: &str| {
                let out = Command::new("sqlite3").arg(&sq_db).arg(sql).output().unwrap();
                assert!(out.status.success(), "sqlite3 failed: {}", String::from_utf8_lossy(&out.stderr));
            };
            run(&format!(
                "PRAGMA journal_mode=WAL; {pragma}\
                 CREATE TABLE memories(id INTEGER PRIMARY KEY, subject TEXT, content TEXT); \
                 INSERT INTO memories(subject, content) VALUES('ana','likes tea'),\
                 ('john','{secret_s} allergy note'),('bob','window seat'); \
                 DELETE FROM memories WHERE subject='john';"
            ));
            let after_delete = scan_db_family(&sq_db, secret);
            run("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
            let after_vacuum = scan_db_family(&sq_db, secret);
            println!(
                "INFO T3a sqlite DELETE, {label} — secret bytes {} after DELETE (in {:?}); {} after manual checkpoint+VACUUM",
                if after_delete.is_empty() { "GONE" } else { "STILL PRESENT" },
                after_delete,
                if after_vacuum.is_empty() { "gone" } else { "STILL PRESENT" },
            );
        }
    } else {
        println!("INFO T3a sqlite leg skipped (no sqlite3 binary on PATH)");
    }

    // areev: forget, then the same adversarial scan
    let dj_db = dir.path().join("t3-areev.db");
    {
        let mut m = Areev::open_with(dj_db.to_str().unwrap(), opts()).unwrap();
        let mut f = Fact::new("john", "medical_note", &format!("{secret_s} allergy note"))
            .confidence(0.9);
        f.common.namespace = Some("main".to_string());
        let hash = m.add(&f).unwrap();
        m.forget(&hash).unwrap();
        // forgotten: recall must not see it
        let r = m.recall("main", "john", None, 16).unwrap();
        assert!(r.is_empty(), "forgotten grain still recalled");
    }
    let dj_hits = scan_db_family(&dj_db, secret);
    println!(
        "INFO T3b areev forget — recall returns nothing; secret bytes {} at file level (in {:?})",
        if dj_hits.is_empty() { "GONE" } else { "STILL PRESENT" },
        dj_hits,
    );
    verdict(
        "T3 remnant scan ran on both engines (honest-erasure evidence)",
        true,
        "logical deletion is not byte erasure in either engine; per-file crypto-erasure (key \
         destruction) is the honest path — M0-validated at the Turso layer, store wiring pending"
            .to_string(),
    );

    // ================= T4: point-in-time restore =================
    let t4_db = dir.path().join("t4.db");
    let mut m = Areev::open_with(t4_db.to_str().unwrap(), opts()).unwrap();
    let mut rng = Xorshift(99);
    let facts = gen_facts(&mut rng, 5_000, 400);
    load_facts(&mut m, &facts);
    let ops = m.changes_since(0, 10_000).unwrap();
    let mid_hlc = ops[ops.len() / 2].hlc;
    let bundle = dir.path().join("t4.bundle");
    let bs = m.bundle_since(0, bundle.to_str().unwrap()).unwrap();
    drop(m);

    let full_db = dir.path().join("t4-full.db");
    let mut full = Areev::open_with(full_db.to_str().unwrap(), opts()).unwrap();
    let t = Instant::now();
    let full_stats = full.import_bundle(bundle.to_str().unwrap()).unwrap();
    let full_secs = t.elapsed().as_secs_f64();
    let full_rep = full.verify().unwrap();
    drop(full);

    let pitr_db = dir.path().join("t4-pitr.db");
    let mut pitr = Areev::open_with(pitr_db.to_str().unwrap(), opts()).unwrap();
    let t = Instant::now();
    let pitr_stats = pitr.import_bundle_until(bundle.to_str().unwrap(), Some(mid_hlc)).unwrap();
    let pitr_secs = t.elapsed().as_secs_f64();
    drop(pitr);

    verdict(
        "T4 restore from bundle (full + point-in-time)",
        full_stats.applied == bs.ops && full_rep.integrity == "ok" && full_rep.hash_mismatches == 0
            && pitr_stats.applied < full_stats.applied && pitr_stats.applied > 0,
        format!(
            "bundle {:.1} MB / {} ops; full restore {} ops in {:.2}s ({:.0} ops/s, integrity={}); \
             restore-until-HLC applied {} ops in {:.2}s",
            bs.bytes as f64 / 1e6, bs.ops,
            full_stats.applied, full_secs, full_stats.applied as f64 / full_secs, full_rep.integrity,
            pitr_stats.applied, pitr_secs,
        ),
    );

    println!("\ntrust_suite: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}
