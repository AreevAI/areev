//! encryption-at-rest: AES-256-GCM via Turso's page cipher, wired through
//! `AreevOptions::encryption_key` / `Areev::open_encrypted`.
//! Proves round-trip with the key, and that a wrong/absent key is denied —
//! the crypto-erasure property (destroy the key ⇒ the memory is unreadable).

use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions};

fn enc_opts(key: [u8; 32]) -> AreevOptions {
    AreevOptions { index_text: false, encryption_key: Some(key), ..Default::default() }
}

fn fact(s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some("main".into());
    f
}

/// db file + its -wal sibling, concatenated, for the plaintext-leak scan.
fn file_family_bytes(db: &std::path::Path) -> Vec<u8> {
    let mut out = std::fs::read(db).unwrap_or_default();
    let wal = db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy()));
    if let Ok(w) = std::fs::read(&wal) {
        out.extend_from_slice(&w);
    }
    out
}

#[test]
fn encryption_at_rest_roundtrip_and_key_denial() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("enc.db");
    let p = path.to_str().unwrap();
    let key = [7u8; 32];
    let marker = "SECRETMARKER-zzq-9931";

    // write encrypted
    {
        let mut m = Areev::open_with(p, enc_opts(key)).unwrap();
        m.add(&fact("alice", "medical_note", &format!("{marker}-penicillin-allergy"))).unwrap();
        assert_eq!(m.recall("main", "alice", None, 8).unwrap().len(), 1);
    }

    // the plaintext marker must NOT appear in the raw db/-wal bytes
    let raw = file_family_bytes(&path);
    let leaked = raw.windows(marker.len()).any(|w| w == marker.as_bytes());
    assert!(!leaked, "plaintext leaked in encrypted db file — cipher not effective");

    // reopen with the correct key → readable, content intact
    {
        let mut m = Areev::open_with(p, enc_opts(key)).unwrap();
        let r = m.recall("main", "alice", None, 8).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].get_str("object").unwrap().contains(marker));
    }

    // wrong key → denied
    assert!(Areev::open_with(p, enc_opts([9u8; 32])).is_err(), "wrong key must be rejected");
    // no key (bare open) → denied — destroy the key and the memory is gone
    assert!(Areev::open(p).is_err(), "opening an encrypted file without a key must fail");
}

#[test]
fn open_encrypted_convenience_roundtrips() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("enc2.db");
    let p = path.to_str().unwrap();
    let key = [42u8; 32];
    {
        let mut m = Areev::open_encrypted(p, key).unwrap();
        m.add(&fact("bob", "prefers", "window seat")).unwrap();
    }
    let mut m = Areev::open_encrypted(p, key).unwrap();
    assert_eq!(m.recall("main", "bob", None, 8).unwrap().len(), 1);
}

