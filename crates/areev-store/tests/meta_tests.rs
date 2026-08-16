//! File-carried declarations (meta table): settings travel with the file,
//! explicit opens re-stamp them, mismatches warn instead of silently
//! degrading indexes.

use areev_core::error::Result;
use areev_core::types::Fact;
use areev_store::{Areev, AreevOptions, EmbedBackend};
use tempfile::TempDir;

fn opts(index_text: bool) -> AreevOptions {
    AreevOptions { index_text, ..Default::default() }
}

struct FakeEmbed {
    name: &'static str,
    dim: usize,
}
impl EmbedBackend for FakeEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(vec![0.5; self.dim])
    }
    fn model(&self) -> &str {
        self.name
    }
}

#[test]
fn declarations_persist_across_bare_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();

    // create with text index off (voice/edge profile)
    {
        let mut m = Areev::open_with(path, opts(false)).unwrap();
        assert!(!m.index_text_enabled());
        assert!(m.open_warnings().is_empty(), "{:?}", m.open_warnings());
        m.add(&Fact::new("john", "prefers", "tea")).unwrap();
    }

    // a bare open honors the file's declaration — no host config needed
    {
        let m = Areev::open(path).unwrap();
        assert!(!m.index_text_enabled(), "file declared text_index=off");
        assert!(m.open_warnings().is_empty(), "{:?}", m.open_warnings());
    }
}

#[test]
fn explicit_open_restamps_and_warns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();

    Areev::open_with(path, opts(false)).unwrap();

    // deliberate change: explicit options win, but the change is loud
    {
        let m = Areev::open_with(path, opts(true)).unwrap();
        assert!(m.index_text_enabled());
        assert!(
            m.open_warnings().iter().any(|w| w.contains("text_index")),
            "{:?}",
            m.open_warnings()
        );
    }

    // ... and the file now declares the new setting
    {
        let m = Areev::open(path).unwrap();
        assert!(m.index_text_enabled());
        assert!(m.open_warnings().is_empty(), "{:?}", m.open_warnings());
    }
}

#[test]
fn embedding_provenance_recorded_and_mismatch_warns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();

    // first backend stamps provenance
    {
        let mut m = Areev::open(path).unwrap();
        m.set_embedder(Box::new(FakeEmbed { name: "fake-a", dim: 4 }));
        assert_eq!(m.declared_embedding(), Some(("fake-a", 4)));
        m.add(&Fact::new("john", "prefers", "tea")).unwrap();
    }

    // provenance survives reopen; a different-dim backend warns
    {
        let mut m = Areev::open(path).unwrap();
        assert_eq!(m.declared_embedding(), Some(("fake-a", 4)));
        m.set_embedder(Box::new(FakeEmbed { name: "fake-b", dim: 8 }));
        assert!(
            m.open_warnings().iter().any(|w| w.contains("mismatch")),
            "{:?}",
            m.open_warnings()
        );
    }
}
