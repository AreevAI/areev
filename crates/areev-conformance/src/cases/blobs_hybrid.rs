//! CAS blob store and hybrid-recall legs — backend-generic: the embedded
//! backend keeps blobs in the `.blobs` fan-out dir and scans vectors with
//! `vector_distance_cos`; the postgres backend uses an in-schema `blobs`
//! table and pgvector. Same contract either way.

use crate::{fact, fact_at, Backend};
use areev_core::error::Result;
use areev_store::EmbedBackend;

pub fn cas_blob_roundtrip_and_gc(b: &dyn Backend) {
    let mut m = b.open();
    let uri = m.put_blob(b"hello media bytes").unwrap();
    assert!(uri.starts_with("cas://sha256:"));
    // idempotent put, verified get
    assert_eq!(m.put_blob(b"hello media bytes").unwrap(), uri);
    assert_eq!(m.get_blob(&uri).unwrap(), b"hello media bytes");
    // malformed / absent URIs are clean errors, never panics
    assert!(m.get_blob("cas://sha256:xyz").is_err());
    assert!(m.get_blob(&format!("cas://sha256:{}", "a".repeat(64))).is_err());
    // nothing references the blob -> gc reclaims exactly it
    let removed = m.gc_blobs().unwrap();
    assert_eq!(removed, 1, "unreferenced blob must be reclaimed");
    assert!(m.get_blob(&uri).is_err(), "reclaimed blob is gone");
}

/// `blob_len` reports a blob's plaintext size without reading its body
/// (#339) — the egress broker refuses an oversized `body_ref` upload on this
/// number before loading a byte. Same answer on both backends; an absent or
/// malformed address is a clean error.
pub fn blob_len_reports_size_without_reading(b: &dyn Backend) {
    let mut m = b.open();
    for bytes in [&b""[..], b"x", &[0x80u8; 70_000][..]] {
        let uri = m.put_blob(bytes).unwrap();
        assert_eq!(m.blob_len(&uri).unwrap(), bytes.len() as u64, "[{}]", b.name());
    }
    assert!(m.blob_len(&format!("cas://sha256:{}", "c".repeat(64))).is_err());
    assert!(m.blob_len("cas://sha256:xyz").is_err());
}

/// `read_blob_offline` reaches a blob by address without opening the memory,
/// on either backend (#202) — which is what lets a sandboxed tool read the
/// attachment its own run filed while that run still holds the memory.
pub fn blob_reads_without_opening_the_memory(b: &dyn Backend) {
    let uri = {
        let mut m = b.open_named("offline_blob");
        m.put_blob(b"attachment bytes").unwrap()
    };
    let locator = b.locator("offline_blob");
    let _held_open_throughout = b.open_named("offline_blob");

    let got = areev_store::read_blob_offline(&locator, &uri)
        .expect("a stored blob is readable by address")
        .expect("a plaintext blob is not sealed");
    assert_eq!(got, b"attachment bytes");

    let absent = format!("cas://sha256:{}", "b".repeat(64));
    assert!(areev_store::read_blob_offline(&locator, &absent).is_err());
    assert!(areev_store::read_blob_offline(&locator, "cas://sha256:zz").is_err());
}

pub fn bm25_leg_finds_text(b: &dyn Backend) {
    let mut m = b.open();
    m.add(&fact("caller", "john", "allergic_to", "peanuts")).unwrap();
    m.add(&fact("caller", "john", "prefers", "quiet rooms")).unwrap();
    m.add(&fact("caller", "mary", "prefers", "loud music")).unwrap();
    // single-token free-text query, no subject anchor
    let hits = m.recall_hybrid("caller", None, None, Some("peanuts"), 8, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get_str("object"), Some("peanuts"));
    // multi-token (exercises the batched postings path on networked backends)
    let hits = m.recall_hybrid("caller", None, None, Some("quiet rooms"), 8, None).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].get_str("object"), Some("quiet rooms"));
    // a query hitting nothing returns empty, not an error
    assert!(m.recall_hybrid("caller", None, None, Some("zebra"), 8, None).unwrap().is_empty());
    // heads-only: superseded text drops out of the live results
    let h = m.recall_hybrid("caller", None, None, Some("peanuts"), 8, None).unwrap();
    let h0 = h[0].hash;
    let mut v2 = fact("caller", "john", "allergic_to", "shellfish");
    m.supersede(&h0, &mut v2).unwrap();
    assert!(
        m.recall_hybrid("caller", None, None, Some("peanuts"), 8, None).unwrap().is_empty(),
        "superseded grain must not surface in a heads-only text search"
    );
}

