//! PE-corpus scale harness — where retrieval latency goes as a private-equity
//! "firm brain" grows past any size this repo has ever measured.
//!
//! Everything else in `areev-bench` tops out near 10k grains (`pg_bench` 10k,
//! `frame_chart` 10k, the embedded `bench` example 13k). `ARCHITECTURE.md`
//! documents the vector tier as an exact scan with a ~25 ms p50 @2k / ~100 ms
//! @10k slope, but nothing in the tree reproduces it: `pg_bench` installs no
//! embedder, so it never touches the vector leg at all. This binary is that
//! missing measurement, at the sizes a deal corpus actually reaches.
//!
//! Run (embedded, 100k grains, ~1 min):
//!   cargo run --release -p areev-bench --bin pe_scale
//! 1M grains, partitioned by deal, corpus kept for re-runs:
//!   cargo run --release -p areev-bench --bin pe_scale -- \
//!       --deals 2000 --per-deal 500 --db /tmp/pe-1m.db
//! Postgres tier:
//!   DATABASE_URL=postgres://... cargo run --release -p areev-bench \
//!       --features postgres --bin pe_scale -- --postgres
//!
//! **The two layouts ARE the architectural question.** `--layout flat` puts
//! every grain in one namespace, so `nearest_vector` is the unpartitioned
//! exact scan — the worst case, and the one the PE thesis has to survive.
//! `--layout partitioned` gives each deal its own `deal.<id>` namespace: the
//! same k-NN inside one deal touches 1/N of the corpus, while the cross-deal
//! question becomes a `deal.*` prefix hybrid whose vector leg is a single
//! brute-force scan of the union (`recall_hybrid_ids`, its own comment says
//! "one brute-force scan"). Run both at the same size; the gap between them is
//! exactly what structural partitioning buys, and it decides whether an ANN
//! index is on the critical path or an optimization nobody needs.
//!
//! The embedder is a deterministic synthetic (hashed token buckets,
//! L2-normalized) so the harness is keyless, offline and reproducible. It
//! measures index-and-scan cost — the thing the exact-scan slope is about —
//! and says nothing about retrieval quality. `--dump-topk` / `--compare-topk`
//! capture and diff neighbour sets across runs, which is how an ANN index gets
//! held to a recall@k number instead of a latency number alone.

use areev_bench::Xorshift;
use areev_core::error::Result as StoreResult;
use areev_core::types::{Event, Fact, Grain};
use areev_store::{AddableDyn, Areev, EmbedBackend};
use std::net::TcpStream;
use std::time::Instant;

/// Deterministic hashed-bucket embedder. Token -> FNV-1a -> bucket, with the
/// hash's low bit as the sign so buckets cancel rather than only accumulate;
/// L2-normalized so cosine distance is meaningful. No model, no network: the
/// point is to put a realistically-sized float vector on every grain, because
/// dimension is a first-order term in an exact scan's cost.
struct HashEmbed {
    dim: usize,
}

impl EmbedBackend for HashEmbed {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> StoreResult<Vec<f32>> {
        let mut v = vec![0f32; self.dim];
        for tok in text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in tok.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
            let idx = (h % self.dim as u64) as usize;
            v[idx] += if h & 1 == 0 { 1.0 } else { -1.0 };
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }

    fn model(&self) -> &str {
        "synthetic-hash"
    }
}

/// A **real** embedding model, reached over Ollama's HTTP API.
///
/// The synthetic `HashEmbed` above is fine for latency — a scan's cost does not
/// depend on what the vectors mean — but it cannot answer recall@k, because
/// its sparse hashed vectors have geometry no ANN index is built for (see
/// RESULTS.md §8e: recall that refuses to move when `ef_search` does). Recall
/// is a property of the *embedding model*, so measuring it honestly means
/// putting a real one behind the same `EmbedBackend` seam a deployment uses.
///
/// Hand-rolled HTTP/1.1 over `TcpStream` rather than a client dependency —
/// this is one POST to loopback, and the workspace is dependency-light by
/// policy (`areev-server` hand-rolls its server for the same reason).
struct OllamaEmbed {
    model: String,
    addr: String,
    dim: usize,
}

