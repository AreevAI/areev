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
///
/// It is now a DEFAULT rather than the only value (#299). Ten minutes was the
/// whole story, and it had to be: the lease was renewed only at superstep
/// boundaries, so nothing shorter was safe — an abstract node's turns and
/// tool calls all happen inside ONE superstep, and with a 300 s tool timeout
/// and sixteen effects a healthy driver could already outlive 600 s and lose
/// its own run mid-flight. Renewing inside the superstep is what makes a
/// short TTL safe, which is why the two ship together.
pub const DEFAULT_RUN_LEASE_MS: i64 = 600_000;

/// The shortest lease this build accepts (#299).
///
/// Below this, ordinary scheduling jitter between two renewals starts to look
/// like a dead driver, and a takeover mid-flight is worse than a slow
/// recovery — it is the failure the lease exists to prevent.
pub const MIN_RUN_LEASE_MS: i64 = 5_000;

/// A stable-ish identity for this driver: host plus pid (#300).
///
/// The holder used to be `principal#pid`, which two containers running as
/// PID 1 under one service principal produce IDENTICALLY — the normal
/// Kubernetes shape. `RunLease::acquire` re-enters an equal holder by design,
/// so the second pod acquired a LIVE lease, bumped the fence, and both
/// drivers dispatched the open superstep's effects; the first learned of the
/// takeover only at its next renewal, and the dangling-intent default is
/// redelivery.
///
/// The trigger evaluator has included the host since #36; this is the same
/// helper, now shared.
pub fn default_node_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "node".into());
    format!("{host}/{}", std::process::id())
}

/// The lease holder string for one driver.
pub fn holder_for(principal: &str, node: Option<&str>) -> String {
    let node = node
        .map(str::to_string)
        .unwrap_or_else(default_node_id);
    format!("{principal}#{node}")
}

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

    /// The holder string and expiry currently recorded for `run_id`, if any
    /// (#299 item 4) — so a status surface can say WHEN takeover becomes
    /// possible instead of "recovering, up to ten minutes".
    pub fn peek(
        facade: &AreevFacade,
        run_id: &str,
    ) -> Result<Option<(String, i64)>, RunError> {
        let key = format!("{RUN_LEASE_PREFIX}{run_id}");
        let raw = facade
            .with_store(|m| m.meta_get(&key))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;
        Ok(raw.and_then(|raw| {
            serde_json::from_str::<LeaseRow>(&raw)
                .ok()
                .map(|r| (r.holder, r.until_ms))
        }))
    }

    /// This lease's TTL, so the driver can pick a renewal interval from it.
    pub fn lease_ms(&self) -> i64 {
        self.lease_ms
    }

    /// Give the lease up, so the run can be resumed immediately rather than
    /// after the lease times out. Best-effort: failing to release is harmless,
    /// because the lease expires anyway.
    pub fn release(self, facade: &AreevFacade) {
        let _ = facade.with_store(|m| m.meta_delete(&self.key));
    }
}

/// `meta` key prefix for run CONCURRENCY slots (#296). Host-local like the
/// lease, and never replicated.
const RUN_SLOT_PREFIX: &str = "runslot:";

/// One claimed concurrency slot, beside the run lease (#296).
///
/// The run lease is keyed on the run id, so it excludes two drivers on ONE
/// run and nothing else: a bug or a second entry point could start unbounded
/// model spend in a tenant, and the only thing standing in the way was the
/// product's own dispatcher.
///
/// N CAS'd rows is what makes the cap hard under races. Count-then-acquire
/// does not: two starters both read N-1 and both proceed. Each slot is
/// claimed by winning a compare-and-set on a specific `runslot:<scope>:<k>`
/// row, so at most one starter can hold slot `k`.
///
/// The cap is HOST configuration, not a file truth: how many runs a
/// deployment may execute at once is a property of the deployment, and
/// writing it into the memory would make it replicate to hosts with
/// different capacity.
pub struct RunSlots {
    held: Vec<(String, String)>,
    lease_ms: i64,
}