/// Deterministic toy embedder: character histogram, normalized. Same text →
/// same vector on every backend, so ordering assertions are stable.
struct HistEmbed;
impl EmbedBackend for HistEmbed {
    fn dim(&self) -> usize {
        32
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0f32; 32];
        for (i, b) in text.bytes().enumerate() {
            v[(b as usize + i) % 32] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        Ok(v.into_iter().map(|x| x / n).collect())
    }
    fn model(&self) -> &str {
        "hist-test"
    }
}

pub fn vector_leg_roundtrip(b: &dyn Backend) {
    let mut m = b.open();
    m.set_embedder(Box::new(HistEmbed));
    assert!(
        !m.open_warnings().iter().any(|w| w.contains("vector recall disabled")),
        "embedder must install: {:?}",
        m.open_warnings()
    );
    m.add(&fact("caller", "john", "drinks", "espresso every morning")).unwrap();
    m.add(&fact("caller", "john", "eats", "toast with jam")).unwrap();
    // the vector leg answers a free-text query (identical text = identical
    // vector = exact nearest hit)
    let hits = m
        .recall_hybrid("caller", None, None, Some("espresso every morning"), 4, None)
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].get_str("object"), Some("espresso every morning"));
    // nearest_semantic returns scored hashes, best first. What gets embedded
    // is the PROJECTED text ("subject relation object"), so querying that
    // exact projection is the self-similarity ~1 case.
    let near = m
        .nearest_semantic("caller", None, None, "john drinks espresso every morning", 2)
        .unwrap();
    assert!(!near.is_empty());
    let top = m.get(&near[0].0).unwrap();
    assert_eq!(top.get_str("object"), Some("espresso every morning"));
    assert!(near[0].1 > 0.9, "self-similarity should be ~1, got {}", near[0].1);
    // vectors survive reopen (stored, not recomputed)
    drop(m);
    let mut m = b.open();
    m.set_embedder(Box::new(HistEmbed));
    let near = m
        .nearest_semantic("caller", None, None, "john eats toast with jam", 1)
        .unwrap();
    assert_eq!(m.get(&near[0].0).unwrap().get_str("object"), Some("toast with jam"));
}

/// Scored hybrid recall (decision-backend phase 0): a relevance score now
/// leaves the store, so its semantics must match across backends. The scored
/// call returns EXACTLY the unscored call's order, the best hit scores 1.0,
/// and fusion scores never increase down the list — over all three legs
/// (structural, BM25, vector), a prefix scope, and the resolved-list path.
pub fn scored_recall_matches_unscored_order(b: &dyn Backend) {
    use areev_store::RecallTuning;
    let mut m = b.open();
    m.set_embedder(Box::new(HistEmbed));
    let t0 = 1_700_000_000_000;
    m.add(&fact_at("org.a", "john", "drinks", "espresso every morning", t0)).unwrap();
    m.add(&fact_at("org.a", "john", "eats", "toast with jam and espresso", t0 + 1)).unwrap();
    m.add(&fact_at("org.a", "mary", "drinks", "green tea", t0 + 2)).unwrap();
    m.add(&fact_at("org.b", "john", "likes", "espresso martinis", t0 + 3)).unwrap();
    m.add(&fact_at("org.b", "zed", "reads", "novels", t0 + 4)).unwrap();

    let check = |plain: Vec<areev_core::format::deserialize::DeserializedGrain>,
                 scored: Vec<(areev_core::format::deserialize::DeserializedGrain, f32)>,
                 what: &str| {
        assert!(!scored.is_empty(), "[{}] {what}: no hits", b.name());
        assert_eq!(
            plain.iter().map(|g| g.hash).collect::<Vec<_>>(),
            scored.iter().map(|(g, _)| g.hash).collect::<Vec<_>>(),
            "[{}] {what}: scored order must equal unscored order",
            b.name()
        );
        let s: Vec<f32> = scored.iter().map(|(_, s)| *s).collect();
        assert_eq!(s[0], 1.0, "[{}] {what}: top hit scores exactly 1.0: {s:?}", b.name());
        assert!(s.windows(2).all(|w| w[0] >= w[1]), "[{}] {what}: non-increasing: {s:?}", b.name());
        assert!(s.iter().all(|x| *x > 0.0 && *x <= 1.0), "[{}] {what}: in (0,1]: {s:?}", b.name());
    };

    let d = RecallTuning::default();
    for (ns, subject, query) in [
        ("org.a", Some("john"), Some("espresso")),
        ("org.a", None, Some("espresso every morning")),
        ("org.a", Some("john"), None),
        ("org.*", None, Some("espresso")),
    ] {
        let plain = m.recall_hybrid_tuned(ns, subject, None, query, 8, None, d).unwrap();
        let scored = m.recall_hybrid_scored(ns, subject, None, query, 8, None, d).unwrap();
        check(plain, scored, &format!("{ns}/{subject:?}/{query:?}"));
    }
    let list = vec!["org.a".to_string(), "org.b".to_string()];
    let plain = m.recall_hybrid_scoped(&list, Some("john"), None, Some("espresso"), 8, None, d).unwrap();
    let scored =
        m.recall_hybrid_scoped_scored(&list, Some("john"), None, Some("espresso"), 8, None, d).unwrap();
    check(plain, scored, "resolved list");
}

