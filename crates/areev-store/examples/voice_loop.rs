//! Voice-loop simulation: 50ms-cadence recall on the "audio
//! loop" with batched write-back interleaved — the M4 CI gate shape.
//! Run: cargo run --release -p areev-store --example voice_loop
//!
//! Telemetry is **on** (`aggregate`) here on purpose: a voice receptionist
//! using Areev Loop runs with recall telemetry enabled, so this gate proves the
//! buffered, non-blocking capture stays inside the 50ms voice budget. The
//! off path is a strict subset (a single `None` branch), so protecting the
//! on path protects both.

use areev_core::types::{Event, Fact};
use areev_store::{AddableDyn, Areev, TelemetryMode};
use std::time::{Duration, Instant};

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    // §6 edge profile: FTS off on the voice hot path; telemetry on (aggregate).
    let opts = areev_store::AreevOptions {
        index_text: false,
        telemetry: TelemetryMode::Aggregate,
        ..Default::default()
    };
    let mut m = Areev::open_with(dir.path().join("call.db").to_str().unwrap(), opts).unwrap();

    // seed a realistic caller profile
    let mut batch: Vec<Fact> = Vec::new();
    for i in 0..2000 {
        let mut f = Fact::new(&format!("caller:{:03}", i % 120), "attr", &format!("v{i}"));
        f.common.namespace = Some("main".into());
        batch.push(f);
    }
    let refs: Vec<&dyn AddableDyn> = batch.iter().map(|f| f as &dyn AddableDyn).collect();
    m.add_batch(&refs).unwrap();

    // simulate a 20-second call: one recall per 50ms frame, write-back of
    // 4 transcript events every 8 frames (≈2.5 turns/sec)
    let frames = 400usize;
    let mut reads: Vec<u128> = Vec::with_capacity(frames);
    let mut writes: Vec<u128> = Vec::new();
    let mut x: u64 = 7;
    let mut rng = move || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
    let call_start = Instant::now();
    for frame in 0..frames {
        let frame_deadline = call_start + Duration::from_millis((frame as u64 + 1) * 50);
        let subj = format!("caller:{:03}", rng() % 120);
        let t = Instant::now();
        let r = m.recall_hybrid("main", Some(&subj), None, None, 8, Some(Duration::from_millis(5))).unwrap();
        reads.push(t.elapsed().as_nanos());
        std::hint::black_box(r);
        if frame % 8 == 7 {
            let mut evs: Vec<Event> = Vec::new();
            for k in 0..4 {
                let mut e = Event::new(&format!("utterance frame {frame} part {k} about bookings"));
                e.common.namespace = Some("main".into());
                e.session_id = Some("live-call".into());
                evs.push(e);
            }
            let refs: Vec<&dyn AddableDyn> = evs.iter().map(|e| e as &dyn AddableDyn).collect();
            let t = Instant::now();
            m.add_batch(&refs).unwrap();
            writes.push(t.elapsed().as_nanos());
        }
        // Busy-wait to the frame boundary: a real voice loop is hot
        // (processing audio continuously); thread::sleep would park us and
        // every recall would wake cache-cold on a down-clocked core —
        // measured at +250µs of pure scheduler artifact on M4 Max.
        while Instant::now() < frame_deadline {
            std::hint::spin_loop();
        }
    }
    let pct = |v: &mut Vec<u128>, q: f64| {
        v.sort_unstable();
        v[((v.len() as f64 * q) as usize).min(v.len() - 1)] as f64 / 1000.0
    };
    let (mut r, mut w) = (reads, writes);
    println!("voice loop: {frames} frames @50ms, {} write-backs, wall {:.1}s  (telemetry: aggregate)", w.len(), call_start.elapsed().as_secs_f64());
    println!("frame recall  p50 {:.1}µs  p95 {:.1}µs  p99 {:.1}µs  (target <200µs, telemetry on)", pct(&mut r, 0.5), pct(&mut r, 0.95), pct(&mut r, 0.99));
    println!("write-back    p50 {:.1}µs  p95 {:.1}µs  (off audio thread in prod)", pct(&mut w, 0.5), pct(&mut w, 0.95));
    let ok = pct(&mut r, 0.5) < 200.0;
    println!("verdict: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
