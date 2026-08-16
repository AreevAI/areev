//! Async-safe access to [`Areev`].
//!
//! Use [`AsyncAreev`] whenever the calling code is async. It is the supported
//! entry point for Tokio-based hosts, which is where most agent code lives.
//!
//! # Why a separate handle
//!
//! [`Areev`] is a blocking store: it owns a Tokio runtime internally and drives
//! each operation with `Runtime::block_on`. Tokio does not permit a runtime to be
//! started from within another runtime, so a [`Areev`] driven directly from async
//! code panics — in two distinct places:
//!
//! - when an **operation** is called (`Cannot start a runtime from within a runtime`);
//! - when the handle is **dropped**, because dropping it drops the runtime it owns.
//!
//! The second is the easier one to miss: code can look correct and still panic at
//! teardown.
//!
//! # What this handle does
//!
//! - Every operation runs on Tokio's **blocking pool**, where blocking is permitted.
//! - Concurrent callers **queue asynchronously**, so a burst of operations cannot
//!   exhaust the host's blocking pool with threads that are only waiting their turn.
//! - **`Drop` moves the store to a dedicated OS thread**, so teardown never blocks
//!   an async worker.
//!
//! Callers simply `.await`. The blocking [`Areev`] API is unchanged, and sync
//! callers pay nothing for this module.
//!
//! # Example
//!
//! ```no_run
//! use areev_store::AsyncAreev;
//! use areev_core::types::Fact;
//!
//! # async fn demo() -> areev_core::error::Result<()> {
//! let db = AsyncAreev::open("agent.db").await?;
//! db.add(Fact::new("john", "prefers", "dark mode")).await?;
//! let latest = db.latest("caller", "john", "prefers").await?;
//! # Ok(())
//! # }
//! ```

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use areev_core::error::{AreevError, Hash, Result};
use areev_core::types::{Grain, GrainType};

use crate::{Areev, AreevOptions, DeserializedGrain, StoreStats};

struct Shared {
    db: Mutex<Option<Areev>>,
    // The store is `&mut`-driven, so operations serialise regardless. Queue callers
    // here rather than in the blocking pool: without this, N concurrent operations
    // occupy N blocking threads that only wait on the mutex, and can exhaust the
    // host's pool. One permit keeps at most one blocking thread busy per store.
    gate: Semaphore,
}

/// A [`Areev`] that is safe to use from async code.
///
/// Cheap to clone: every clone shares one store. Operations against it serialise,
/// because the underlying store is `&mut`-driven; callers queue asynchronously
/// rather than occupying blocking threads. Clone this into tasks rather than
/// wrapping it in an `Arc`.
///
/// Call [`close`](Self::close) when the store must be shut down before the process
/// moves on; otherwise dropping the last handle is enough.
#[derive(Clone)]
pub struct AsyncAreev {
    inner: Arc<Shared>,
}

impl AsyncAreev {
    /// Open a store at `path`.
    pub async fn open(path: &str) -> Result<Self> {
        let p = path.to_owned();
        Self::opened(move || Areev::open(&p)).await
    }

    /// Open with explicit [`AreevOptions`].
    pub async fn open_with(path: &str, opts: AreevOptions) -> Result<Self> {
        let p = path.to_owned();
        Self::opened(move || Areev::open_with(&p, opts)).await
    }

    /// Open an encrypted store.
    ///
    /// The key is held in a [`zeroize::Zeroizing`] buffer for as long as this call
    /// owns it, so the copy handed to the blocking pool is wiped once the store is
    /// open rather than left in freed memory.
    pub async fn open_encrypted(path: &str, key: [u8; 32]) -> Result<Self> {
        let p = path.to_owned();
        let key = zeroize::Zeroizing::new(key);
        Self::opened(move || Areev::open_encrypted(&p, *key)).await
    }

    async fn opened<F>(open: F) -> Result<Self>
    where
        F: FnOnce() -> Result<Areev> + Send + 'static,
    {
        let db = offload(open).await?;
        Ok(Self {
            inner: Arc::new(Shared {
                db: Mutex::new(Some(db)),
                gate: Semaphore::new(1),
            }),
        })
    }

