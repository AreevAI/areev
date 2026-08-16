//! Areev Loop golden dataset generator — a deterministic memory file in which
//! every deterministic built-in analyzer has a deliberately seeded target,
//! exported as a committed bundle (`loop.bundle`) plus a hash manifest
//! (`loop-manifest.json`).
//!
//! Kept separate from the memory-stack golden dataset (`generator.rs`), which
//! is deliberately *clean* — zero loop findings — so the two suites don't
//! couple. Every timestamp is a fixed offset from the shared `BASE_EPOCH_MS`;
//! golden tests run the real `areev` binary with `AREEV_LOOP_NOW_MS` pinned to the
//! same epoch, so analyzer output (age_days, windows, severities, and — with
//! the substrate stamping recommendation grains from engine time — content
//! addresses) reproduces on any machine, on any day.
//!
//! Seed inventory (ns `agent`) → the 11 findings a first run stores:
//!
//! | Seed | Analyzer | Expected |
//! |---|---|---|
//! | acme tier ×3 (case variants, distinct times) | duplicate_sweep (+fork) | duplicate.exact low + fork.multi_head medium |
//! | 2 near-identical observations (Jaccard 11/12) | duplicate_sweep | duplicate.near, info |
//! | sam lives_in berlin + tokyo (both live) | contradiction_sweep (+fork) | contradiction.functional + fork.multi_head, medium ×2 |
//! | deploy region fork (eu-west-1 vs ap-south-1) | fork_surfacing + contradiction_sweep | fork.multi_head + contradiction, medium ×2 |
//! | 2 promos past valid_to (100d / 10d ago) | staleness | staleness.expired, medium + low (FORGET, destructive) |
//! | stripe_refund: 5 identical errors + 1 ok | tool_failure | tool_failure.cluster, high (lesson ADD + recurrence metric) |
//! | parse_invoices skill: practiced 12×, proficiency 0.25 | skill_stall | skill.stall, medium |
//! | ship-v2 goal: active, 0.05 progress, 45d old | goal_stagnation | none — default-off; pins the "disabled" skip |
//! | kai prefers dark-mode (3d old) | — | healthy contrast; stays finding-free |
//!
//! Note the fork multiplicity: the golden file is a bundle **import**, and
//! import UNIONs heads (that is what makes forks possible at all), so the
//! acme and sam seeds — plain repeated ADDs to one (subject, relation) that
//! the *source* store had collapsed to a single head — are genuine multi-head
//! groups in the imported file. `fork_surfacing` firing on them alongside the
//! engineered deploy fork is correct imported-file semantics, pinned
//! deliberately.
//!
//! The telemetry-fed trio (cold_grains / coverage_gap / budget_pressure) has
//! no committed seed: telemetry lives in a per-file sidecar the bundle never
//! carries. The primary suite pins their capability-skip ladder under
//! `--telemetry off`; the dedicated telemetry suite generates rollups live
//! through the real CLI.

use super::generator::{Manifest, ManifestEntry, BASE_EPOCH_MS};
use areev_core::types::{Fact, Goal, Grain, Observation, Skill, Tool};
use areev_store::Areev;
use std::path::Path;

const DAY_MS: i64 = 86_400_000;
const MIN_MS: i64 = 60_000;

/// The pinned "now" the golden suite runs the engine at (`AREEV_LOOP_NOW_MS`).
/// Same instant as the dataset's base epoch: seeds are backdated from it.
pub const AREEV_LOOP_NOW0_MS: i64 = BASE_EPOCH_MS;

