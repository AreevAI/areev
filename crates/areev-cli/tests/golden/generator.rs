//! Golden dataset generator — builds a deterministic memory file and exports
//! it as a committed bundle (`golden.bundle`) plus a hash manifest
//! (`manifest.json`).
//!
//! Every grain uses fixed `created_at` timestamps, namespaces, and content,
//! so the same definitions produce the same SHA-256 content addresses on
//! every run, on every machine. If a regeneration ever yields different
//! hashes, canonical serialization has changed — that is a frozen-format
//! (OMS conformance) break, not a dataset problem.

use areev_core::types::{Event, Fact, Goal, Grain, Role};
use areev_store::Areev;
use std::path::Path;

/// Base epoch: 2026-01-15 00:00:00 UTC, in milliseconds.
pub const BASE_EPOCH_MS: i64 = 1_768_435_200_000;
const DAY_MS: i64 = 86_400_000;
const MIN_MS: i64 = 60_000;

/// One manifest row per grain the generator created.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    pub hash: String,
    pub gtype: String,
    pub ns: String,
    pub desc: String,
    pub superseded: bool,
    pub forgotten: bool,
}

#[derive(Debug)]
pub struct Manifest {
    pub schema: u32,
    pub base_epoch_ms: i64,
    /// Grain rows the generator created (includes superseded versions and
    /// the forgotten tombstone row).
    pub total_grains: usize,
    pub grains: Vec<ManifestEntry>,
}

