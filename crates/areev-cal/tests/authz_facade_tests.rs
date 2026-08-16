//! The facade's principal binding: owner by default (zero behavior change
//! for every existing caller), fail-closed restricted when a principal is
//! bound, rights read from the file's own grant grains.

use areev_cal::AreevFacade;
use areev_core::authz::{Verb, AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    (m, dir)
}

#[test]
fn default_session_is_owner_and_allows_everything() {
    let (m, _dir) = open_mem();
    let facade = AreevFacade::new(m);
    let authz = facade.authz();
    assert!(authz.is_owner());
    for v in Verb::ALL {
        assert!(authz.check(v, "any-ns").is_ok());
    }
}

#[test]
fn bound_principal_gets_exactly_the_files_grants() {
    let (mut m, _dir) = open_mem();
    m.add(
        &Fact::new("agent:bot", REL_PERMITS, "read,write ON caller")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();

    let facade = AreevFacade::new(m).with_principal("agent:bot").unwrap();
    let authz = facade.authz();
    assert!(!authz.is_owner());
    assert_eq!(authz.principal(), "agent:bot");
    assert!(authz.check(Verb::Read, "caller").is_ok());
    assert!(authz.check(Verb::Write, "caller").is_ok());
    assert!(authz.check(Verb::Write, "shared").is_err());
    let err = authz.check(Verb::Delete, "caller").unwrap_err();
    assert_eq!(err.code(), "AUT-E001");
}

#[test]
fn unknown_principal_fails_closed() {
    let (m, _dir) = open_mem();
    let facade = AreevFacade::new(m).with_principal("agent:stranger").unwrap();
    for v in Verb::ALL {
        assert!(facade.authz().check(v, "caller").is_err(), "{v} must be refused");
    }
}
