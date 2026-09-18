//! #322: the blocking store must not panic from inside Tokio.
//!
//! `Areev` drives its own current-thread runtime and `block_on`s it, so it
//! panicked in two places when driven from async code — on the OPEN ("Cannot
//! start a runtime from within a runtime") and on the DROP ("Cannot drop a
//! runtime in a context where blocking is not allowed"). The second is the
//! easier to miss: it fires at shutdown or in a test's drop, long after code
//! that looked correct.
//!
//! Both are now handled: the open returns `STO-E010` naming the async APIs,
//! and the drop relocates the runtime to a plain thread.

use areev_store::{Areev, AsyncAreev};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocking_open_on_a_runtime_worker_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.db");

    let err = match Areev::open(path.to_str().unwrap()) {
        Err(e) => e,
        Ok(_) => panic!("a blocking open on a runtime worker must be refused"),
    };

    assert_eq!(err.code(), "STO-E010", "got {err}");
    let msg = err.to_string();
    // The message has to name the way OUT, not just the problem: the panic it
    // replaces named no Areev API at all.
    assert!(msg.contains("AsyncAreev"), "{msg}");
    assert!(msg.contains("AsyncFacade"), "{msg}");
    assert!(msg.contains("spawn_blocking"), "{msg}");

    // And it refused before creating anything.
    assert!(!path.exists(), "a refused open must not leave a file behind");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_store_on_a_runtime_worker_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("y.db").to_str().unwrap().to_string();

    // Open where it is legal, then drop where it used to panic.
    let m = tokio::task::spawn_blocking(move || Areev::open(&p).unwrap())
        .await
        .unwrap();
    drop(m); // the panic under test

    // Reaching here at all is the assertion; do real async work after it so a
    // wedged worker would show up as a hang rather than a pass.
    tokio::task::yield_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_async_handle_still_opens_from_a_runtime() {
    // The open guard must not catch the supported path: `AsyncAreev` opens on
    // the blocking pool, where a runtime is in context but nested blocking is
    // legal. A guard keyed on "is there a runtime" alone would break this.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("z.db").to_str().unwrap().to_string();

    let db = AsyncAreev::open(&p).await.expect("AsyncAreev must still open");
    let stats = db.stats().await.expect("and still work");
    assert_eq!(stats.grains, 0);
    db.close().await.expect("and still close");
}

#[test]
fn a_blocking_open_off_any_runtime_is_unaffected() {
    // The positive control: the guard must be invisible to every sync caller,
    // which is nearly all of them.
    let dir = tempfile::tempdir().unwrap();
    let m = Areev::open(dir.path().join("plain.db").to_str().unwrap())
        .expect("a plain blocking open must be unaffected");
    drop(m);
}
