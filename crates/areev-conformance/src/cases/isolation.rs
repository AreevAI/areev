//! Two memories in one process share nothing — the cross-schema half of
//! #181. On Postgres both memories' statements now travel through ONE pool,
//! so the same connection serves both in turn; every read path must still
//! answer only for the memory it was asked about.

use areev_core::error::Result;
use areev_store::EmbedBackend;

use crate::{fact, Backend};

/// Deterministic toy embedder (character histogram), so both memories embed
/// identically and a leak would surface as a hit.
struct Hist;
impl EmbedBackend for Hist {
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0f32; 16];
        for (i, b) in text.bytes().enumerate() {
            v[(b as usize + i) % 16] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        Ok(v.into_iter().map(|x| x / n).collect())
    }
    fn model(&self) -> &str {
        "hist-iso"
    }
}

pub fn memories_in_one_process_share_nothing(b: &dyn Backend) {
    let mut a = b.open_named("iso_a");
    let mut z = b.open_named("iso_z");
    a.set_embedder(Box::new(Hist));
    z.set_embedder(Box::new(Hist));

    let h = a.add(&fact("caller", "john", "prefers", "tea")).unwrap();
    let uri = a.put_blob(b"iso attachment").unwrap();

    // Every read path on Z answers empty for A's write — the same namespace,
    // the same subject, the same text, the same vector.
    assert_eq!(z.count().unwrap(), 0);
    assert!(!z.has(&h).unwrap());
    assert!(z.get(&h).is_err());
    assert!(z.recall("caller", "john", None, 8).unwrap().is_empty());
    assert!(z.latest("caller", "john", "prefers").unwrap().is_none());
    assert!(z.recall_hybrid("caller", None, None, Some("tea"), 8, None).unwrap().is_empty());
    assert!(z.nearest_semantic("caller", None, None, "john prefers tea", 4).unwrap().is_empty());
    assert!(z.get_blob(&uri).is_err());
    assert!(areev_store::read_blob_offline(&b.locator("iso_z"), &uri).is_err());
    assert!(z.changes_since(0, 100).unwrap().is_empty());

    // Z's own write, same key, stays its own — and A's head is untouched.
    let hz = z.add(&fact("caller", "john", "prefers", "coffee")).unwrap();
    assert!(!a.has(&hz).unwrap());
    assert_eq!(a.count().unwrap(), 1);
    assert_eq!(a.latest("caller", "john", "prefers").unwrap().unwrap().get_str("object"), Some("tea"));
    assert_eq!(z.latest("caller", "john", "prefers").unwrap().unwrap().get_str("object"), Some("coffee"));

    // Interleaved statements from both handles: the pooled transport may
    // serve them from one connection in turn, and each must still land in
    // its own memory only.
    for i in 0..12 {
        a.add(&fact("caller", &format!("s{i}"), "side", "a")).unwrap();
        z.add(&fact("caller", &format!("s{i}"), "side", "z")).unwrap();
    }
    assert_eq!(a.count().unwrap(), 13);
    assert_eq!(z.count().unwrap(), 13);
    for i in 0..12 {
        let s = format!("s{i}");
        assert_eq!(a.latest("caller", &s, "side").unwrap().unwrap().get_str("object"), Some("a"));
        assert_eq!(z.latest("caller", &s, "side").unwrap().unwrap().get_str("object"), Some("z"));
    }
    // Hybrid recall (text + vector legs) answers only from the memory asked:
    // A's hits include its "tea", Z's never do, and no hash crosses over.
    let a_hits = a.recall_hybrid("caller", None, None, Some("tea"), 8, None).unwrap();
    assert!(a_hits.iter().any(|g| g.get_str("object") == Some("tea")));
    let z_hits = z.recall_hybrid("caller", None, None, Some("tea"), 8, None).unwrap();
    assert!(z_hits.iter().all(|g| g.get_str("object") != Some("tea")), "{z_hits:?}");
    assert!(z_hits.iter().all(|g| a_hits.iter().all(|h| h.hash != g.hash)));
    let near = z.nearest_semantic("caller", None, None, "john prefers coffee", 1).unwrap();
    assert_eq!(z.get(&near[0].0).unwrap().get_str("object"), Some("coffee"));

    // A supersession in one memory does not move the other's head.
    let mut v2 = fact("caller", "john", "prefers", "oolong");
    a.supersede(&h, &mut v2).unwrap();
    assert_eq!(a.latest("caller", "john", "prefers").unwrap().unwrap().get_str("object"), Some("oolong"));
    assert_eq!(z.latest("caller", "john", "prefers").unwrap().unwrap().get_str("object"), Some("coffee"));

    // And nothing crossed over that only a reopen would reveal.
    drop(a);
    drop(z);
    let mut a = b.open_named("iso_a");
    let mut z = b.open_named("iso_z");
    assert_eq!(a.count().unwrap(), 14);
    assert_eq!(z.count().unwrap(), 13);
    assert!(z.get_blob(&uri).is_err());
    assert_eq!(a.get_blob(&uri).unwrap(), b"iso attachment");
}