/// `forget` must reclaim a tombstoned grain's CAS attachments when nothing
/// else references them — on the origin AND on a replica replaying the
/// tombstone. An erasure whose attachment bytes survive on the replica's
/// disk has not honored the right to erasure.
pub fn forget_reclaims_sole_referenced_blob(b: &dyn Backend) {
    let attach = |uri: &str| areev_core::types::ContentRef {
        uri: uri.to_string(),
        modality: Some("audio".to_string()),
        mime_type: Some("audio/wav".to_string()),
        size_bytes: None,
        checksum: None,
        metadata: None,
    };

    let mut a = b.open_named("blobf_a");
    let sole = a.put_blob(&[7u8; 512]).unwrap();
    let shared = a.put_blob(&[9u8; 64]).unwrap();
    let mut f1 = fact("ns", "alice", "said", "message one");
    f1.common.content_refs = vec![attach(&sole), attach(&shared)];
    let gone = a.add(&f1).unwrap();
    let mut f2 = fact("ns", "alice", "said", "message two");
    f2.common.content_refs = vec![attach(&shared)];
    a.add(&f2).unwrap();

    // Replicate both grains to a peer; CAS bytes travel out-of-band
    // (bundles carry grain blobs, not attachment bytes).
    let b1 = b.scratch().join("blobf_adds.mgb");
    let st = a.bundle_since(0, b1.to_str().unwrap()).unwrap();
    let mut peer = b.open_named("blobf_b");
    peer.put_blob(&[7u8; 512]).unwrap();
    peer.put_blob(&[9u8; 64]).unwrap();
    peer.import_bundle(b1.to_str().unwrap()).unwrap();

    // Origin: the sole-referenced attachment goes with its grain; the
    // shared one survives (the check is targeted, not a store-wide gc).
    a.forget(&gone).unwrap();
    assert!(
        a.get_blob(&sole).is_err(),
        "sole-referenced attachment must be reclaimed with its grain"
    );
    assert!(
        a.get_blob(&shared).is_ok(),
        "attachment still referenced by a live grain must survive"
    );

    // Replica: the tombstone replay must reach the blob sidecar too.
    let b2 = b.scratch().join("blobf_delta.mgb");
    a.bundle_since(st.last_op_seq, b2.to_str().unwrap()).unwrap();
    peer.import_bundle(b2.to_str().unwrap()).unwrap();
    assert!(peer.get(&gone).is_err(), "tombstone must replay on the peer");
    assert!(
        peer.get_blob(&sole).is_err(),
        "tombstone replay must reclaim the replica's sole-referenced blob"
    );
    assert!(peer.get_blob(&shared).is_ok());
}

