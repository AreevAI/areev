//! The one place this crate reads the wall clock.
//!
//! Modelled on `areev_run::Clock`, and for the same reason: a subsystem whose
//! whole job is time-dependent is untestable if it calls `SystemTime::now()`
//! inline. Every decision here — is this due, has the lease expired, when is the
//! next occurrence — takes `now_ms` as a value, so a test can step across a cron
//! boundary or a lease expiry without sleeping.
//!
//! On the embedded backend the host clock *is* the database clock, because
//! there is exactly one writer on one machine. On Postgres, where several nodes
//! share a memory, readings should come from the server (`now()`) instead:
//! skewed node clocks otherwise make triggers fire early, late, or twice, and
//! that is miserable to debug after the fact.

/// A source of epoch-milliseconds.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The wall clock, with a simulation override.
///
/// `AREEV_TRIGGER_NOW_MS` pins the reading, mirroring the loop's
/// `AREEV_LOOP_NOW_MS` seam. A set-but-unparseable value **panics**: the caller
/// explicitly asked for simulated time, and silently running at wall time
/// instead would defeat the point and make a golden test pass for the wrong
/// reason.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        if let Ok(v) = std::env::var("AREEV_TRIGGER_NOW_MS") {
            return v.parse::<i64>().unwrap_or_else(|_| {
                panic!("AREEV_TRIGGER_NOW_MS is set but not an integer: {v:?}")
            });
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A clock that returns exactly what a test tells it to.
pub struct FixedClock(pub std::sync::atomic::AtomicI64);

impl FixedClock {
    pub fn new(now_ms: i64) -> Self {
        FixedClock(std::sync::atomic::AtomicI64::new(now_ms))
    }
    pub fn set(&self, now_ms: i64) {
        self.0.store(now_ms, std::sync::atomic::Ordering::SeqCst);
    }
    /// Move time forward. Never backward — a clock that can go back would let a
    /// test pass a case that cannot happen.
    pub fn advance(&self, ms: i64) {
        assert!(ms >= 0, "time does not run backwards");
        self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_reads_and_advances() {
        let c = FixedClock::new(1_000);
        assert_eq!(c.now_ms(), 1_000);
        c.advance(500);
        assert_eq!(c.now_ms(), 1_500);
        c.set(42);
        assert_eq!(c.now_ms(), 42);
    }

    #[test]
    #[should_panic(expected = "time does not run backwards")]
    fn fixed_clock_refuses_to_rewind() {
        FixedClock::new(10).advance(-1);
    }

    #[test]
    fn the_simulation_seam_is_honoured() {
        // Serialised with the next test by running them in one #[test]: env
        // vars are process-global and these two would race.
        std::env::set_var("AREEV_TRIGGER_NOW_MS", "1755600000000");
        assert_eq!(SystemClock.now_ms(), 1_755_600_000_000);
        std::env::remove_var("AREEV_TRIGGER_NOW_MS");

        // …and a garbled value must fail loud rather than falling back to wall
        // time, which would make a time-travel test silently meaningless.
        std::env::set_var("AREEV_TRIGGER_NOW_MS", "soon");
        let panicked = std::panic::catch_unwind(|| SystemClock.now_ms()).is_err();
        std::env::remove_var("AREEV_TRIGGER_NOW_MS");
        assert!(panicked, "a set-but-unparseable override must panic");
    }
}
