//! Run leases — preventing two drivers from advancing one run.
//!
//! ## What this replaces
//!
//! Before this, two drivers advancing the same run last-write-wins in the
//! journal, silently. `RunError::Tainted`'s doc comment claimed forked
//! supersession tips were detected as taint; they were not — `journal::ingest`
//! simply overwrites a second result for the same key, and the manifest's
//! owner-nonce check is recorded as a known gap. So the failure mode was not
//! "fails safe", it was "fails quietly", which is worse.
//!
//! A lease turns that into prevention: a driver that stalls past its lease
//! loses it, and its next checkpoint is refused with `RUN-E021` instead of
//! landing behind whoever took over.
//!
//! ## Why the fence needs no token column
//!
//! The lease is a `meta` row and the fence rides *inside* its value, so a
//! renewal is a compare-and-swap against the exact row the holder last saw. A
//! stalled driver's row no longer matches, its swap affects nothing, and it is
//! refused. That is Kleppmann's fencing-token argument obtained without a
//! token, available because the lock and the data are the same row in the same
//! store.
//!
//! ## Tier behaviour
//!
//! On the embedded backend one memory is one writer, enforced at open, so two
//! drivers cannot reach the same run in the first place and this is a cheap
//! no-op that always succeeds. It earns its keep on Postgres, where several
//! nodes genuinely share a memory.

use areev_cal::AreevFacade;
use areev_run_core::RunError;
use serde::{Deserialize, Serialize};

/// `meta` key prefix for run leases. Host-local usage, never replicated —
/// a lease that rode a bundle would describe a holder on another machine.
const RUN_LEASE_PREFIX: &str = "runlease:";

/// How long a lease is held before another driver may take the run over.
///
/// Generous on purpose: a lease shorter than a slow superstep causes takeovers
/// mid-flight, which is the failure this exists to prevent.
pub const DEFAULT_RUN_LEASE_MS: i64 = 600_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct LeaseRow {
    holder: String,
    fence: u64,
    until_ms: i64,
}

/// A held run lease. Renew it as the run advances; drop releases nothing on its
/// own, because a crashed driver must leave the lease to expire rather than
/// have it cleaned up by something that is no longer running.
pub struct RunLease {
    key: String,
    holder: String,
    /// The exact row we last wrote — what a renewal must present to win.
    seen: String,
    fence: u64,
    lease_ms: i64,
}