impl RunSlots {
    /// Claim one slot in each configured scope, or refuse.
    ///
    /// Refuses BEFORE anything is written, so a run turned away at the cap
    /// leaves nothing behind and its id stays free to start once a slot
    /// frees.
    pub fn claim(
        facade: &AreevFacade,
        run_id: &str,
        principal: &str,
        now_ms: i64,
        lease_ms: i64,
        max_memory: Option<u32>,
        max_principal: Option<u32>,
    ) -> Result<RunSlots, RunError> {
        let mut slots = RunSlots { held: Vec::new(), lease_ms };
        let scopes = [
            (max_memory, "mem".to_string(), "this memory".to_string()),
            (
                max_principal,
                format!("principal:{principal}"),
                format!("principal '{principal}'"),
            ),
        ];
        for (cap, scope, describe) in scopes {
            let Some(cap) = cap else { continue };
            match slots.claim_one(facade, run_id, &scope, now_ms, cap) {
                Ok(()) => {}
                Err(e) => {
                    // Release whatever we already took, so a refusal in the
                    // second scope does not strand a slot in the first.
                    slots.release(facade);
                    return Err(match e {
                        RunError::Storage { detail } => RunError::Storage { detail },
                        _ => RunError::ConcurrencyLimit {
                            scope: describe,
                            limit: cap,
                        },
                    });
                }
            }
        }
        Ok(slots)
    }

    fn claim_one(
        &mut self,
        facade: &AreevFacade,
        run_id: &str,
        scope: &str,
        now_ms: i64,
        cap: u32,
    ) -> Result<(), RunError> {
        for k in 0..cap {
            let key = format!("{RUN_SLOT_PREFIX}{scope}:{k}");
            let existing = facade
                .with_store(|m| m.meta_get(&key))
                .map_err(|e| RunError::Storage { detail: e.to_string() })?;
            if let Some(raw) = &existing {
                let row: LeaseRow = serde_json::from_str(raw).unwrap_or_default();
                // A live slot held by another run is taken. A crashed
                // holder's slot is reclaimable once its TTL passes — the
                // same rule the run lease follows.
                if row.until_ms > now_ms && row.holder != run_id {
                    continue;
                }
            }
            let row = LeaseRow {
                holder: run_id.to_string(),
                fence: 0,
                until_ms: now_ms + self.lease_ms,
            };
            let json = serde_json::to_string(&row).unwrap_or_default();
            let won = facade
                .with_store(|m| m.meta_cas(&key, existing.as_deref(), &json))
                .map_err(|e| RunError::Storage { detail: e.to_string() })?;
            if won {
                self.held.push((key, json));
                return Ok(());
            }
            // Lost the CAS — another starter took this slot between our read
            // and our write. Try the next one.
        }
        Err(RunError::ConcurrencyLimit { scope: scope.to_string(), limit: cap })
    }

    /// Extend every held slot, alongside the run lease.
    pub fn renew(&mut self, facade: &AreevFacade, run_id: &str, now_ms: i64) {
        for (key, seen) in self.held.iter_mut() {
            let row = LeaseRow {
                holder: run_id.to_string(),
                fence: 0,
                until_ms: now_ms + self.lease_ms,
            };
            let json = serde_json::to_string(&row).unwrap_or_default();
            // Best-effort: a lost slot renewal is not a reason to fail a run
            // mid-flight. The run LEASE is the authority on who may write.
            if facade
                .with_store(|m| m.meta_cas(key, Some(seen.as_str()), &json))
                .unwrap_or(false)
            {
                *seen = json;
            }
        }
    }

    /// Give every held slot back. A parked run holds no slot — it releases
    /// the run lease too — so a queue of parked approvals never starves live
    /// work.
    pub fn release(&mut self, facade: &AreevFacade) {
        for (key, _) in std::mem::take(&mut self.held) {
            let _ = facade.with_store(|m| m.meta_delete(&key));
        }
    }

    /// Current occupancy, for `run list` (#296 item 5).
    pub fn occupancy(facade: &AreevFacade, now_ms: i64) -> Result<Vec<(String, String)>, RunError> {
        let rows = facade
            .with_store(|m| m.meta_scan(RUN_SLOT_PREFIX))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;
        Ok(rows
            .into_iter()
            .filter_map(|(k, raw)| {
                let row: LeaseRow = serde_json::from_str(&raw).ok()?;
                (row.until_ms > now_ms).then_some((k, row.holder))
            })
            .collect())
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