/// Build the loop golden store in `work_dir`, export it to `bundle_path`,
/// and return the manifest. Deterministic by construction — no `now()`
/// reaches any grain field, and the fork is produced by a fixed bundle
/// exchange with a twin store.
pub fn generate_loop(work_dir: &Path, bundle_path: &Path) -> Manifest {
    let db_path = work_dir.join("loop-src.db");
    let mut m = Areev::open(db_path.to_str().unwrap()).expect("open loop source store");
    let mut grains: Vec<ManifestEntry> = Vec::new();

    let push = |grains: &mut Vec<ManifestEntry>, hash: String, gtype: &str, desc: &str| {
        grains.push(ManifestEntry {
            hash,
            gtype: gtype.into(),
            ns: "agent".into(),
            desc: desc.into(),
            superseded: false,
            forgotten: false,
        });
    };

    let fact = |m: &mut Areev, s: &str, r: &str, o: &str, ts: i64| -> areev_core::error::Hash {
        let mut f = Fact::new(s, r, o).confidence(0.9);
        f.common.namespace = Some("agent".to_string());
        f.common.created_at = Some(ts);
        m.add(&f).unwrap_or_else(|e| panic!("add {s} {r} {o}: {e}"))
    };

    // -- duplicate_sweep target: 3 exact duplicates (NFC + case-fold) --------
    // Distinct created_at → distinct content addresses; earliest is canonical.
    for (i, (o, ts)) in [
        ("Enterprise", AREEV_LOOP_NOW0_MS - 40 * DAY_MS),
        ("enterprise", AREEV_LOOP_NOW0_MS - 30 * DAY_MS),
        ("Enterprise", AREEV_LOOP_NOW0_MS - 20 * DAY_MS),
    ]
    .iter()
    .enumerate()
    {
        let h = fact(&mut m, "acme", "tier", o, *ts);
        push(&mut grains, h.to_hex(), "fact", &format!("dup target {}: acme tier {o}", i + 1));
    }

    // -- duplicate_sweep near-dup target: two observations, Jaccard 11/12 ----
    // The body rides in `extra_fields` (preserved verbatim through the .mg
    // round-trip), where the analyzer's body/content/text probe finds it.
    let obs_body = "user asked about pricing tiers refunds billing invoices during onboarding call";
    for (i, (body, ts)) in [
        (obs_body.to_string(), AREEV_LOOP_NOW0_MS - 15 * DAY_MS),
        (format!("{obs_body} please"), AREEV_LOOP_NOW0_MS - 15 * DAY_MS + MIN_MS),
    ]
    .iter()
    .enumerate()
    {
        let mut o = Observation::new("agent-1", "agent").subject("onboarding");
        o.common.namespace = Some("agent".to_string());
        o.common.created_at = Some(*ts);
        o.common.extra_fields.insert("body".into(), serde_json::json!(body));
        let h = m.add(&o).expect("add near-dup observation");
        push(&mut grains, h.to_hex(), "observation", &format!("near-dup observation {}", i + 1));
    }

    // -- contradiction_sweep target: two live values under functional lives_in
    let h = fact(&mut m, "sam", "lives_in", "berlin", AREEV_LOOP_NOW0_MS - 10 * DAY_MS);
    push(&mut grains, h.to_hex(), "fact", "contradiction older: sam lives_in berlin");
    let h = fact(&mut m, "sam", "lives_in", "tokyo", AREEV_LOOP_NOW0_MS - 5 * DAY_MS);
    push(&mut grains, h.to_hex(), "fact", "contradiction newer: sam lives_in tokyo");

    // -- staleness targets: declared valid_to elapsed (100d → medium, 10d → low)
    for (s, o, expired_days, desc) in [
        ("promo-black-friday", "BF25", 100, "stale 100d: promo-black-friday"),
        ("promo-spring", "SPRING10", 10, "stale 10d: promo-spring"),
    ] {
        let mut f = Fact::new(s, "discount_code", o)
            .confidence(1.0)
            .valid_to(AREEV_LOOP_NOW0_MS - expired_days * DAY_MS);
        f.common.namespace = Some("agent".to_string());
        f.common.created_at = Some(AREEV_LOOP_NOW0_MS - 120 * DAY_MS);
        let h = m.add(&f).expect("add stale promo");
        push(&mut grains, h.to_hex(), "fact", desc);
    }

    // -- tool_failure target: 5 identical failures + 1 success (rate 5/6) ----
    for i in 0..6u32 {
        let failed = i < 5;
        let mut t = Tool::new("stripe_refund")
            .content(if failed { "rate limited 429 retry later" } else { "refund ok" })
            .is_error(failed);
        t.common.namespace = Some("agent".to_string());
        t.common.created_at = Some(AREEV_LOOP_NOW0_MS - 2 * DAY_MS + (i as i64) * 5 * MIN_MS);
        let h = m.add(&t).expect("add tool call");
        push(
            &mut grains,
            h.to_hex(),
            "tool",
            &format!("stripe_refund call {} ({})", i + 1, if failed { "error" } else { "ok" }),
        );
    }

    // -- skill_stall target: practiced 12×, proficiency stuck at 0.25 --------
    // Proficiency aliases common.confidence (core D3); practice_count is real.
    let mut s = Skill::new("parse_invoices", "extract fields from supplier invoices");
    s.practice_count = Some(12);
    s.common.confidence = 0.25;
    s.common.namespace = Some("agent".to_string());
    s.common.created_at = Some(AREEV_LOOP_NOW0_MS - 20 * DAY_MS);
    let h = m.add(&s).expect("add stalled skill");
    push(&mut grains, h.to_hex(), "skill", "stalled skill: parse_invoices");

    // -- goal_stagnation seed (analyzer ships default-OFF) -------------------
    // Present so the "disabled" skip is honest — a real target exists, and
    // flipping the default without re-blessing the goldens fails loudly.
    let mut g = Goal::new("ship v2 billing");
    g.subject = Some("ship-v2".to_string());
    g.progress = Some(0.05);
    g.common.namespace = Some("agent".to_string());
    g.common.created_at = Some(AREEV_LOOP_NOW0_MS - 45 * DAY_MS);
    let h = m.add(&g).expect("add stalled goal");
    push(&mut grains, h.to_hex(), "goal", "stalled goal seed: ship-v2 (analyzer default-off)");

    // -- healthy contrast: young, single-valued, non-functional --------------
    let h = fact(&mut m, "kai", "prefers", "dark-mode", AREEV_LOOP_NOW0_MS - 3 * DAY_MS);
    push(&mut grains, h.to_hex(), "fact", "healthy contrast: kai prefers dark-mode");

    // -- fork_surfacing target: divergent supersessions of one base grain ----
    // A twin store imports the base, supersedes it differently, and its bundle
    // is imported back: the store keeps both tips (`apply_supersede_flip`).
    // `region` is also a seeded functional relation, so the same entity
    // deliberately yields a contradiction finding too — one entity, two
    // families, two recommendations.
    let base = fact(&mut m, "deploy", "region", "us-east-1", AREEV_LOOP_NOW0_MS - 30 * DAY_MS);
    push(&mut grains, base.to_hex(), "fact", "fork base: deploy region us-east-1");
    grains.last_mut().unwrap().superseded = true;
    let base_bundle = work_dir.join("loop-base.bundle");
    m.bundle_since(0, base_bundle.to_str().unwrap()).expect("export base bundle");

    // Twin: import the base, supersede to ap-south-1 (T-8d).
    let twin_path = work_dir.join("loop-twin.db");
    let mut twin = Areev::open(twin_path.to_str().unwrap()).expect("open twin store");
    twin.import_bundle(base_bundle.to_str().unwrap()).expect("twin imports base");
    let mut ap = Fact::new("deploy", "region", "ap-south-1").confidence(0.9);
    ap.common.namespace = Some("agent".to_string());
    ap.common.created_at = Some(AREEV_LOOP_NOW0_MS - 8 * DAY_MS);
    let ap_hash = twin.supersede(&base, &mut ap).expect("twin supersede");
    let fork_bundle = work_dir.join("loop-fork.bundle");
    twin.bundle_since(0, fork_bundle.to_str().unwrap()).expect("export fork bundle");

    // Main: supersede to eu-west-1 FIRST (T-9d), then import the twin's
    // divergent supersession — the flip keeps both tips.
    let mut eu = Fact::new("deploy", "region", "eu-west-1").confidence(0.9);
    eu.common.namespace = Some("agent".to_string());
    eu.common.created_at = Some(AREEV_LOOP_NOW0_MS - 9 * DAY_MS);
    let eu_hash = m.supersede(&base, &mut eu).expect("main supersede");
    push(&mut grains, eu_hash.to_hex(), "fact", "fork head A: deploy region eu-west-1");
    m.import_bundle(fork_bundle.to_str().unwrap()).expect("import divergent supersession");
    push(&mut grains, ap_hash.to_hex(), "fact", "fork head B: deploy region ap-south-1 (imported)");

    // In the SOURCE store only the engineered divergent supersession is a
    // fork (local adds collapse heads). The imported golden file additionally
    // forks the acme/sam multi-ADD seeds — import UNIONs heads (see the
    // module docs).
    let forks = m.open_forks().expect("open_forks");
    assert_eq!(forks.len(), 1, "expected exactly the deploy/region fork, got {forks:?}");
    assert_eq!(forks[0].heads.len(), 2, "expected 2 competing heads: {forks:?}");

    // -- export ---------------------------------------------------------------
    m.bundle_since(0, bundle_path.to_str().unwrap()).expect("loop bundle export");

    Manifest {
        schema: 1,
        base_epoch_ms: BASE_EPOCH_MS,
        total_grains: grains.len(),
        grains,
    }
}