impl RunLease {
    /// Take the lease for `run_id`, or fail if a live one is held elsewhere.
    pub fn acquire(
        facade: &AreevFacade,
        run_id: &str,
        holder: &str,
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<RunLease, RunError> {
        let key = format!("{RUN_LEASE_PREFIX}{run_id}");
        let existing = facade
            .with_store(|m| m.meta_get(&key))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;

        let (expected, next_fence) = match &existing {
            Some(raw) => {
                let row: LeaseRow = serde_json::from_str(raw).unwrap_or_default();
                // A live lease held by someone else is the whole point.
                // Re-entering our own is fine: a driver resuming a run it
                // already holds is the ordinary case.
                if row.until_ms > now_ms && row.holder != holder {
                    return Err(RunError::LeaseLost { run_id: run_id.to_string() });
                }
                (Some(raw.clone()), row.fence.wrapping_add(1))
            }
            None => (None, 1),
        };

        let row = LeaseRow {
            holder: holder.to_string(),
            fence: next_fence,
            until_ms: now_ms + lease_ms,
        };
        let json = serde_json::to_string(&row).unwrap_or_default();
        let won = facade
            .with_store(|m| m.meta_cas(&key, expected.as_deref(), &json))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;
        if !won {
            // Someone else moved the row between our read and our write.
            return Err(RunError::LeaseLost { run_id: run_id.to_string() });
        }
        Ok(RunLease { key, holder: holder.to_string(), seen: json, fence: next_fence, lease_ms })
    }

    /// Extend the lease. Fails if we no longer hold it — which is exactly the
    /// moment a stalled driver must stop writing.
    pub fn renew(&mut self, facade: &AreevFacade, run_id: &str, now_ms: i64) -> Result<(), RunError> {
        let row = LeaseRow {
            holder: self.holder.clone(),
            fence: self.fence,
            until_ms: now_ms + self.lease_ms,
        };
        let json = serde_json::to_string(&row).unwrap_or_default();
        let won = facade
            .with_store(|m| m.meta_cas(&self.key, Some(&self.seen), &json))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;
        if !won {
            return Err(RunError::LeaseLost { run_id: run_id.to_string() });
        }
        self.seen = json;
        Ok(())
    }

    /// Give the lease up, so the run can be resumed immediately rather than
    /// after the lease times out. Best-effort: failing to release is harmless,
    /// because the lease expires anyway.
    pub fn release(self, facade: &AreevFacade) {
        let _ = facade.with_store(|m| m.meta_delete(&self.key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use areev_store::Areev;

    fn facade(dir: &tempfile::TempDir, name: &str) -> AreevFacade {
        AreevFacade::new(Areev::open(dir.path().join(name).to_str().unwrap()).unwrap())
    }

    #[test]
    fn one_holder_at_a_time() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = facade(&dir, "a.db");
        let _a = RunLease::acquire(&f, "r1", "A", 1_000, 60_000).unwrap();
        let b = RunLease::acquire(&f, "r1", "B", 1_000, 60_000);
        assert!(matches!(b, Err(RunError::LeaseLost { .. })));
    }

    #[test]
    fn re_entering_our_own_lease_is_fine() {
        // Resuming a run this driver already holds is ordinary, not a conflict.
        let dir = tempfile::TempDir::new().unwrap();
        let f = facade(&dir, "b.db");
        let _a = RunLease::acquire(&f, "r1", "A", 1_000, 60_000).unwrap();
        assert!(RunLease::acquire(&f, "r1", "A", 2_000, 60_000).is_ok());
    }

    #[test]
    fn an_expired_lease_can_be_taken_over() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = facade(&dir, "c.db");
        let _a = RunLease::acquire(&f, "r1", "A", 1_000, 10).unwrap();
        // A died. Its lease expires and B must be able to continue the run,
        // otherwise one crash parks a run forever.
        assert!(RunLease::acquire(&f, "r1", "B", 1_000_000, 60_000).is_ok());
    }

    #[test]
    fn a_stalled_holder_cannot_renew_after_being_replaced() {
        // The whole point: A's writes stop the moment B takes over, rather than
        // silently landing behind B's.
        let dir = tempfile::TempDir::new().unwrap();
        let f = facade(&dir, "d.db");
        let mut a = RunLease::acquire(&f, "r1", "A", 1_000, 10).unwrap();
        let _b = RunLease::acquire(&f, "r1", "B", 1_000_000, 60_000).unwrap();

        let err = a.renew(&f, "r1", 1_000_001).unwrap_err();
        assert_eq!(err.code(), "RUN-E021");
        assert!(err.to_string().starts_with("RUN-E021"));
    }

    #[test]
    fn renewing_extends_the_deadline() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = facade(&dir, "e.db");
        let mut a = RunLease::acquire(&f, "r1", "A", 1_000, 5_000).unwrap();
        a.renew(&f, "r1", 4_000).unwrap();
        // Past the ORIGINAL deadline but inside the renewed one: B must lose.
        assert!(RunLease::acquire(&f, "r1", "B", 6_500, 5_000).is_err());
    }

    #[test]
    fn releasing_lets_the_next_driver_start_at_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = facade(&dir, "f.db");
        let a = RunLease::acquire(&f, "r1", "A", 1_000, 600_000).unwrap();
        a.release(&f);
        assert!(RunLease::acquire(&f, "r1", "B", 1_100, 600_000).is_ok());
    }
}