impl Manifest {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": self.schema,
            "base_epoch_ms": self.base_epoch_ms,
            "total_grains": self.total_grains,
            "grains": self.grains.iter().map(|g| serde_json::json!({
                "hash": g.hash, "gtype": g.gtype, "ns": g.ns, "desc": g.desc,
                "superseded": g.superseded, "forgotten": g.forgotten,
            })).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &serde_json::Value) -> Manifest {
        let s = |x: &serde_json::Value, k: &str| x[k].as_str().unwrap_or_default().to_string();
        Manifest {
            schema: v["schema"].as_u64().unwrap_or(0) as u32,
            base_epoch_ms: v["base_epoch_ms"].as_i64().unwrap_or(0),
            total_grains: v["total_grains"].as_u64().unwrap_or(0) as usize,
            grains: v["grains"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|g| ManifestEntry {
                            hash: s(g, "hash"),
                            gtype: s(g, "gtype"),
                            ns: s(g, "ns"),
                            desc: s(g, "desc"),
                            superseded: g["superseded"].as_bool().unwrap_or(false),
                            forgotten: g["forgotten"].as_bool().unwrap_or(false),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Build the golden store in `work_dir`, export the bundle to `bundle_path`,
/// and return the manifest. Deterministic by construction: no `now()`
/// reaches any grain field.
pub fn generate(work_dir: &Path, bundle_path: &Path) -> Manifest {
    let db_path = work_dir.join("golden-src.db");
    let mut m = Areev::open(db_path.to_str().unwrap()).expect("open golden source store");
    let mut grains: Vec<ManifestEntry> = Vec::new();

    let fact = |m: &mut Areev,
                    grains: &mut Vec<ManifestEntry>,
                    ns: &str,
                    s: &str,
                    r: &str,
                    o: &str,
                    conf: f64,
                    ts: i64,
                    desc: &str|
     -> areev_core::error::Hash {
        let mut f = Fact::new(s, r, o).confidence(conf);
        f.common.namespace = Some(ns.to_string());
        f.common.created_at = Some(ts);
        let h = m.add(&f).expect(desc);
        grains.push(ManifestEntry {
            hash: h.to_hex(),
            gtype: "fact".into(),
            ns: ns.into(),
            desc: desc.into(),
            superseded: false,
            forgotten: false,
        });
        h
    };

    // -- 10 john facts (ns personal): entity-centric recall ------------------
    let john: [(&str, &str, f64, i64); 10] = [
        ("prefers", "coffee", 0.90, BASE_EPOCH_MS - DAY_MS),
        ("prefers", "window seat", 0.95, BASE_EPOCH_MS - 2 * DAY_MS),
        ("lives_in", "berlin", 1.0, BASE_EPOCH_MS - 3 * DAY_MS),
        ("speaks", "german", 1.0, BASE_EPOCH_MS - 4 * DAY_MS),
        ("speaks", "english", 0.90, BASE_EPOCH_MS - 5 * DAY_MS),
        ("works_at", "acme", 1.0, BASE_EPOCH_MS - 6 * DAY_MS),
        ("role", "engineer", 0.85, BASE_EPOCH_MS - 7 * DAY_MS),
        ("allergic_to", "peanuts", 1.0, BASE_EPOCH_MS - 8 * DAY_MS),
        ("birthday", "1990-03-15", 1.0, BASE_EPOCH_MS - 9 * DAY_MS),
        ("likes", "jazz", 0.80, BASE_EPOCH_MS - 10 * DAY_MS),
    ];
    for (r, o, c, ts) in john {
        fact(&mut m, &mut grains, "personal", "john", r, o, c, ts, &format!("john {r} {o}"));
    }

    // -- 8 bob facts (ns work): namespace isolation + cross-subject ----------
    let bob: [(&str, &str, f64, i64); 8] = [
        ("role", "manager", 1.0, BASE_EPOCH_MS - DAY_MS),
        ("works_at", "acme", 1.0, BASE_EPOCH_MS - 2 * DAY_MS),
        ("prefers", "tea", 0.90, BASE_EPOCH_MS - 3 * DAY_MS),
        ("lives_in", "munich", 1.0, BASE_EPOCH_MS - 4 * DAY_MS),
        ("reports_to", "carol", 1.0, BASE_EPOCH_MS - 5 * DAY_MS),
        ("likes", "hiking", 0.80, BASE_EPOCH_MS - 6 * DAY_MS),
        ("drinks", "espresso", 0.85, BASE_EPOCH_MS - 7 * DAY_MS),
        ("speaks", "english", 0.95, BASE_EPOCH_MS - 8 * DAY_MS),
    ];
    for (r, o, c, ts) in bob {
        fact(&mut m, &mut grains, "work", "bob", r, o, c, ts, &format!("bob {r} {o}"));
    }

    // -- 1 unicode fact (ns personal): NFC canonicalization anchor -----------
    // Stored in composed form; `golden_nfc_hash_equivalence` re-adds the
    // decomposed spelling elsewhere and must land on this exact hash.
    fact(
        &mut m,
        &mut grains,
        "personal",
        "ren\u{00e9}",
        "prefers",
        "caf\u{00e9} m\u{00fc}nchen",
        1.0,
        BASE_EPOCH_MS - 11 * DAY_MS,
        "unicode NFC anchor: rené prefers café münchen",
    );

    // -- 10 events (ns shared): 2 sessions x 5, unique BM25 tokens -----------
    for (si, session) in ["call-001", "call-002"].iter().enumerate() {
        for i in 0..5u32 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            let text = format!(
                "utterance {i} in {session}: golden-token-{}{i} about travel plans",
                ["alpha-", "beta-"][si]
            );
            let mut e = Event::new(&text);
            e.common.namespace = Some("shared".to_string());
            e.common.created_at =
                Some(BASE_EPOCH_MS - 12 * DAY_MS + (si as i64) * 60 * MIN_MS + (i as i64) * MIN_MS);
            e.session_id = Some(session.to_string());
            e.role = Role::from_str(role);
            let h = m.add(&e).expect("add event");
            grains.push(ManifestEntry {
                hash: h.to_hex(),
                gtype: "event".into(),
                ns: "shared".into(),
                desc: format!("event {i} {session} ({role})"),
                superseded: false,
                forgotten: false,
            });
        }
    }

    // -- 2 goals (ns work): a second non-triple grain type -------------------
    for (i, desc) in ["confirm flight booking", "renew acme contract"].iter().enumerate() {
        let mut g = Goal::new(desc);
        g.subject = Some("bob".to_string());
        g.common.namespace = Some("work".to_string());
        g.common.created_at = Some(BASE_EPOCH_MS - 13 * DAY_MS + (i as i64) * MIN_MS);
        let h = m.add(&g).expect("add goal");
        grains.push(ManifestEntry {
            hash: h.to_hex(),
            gtype: "goal".into(),
            ns: "work".into(),
            desc: format!("goal: {desc}"),
            superseded: false,
            forgotten: false,
        });
    }

    // -- supersession chain (ns personal): kim status v1 -> v2 -> v3 ---------
    let v1 = fact(
        &mut m, &mut grains, "personal", "kim", "status", "intern", 1.0,
        BASE_EPOCH_MS - 14 * DAY_MS, "kim status v1 (intern)",
    );
    let mut v2f = Fact::new("kim", "status", "junior").confidence(1.0);
    v2f.common.namespace = Some("personal".to_string());
    v2f.common.created_at = Some(BASE_EPOCH_MS - 7 * DAY_MS);
    let v2 = m.supersede(&v1, &mut v2f).expect("supersede v1->v2");
    grains.push(ManifestEntry {
        hash: v2.to_hex(),
        gtype: "fact".into(),
        ns: "personal".into(),
        desc: "kim status v2 (junior)".into(),
        superseded: false,
        forgotten: false,
    });
    let mut v3f = Fact::new("kim", "status", "senior").confidence(1.0);
    v3f.common.namespace = Some("personal".to_string());
    v3f.common.created_at = Some(BASE_EPOCH_MS - DAY_MS);
    let v3 = m.supersede(&v2, &mut v3f).expect("supersede v2->v3");
    grains.push(ManifestEntry {
        hash: v3.to_hex(),
        gtype: "fact".into(),
        ns: "personal".into(),
        desc: "kim status v3 (senior, head)".into(),
        superseded: false,
        forgotten: false,
    });
    // Mark the superseded versions in the manifest.
    for e in grains.iter_mut() {
        if e.hash == v1.to_hex() || e.hash == v2.to_hex() {
            e.superseded = true;
        }
    }

    // -- 4 WITH-option targets (ns personal) ----------------------------------
    // dave/erin both drink coffee -> `WITH dedup(object)` has a duplicate to
    // collapse; fay's matcha must survive. acme's industry fact gives the
    // entity graph a 2-hop path (john -> works_at -> acme -> industry -> ...).
    let with_targets: [(&str, &str, &str, f64, i64, &str); 4] = [
        ("dave", "drinks", "coffee", 0.80, BASE_EPOCH_MS - 16 * DAY_MS, "dedup target: dave drinks coffee"),
        ("erin", "drinks", "coffee", 0.70, BASE_EPOCH_MS - 16 * DAY_MS + MIN_MS, "dedup target: erin drinks coffee (duplicate object)"),
        ("fay", "drinks", "matcha", 0.90, BASE_EPOCH_MS - 16 * DAY_MS + 2 * MIN_MS, "dedup survivor: fay drinks matcha"),
        ("acme", "industry", "software", 1.0, BASE_EPOCH_MS - 17 * DAY_MS, "entity-graph hop: acme industry software"),
    ];
    for (s, r, o, c, ts, desc) in with_targets {
        fact(&mut m, &mut grains, "personal", s, r, o, c, ts, desc);
    }

    // -- 1 forgotten grain (ns work): tombstone survives the bundle ----------
    let doomed = fact(
        &mut m, &mut grains, "work", "classified", "project_code", "omega-shred", 1.0,
        BASE_EPOCH_MS - 15 * DAY_MS, "forgotten grain (tombstoned before export)",
    );
    m.forget(&doomed).expect("forget");
    grains.last_mut().unwrap().forgotten = true;

    // -- export ---------------------------------------------------------------
    m.bundle_since(0, bundle_path.to_str().unwrap()).expect("bundle export");

    Manifest {
        schema: 1,
        base_epoch_ms: BASE_EPOCH_MS,
        total_grains: grains.len(),
        grains,
    }
}