/// The `.blobs` CAS sidecar is encrypted under an HKDF-derived key when the
/// memory is (GDPR Art. 32). Attachments are the most sensitive payload a
/// memory carries, and they used to sit in the clear beside an encrypted
/// database.
#[test]
fn blob_sidecar_is_encrypted_at_rest() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let p = path.to_str().unwrap();
    let key = [42u8; 32];
    let secret = b"WELLUMBRIX-CONFIDENTIAL-AUDIO-PAYLOAD".to_vec();

    let uri = {
        let mut m = Areev::open_with(p, enc_opts(key)).unwrap();
        let uri = m.put_blob(&secret).unwrap();
        // Readable through the API with the key.
        assert_eq!(m.get_blob(&uri).unwrap(), secret);
        uri
    };

    // On disk, the plaintext must be absent from every sidecar file.
    let blobs_dir = std::path::PathBuf::from(format!("{p}.blobs"));
    let mut on_disk = Vec::new();
    for shard in std::fs::read_dir(&blobs_dir).unwrap().flatten() {
        for f in std::fs::read_dir(shard.path()).unwrap().flatten() {
            on_disk.extend(std::fs::read(f.path()).unwrap());
        }
    }
    assert!(!on_disk.is_empty(), "the attachment was written somewhere");
    assert!(
        !on_disk.windows(secret.len()).any(|w| w == secret.as_slice()),
        "attachment plaintext must not reach the sidecar"
    );

    // Reopening with the key still reads it; the wrong key does not.
    let mut m = Areev::open_with(p, enc_opts(key)).unwrap();
    assert_eq!(m.get_blob(&uri).unwrap(), secret);
    drop(m);
    // A different key derives a different blob key → the AEAD refuses.
    // (Opening the DB itself with a wrong key fails first on most engines,
    // so drive the sidecar directly through a same-key store whose blob was
    // written under another key.)
    let dir2 = tempfile::TempDir::new().unwrap();
    let p2 = dir2.path().join("n.db");
    let p2 = p2.to_str().unwrap();
    let mut other = Areev::open_with(p2, enc_opts([7u8; 32])).unwrap();
    let other_uri = other.put_blob(&secret).unwrap();
    let hex = other_uri.trim_start_matches("cas://sha256:");
    let src = std::path::PathBuf::from(format!("{p2}.blobs"))
        .join(&hex[..2])
        .join(&hex[2..]);
    let foreign = std::fs::read(&src).unwrap();
    drop(other);
    let dst = blobs_dir.join(&hex[..2]).join(&hex[2..]);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::write(&dst, &foreign).unwrap();
    let mut m = Areev::open_with(p, enc_opts(key)).unwrap();
    assert!(
        m.get_blob(&other_uri).is_err(),
        "a blob sealed under another memory's key must not decrypt here"
    );
}

/// Plaintext attachments written before sidecar encryption existed keep
/// reading, and `encrypt_blobs` migrates them — idempotently.
#[test]
fn legacy_plaintext_blobs_read_and_migrate() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let p = path.to_str().unwrap();
    let key = [9u8; 32];
    let legacy = b"legacy cleartext attachment".to_vec();

    // Simulate an older build: the DB is encrypted, the blob is not.
    let uri = {
        let mut m = Areev::open_with(p, enc_opts(key)).unwrap();
        let uri = m.put_blob(&legacy).unwrap();
        let hex = uri.trim_start_matches("cas://sha256:").to_string();
        drop(m);
        let f = std::path::PathBuf::from(format!("{p}.blobs"))
            .join(&hex[..2])
            .join(&hex[2..]);
        std::fs::write(&f, &legacy).unwrap(); // overwrite with cleartext
        uri
    };

    let mut m = Areev::open_with(p, enc_opts(key)).unwrap();
    assert_eq!(m.get_blob(&uri).unwrap(), legacy, "legacy blobs stay readable");

    let (encrypted, already) = m.encrypt_blobs().unwrap();
    assert_eq!((encrypted, already), (1, 0));
    assert_eq!(m.get_blob(&uri).unwrap(), legacy, "still readable after migration");
    // Idempotent: a second pass finds nothing left to do.
    assert_eq!(m.encrypt_blobs().unwrap(), (0, 1));

    // And the cleartext is gone from disk.
    let hex = uri.trim_start_matches("cas://sha256:");
    let f = std::path::PathBuf::from(format!("{p}.blobs")).join(&hex[..2]).join(&hex[2..]);
    let raw = std::fs::read(&f).unwrap();
    assert!(!raw.windows(legacy.len()).any(|w| w == legacy.as_slice()));
}

/// On a plaintext memory there is no key to seal under; saying "done"
/// would read as success.
#[test]
fn encrypt_blobs_refuses_on_a_plaintext_memory() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let mut m = Areev::open(path.to_str().unwrap()).unwrap();
    m.put_blob(b"clear").unwrap();
    let err = m.encrypt_blobs().unwrap_err().to_string();
    assert!(err.contains("encrypted memory"), "{err}");
}
