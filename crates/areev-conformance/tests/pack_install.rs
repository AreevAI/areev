//! `areev::pack` install over each backend (#341).
//!
//! The pack library is what the Node and Python bindings' `packInstall` /
//! `pack_install` call — the bindings only marshal — so this is the
//! backend-parameterized half of their contract: a shipped example pack
//! installs into a fresh memory under a NON-owner principal holding `write`
//! on the pack's namespace, at exactly the plan hash `validate_pack` (the
//! function `areev pack validate|install --format json` prints) reports; and
//! a principal without `write` gets `AUT-E001` with the memory untouched.
//!
//! It lives here rather than in a `cases/` function because the case list
//! is store-level (`areev-store` only) while this drives `areev::pack`, the
//! binary crate's library half, which only a dev-dependency can reach.

use std::path::{Path, PathBuf};

use areev::pack::{install_pack, validate_pack, InstallOptions, PackReport};
use areev_cal::AreevFacade;
use areev_conformance::{Backend, TursoBackend};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/agents")
        .join(name)
        .join("pack")
}

fn plans(r: &PackReport) -> Vec<String> {
    let mut v: Vec<String> = r
        .grains
        .iter()
        .filter(|g| g.grain_type == "workflow")
        .map(|g| g.hash.clone())
        .collect();
    v.sort();
    v
}

/// A memory whose only principal besides the owner is `principal`, holding
/// `grant` (e.g. `"write ON org.ops"`), opened as that principal.
fn as_principal(b: &dyn Backend, name: &str, principal: &str, grant: &str) -> AreevFacade {
    let mut m = b.open_named(name);
    m.add(&Fact::new(principal, REL_PERMITS, grant).namespace(AUTHZ_NS).created_at(1_000))
        .unwrap();
    AreevFacade::with_session(m, None, None).with_principal(principal).unwrap()
}

fn drive(b: &dyn Backend) {
    // A registry-carrying pack (saved queries + templates) and a
    // code-carrying one (a blob, a Definition naming it).
    for (i, pack) in ["invoice-to-accounting", "sanctions-screening"].into_iter().enumerate() {
        let dir = example(pack);
        let validated = validate_pack(&dir).expect("the shipped pack validates");
        let ns = validated.namespace.clone().expect("every shipped pack names its namespace");
        assert!(!plans(&validated).is_empty(), "{pack}: no plan");

        // Refused: `read` only. Nothing lands — no op, no registry row, no blob.
        let reader = as_principal(b, &format!("reader{i}"), "user:reader", &format!("read ON {ns}"));
        let before = reader.with_store(|s| s.head_op_seq()).unwrap();
        let err = install_pack(&reader, &dir, &InstallOptions::default()).unwrap_err();
        assert_eq!(err.code(), "AUT-E001", "{} {pack}: {err}", b.name());
        assert_eq!(
            reader.with_store(|s| s.head_op_seq()).unwrap(),
            before,
            "{} {pack}: a refused install moved the op-log",
            b.name()
        );
        for key in &validated.registry {
            assert!(
                reader.with_store(|s| s.meta_get(key)).unwrap().is_none(),
                "{} {pack}: {key} landed ahead of the refusal",
                b.name()
            );
        }
        for blob in &validated.blobs {
            assert!(
                reader.with_store(|s| s.get_blob(&blob.address)).is_err(),
                "{} {pack}: blob {} landed ahead of the refusal",
                b.name(),
                blob.name
            );
        }
        drop(reader);

        // Installed: `write` on the pack's namespace, pinned to its own code.
        let writer = as_principal(b, &format!("writer{i}"), "user:installer", &format!("write ON {ns}"));
        let opts = InstallOptions {
            executor_pins: validated
                .executors
                .iter()
                .map(|x| (x.tool.clone(), x.executor_uri.clone()))
                .collect(),
            ..Default::default()
        };
        let installed = install_pack(&writer, &dir, &opts)
            .unwrap_or_else(|e| panic!("{} {pack}: {e}", b.name()));
        assert_eq!(
            plans(&installed),
            plans(&validated),
            "{} {pack}: the installed plan differs from the validated one",
            b.name()
        );
        assert!(installed.executors.iter().all(|x| x.pinned), "{pack}: {:?}", installed.executors);
        for g in &installed.grains {
            let h = areev_core::error::Hash::from_hex(&g.hash).unwrap();
            assert!(writer.with_store(|s| s.has(&h)).unwrap(), "{pack}: {} missing", g.file);
        }
        for key in &validated.registry {
            assert!(writer.with_store(|s| s.meta_get(key)).unwrap().is_some(), "{pack}: {key}");
        }
    }
}

#[test]
fn pack_install_over_turso() {
    drive(&TursoBackend::new());
}

#[cfg(feature = "postgres")]
#[test]
fn pack_install_over_postgres() {
    let url = match std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
    {
        Some(u) => u,
        None => {
            if std::env::var("CI").as_deref() == Ok("true") {
                panic!("CI=true but no DATABASE_URL — the postgres job must not silently skip");
            }
            eprintln!("skipping: no DATABASE_URL/AREEV_PG_URL");
            return;
        }
    };
    drive(&areev_conformance::PgBackend::new(&url));
}