    /// Run an arbitrary operation against the store, safely, on the blocking pool.
    ///
    /// This is the escape hatch: any [`Areev`] method not wrapped below stays
    /// reachable without reintroducing the runtime hazard.
    ///
    /// ```no_run
    /// # use areev_store::AsyncAreev;
    /// # async fn demo(db: &AsyncAreev) -> areev_core::error::Result<()> {
    /// let rebuilt = db.with(|db| db.rebuild_text_index()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn with<T, F>(&self, op: F) -> Result<T>
    where
        F: FnOnce(&mut Areev) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self.inner.gate.acquire().await.map_err(|_| closed())?;
        let inner = Arc::clone(&self.inner);
        offload(move || {
            let mut guard = inner.db.lock().map_err(|_| poisoned())?;
            let db = guard.as_mut().ok_or_else(closed)?;
            op(db)
        })
        .await
    }

    /// Add a grain, returning its content hash.
    pub async fn add<G>(&self, grain: G) -> Result<Hash>
    where
        G: Grain + Send + 'static,
    {
        self.with(move |db| db.add(&grain)).await
    }

    /// Add a grain only if its content is not already present.
    pub async fn add_if_novel<G>(&self, grain: G) -> Result<(Hash, bool)>
    where
        G: Grain + Send + 'static,
    {
        self.with(move |db| db.add_if_novel(&grain)).await
    }

    /// The current grain for `(namespace, subject, relation)`.
    pub async fn latest(
        &self,
        ns: &str,
        subject: &str,
        relation: &str,
    ) -> Result<Option<DeserializedGrain>> {
        let (ns, subject, relation) = (ns.to_owned(), subject.to_owned(), relation.to_owned());
        self.with(move |db| db.latest(&ns, &subject, &relation))
            .await
    }

    /// Hybrid recall (triple + text + vector legs, RRF-fused).
    pub async fn recall_hybrid(
        &self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        query: Option<&str>,
        k: usize,
        deadline: Option<std::time::Duration>,
    ) -> Result<Vec<DeserializedGrain>> {
        let ns = ns.to_owned();
        let subject = subject.map(str::to_owned);
        let relation = relation.map(str::to_owned);
        let query = query.map(str::to_owned);
        self.with(move |db| {
            db.recall_hybrid(
                &ns,
                subject.as_deref(),
                relation.as_deref(),
                query.as_deref(),
                k,
                deadline,
            )
        })
        .await
    }

    /// The most recent grains in a namespace.
    pub async fn recent(
        &self,
        ns: &str,
        gtype: Option<GrainType>,
        limit: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        let ns = ns.to_owned();
        self.with(move |db| db.recent(&ns, gtype, limit)).await
    }

    /// Tombstone a grain by hash.
    pub async fn forget(&self, hash: Hash) -> Result<()> {
        self.with(move |db| db.forget(&hash)).await
    }

    /// Store statistics.
    pub async fn stats(&self) -> Result<StoreStats> {
        self.with(|db| db.stats()).await
    }

    /// Close the store, waiting for teardown to finish.
    ///
    /// Dropping the last handle also closes the store, but it does so on a detached
    /// thread and cannot be awaited: if the process exits immediately afterwards,
    /// teardown may not have run. Call `close` when the store must be shut down
    /// before the program moves on — on graceful shutdown, before copying the `.db`
    /// file, or at the end of a test.
    ///
    /// The store is shared by every clone, so this closes it for **all** of them:
    /// subsequent operations on any handle fail. It waits for an in-flight operation
    /// to finish first, and closing an already-closed store is a no-op.
    pub async fn close(self) -> Result<()> {
        let _permit = self.inner.gate.acquire().await.map_err(|_| closed())?;
        let taken = {
            let mut guard = self.inner.db.lock().map_err(|_| poisoned())?;
            guard.take()
        };
        match taken {
            Some(db) => {
                offload(move || {
                    drop(db);
                    Ok(())
                })
                .await
            }
            None => Ok(()),
        }
    }
}

