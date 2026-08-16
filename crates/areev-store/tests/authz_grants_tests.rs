//! The store's grant-grain reader: live `mg:permits` heads in the reserved
//! `agent:authz` namespace become a principal's grants; a revoke is a
//! retraction by supersession, so a revoked grant simply stops being a head.

use areev_core::authz::{Grant, Verb, AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    (m, dir)
}

fn grant_fact(principal: &str, object: &str, at: i64) -> Fact {
    Fact::new(principal, REL_PERMITS, object)
        .namespace(AUTHZ_NS)
        .created_at(at)
}

#[test]
fn live_grants_parse_and_scope_correctly() {
    let (mut m, _dir) = open_mem();
    m.add(&grant_fact("agent:bot", "read,write ON caller", 1_000))
        .unwrap();
    m.add(&grant_fact("agent:bot", "erase ON *", 2_000)).unwrap();
    // Another principal's grant must not bleed over.
    m.add(&grant_fact("user:anna", "admin ON *", 3_000)).unwrap();

    let grants = m.authz_grants("agent:bot").unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.iter().any(|g| g.covers(Verb::Read, "caller")));
    assert!(grants.iter().any(|g| g.covers(Verb::Erase, "anything")));
    assert!(!grants.iter().any(|g| g.covers(Verb::Admin, "caller")));

    assert_eq!(m.authz_grants("user:nobody").unwrap().len(), 0);
}

#[test]
fn a_revoked_grant_stops_being_a_head() {
    let (mut m, _dir) = open_mem();
    let h = m
        .add(&grant_fact("agent:bot", "delete ON caller", 1_000))
        .unwrap();
    assert_eq!(m.authz_grants("agent:bot").unwrap().len(), 1);

    // REVOKE = retraction by supersession (OMS 1.6 §12.6): a partial revoke
    // supersedes with the reduced grant; a full revoke retracts outright.
    let mut reduced = grant_fact("agent:bot", "read ON caller", 2_000);
    m.supersede(&h, &mut reduced).unwrap();

    let grants = m.authz_grants("agent:bot").unwrap();
    assert_eq!(grants, vec![Grant {
        verbs: vec![Verb::Read],
        namespaces: vec!["caller".to_string()],
    }]);
    assert!(!grants[0].covers(Verb::Delete, "caller"));
}

#[test]
fn malformed_grant_grains_are_skipped_not_granted() {
    let (mut m, _dir) = open_mem();
    m.add(&grant_fact("agent:bot", "not a grant string", 1_000))
        .unwrap();
    m.add(&grant_fact("agent:bot", "fly ON caller", 2_000)).unwrap();
    m.add(&grant_fact("agent:bot", "read ON caller", 3_000)).unwrap();

    // Under-granting is the safe failure: only the well-formed grant counts.
    let grants = m.authz_grants("agent:bot").unwrap();
    assert_eq!(grants.len(), 1);
    assert!(grants[0].covers(Verb::Read, "caller"));
}
