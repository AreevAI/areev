//! Async-safe ownership of the **governed** facade (#322).
//!
//! [`AsyncAreev`](areev_store::AsyncAreev) covers the raw store: `open`, `add`,
//! `latest`, `recall_hybrid`, `recent`, `forget`, `stats`, `close`. Nothing in
//! this crate referred to it, so an async host that also needed
//! **authorization** — [`AreevFacade`], [`PrincipalSession`](crate::PrincipalSession),
//! `set_grants`, `authz_epoch`, CAL under a session — had no async-safe owner
//! and hand-rolled one: open on a plain thread, every call through
//! `spawn_blocking`, and a `Drop` that released the last handle on a dedicated
//! thread. Two of those three exist because the blocking store panics from
//! inside Tokio, and the teardown one is easy to miss because it only fires at
//! shutdown or in a test's drop.
//!
//! [`AsyncFacade`] is that owner, in the shape of `AsyncAreev::with`.
//!
//! ```no_run
//! use areev_cal::AsyncFacade;
//!
//! # async fn demo() -> areev_core::error::Result<()> {
//! let f = AsyncFacade::open("agent.db", Some("ops")).await?;
//!
//! // Anything the governed facade can do, with the facade borrowed for the
//! // length of the closure — so a `PrincipalSession<'_>` can be created and
//! // used inside it.
//! let hits = f
//!     .with(|facade| {
//!         let session = facade.principal_session("user:amy")?;
//!         session.authz().check(areev_core::authz::Verb::Read, "ops")?;
//!         facade.authz_epoch()
//!     })
//!     .await?;
//! # let _ = hits;
//!
//! f.close().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Why the closure, rather than one async method per facade method
//!
//! `PrincipalSession<'f>` borrows its facade, so it cannot cross an `.await`.
//! A per-method async wrapper would therefore be unable to express the thing
//! an authorizing host does on every request — resolve a principal, then run
//! statements under it. Handing the borrow to a closure that runs entirely on
//! one blocking thread expresses it exactly, and keeps the whole call off the
//! executor, which is the other half of the problem.

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use areev_core::error::{AreevError, Result};
use areev_store::{Areev, AreevOptions};

use crate::AreevFacade;

struct Shared {
    facade: Mutex<Option<AreevFacade>>,
    /// The facade serialises on its own store mutex anyway. Queue callers here
    /// rather than in the blocking pool: without this, N concurrent calls
    /// occupy N blocking threads that only wait on that mutex and can exhaust
    /// the host's pool. One permit keeps at most one blocking thread busy.
    gate: Semaphore,
}

/// An [`AreevFacade`] that is safe to own, call and drop from async code.
///
/// Cheap to clone: every clone shares one facade, and calls against it
/// serialise. Clone this into tasks rather than wrapping it in an `Arc`.
#[derive(Clone)]
pub struct AsyncFacade {
    inner: Arc<Shared>,
}

impl AsyncFacade {
    /// Open a memory and wrap its governed facade, with an optional session
    /// namespace.
    pub async fn open(path: &str, namespace: Option<&str>) -> Result<Self> {
        let p = path.to_owned();
        let ns = namespace.map(str::to_owned);
        Self::built(move || {
            let store = Areev::open(&p)?;
            Ok(AreevFacade::with_session(store, ns, None))
        })
        .await
    }

    /// [`Self::open`] with explicit [`AreevOptions`].
    pub async fn open_with(
        path: &str,
        opts: AreevOptions,
        namespace: Option<&str>,
    ) -> Result<Self> {
        let p = path.to_owned();
        let ns = namespace.map(str::to_owned);
        Self::built(move || {
            let store = Areev::open_with(&p, opts)?;
            Ok(AreevFacade::with_session(store, ns, None))
        })
        .await
    }

    /// Wrap a facade the caller already built — for a host that mounts
    /// read-only replicas, installs an embedder, or otherwise configures the
    /// facade before serving requests.
    ///
    /// Build it off the executor (a plain thread, or `spawn_blocking`): the
    /// open inside is blocking, and this only takes ownership.
    pub fn from_facade(facade: AreevFacade) -> Self {
        Self {
            inner: Arc::new(Shared {
                facade: Mutex::new(Some(facade)),
                gate: Semaphore::new(1),
            }),
        }
    }

    async fn built<F>(build: F) -> Result<Self>
    where
        F: FnOnce() -> Result<AreevFacade> + Send + 'static,
    {
        Ok(Self::from_facade(offload(build).await?))
    }

    /// Run `op` against the facade on the blocking pool, with the facade
    /// **borrowed for the length of the closure**.
    ///
    /// That borrow is the point: a [`PrincipalSession`](crate::PrincipalSession)
    /// borrows its facade and so cannot cross an `.await`, but inside here it
    /// can be created, used for a whole request, and dropped — all on one
    /// thread where blocking is legal.
    pub async fn with<T, F>(&self, op: F) -> Result<T>
    where
        F: FnOnce(&AreevFacade) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self.inner.gate.acquire().await.map_err(|_| closed())?;
        let inner = Arc::clone(&self.inner);
        offload(move || {
            let guard = inner.facade.lock().map_err(|_| poisoned())?;
            let facade = guard.as_ref().ok_or_else(closed)?;
            op(facade)
        })
        .await
    }

    /// Run `op` against the facade **mutably** — for the few methods that need
    /// it, `mount` above all.
    pub async fn with_mut<T, F>(&self, op: F) -> Result<T>
    where
        F: FnOnce(&mut AreevFacade) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self.inner.gate.acquire().await.map_err(|_| closed())?;
        let inner = Arc::clone(&self.inner);
        offload(move || {
            let mut guard = inner.facade.lock().map_err(|_| poisoned())?;
            let facade = guard.as_mut().ok_or_else(closed)?;
            op(facade)
        })
        .await
    }

    /// Close the memory, off the executor, waiting for any in-flight call.
    ///
    /// The facade is shared by every clone, so this closes it for **all** of
    /// them; subsequent calls on any handle fail. Closing twice is a no-op.
    /// Dropping the last handle is enough — this exists for the cases where
    /// teardown must have HAPPENED before the program moves on (graceful
    /// shutdown, copying the `.db` file, the end of a test).
    pub async fn close(self) -> Result<()> {
        let _permit = self.inner.gate.acquire().await.map_err(|_| closed())?;
        let taken = {
            let mut guard = self.inner.facade.lock().map_err(|_| poisoned())?;
            guard.take()
        };
        match taken {
            Some(f) => {
                offload(move || {
                    drop(f);
                    Ok(())
                })
                .await
            }
            None => Ok(()),
        }
    }
}

impl Drop for AsyncFacade {
    fn drop(&mut self) {
        // Dropping the facade drops the store, which drops the runtime it
        // owns — a panic on an async worker. `get_mut` yields it only for the
        // LAST handle with no call in flight; otherwise the last holder is a
        // blocking-pool thread, where dropping is legal.
        //
        // The store's own `Drop` relocates its runtime too (#322), so a missed
        // `close()` is a slow drop rather than a panic either way. This keeps
        // the whole teardown off the executor rather than just its last step.
        if let Some(f) = Arc::get_mut(&mut self.inner)
            .and_then(|s| s.facade.get_mut().ok())
            .and_then(Option::take)
        {
            std::thread::spawn(move || drop(f));
        }
    }
}

/// Run a blocking facade call on Tokio's blocking pool, where `block_on` is
/// legal.
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
    AreevError::Storage("areev facade poisoned by a panic in another task".into())
}

fn closed() -> AreevError {
    AreevError::Storage("areev facade already closed".into())
}