impl OllamaEmbed {
    fn connect(model: &str, addr: &str) -> Self {
        let probe = Self { model: model.into(), addr: addr.into(), dim: 0 };
        let v = probe.post("dimension probe").unwrap_or_else(|e| {
            panic!("cannot reach Ollama at {addr} for model {model}: {e}\n\
                    start it with `ollama serve` and `ollama pull {model}`")
        });
        println!("embedder: {model} via {addr}, dim {}", v.len());
        Self { model: model.into(), addr: addr.into(), dim: v.len() }
    }

    fn post(&self, text: &str) -> Result<Vec<f32>, String> {
        use std::io::{Read, Write};
        let body =
            serde_json::json!({ "model": self.model, "input": text }).to_string();
        let mut sock = TcpStream::connect(&self.addr).map_err(|e| e.to_string())?;
        let req = format!(
            "POST /api/embed HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.addr,
            body.len(),
            body
        );
        sock.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        let text_resp = String::from_utf8_lossy(&raw);
        let (head, rest) = text_resp
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("malformed HTTP response: {}", &text_resp[..text_resp.len().min(200)]))?;
        // Ollama answers `Transfer-Encoding: chunked` regardless of
        // `Connection: close`, so the body is chunk-framed and not JSON until
        // the frames are stripped. Walk them rather than guessing.
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            let mut out = String::new();
            let mut cur = rest;
            while let Some((len_line, tail)) = cur.split_once("\r\n") {
                // A chunk size may carry `;ext` metadata after the hex length.
                let hex = len_line.split(';').next().unwrap_or("").trim();
                let n = usize::from_str_radix(hex, 16).map_err(|e| format!("bad chunk size {hex:?}: {e}"))?;
                if n == 0 {
                    break;
                }
                if tail.len() < n {
                    return Err("truncated chunk".into());
                }
                out.push_str(&tail[..n]);
                cur = tail[n..].strip_prefix("\r\n").unwrap_or(&tail[n..]);
            }
            out
        } else {
            rest.to_string()
        };
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("{e}: {}", &body[..body.len().min(200)]))?;
        let arr = v["embeddings"][0]
            .as_array()
            .ok_or_else(|| format!("no embeddings in response: {}", &body[..body.len().min(200)]))?;
        Ok(arr.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect())
    }
}

impl EmbedBackend for OllamaEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> StoreResult<Vec<f32>> {
        self.post(text).map_err(areev_core::error::AreevError::Storage)
    }
    fn model(&self) -> &str {
        &self.model
    }
}

// A deal corpus is not 10k uniform facts about 800 people. It is a few
// thousand deals, each with a structured extraction layer (the Facts a
// diligence pipeline pulls out of a CIM) and a much larger unstructured layer
// (memos, calls, IC notes). The ratio matters: the structured layer is what
// makes a pre-filter selective, and the unstructured layer is what makes the
// vector scan expensive.
const RELS: [&str; 10] = [
    "sector",
    "revenue",
    "ebitda",
    "sponsor",
    "stage",
    "geography",
    "entry_multiple",
    "ic_status",
    "owner_partner",
    "close_date",
];

const SECTORS: [&str; 8] = [
    "industrial services",
    "healthcare software",
    "specialty chemicals",
    "business process outsourcing",
    "aerospace components",
    "veterinary clinics",
    "facilities management",
    "payments infrastructure",
];

/// Short slugs for the hierarchical namespace `deal.<sector>.<id>`. A real
/// firm brain is not flat: a query is far more often "my healthcare deals"
/// than "all 2,000 deals", and a prefix scope can only express that if the
/// hierarchy is in the namespace to begin with.
const SECTOR_SLUG: [&str; 8] = [
    "industrial", "healthcare", "chemicals", "bpo", "aero", "vet", "facilities", "payments",
];