/// The external seam with no embedder installed. On Postgres the `vector(dim)`
/// column is created lazily and only `set_embedder` reached that DDL, so this
/// route failed `42703` — and stamped provenance anyway.
pub fn external_vectors_need_no_embedder(b: &dyn Backend) {
    let mut m = b.open();
    assert!(m.declared_embedding().is_none(), "a fresh memory declares no vectors");

    let a = m.add(&fact("caller", "john", "drinks", "espresso")).unwrap();
    let c = m.add(&fact("caller", "john", "eats", "toast")).unwrap();

    let mut v1 = vec![0.0f32; 8];
    v1[0] = 1.0;
    let mut v2 = vec![0.0f32; 8];
    v2[1] = 1.0;
    m.set_grain_embedding(&a, &v1).unwrap();
    m.set_grain_embedding(&c, &v2).unwrap();

    let (model, dim) = m.declared_embedding().expect("a stored vector declares provenance");
    assert_eq!((model, dim), ("external", 8));

    let near = m.nearest_vector("caller", None, None, &v1, 2).unwrap();
    assert_eq!(near[0].0, a, "the matching vector must rank first");
    assert!(near[0].1 > 0.9, "self-similarity should be ~1, got {}", near[0].1);
}

/// #141: the bulk embedding write is one transaction on both backends —
/// every vector lands and reads back, a repeated hash keeps its last vector,
/// and a batch with one bad row writes NOTHING. Storage semantics, so it must
/// hold identically on the embedded engine and on Postgres, where the point
/// of the batch (round trips per chunk, not per vector) actually shows.
pub fn bulk_embeddings_land_atomically(b: &dyn Backend) {
    let mut m = b.open();
    let n = areev_store::EMBEDDING_BATCH_CHUNK + 5;
    let mut items = Vec::with_capacity(n);
    for i in 0..n {
        let h = m.add(&fact("caller", &format!("s{i}"), "has", "vector")).unwrap();
        let a = i as f32 * 0.7;
        items.push((h, vec![a.cos(), a.sin(), 0.0, 0.0]));
    }
    assert_eq!(m.set_grain_embeddings(&items).unwrap(), n);
    assert_eq!(m.declared_embedding(), Some(("external", 4)));
    for (h, v) in items.iter().step_by(41) {
        let near = m.nearest_vector("caller", None, None, v, 1).unwrap();
        assert_eq!(near[0].0, *h, "each vector is its own nearest neighbour");
    }

    // One unknown address refuses the whole batch, and nothing is written.
    let fresh = m.add(&fact("caller", "unvectored", "has", "nothing")).unwrap();
    let ghost = areev_core::error::Hash::from_hex(&"e".repeat(64)).unwrap();
    assert!(m.set_grain_embeddings(&[(fresh, vec![1.0, 0.0, 0.0, 0.0]), (ghost, vec![0.0, 1.0, 0.0, 0.0])]).is_err());
    let near = m.nearest_vector("caller", None, None, &[1.0, 0.0, 0.0, 0.0], 1).unwrap();
    assert_ne!(near[0].0, fresh, "the refused batch left no vector behind");
}

/// Provenance must not outlive a failed write.
pub fn a_refused_vector_declares_nothing(b: &dyn Backend) {
    let mut m = b.open();
    let a = m.add(&fact("caller", "john", "drinks", "espresso")).unwrap();
    m.set_grain_embedding(&a, &[0.5f32; 8]).unwrap();

    assert!(m.set_grain_embedding(&a, &[0.5f32; 9]).is_err(), "a dim change must be refused");
    assert_eq!(
        m.declared_embedding().map(|(x, d)| (x.to_string(), d)),
        Some(("external".to_string(), 8)),
        "a refused write must not move declared provenance"
    );
}


