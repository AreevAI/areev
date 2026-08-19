//! Areev triggers — declared as grains, evaluated by a one-shot idempotent
//! command, safe to invoke concurrently.
//!
//! There is still no daemon and no scheduler. What changes is that the
//! *cadence* becomes data in the memory instead of a fact buried in someone's
//! crontab: the OS scheduler stays a dumb heartbeat, and the memory decides
//! what is actually due. That is the same posture `areev loop run --if-stale`
//! already establishes, and the same one anacron and systemd's
//! `Persistent=true` take — a persisted watermark plus catch-up on the next
//! invocation, rather than a resident process holding a timer.
pub mod clock;
pub mod error;
pub mod schedule;

pub use clock::{Clock, FixedClock, SystemClock};
pub use error::{Result, TriggerError};