const MEMO: [&str; 10] = [
    "management presented a bridge from reported to adjusted EBITDA",
    "customer concentration remains the principal diligence risk",
    "the sponsor is running a tight process with a two-week exclusivity window",
    "quality of earnings flagged non-recurring items in the addback schedule",
    "recurring revenue mix improved following the platform migration",
    "the founder intends to roll over a meaningful minority stake",
    "working capital peg was negotiated against a twelve-month average",
    "site visit confirmed capacity headroom at the primary facility",
    "the add-on pipeline supports a buy-and-build thesis in adjacent regions",
    "lender feedback indicates unitranche appetite at the modelled leverage",
];

struct Args {
    deals: usize,
    per_deal: usize,
    dim: usize,
    k: usize,
    iters: usize,
    layout_flat: bool,
    db: Option<String>,
    postgres: bool,
    dump_topk: Option<String>,
    compare_topk: Option<String>,
    /// `m,ef_construction,ef_search` — build a pgvector HNSW index before
    /// measuring. Postgres only; the point of pairing it with
    /// `--compare-topk` is that an ANN index must be held to a recall number,
    /// not just a latency one.
    ann: Option<(usize, usize, usize)>,
    drop_ann: bool,
    /// Give every grain enough unique vocabulary that its vector is
    /// distinguishable from its neighbours'.
    ///
    /// The default corpus is built from a handful of memo templates crossed
    /// with a handful of sectors, which is realistic for *latency* (scan cost
    /// does not care what the vectors contain) but useless for **recall@k**:
    /// thousands of grains land at near-identical cosine distance from a
    /// templated query, so the exact top-10 is an arbitrary draw from a huge
    /// tie class and any ANN index "loses" most of it no matter how well it
    /// works. Symptom to recognise: recall that does not rise with
    /// `ef_search`. Under `--distinct` each grain carries tokens from a wide
    /// vocabulary, distances separate, and recall@k measures the index.
    distinct: bool,
    /// `model[@host:port]` — embed with a real model over Ollama instead of
    /// the synthetic hash embedder. The only way to get a recall@k number
    /// that means anything.
    ollama: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args {
        deals: 200,
        per_deal: 500,
        dim: 384,
        k: 10,
        iters: 200,
        layout_flat: false,
        db: None,
        postgres: false,
        dump_topk: None,
        compare_topk: None,
        ann: None,
        drop_ann: false,
        distinct: false,
        ollama: None,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    // Hand-rolled, per the workspace's no-clap policy. `val()` as a closure
    // would borrow `i` for the whole match; a fn taking it explicitly does not.
    fn val(argv: &[String], i: &mut usize) -> String {
        *i += 1;
        argv.get(*i).cloned().unwrap_or_else(|| panic!("missing value for {}", argv[*i - 1]))
    }
    while i < argv.len() {
        match argv[i].as_str() {
            "--deals" => a.deals = val(&argv, &mut i).parse().expect("--deals"),
            "--per-deal" => a.per_deal = val(&argv, &mut i).parse().expect("--per-deal"),
            "--dim" => a.dim = val(&argv, &mut i).parse().expect("--dim"),
            "--k" => a.k = val(&argv, &mut i).parse().expect("--k"),
            "--iters" => a.iters = val(&argv, &mut i).parse().expect("--iters"),
            "--layout" => a.layout_flat = val(&argv, &mut i) == "flat",
            "--db" => a.db = Some(val(&argv, &mut i)),
            "--postgres" => a.postgres = true,
            "--dump-topk" => a.dump_topk = Some(val(&argv, &mut i)),
            "--compare-topk" => a.compare_topk = Some(val(&argv, &mut i)),
            "--ann" => {
                let v = val(&argv, &mut i);
                let p: Vec<usize> =
                    v.split(',').map(|x| x.trim().parse().expect("--ann m,efc,efs")).collect();
                assert_eq!(p.len(), 3, "--ann takes m,ef_construction,ef_search");
                a.ann = Some((p[0], p[1], p[2]));
            }
            "--drop-ann" => a.drop_ann = true,
            "--distinct" => a.distinct = true,
            "--ollama" => a.ollama = Some(val(&argv, &mut i)),
            "-h" | "--help" => {
                println!(
                    "pe_scale [--deals N] [--per-deal N] [--dim N] [--k N] [--iters N]\n\
                              [--layout flat|partitioned] [--db PATH] [--postgres]\n\
                              [--dump-topk PATH] [--compare-topk PATH]\n\
                              [--ann m,ef_construction,ef_search] [--drop-ann] [--distinct]\n\
                              [--ollama MODEL[@HOST:PORT]]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown flag {other}"),
        }
        i += 1;
    }
    a
}

/// p50/p95/p99 in **milliseconds** — the class this harness lives in. The
/// shared `areev_bench::pct` reports µs, which is the embedded hot path's
/// unit, not a hundred-thousand-grain vector scan's.
fn row(mut ns: Vec<u128>, name: &str) -> f64 {
    ns.sort_unstable();
    let n = ns.len().max(1);
    let pick = |q: f64| ns[((n as f64 * q) as usize).min(n - 1)] as f64 / 1_000_000.0;
    let (p50, p95, p99) = (pick(0.5), pick(0.95), pick(0.99));
    println!("| {name} | {p50:.2} | {p95:.2} | {p99:.2} |");
    p50
}

fn main() {
    let mut a = parse_args();
    let total = a.deals * a.per_deal;
    let layout = if a.layout_flat { "flat" } else { "partitioned" };

    // Two instances of the same backend: the store owns one for the write
    // path, the harness keeps one to embed query text OUTSIDE the timer. With
    // a real model that separation is what keeps ~12ms of inference out of the
    // measured k-NN latency.
    let mk = |a: &Args| -> Box<dyn EmbedBackend> {
        match &a.ollama {
            Some(spec) => {
                let (model, addr) =
                    spec.split_once('@').unwrap_or((spec.as_str(), "127.0.0.1:11434"));
                Box::new(OllamaEmbed::connect(model, addr))
            }
            None => Box::new(HashEmbed { dim: a.dim }),
        }
    };
    let qembed = mk(&a);
    // The file's declared dim wins over the flag — a real model's dim is
    // whatever the model says, and reporting the flag would misdescribe it.
    a.dim = qembed.dim();

    let (mut m, where_) = open_store(&a);
    m.set_embedder(mk(&a));
    if a.ollama.is_some() {
        println!(
            "NOTE: the recall_hybrid row embeds its query INSIDE the store, so under --ollama \
             it carries model inference time. Read the nearest_vector rows, not that one."
        );
    }

    // Reuse a populated corpus: building 1M grains is minutes, and the whole
    // point of the flag is to re-query the same corpus (with and without an
    // index) without paying the load again.
    let existing = m.count().unwrap_or(0);
    let t0 = Instant::now();
    if existing >= total {
        println!("reusing corpus at {where_}: {existing} grains already present");
    } else {
        load(&mut m, &a);
        let secs = t0.elapsed().as_secs_f64();
        println!(
            "loaded {total} grains ({layout}, dim {}) in {secs:.1}s = {:.0} grains/s -> {where_}",
            a.dim,
            total as f64 / secs
        );
    }

    if a.drop_ann {
        match m.drop_vector_index() {
            Ok(()) => println!("ANN index dropped — vector recall is exact again"),
            Err(e) => println!("drop_vector_index: {e}"),
        }
    }
    if let Some((hm, efc, efs)) = a.ann {
        let t = Instant::now();
        match m.ensure_vector_index(hm, efc, efs) {
            Ok(()) => println!(
                "HNSW built (m={hm}, ef_construction={efc}, ef_search={efs}) in {:.1}s",
                t.elapsed().as_secs_f64()
            ),
            // Not fatal: STO-E007 on the embedded backend is the documented
            // answer, and printing it beside the exact-scan numbers is more
            // useful than aborting the run.
            Err(e) => println!("ensure_vector_index refused: {e}"),
        }
    }
    match m.vector_index() {
        Ok(Some(name)) => println!("vector index in use: {name} (results are APPROXIMATE)"),
        Ok(None) => println!("no vector index: results are exact"),
        Err(e) => println!("vector_index: {e}"),
    }

    println!();
    println!("| pe_scale · {total} grains · {} deals · {layout} · dim {} · k={} | p50 ms | p95 ms | p99 ms |", a.deals, a.dim, a.k);
    println!("|---|---|---|---|");

    let mut rng = Xorshift(7);
    let mut captured: Vec<String> = Vec::new();

    // ---- the vector leg, which is the whole question ----
    if a.layout_flat {
        // Unpartitioned exact scan over the full corpus. THE headline number:
        // every stored vector is touched, so this is where linear-in-corpus
        // shows up undisguised.
        let mut lat = Vec::with_capacity(a.iters);
        for i in 0..a.iters + 20 {
            let q = query_text(&mut rng, a.distinct);
            let qv = qembed.embed(&q).unwrap();
            let t = Instant::now();
            let out = m.nearest_vector("firm", None, None, &qv, a.k).expect("nearest_vector");
            if i >= 20 {
                lat.push(t.elapsed().as_nanos());
                if a.dump_topk.is_some() || a.compare_topk.is_some() {
                    captured.push(topk_line(&out));
                }
            }
            std::hint::black_box(&out);
        }
        row(lat, "vector k-NN, whole corpus (exact scan, no filter)");

        // Same k-NN behind a structural pre-filter. `nearest_vector`'s
        // subject arm adds `AND g.s = ?` to the same statement, so the planner
        // can cut the candidate set from the triple indexes before computing a
        // single distance. This is the "lean on structural filtering" lever,
        // measured rather than assumed.
        let mut lat = Vec::with_capacity(a.iters);
        for i in 0..a.iters + 20 {
            let subj = format!("deal{}", rng.next() as usize % a.deals);
            let q = query_text(&mut rng, a.distinct);
            let qv = qembed.embed(&q).unwrap();
            let t = Instant::now();
            let out =
                m.nearest_vector("firm", Some(&subj), None, &qv, a.k).expect("nearest_vector");
            if i >= 20 {
                lat.push(t.elapsed().as_nanos());
            }
            std::hint::black_box(&out);
        }
        row(lat, "vector k-NN, one deal pre-filtered (subject arm)");
    } else {
        // Partitioned: the same k-NN, but the namespace has already cut the
        // corpus to one deal before the scan starts.
        let mut lat = Vec::with_capacity(a.iters);
        for i in 0..a.iters + 20 {
            let d = rng.next() as usize % a.deals;
            let ns = format!("deal.{}.{d}", SECTOR_SLUG[d % SECTOR_SLUG.len()]);
            let q = query_text(&mut rng, a.distinct);
            let qv = qembed.embed(&q).unwrap();
            let t = Instant::now();
            let out = m.nearest_vector(&ns, None, None, &qv, a.k).expect("nearest_vector");
            if i >= 20 {
                lat.push(t.elapsed().as_nanos());
                if a.dump_topk.is_some() || a.compare_topk.is_some() {
                    captured.push(topk_line(&out));
                }
            }
            std::hint::black_box(&out);
        }
        row(lat, "vector k-NN, ONE deal (exact ns)");

        // One sector — roughly 1/8 of the corpus. The realistic cross-deal
        // question ("what do we know across healthcare?"), and the one a
        // hierarchical namespace exists to make cheap.
        let mut lat = Vec::with_capacity(a.iters);
        for i in 0..a.iters + 20 {
            let ns = format!("deal.{}.*", SECTOR_SLUG[rng.next() as usize % SECTOR_SLUG.len()]);
            let q = query_text(&mut rng, a.distinct);
            let qv = qembed.embed(&q).unwrap();
            let t = Instant::now();
            let out = m.nearest_vector(&ns, None, None, &qv, a.k).expect("sector prefix");
            if i >= 20 {
                lat.push(t.elapsed().as_nanos());
            }
            std::hint::black_box(&out);
        }
        row(lat, "vector k-NN, ONE SECTOR (deal.<sector>.* prefix)");

        // The whole tree, directly. Before prefix scoping reached
        // `nearest_vector` this query could not be spelled at all.
        let mut lat = Vec::with_capacity(a.iters);
        for i in 0..a.iters + 20 {
            let q = query_text(&mut rng, a.distinct);
            let qv = qembed.embed(&q).unwrap();
            let t = Instant::now();
            let out = m.nearest_vector("deal.*", None, None, &qv, a.k).expect("tree prefix");
            if i >= 20 {
                lat.push(t.elapsed().as_nanos());
            }
            std::hint::black_box(&out);
        }
        row(lat, "vector k-NN, ALL deals (deal.* prefix, direct)");

        // Cross-deal semantic search. `nearest_vector` is exact-namespace only
        // (`require_exact_ns`), so "find every deal that looks like this one"
        // has to go through prefix-scoped hybrid — whose vector leg inlines the
        // namespace ids and scans the union. Partitioning does NOT save this
        // query, which is the finding that matters for a firm-wide brain.
        let mut lat = Vec::with_capacity(a.iters);
        for i in 0..a.iters + 20 {
            let q = query_text(&mut rng, a.distinct);
            let t = Instant::now();
            let out =
                m.recall_hybrid("deal.*", None, None, Some(&q), a.k, None).expect("hybrid prefix");
            if i >= 20 {
                lat.push(t.elapsed().as_nanos());
            }
            std::hint::black_box(&out);
        }
        row(lat, "  ^ same question via recall_hybrid (the old only path)");
    }

    // ---- the legs that are already index-backed, for contrast ----
    let wide = |rng: &mut Xorshift, flat: bool, deals: usize| {
        if flat {
            "firm".to_string()
        } else {
            let d = rng.next() as usize % deals;
            format!("deal.{}.{d}", SECTOR_SLUG[d % SECTOR_SLUG.len()])
        }
    };

    let mut lat = Vec::with_capacity(a.iters);
    for i in 0..a.iters + 20 {
        let ns = wide(&mut rng, a.layout_flat, a.deals);
        let q = query_text(&mut rng, a.distinct);
        let t = Instant::now();
        let out = m.search_text(&ns, &q, a.k).expect("bm25");
        if i >= 20 {
            lat.push(t.elapsed().as_nanos());
        }
        std::hint::black_box(&out);
    }
    row(lat, "BM25 text search (posting-list index)");

    let mut lat = Vec::with_capacity(a.iters);
    for i in 0..a.iters + 20 {
        let ns = wide(&mut rng, a.layout_flat, a.deals);
        let subj = format!("deal{}", rng.next() as usize % a.deals);
        let t = Instant::now();
        let out = m.recall(&ns, &subj, None, 16).expect("recall");
        if i >= 20 {
            lat.push(t.elapsed().as_nanos());
        }
        std::hint::black_box(&out);
    }
    row(lat, "structural recall about a deal (triple index)");

    if let Some(path) = &a.dump_topk {
        std::fs::write(path, captured.join("\n")).expect("write topk");
        println!("\ntop-{} neighbour sets written to {path} ({} queries)", a.k, captured.len());
    }
    if let Some(path) = &a.compare_topk {
        let prior = std::fs::read_to_string(path).expect("read topk baseline");
        let base: Vec<&str> = prior.lines().collect();
        let n = base.len().min(captured.len());
        if n == 0 {
            println!("\nrecall@{}: no comparable queries in baseline", a.k);
        } else {
            let mut hit = 0usize;
            let mut want = 0usize;
            for i in 0..n {
                let b: Vec<&str> = base[i].split(',').collect();
                want += b.len();
                hit += b.iter().filter(|h| captured[i].split(',').any(|c| c == **h)).count();
            }
            println!(
                "\nrecall@{} vs baseline {path}: {:.4} ({hit}/{want} over {n} queries)",
                a.k,
                hit as f64 / want.max(1) as f64
            );
        }
    }
}

fn topk_line(out: &[(areev_core::Hash, f32)]) -> String {
    out.iter().map(|(h, _)| h.to_hex()).collect::<Vec<_>>().join(",")
}

/// Vocabulary wide enough that a 6-token draw is effectively unique. 5,000
/// terms choose(5000,6) ways is far more room than any corpus size here, so
/// two grains sharing four of six tokens is already vanishingly rare.
const VOCAB: usize = 5_000;

fn distinct_tokens(rng: &mut Xorshift, n: usize) -> String {
    (0..n).map(|_| format!("t{}", rng.next() as usize % VOCAB)).collect::<Vec<_>>().join(" ")
}

fn query_text(rng: &mut Xorshift, distinct: bool) -> String {
    let base = format!(
        "{} {}",
        MEMO[rng.next() as usize % MEMO.len()],
        SECTORS[rng.next() as usize % SECTORS.len()]
    );
    if distinct {
        format!("{base} {}", distinct_tokens(rng, 6))
    } else {
        base
    }
}

fn load(m: &mut Areev, a: &Args) {
    let mut rng = Xorshift(42);
    // One deal at a time so each batch lands in one namespace, and the
    // partitioned layout exercises the ns registry the way real ingest would.
    for d in 0..a.deals {
        let ns = if a.layout_flat {
            "firm".to_string()
        } else {
            format!("deal.{}.{d}", SECTOR_SLUG[d % SECTOR_SLUG.len()])
        };
        let subject = format!("deal{d}");
        let mut facts: Vec<Fact> = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        // ~1 structured extraction per 10 unstructured grains: a CIM yields a
        // few dozen fields and a few hundred pages of prose.
        for j in 0..a.per_deal {
            if j % 10 == 0 {
                let rel = RELS[rng.next() as usize % RELS.len()];
                let obj = format!(
                    "{} {}",
                    SECTORS[rng.next() as usize % SECTORS.len()],
                    rng.next() % 10_000
                );
                let mut f = Fact::new(&subject, rel, &obj).confidence(0.9);
                f.common.namespace = Some(ns.clone());
                facts.push(f);
            } else {
                let body = format!(
                    "{} — {} (deal {d}, note {j})",
                    MEMO[rng.next() as usize % MEMO.len()],
                    SECTORS[rng.next() as usize % SECTORS.len()]
                );
                let body = if a.distinct {
                    format!("{body} {}", distinct_tokens(&mut rng, 6))
                } else {
                    body
                };
                let mut e = Event::new(&body);
                e.subject = Some(subject.clone());
                e.common.namespace = Some(ns.clone());
                events.push(e);
            }
        }
        let mut refs: Vec<&dyn AddableDyn> = Vec::with_capacity(a.per_deal);
        for f in &facts {
            refs.push(f as &dyn AddableDyn);
        }
        for e in &events {
            refs.push(e as &dyn AddableDyn);
        }
        m.add_batch(&refs).expect("batch load");
        if d > 0 && d % 200 == 0 {
            println!("  … {d}/{} deals loaded", a.deals);
        }
    }
}

#[cfg(feature = "postgres")]
fn open_store(a: &Args) -> (Areev, String) {
    if a.postgres {
        let url = std::env::var("AREEV_PG_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .expect("set AREEV_PG_URL or DATABASE_URL for --postgres");
        // Stable name, not pid-suffixed: --db-style corpus reuse is the whole
        // point at these sizes. NOT `pg_…` (Postgres reserves that, 42939).
        let schema = a.db.clone().unwrap_or_else(|| "pe_scale".to_string());
        let m = Areev::open_postgres(&url, &schema).expect("open postgres");
        return (m, format!("postgres schema {schema}"));
    }
    open_embedded(a)
}

#[cfg(not(feature = "postgres"))]
fn open_store(a: &Args) -> (Areev, String) {
    assert!(!a.postgres, "--postgres needs `--features postgres`");
    open_embedded(a)
}

fn open_embedded(a: &Args) -> (Areev, String) {
    let path = a.db.clone().unwrap_or_else(|| {
        std::env::temp_dir().join("pe_scale.db").to_string_lossy().into_owned()
    });
    let m = Areev::open(&path).expect("open embedded store");
    (m, path)
}