/// A decision-backend reranker (`DecisionRerank`, proposal §4 A1) over a
/// real `areev_llm` chain talking to an in-process fake `/v1/systemone`:
/// with `rerank: true` the scored recall follows the backend's relevance
/// levels (top = 1.0, min-max over the pool), and a backend answering 503
/// falls back to EXACTLY the fusion order and fusion scores — on both
/// backends. The fake's hit counter is the positive control: the fallback
/// is only meaningful because the reranker demonstrably ran.
pub fn decision_rerank_orders_by_the_backend_and_falls_back_on_failure(b: &dyn Backend) {
    use crate::systemone_fake::{score_candidates, FakeSystemOne, Reply};
    use areev_store::{DecisionRerank, RecallTuning};
    use std::time::Duration;

    let chain = |url: &str| {
        areev_llm::resolve_chain_with(
            Some(&format!("systemone:{url}#fake-jev")),
            None,
            Some(Duration::from_secs(5)),
            |_| None,
        )
        .expect("resolve")
        .expect("a chain")
    };
    let t0 = 1_700_000_000_000;
    let seed = |m: &mut areev_store::Areev| {
        m.add(&fact_at("drk", "john", "drinks", "espresso every morning", t0)).unwrap();
        m.add(&fact_at("drk", "john", "eats", "toast with jam and a small espresso on the side", t0 + 1)).unwrap();
        m.add(&fact_at("drk", "ann", "likes", "espresso martinis", t0 + 2)).unwrap();
        m.add(&fact_at("drk", "mary", "drinks", "green tea", t0 + 3)).unwrap();
    };
    let objects = |hits: &[(areev_core::format::deserialize::DeserializedGrain, f32)]| {
        hits.iter().map(|(g, _)| g.get_str("object").unwrap_or("").to_string()).collect::<Vec<_>>()
    };
    let fusion = RecallTuning::default();
    let rerank = RecallTuning { rerank: true, ..RecallTuning::default() };

    // --- the backend's order wins -------------------------------------------
    let fake = FakeSystemOne::start(|req| {
        Reply::ok(score_candidates(req, |t| {
            if t.contains("toast") {
                3
            } else if t.contains("martini") {
                2
            } else {
                0
            }
        }))
    });
    let mut m = b.open_named("decision_rerank_ok");
    seed(&mut m);
    let plain = m.recall_hybrid_scored("drk", None, None, Some("espresso"), 8, None, fusion).unwrap();
    m.set_reranker(Box::new(DecisionRerank::new(chain(&fake.url))));
    let hits = m.recall_hybrid_scored("drk", None, None, Some("espresso"), 8, None, rerank).unwrap();
    let got = objects(&hits);
    assert_eq!(
        &got[..2],
        ["toast with jam and a small espresso on the side", "espresso martinis"],
        "[{}] reranked order must follow the backend's levels: {got:?}",
        b.name()
    );
    assert_ne!(got, objects(&plain), "[{}] the fixture must make reranking visible", b.name());
    let s: Vec<f32> = hits.iter().map(|(_, s)| *s).collect();
    assert_eq!(s[0], 1.0, "[{}] top reranked hit is 1.0: {s:?}", b.name());
    assert!(s.windows(2).all(|w| w[0] >= w[1]), "[{}] {s:?}", b.name());
    assert_eq!(fake.hits(), 1, "[{}] one reranked recall = one decision request", b.name());
    let sent = &fake.bodies()[0];
    assert_eq!(sent["model"], "fake-jev");
    assert_eq!(sent["state"]["query"], "espresso");
    // The cache: the same recall again sends nothing.
    m.recall_hybrid_scored("drk", None, None, Some("espresso"), 8, None, rerank).unwrap();
    assert_eq!(fake.hits(), 1, "[{}] cached candidates are not re-sent", b.name());
    drop(m);

    // --- a failing backend falls back to fusion, exactly --------------------
    let down = FakeSystemOne::start(|_| Reply::status(503, r#"{"error":{"message":"overloaded"}}"#));
    let mut m = b.open_named("decision_rerank_503");
    seed(&mut m);
    let plain = m.recall_hybrid_scored("drk", None, None, Some("espresso"), 8, None, fusion).unwrap();
    m.set_reranker(Box::new(DecisionRerank::new(chain(&down.url))));
    let fell_back = m.recall_hybrid_scored("drk", None, None, Some("espresso"), 8, None, rerank).unwrap();
    assert!(down.hits() >= 1, "[{}] positive control: the reranker must have been asked", b.name());
    assert_eq!(
        fell_back.iter().map(|(g, s)| (g.hash, *s)).collect::<Vec<_>>(),
        plain.iter().map(|(g, s)| (g.hash, *s)).collect::<Vec<_>>(),
        "[{}] a 503 must yield the fusion order AND fusion scores",
        b.name()
    );

    // --- a structural-only recall never calls the reranker -------------------
    let before = down.hits();
    m.recall_hybrid_scored("drk", Some("john"), None, None, 8, None, rerank).unwrap();
    assert_eq!(down.hits(), before, "[{}] no query, no rerank request", b.name());
}
