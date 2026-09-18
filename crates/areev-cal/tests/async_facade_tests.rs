//! #322: an async host can own the GOVERNED facade without hand-rolling it.
//!
//! `AsyncAreev` covered the raw store only, so a host that needed both async
//! and authorization wrapped the blocking facade itself — opening on a plain
//! thread, funnelling every call through `spawn_blocking`, and implementing
//! `Drop` to release the last handle on a dedicated thread — and got two
//! panics on the way, the second (teardown) only at shutdown or in a test.
//!
//! Every test here runs on a multi-thread runtime, because that is where both
//! panics fired.

use areev_cal::{AreevFacade, AsyncFacade, CalExecutor, CalExecutorConfig};
use areev_core::authz::{Verb, AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

/// Seed a memory with a grant and hand back its path.
///
/// Off the executor deliberately: the blocking open is refused on a runtime
/// worker (STO-E010, #322), which is the very thing these tests exist for. A
/// helper that ignored its own advice would be the first thing to break.
async fn seeded(dir: &TempDir) -> String {
    let path = dir.path().join("async.db").to_str().unwrap().to_string();
    let p = path.clone();
    tokio::task::spawn_blocking(move || {
        let mut m = Areev::open(&p).unwrap();
        m.add(
            &Fact::new("user:amy", REL_PERMITS, "read,write ON ops")
                .namespace(AUTHZ_NS)
                .created_at(1_000),
        )
        .unwrap();
        m.add(&Fact::new("deal:1", "stage", "diligence").namespace("ops"))
            .unwrap();
    })
    .await
    .unwrap();
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_grant_recall_under_a_session_epoch_close() {
    // The acceptance path, end to end: open, set_grants, run a RECALL under a
    // principal_session inside `with`, read authz_epoch, close.
    let dir = TempDir::new().unwrap();
    let path = seeded(&dir).await;

    let f = AsyncFacade::open(&path, Some("ops")).await.expect("open");

    let epoch_before = f.with(|facade| facade.authz_epoch()).await.expect("epoch");

    f.with(|facade| {
        facade.set_grants(
            "user:bob",
            &[areev_core::authz::Grant {
                verbs: vec![Verb::Read],
                namespaces: vec!["ops".to_string()],
            }],
            "onboarding bob",
        )?;
        Ok(())
    })
    .await
    .expect("set_grants");

    let epoch_after = f.with(|facade| facade.authz_epoch()).await.expect("epoch");
    assert_ne!(epoch_before, epoch_after, "granting must move the epoch");

    // A PrincipalSession borrows its facade and cannot cross an `.await` — so
    // the whole request runs inside one closure, which is the shape this type
    // exists to provide.
    let found = f
        .with(|facade| {
            let session = facade.principal_session("user:amy")?;
            let ex = CalExecutor::new(CalExecutorConfig::default());
            let res = ex
                .execute(
                    r#"RECALL facts WHERE subject = "deal:1" AND namespace = "ops""#,
                    &session,
                )
                .map_err(|e| areev_core::error::AreevError::Validation(e.to_string()))?;
            let v = serde_json::to_value(res.payload_json().unwrap()).unwrap();
            Ok(v["grains"].as_array().map(|a| a.len()).unwrap_or(0))
        })
        .await
        .expect("recall under a session");
    assert_eq!(found, 1, "the session must read what its grant covers");

    // And the session's rights still fail closed inside the closure.
    let refused = f
        .with(|facade| {
            let session = facade.principal_session("user:amy")?;
            Ok(session.authz().check(Verb::Read, "secret").is_err())
        })
        .await
        .expect("check");
    assert!(refused, "fail-closed must hold under the async owner too");

    f.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_without_close_does_not_panic() {
    // The half that is easy to miss: teardown. Letting the facade drop at the
    // end of an async test used to panic in tokio's blocking shutdown.
    let dir = TempDir::new().unwrap();
    let path = seeded(&dir).await;
    {
        let f = AsyncFacade::open(&path, Some("ops")).await.expect("open");
        let n = f.with(|facade| facade.authz_epoch()).await.expect("call");
        assert!(n >= 0);
        // no close() — the drop below is the assertion
    }
    // Real async work after the drop, so a wedged worker shows up as a hang
    // rather than a pass.
    tokio::task::yield_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_is_idempotent_and_later_calls_fail_cleanly() {
    let dir = TempDir::new().unwrap();
    let path = seeded(&dir).await;
    let f = AsyncFacade::open(&path, Some("ops")).await.expect("open");
    let g = f.clone();

    f.close().await.expect("close");
    // The facade is shared by every clone, so the clone is closed too — and
    // says so rather than panicking or hanging.
    let err = g
        .with(|facade| facade.authz_epoch())
        .await
        .expect_err("a call after close must fail");
    assert!(err.to_string().contains("already closed"), "{err}");
    g.close().await.expect("closing twice is a no-op");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_callers_serialise_without_deadlocking() {
    // Clones share one facade; calls queue on the semaphore rather than piling
    // up blocking threads that only wait on the store mutex.
    let dir = TempDir::new().unwrap();
    let path = seeded(&dir).await;
    let f = AsyncFacade::open(&path, Some("ops")).await.expect("open");

    let mut tasks = Vec::new();
    for i in 0..8 {
        let h = f.clone();
        tasks.push(tokio::spawn(async move {
            h.with(move |facade| {
                let session = facade.principal_session("user:amy")?;
                let mut fields = serde_json::Map::new();
                fields.insert("subject".into(), serde_json::json!(format!("deal:{i}")));
                fields.insert("relation".into(), serde_json::json!("stage"));
                fields.insert("object".into(), serde_json::json!("queued"));
                fields.insert("namespace".into(), serde_json::json!("ops"));
                session.cal_add("fact", &fields)
            })
            .await
        }));
    }
    for t in tasks {
        t.await.expect("join").expect("write under a session");
    }

    let count = f
        .with(|facade| {
            facade.with_store(|m| m.recall("ops", "deal:3", Some("stage"), 8)).map(|g| g.len())
        })
        .await
        .expect("recall");
    assert_eq!(count, 1);
    f.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn from_facade_takes_a_host_configured_facade() {
    // A host that mounts replicas or installs an embedder builds the facade
    // itself; the async owner must accept one rather than only opening its own.
    let dir = TempDir::new().unwrap();
    let path = seeded(&dir).await;
    let built = tokio::task::spawn_blocking(move || {
        let store = Areev::open(&path).unwrap();
        AreevFacade::with_session(store, Some("ops".to_string()), None)
    })
    .await
    .unwrap();

    let f = AsyncFacade::from_facade(built);
    let epoch = f.with(|facade| facade.authz_epoch()).await.expect("call");
    assert!(epoch >= 0);
    f.close().await.expect("close");
}
