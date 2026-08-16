//! Streaming (§6.10) — **observational only**. Events describe what the
//! driver journaled; they never carry authority and never backpressure the
//! scheduler: `emit` is non-blocking by construction (bounded buffer,
//! drop-oldest + counter), and the subscriber runs on its own thread. The
//! §6.10 check is that journals are IDENTICAL with no subscriber, a
//! subscriber, and a deliberately slow subscriber.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// One §6.10 event, stamped with where in the run it happened. The enum is
/// append-only (subscribers match non-exhaustively).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "event")]
pub enum RunEvent {
    RunStarted { run_id: String },
    RunResumed { run_id: String },
    NodeDispatched {
        superstep: u64,
        node: String,
        task_path: String,
        attempt: u32,
        effect_seq: u32,
    },
    EffectSettled {
        superstep: u64,
        node: String,
        task_path: String,
        attempt: u32,
        effect_seq: u32,
        ok: bool,
    },
    AskRaised { superstep: u64, node: String, tool_call_id: String },
    CheckpointWritten { superstep: u64, hash: String },
    /// A streamed text delta from an abstract node's model turn (Wave 2f —
    /// reserved in the vocabulary now so subscribers can already match it).
    TokenChunk { node: String, task_path: String, attempt: u32, text: String },
    /// Always the last event. `dropped_events` is the §6.10 honesty
    /// counter: how many events the bounded buffer discarded.
    RunFinished { run_id: String, outcome: String, dropped_events: u64 },
}

/// A host-supplied event subscriber. Called on the bus's OWN thread — a
/// slow implementation delays events (and eventually loses the oldest),
/// never the run.
pub trait RunObserver: Send + Sync {
    fn event(&self, ev: &RunEvent);
}

struct Queue {
    buf: VecDeque<RunEvent>,
    dropped: u64,
    closed: bool,
}

struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
}

/// The bounded drop-oldest event bus. One per drive; dropped on drive exit
/// (Drop drains what is queued, then joins the delivery thread).
pub(crate) struct EventBus {
    shared: Arc<Shared>,
    capacity: usize,
    worker: Option<std::thread::JoinHandle<()>>,
}

pub(crate) const EVENT_BUFFER: usize = 1024;

impl EventBus {
    pub fn new(observer: Arc<dyn RunObserver>, capacity: usize) -> Self {
        let shared = Arc::new(Shared {
            q: Mutex::new(Queue { buf: VecDeque::new(), dropped: 0, closed: false }),
            cv: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || loop {
            // One event per iteration: the lock is never held during
            // delivery, so `emit` stays wait-free for the driver.
            let ev = {
                let mut q = worker_shared.q.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(ev) = q.buf.pop_front() {
                        break ev;
                    }
                    if q.closed {
                        return;
                    }
                    q = worker_shared
                        .cv
                        .wait(q)
                        .unwrap_or_else(|e| e.into_inner());
                }
            };
            observer.event(&ev);
        });
        EventBus { shared, capacity, worker: Some(worker) }
    }

    /// Non-blocking: over capacity, the OLDEST queued event is discarded
    /// and counted — the run never waits for a subscriber.
    pub fn emit(&self, ev: RunEvent) {
        let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.buf.len() >= self.capacity {
            q.buf.pop_front();
            q.dropped += 1;
        }
        q.buf.push_back(ev);
        drop(q);
        self.shared.cv.notify_one();
    }

    pub fn dropped(&self) -> u64 {
        self.shared.q.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }
}

impl Drop for EventBus {
    fn drop(&mut self) {
        {
            let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
            q.closed = true;
        }
        self.shared.cv.notify_one();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// An observer that BLOCKS inside its first delivery until released —
    /// the §6.10 slow-subscriber case, made deterministic.
    struct Blocking {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        seen: Mutex<Vec<RunEvent>>,
    }

    impl RunObserver for Blocking {
        fn event(&self, ev: &RunEvent) {
            let first = {
                let mut seen = self.seen.lock().unwrap();
                seen.push(ev.clone());
                seen.len() == 1
            };
            if first {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
            }
        }
    }

    #[test]
    fn bounded_buffer_drops_oldest_and_counts() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let obs = Arc::new(Blocking {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            seen: Mutex::new(Vec::new()),
        });
        let bus = EventBus::new(Arc::clone(&obs) as Arc<dyn RunObserver>, 4);
        let ev = |i: u64| RunEvent::CheckpointWritten { superstep: i, hash: String::new() };

        // First event enters delivery and blocks there.
        bus.emit(ev(1));
        entered_rx.recv().unwrap();
        // Nine more queue behind it; capacity 4 keeps only the newest four.
        for i in 2..=10 {
            bus.emit(ev(i));
        }
        assert_eq!(bus.dropped(), 5, "events 2..=6 were discarded, oldest first");
        release_tx.send(()).unwrap();
        drop(bus); // drains the survivors, then joins

        let seen = obs.seen.lock().unwrap();
        let got: Vec<u64> = seen
            .iter()
            .map(|e| match e {
                RunEvent::CheckpointWritten { superstep, .. } => *superstep,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(got, vec![1, 7, 8, 9, 10], "in-flight + the newest four");
    }
}
