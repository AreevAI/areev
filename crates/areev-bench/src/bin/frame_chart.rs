//! frame_chart — the "recall inside an audio frame" wall chart (LR-1).
//!
//! One retrieval op (up to 16 most-recent facts about a caller), identical
//! 10k-fact dataset and query workload, measured over every surface a voice
//! developer could actually deploy:
//!
//!   A  in-process        Areev::recall            (the shipped hot path)
//!   B  localhost HTTP    POST /api/cal to UiServer (sidecar topology)
//!   C  MCP stdio         tools/call areev_recall  (agent-host topology)
//!
//! Reference lines are printed, not measured: the 50ms audio-frame budget
//! and Zep's own published "retrieval under 200 ms" enterprise headline.
//! Cloud-service bars belong to vendor-measured adapters (LongMemEval
//! harness, later) — nothing simulated here.

use areev_bench::{areev_bin, gen_facts, load_facts, pct, Xorshift};
use areev_cal::AreevFacade;
use areev_server::UiServer;
use areev_store::{Areev, AreevOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Instant;

const N_FACTS: usize = 10_000;
const N_SUBJECTS: u64 = 800;
const WARMUP: usize = 300;
const N_QUERIES: usize = 2_000;
const FRAME_MS: f64 = 50.0;

fn opts() -> AreevOptions {
    // voice/edge profile: structural recall, no experimental-FTS write tax
    AreevOptions { index_text: false, ..Default::default() }
}

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut rng = Xorshift(42);
    let facts = gen_facts(&mut rng, N_FACTS, N_SUBJECTS);
    let queries: Vec<String> = (0..WARMUP + N_QUERIES)
        .map(|_| format!("caller:{:04}", rng.next() % N_SUBJECTS))
        .collect();

    println!("frame_chart: {} facts / {} subjects, {} queries per surface (+{} warmup)\n",
        N_FACTS, N_SUBJECTS, N_QUERIES, WARMUP);
    println!("| surface | p50 µs | p95 µs | p99 µs |");
    println!("|---|---|---|---|");

    // ---------- A: in-process ----------
    let path_a = dir.path().join("a.db");
    let mut m = Areev::open_with(path_a.to_str().unwrap(), opts()).unwrap();
    load_facts(&mut m, &facts);
    let mut v = Vec::new();
    for (i, s) in queries.iter().enumerate() {
        let t = Instant::now();
        let r = m.recall("main", s, None, 16).unwrap();
        let el = t.elapsed().as_nanos();
        if i >= WARMUP {
            v.push(el);
        }
        std::hint::black_box(r);
    }
    let a = pct(v, "A in-process `recall` (voice hot path)");

    // ---------- B: localhost HTTP (sidecar topology) ----------
    let path_b = dir.path().join("b.db");
    let mut mb = Areev::open_with(path_b.to_str().unwrap(), opts()).unwrap();
    load_facts(&mut mb, &facts);
    let facade = AreevFacade::with_session(mb, Some("main".to_string()), None);
    let server = UiServer::new(facade, "bench".to_string());
    let listener = UiServer::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || server.serve(listener));
    // one request per connection is the server's model — the connection
    // cost is part of what a sidecar topology really pays per lookup
    let http_recall = |subject: &str| -> Vec<u8> {
        let body = serde_json::json!({
            "query": format!("RECALL facts WHERE subject = \"{subject}\" LIMIT 16")
        })
        .to_string();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_nodelay(true).unwrap();
        write!(
            stream,
            "POST /api/cal HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        buf
    };
    let first = String::from_utf8_lossy(&http_recall(&queries[0])).to_string();
    assert!(first.contains("\"ok\":true"), "HTTP leg sanity: {first}");
    let mut v = Vec::new();
    for (i, s) in queries.iter().enumerate() {
        let t = Instant::now();
        let r = http_recall(s);
        let el = t.elapsed().as_nanos();
        if i >= WARMUP {
            v.push(el);
        }
        std::hint::black_box(r);
    }
    let b = pct(v, "B localhost HTTP `/api/cal` (sidecar)");

    // ---------- C: MCP stdio (agent-host topology) ----------
    let path_c = dir.path().join("c.db");
    {
        let mut mc = Areev::open_with(path_c.to_str().unwrap(), opts()).unwrap();
        load_facts(&mut mc, &facts);
    } // close before the MCP server takes the single-writer seat
    let bin = areev_bin();
    assert!(bin.exists(), "release areev binary missing: {} (cargo build --release -p areev)", bin.display());
    let mut child = Command::new(&bin)
        .args(["serve", "--mcp", "--db", path_c.to_str().unwrap(), "--ns", "main"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = |line: &str| writeln!(stdin, "{line}").and_then(|_| stdin.flush()).unwrap();
    send(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"frame_chart","version":"0"}}}).to_string());
    lines.next().unwrap().unwrap(); // initialize response
    send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let mut v = Vec::new();
    for (i, s) in queries.iter().enumerate() {
        let req = serde_json::json!({"jsonrpc":"2.0","id":i+2,"method":"tools/call",
            "params":{"name":"areev_recall","arguments":{"subject": s}}})
        .to_string();
        let t = Instant::now();
        send(&req);
        let resp = lines.next().unwrap().unwrap();
        let el = t.elapsed().as_nanos();
        if i == 0 {
            let val: serde_json::Value = serde_json::from_str(&resp).unwrap();
            assert_eq!(val["result"]["isError"], false, "MCP leg sanity: {resp}");
        }
        if i >= WARMUP {
            v.push(el);
        }
        std::hint::black_box(resp);
    }
    let c = pct(v, "C MCP stdio `areev_recall` (agent host)");
    drop(stdin);
    let _ = child.wait();

    // ---------- the wall chart ----------
    println!("\nagainst the 50ms voice-frame budget (p99):");
    for (name, p) in [("in-process", &a), ("localhost HTTP", &b), ("MCP stdio", &c)] {
        let pctof = p.p99 / (FRAME_MS * 1000.0) * 100.0;
        println!("  {name:<16} {:>9.1} µs = {pctof:>6.2}% of one frame", p.p99);
    }
    println!("  network memory service: not measurable in-process — Zep's own enterprise");
    println!("  headline is \"retrieval under 200 ms\" = 400% of one frame (vendor-stated).");

    println!(
        "\nCHART_JSON: {}",
        serde_json::json!({
            "frame_ms": FRAME_MS,
            "surfaces": [
                {"name": "in-process", "p50_us": a.p50, "p95_us": a.p95, "p99_us": a.p99},
                {"name": "localhost HTTP", "p50_us": b.p50, "p95_us": b.p95, "p99_us": b.p99},
                {"name": "MCP stdio", "p50_us": c.p50, "p95_us": c.p95, "p99_us": c.p99},
            ],
            "references": [{"name": "Zep (vendor-stated)", "us": 200000.0}]
        })
    );
}
