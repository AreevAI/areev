//! #430: `grains_of_type_since` — the bounded read behind the Loop's gate and
//! analysis pass ("grains of type T created since the watermark"), on both
//! backends. It must return exactly the grains at or after `since`, page
//! completely by keyset cursor, honor `live_only` and a namespace scope, and
//! decode no blob older than `since`.

use crate::Backend;
use areev_core::types::{Fact, Grain, GrainType, Tool};

const T0: i64 = 1_700_000_000_000;

pub fn type_since_reads_only_what_is_new_and_pages_completely(b: &dyn Backend) {
    let mut m = b.open_named("type_since");
    for i in 0..30i64 {
        m.add(&Tool::new("crm.lookup").namespace("desk.a").content(&format!("old {i}")).created_at(T0 + i))
            .unwrap();
    }
    // Same type, other namespace; and a different type in the window.
    for i in 0..7i64 {
        m.add(&Tool::new("crm.lookup").namespace("desk.b").content(&format!("new b {i}")).created_at(T0 + 1_000 + i))
            .unwrap();
    }
    for i in 0..5i64 {
        m.add(&Tool::new("crm.lookup").namespace("desk.a").content(&format!("new a {i}")).created_at(T0 + 1_000 + i))
            .unwrap();
    }
    m.add(&Fact::new("acme", "tier", "gold").namespace("desk.a").created_at(T0 + 1_500)).unwrap();

    let since = Some(T0 + 1_000);
    let before = m.blobs_decoded();
    let mut got = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = m.grains_of_type_since(GrainType::Tool, None, since, cursor, 3, false).unwrap();
        assert!(page.grains.len() <= 3, "[{}] a page never exceeds its limit", b.name());
        got.extend(page.grains);
        pages += 1;
        match page.next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(got.len(), 12, "[{}] every new Tool grain, both namespaces, once", b.name());
    assert!(pages >= 4, "[{}] paged, not one read", b.name());
    let mut hashes: Vec<String> = got.iter().map(|g| g.hash.to_hex()).collect();
    hashes.sort();
    hashes.dedup();
    assert_eq!(hashes.len(), 12, "[{}] no grain repeated across pages", b.name());
    assert!(got.iter().all(|g| g.grain_type == GrainType::Tool), "[{}]", b.name());
    let created: Vec<i64> = got.iter().map(|g| g.get_i64("created_at").unwrap()).collect();
    assert!(created.windows(2).all(|w| w[0] <= w[1]), "[{}] created_at order", b.name());
    assert!(created.iter().all(|c| *c >= T0 + 1_000), "[{}] nothing older than since", b.name());
    assert_eq!(
        m.blobs_decoded() - before,
        12,
        "[{}] decoded exactly the new grains — none of the 30 older ones",
        b.name()
    );

    // Namespace: exact and prefix scope.
    let a = m.grains_of_type_since(GrainType::Tool, Some("desk.a"), since, None, 100, false).unwrap();
    assert_eq!(a.grains.len(), 5, "[{}] exact namespace", b.name());
    assert!(a.next.is_none(), "[{}] a short page is the last", b.name());
    let all = m.grains_of_type_since(GrainType::Tool, Some("desk.*"), since, None, 100, false).unwrap();
    assert_eq!(all.grains.len(), 12, "[{}] prefix scope", b.name());
    let none = m.grains_of_type_since(GrainType::Tool, Some("nobody"), since, None, 100, false).unwrap();
    assert!(none.grains.is_empty(), "[{}] unknown namespace is empty, not an error", b.name());

    // Unbounded: every grain of the type.
    let every = m.grains_of_type_since(GrainType::Tool, None, None, None, 1_000, false).unwrap();
    assert_eq!(every.grains.len(), 42, "[{}] since = None reads the whole type", b.name());

    // live_only drops what the index layer records as superseded.
    let target = a.grains[0].hash;
    m.supersede(
        &target,
        &mut Tool::new("crm.lookup").namespace("desk.a").content("revised").created_at(T0 + 2_000),
    )
    .unwrap();
    let live = m.grains_of_type_since(GrainType::Tool, Some("desk.a"), since, None, 100, true).unwrap();
    assert_eq!(live.grains.len(), 5, "[{}] 4 untouched + the revision", b.name());
    assert!(live.grains.iter().all(|g| g.hash != target), "[{}] superseded grain dropped", b.name());
    let with_history = m.grains_of_type_since(GrainType::Tool, Some("desk.a"), since, None, 100, false).unwrap();
    assert_eq!(with_history.grains.len(), 6, "[{}] live_only = false keeps it", b.name());
}
