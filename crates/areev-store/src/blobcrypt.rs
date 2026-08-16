//! CAS sidecar encryption (GDPR Art. 32).
//!
//! The memory database is encrypted by Turso's page cipher; the `.blobs`
//! sidecar sits outside it, so attachments — the most sensitive payloads a
//! memory carries (audio, images, documents) — were written in the clear
//! next to an encrypted file. This module closes that gap.
//!
//! **Key.** The page-cipher key is deliberately not retained on the handle
//! (`open_internal` zeroizes it once the engine has ingested it). Rather
//! than keep a second copy of it, we derive a *separate* blob key with
//! HKDF-SHA256 under a domain-separation string, and retain only that. So
//! the blob key cannot be used to open the database, and destroying the
//! page key still destroys the blobs (they are unrecoverable without the
//! derivation input) — crypto-erasure keeps working.
//!
//! **Content addressing is over PLAINTEXT.** The `cas://sha256:` address and
//! the filename stay the digest of the clear bytes: addresses must be stable
//! across encrypted and plaintext stores, or a bundle written by one could
//! not be read by the other and identical attachments would stop deduping.
//! The read path decrypts, then verifies the digest — so tampering is still
//! caught, now by two independent mechanisms (the AEAD tag and the content
//! address). The documented cost is that a filename is a content-equality
//! oracle: someone holding a candidate file can tell whether this memory
//! stores it. Acceptable for a local sidecar, stated in docs/gdpr.md.
//!
//! **Format.** `MGB-BLOB1` magic, 12-byte random nonce, then the AES-256-GCM
//! ciphertext (tag included). The magic is what lets one directory hold both
//! plaintext blobs written by older builds and encrypted ones, so migration
//! is incremental and a half-migrated directory still reads.

use crate::{AreevError, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use zeroize::Zeroizing;

/// File magic for an encrypted blob. Chosen to be distinguishable from any
/// plausible plaintext prefix at a fixed offset.
pub(crate) const BLOB_MAGIC: &[u8; 9] = b"MGB-BLOB1";
const NONCE_LEN: usize = 12;

/// Domain separation for the blob key. Changing this string re-keys every
/// blob and is therefore a format break — never edit it in place; add a v2
/// and migrate.
const HKDF_INFO: &[u8] = b"areev.blobs.v1";

/// Derive the blob key from the page-cipher key. Separate key, same secret:
/// the derivation is one-way, so a leaked blob key does not open the
/// database.
pub(crate) fn derive_blob_key(page_key: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, page_key);
    let mut out = Zeroizing::new([0u8; 32]);
    // Only fails for absurd output lengths; 32 bytes is always valid.
    hk.expand(HKDF_INFO, out.as_mut())
        .expect("HKDF expand of 32 bytes cannot fail");
    out
}

/// Encrypt one blob. The content address is bound in as associated data, so
/// a ciphertext cannot be moved to a different address without detection.
pub(crate) fn seal(key: &[u8; 32], hex_addr: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(key.into());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| AreevError::CryptoError(format!("blob nonce: {e}")))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, Payload { msg: plaintext, aad: hex_addr.as_bytes() })
        .map_err(|_| AreevError::CryptoError("blob encryption failed".into()))?;
    let mut out = Vec::with_capacity(BLOB_MAGIC.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(BLOB_MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// True if these bytes are an encrypted blob (rather than a plaintext one
/// written before this landed).
pub(crate) fn is_sealed(bytes: &[u8]) -> bool {
    bytes.len() >= BLOB_MAGIC.len() + NONCE_LEN && &bytes[..BLOB_MAGIC.len()] == BLOB_MAGIC
}

/// Decrypt one blob previously produced by [`seal`].
pub(crate) fn open(key: &[u8; 32], hex_addr: &str, sealed: &[u8]) -> Result<Vec<u8>> {
    if !is_sealed(sealed) {
        return Err(AreevError::CryptoError("not an encrypted blob".into()));
    }
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Nonce::from_slice(&sealed[BLOB_MAGIC.len()..BLOB_MAGIC.len() + NONCE_LEN]);
    let ct = &sealed[BLOB_MAGIC.len() + NONCE_LEN..];
    cipher
        .decrypt(nonce, Payload { msg: ct, aad: hex_addr.as_bytes() })
        .map_err(|_| {
            AreevError::CryptoError(
                "blob decryption failed — wrong key, or the blob was tampered with".into(),
            )
        })
}

/// Read a blob from disk, decrypting when it is sealed and this handle holds
/// a key. A plaintext blob under an encrypted memory is returned as-is (an
/// older build wrote it) — `areev blobs encrypt` migrates those.
pub(crate) fn read_maybe_sealed(
    key: Option<&[u8; 32]>,
    hex_addr: &str,
    raw: Vec<u8>,
) -> Result<Vec<u8>> {
    if !is_sealed(&raw) {
        return Ok(raw);
    }
    match key {
        Some(k) => open(k, hex_addr, &raw),
        None => Err(AreevError::CryptoError(
            "this attachment is encrypted; reopen the memory with its key".into(),
        )),
    }
}

/// Write bytes for storage: sealed when a key is present, clear otherwise.
pub(crate) fn write_bytes(
    key: Option<&[u8; 32]>,
    hex_addr: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    match key {
        Some(k) => seal(k, hex_addr, plaintext),
        None => Ok(plaintext.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn roundtrip_and_key_separation() {
        let page = [7u8; 32];
        let k = derive_blob_key(&page);
        // The blob key is NOT the page key — a leaked blob key must not open
        // the database.
        assert_ne!(k.as_ref(), &page);
        // Derivation is deterministic, so reopening the same memory reads
        // its own blobs.
        assert_eq!(derive_blob_key(&page).as_ref(), k.as_ref());
        assert_ne!(derive_blob_key(&[8u8; 32]).as_ref(), k.as_ref());

        let sealed = seal(&k, ADDR, b"secret audio bytes").unwrap();
        assert!(is_sealed(&sealed));
        assert!(
            !sealed.windows(6).any(|w| w == b"secret"),
            "plaintext must not survive in the sealed bytes"
        );
        assert_eq!(open(&k, ADDR, &sealed).unwrap(), b"secret audio bytes");
    }

    #[test]
    fn wrong_key_address_or_tamper_is_refused() {
        let k = derive_blob_key(&[1u8; 32]);
        let other = derive_blob_key(&[2u8; 32]);
        let sealed = seal(&k, ADDR, b"payload").unwrap();

        assert!(open(&other, ADDR, &sealed).is_err(), "wrong key");
        let moved = ADDR.replace("0123", "9999");
        assert!(open(&k, &moved, &sealed).is_err(), "address is bound as AAD");

        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(open(&k, ADDR, &tampered).is_err(), "AEAD tag catches tampering");
    }

    #[test]
    fn plaintext_blobs_still_read_and_short_input_is_not_sealed() {
        assert!(!is_sealed(b"short"));
        assert!(!is_sealed(b"an ordinary plaintext attachment"));
        let k = derive_blob_key(&[3u8; 32]);
        // A blob written before encryption landed reads back untouched.
        let plain = b"legacy attachment".to_vec();
        assert_eq!(read_maybe_sealed(Some(&k), ADDR, plain.clone()).unwrap(), plain);
        // A sealed blob without a key is a clean error, never a panic.
        let sealed = seal(&k, ADDR, b"x").unwrap();
        assert!(read_maybe_sealed(None, ADDR, sealed).is_err());
    }
}