impl Drop for AsyncAreev {
    fn drop(&mut self) {
        // Dropping `Areev` drops the runtime it owns, which panics in async context.
        // `get_mut` yields it only for the last handle with no operation in flight;
        // otherwise the last holder is a blocking-pool thread, where blocking is legal.
        if let Some(db) = Arc::get_mut(&mut self.inner)
            .and_then(|s| s.db.get_mut().ok())
            .and_then(Option::take)
        {
            std::thread::spawn(move || drop(db));
        }
    }
}

/// Run a blocking store operation on Tokio's blocking pool, where `block_on` is legal.
async fn offload<T, F>(op: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(op).await {
        Ok(r) => r,
        Err(e) => Err(AreevError::Storage(format!(
            "areev blocking task failed: {e}"
        ))),
    }
}

fn poisoned() -> AreevError {
    AreevError::Storage("areev handle poisoned by a panic in another task".into())
}

fn closed() -> AreevError {
    AreevError::Storage("areev handle already closed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use areev_core::types::Fact;

    fn tmp(name: &str) -> String {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        dir.join(name).to_string_lossy().into_owned()
    }

    /// On the blocking API both the call and the drop panic inside a runtime.
    #[tokio::test]
    async fn is_usable_from_async_code() {
        let path = tmp("async-usable.db");
        let db = AsyncAreev::open(&path).await.expect("open");

        let mut fact = Fact::new("john", "prefers", "dark mode");
        fact.common_mut().namespace = Some("caller".to_string());
        db.add(fact).await.expect("add");

        let got = db
            .latest("caller", "john", "prefers")
            .await
            .expect("latest")
            .expect("a grain");
        assert_eq!(
            got.fields.get("object").and_then(|v| v.as_str()),
            Some("dark mode"),
        );

        drop(db);
    }

    #[tokio::test]
    async fn escape_hatch_runs_arbitrary_ops_safely() {
        let path = tmp("async-with.db");
        let db = AsyncAreev::open(&path).await.expect("open");

        let stats = db.with(|db| db.stats()).await.expect("stats via with");
        assert_eq!(stats.grains, 0);
    }

    #[tokio::test]
    async fn close_awaits_teardown() {
        let path = tmp("async-close.db");
        let db = AsyncAreev::open(&path).await.expect("open");

        let mut fact = Fact::new("grace", "built", "the compiler");
        fact.common_mut().namespace = Some("caller".to_string());
        db.add(fact).await.expect("add");

        db.close().await.expect("close");

        let again = AsyncAreev::open(&path).await.expect("reopen");
        assert!(again
            .latest("caller", "grace", "built")
            .await
            .expect("latest")
            .is_some());
        again.close().await.expect("close again");
    }

    /// Concurrent callers must queue on the semaphore, not in the blocking pool.
    /// Without the gate each of these would hold a blocking thread just to wait on
    /// the mutex, and enough of them would starve the host's pool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_operations_queue_without_hogging_the_blocking_pool() {
        let path = tmp("async-concurrent.db");
        let db = AsyncAreev::open(&path).await.expect("open");

        let writes = (0..64).map(|i| {
            let db = db.clone();
            tokio::spawn(async move {
                let mut fact = Fact::new(&format!("s{i}"), "n", &i.to_string());
                fact.common_mut().namespace = Some("caller".to_string());
                db.add(fact).await
            })
        });
        for w in writes {
            w.await.expect("task").expect("add");
        }

        let stats = db.stats().await.expect("stats");
        assert_eq!(stats.grains, 64);
        db.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clones_share_one_store() {
        let path = tmp("async-clone.db");
        let db = AsyncAreev::open(&path).await.expect("open");
        let handle = db.clone();

        let mut fact = Fact::new("linus", "wrote", "git");
        fact.common_mut().namespace = Some("caller".to_string());
        handle.add(fact).await.expect("add via clone");

        drop(handle);

        assert!(db
            .latest("caller", "linus", "wrote")
            .await
            .expect("latest via original")
            .is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn works_on_a_multi_thread_runtime() {
        let path = tmp("async-multi.db");
        let db = AsyncAreev::open(&path).await.expect("open");

        let mut fact = Fact::new("ada", "wrote", "the first program");
        fact.common_mut().namespace = Some("caller".to_string());
        db.add(fact).await.expect("add");

        assert!(db
            .latest("caller", "ada", "wrote")
            .await
            .expect("latest")
            .is_some());
    }
}
