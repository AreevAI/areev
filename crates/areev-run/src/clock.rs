//! The ONE place the runtime reads the wall clock. Every reading flows into
//! the scheduler as `EventIn::ClockReading` and is journaled in decision
//! records — the scheduler core cannot tell the time (its clippy.toml and
//! the CI dep-tree check enforce it), so this seam is the whole clock
//! surface, and replay/verify substitute journaled readings here.

/// A clock the driver reads. `SystemClock` for live runs; tests and
/// verify-mode inject fixed sequences.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Live wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A scripted clock for tests and replay: returns the given readings in
/// order, then keeps returning the last (a run that reads more often than
/// the script anticipated must not go backwards).
pub struct ScriptedClock {
    readings: std::sync::Mutex<(Vec<u64>, usize)>,
}

impl ScriptedClock {
    pub fn new(readings: Vec<u64>) -> Self {
        ScriptedClock { readings: std::sync::Mutex::new((readings, 0)) }
    }
}

impl Clock for ScriptedClock {
    fn now_ms(&self) -> u64 {
        let mut g = self.readings.lock().unwrap();
        let (readings, idx) = &mut *g;
        let v = readings.get(*idx).copied().or_else(|| readings.last().copied()).unwrap_or(0);
        if *idx < readings.len() {
            *idx += 1;
        }
        v
    }
}
