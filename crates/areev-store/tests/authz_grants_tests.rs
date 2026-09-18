//! Grant resolution bounds and the policy-change epoch (#309).

use areev_core::authz::{Grant, Verb, AUTHZ_NS, REL_PERMITS};
use areev_core::types::Fact;
use areev_store::Areev;
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let d = TempDir::new().unwrap();
    let m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

fn grant(m: &mut Areev, principal: &str, ns: &str) {
    let g = Grant { verbs: vec![Verb::Read], namespaces: vec![ns.to_string()] };
    let mut f = Fact::new(principal, REL_PERMITS, &g.to_object_string());
    f.common.namespace = Some(AUTHZ_NS.to_string());
    m.add(&f).unwrap();
}

#[test]
fn grants_past_the_cap_are_reported_not_silently_truncated() {
    // #309: 300 single-namespace grants used to resolve as the first 256,
    // and a read on the oldest-granted namespace was refused with AUT-E001
    // although its grant grain was live — fail-closed, but undiagnosable.
    let (mut m, _d) = open_mem();
    for i in 0..300 {
        grant(&mut m, "user:p", &format!("deal.n{i:03}"));
    }
    let err = m.authz_grants("user:p").unwrap_err();
    assert!(
        err.to_string().contains("more than 256 grant grains"),
        "the refusal must name the cap: {err}"
    );
    assert!(err.to_string().contains("Pack namespaces into fewer grants"));
}

#[test]
fn a_principal_at_the_cap_still_resolves() {
    let (mut m, _d) = open_mem();
    for i in 0..Areev::AUTHZ_GRANT_CAP {
        grant(&mut m, "user:q", &format!("deal.n{i:03}"));
    }
    let g = m.authz_grants("user:q").unwrap();
    assert_eq!(g.len(), Areev::AUTHZ_GRANT_CAP);
}

#[test]
fn one_grant_may_name_many_namespaces() {
    // The documented way past the cap: grant grains are the unit, not
    // namespaces.
    let (mut m, _d) = open_mem();
    let names: Vec<String> = (0..500).map(|i| format!("deal.n{i:03}")).collect();
    let g = Grant { verbs: vec![Verb::Read], namespaces: names };
    let mut f = Fact::new("user:r", REL_PERMITS, &g.to_object_string());
    f.common.namespace = Some(AUTHZ_NS.to_string());
    m.add(&f).unwrap();
    let got = m.authz_grants("user:r").unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].namespaces.len(), 500);
}

#[test]
fn authz_epoch_moves_only_for_authorization_writes() {
    let (mut m, _d) = open_mem();
    let e0 = m.authz_epoch().unwrap();

    // A write somewhere else must not move it.
    let mut f = Fact::new("alice", "prefers", "tea");
    f.common.namespace = Some("caller".into());
    m.add(&f).unwrap();
    assert_eq!(m.authz_epoch().unwrap(), e0, "an unrelated write is invisible");

    grant(&mut m, "user:p", "a");
    let e1 = m.authz_epoch().unwrap();
    assert_ne!(e1, e0, "a grant moves the epoch");

    grant(&mut m, "user:p", "b");
    let e2 = m.authz_epoch().unwrap();
    assert_ne!(e2, e1);

    // A forget in the namespace moves it too, even though it lowers MAX(seq).
    let heads = m.recall(AUTHZ_NS, "user:p", Some(REL_PERMITS), 10).unwrap();
    let h = heads[0].hash;
    m.forget(&h).unwrap();
    let e3 = m.authz_epoch().unwrap();
    assert_ne!(e3, e2, "a retraction must be observable");
    assert_ne!(e3, e1);
}

#[test]
fn authz_epoch_is_zero_on_a_memory_with_no_policy() {
    let (mut m, _d) = open_mem();
    assert_eq!(m.authz_epoch().unwrap(), 0);
}
