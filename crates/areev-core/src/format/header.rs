use sha2::{Digest, Sha256};

use crate::error::{AreevError, Hash, Result};
use crate::types::GrainType;

/// The 9-byte fixed header for .mg blobs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MgHeader {
    pub version: u8,
    pub flags: u8,
    pub grain_type: u8,
    pub ns_hash: u16,
    pub created_at_sec: u32,
}

impl MgHeader {
    /// Build header from grain metadata.
    pub fn new(grain_type: GrainType, namespace: Option<&str>, created_at_ms: i64) -> Self {
        let ns_hash = match namespace {
            Some(ns) if !ns.is_empty() => {
                let hash = Sha256::digest(ns.as_bytes());
                u16::from_be_bytes([hash[0], hash[1]])
            }
            _ => 0u16,
        };
        let epoch_secs = created_at_ms / 1000;
        let created_at_sec = if epoch_secs < 0 {
            0u32 // Clamp pre-epoch timestamps to zero
        } else {
            u32::try_from(epoch_secs).unwrap_or(u32::MAX) // Clamp overflow to max
        };

        MgHeader {
            version: 0x01,
            flags: 0x00,
            grain_type: grain_type.type_byte(),
            ns_hash,
            created_at_sec,
        }
    }

    /// Set the is_signed flag (bit 0 of the flags byte).
    pub fn set_is_signed(&mut self, signed: bool) {
        if signed {
            self.flags |= 0x01;
        } else {
            self.flags &= !0x01;
        }
    }

    /// Set flags for content_refs presence.
    pub fn set_has_content_refs(&mut self, has: bool) {
        if has {
            self.flags |= 0x08; // bit 3
        } else {
            self.flags &= !0x08;
        }
    }

    /// Set flags for embedding_refs presence.
    pub fn set_has_embedding_refs(&mut self, has: bool) {
        if has {
            self.flags |= 0x10; // bit 4
        } else {
            self.flags &= !0x10;
        }
    }

    /// Set AI-generated content flag (bit 5).
    pub fn set_ai_generated(&mut self, is_ai: bool) {
        if is_ai {
            self.flags |= 0x20; // bit 5
        } else {
            self.flags &= !0x20;
        }
    }

    /// Set sensitivity level (bits 6-7).
    pub fn set_sensitivity(&mut self, level: u8) {
        self.flags = (self.flags & 0x3F) | ((level & 0x03) << 6);
    }

    /// Serialize to 9 bytes.
    pub fn to_bytes(&self) -> [u8; 9] {
        let ns_bytes = self.ns_hash.to_be_bytes();
        let ts_bytes = self.created_at_sec.to_be_bytes();
        [
            self.version,
            self.flags,
            self.grain_type,
            ns_bytes[0],
            ns_bytes[1],
            ts_bytes[0],
            ts_bytes[1],
            ts_bytes[2],
            ts_bytes[3],
        ]
    }

    /// Parse from 9 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 9 {
            return Err(AreevError::Format("header must be at least 9 bytes".into()));
        }
        if bytes[0] != 0x01 {
            return Err(AreevError::Format(format!(
                "unsupported version: {:#x}",
                bytes[0]
            )));
        }
        Ok(MgHeader {
            version: bytes[0],
            flags: bytes[1],
            grain_type: bytes[2],
            ns_hash: u16::from_be_bytes([bytes[3], bytes[4]]),
            created_at_sec: u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]),
        })
    }

    pub fn is_signed(&self) -> bool {
        self.flags & 0x01 != 0
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags & 0x02 != 0
    }

    pub fn is_compressed(&self) -> bool {
        self.flags & 0x04 != 0
    }

    pub fn has_content_refs(&self) -> bool {
        self.flags & 0x08 != 0
    }

    pub fn has_embedding_refs(&self) -> bool {
        self.flags & 0x10 != 0
    }

    pub fn is_ai_generated(&self) -> bool {
        self.flags & 0x20 != 0
    }

    pub fn sensitivity(&self) -> u8 {
        (self.flags >> 6) & 0x03
    }
}

/// Compute SHA-256 content address of complete blob bytes.
pub fn content_address(blob: &[u8]) -> Hash {
    let digest: [u8; 32] = Sha256::digest(blob).into();
    Hash::from_bytes(&digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let header = MgHeader::new(GrainType::Fact, Some("shared"), 1768471200000);
        let bytes = header.to_bytes();
        let parsed = MgHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.version, 0x01);
        assert_eq!(parsed.flags, 0x00);
        assert_eq!(parsed.grain_type, 0x01);
        assert_eq!(parsed.ns_hash, header.ns_hash);
        assert_eq!(parsed.created_at_sec, 1768471200);
    }

    #[test]
    fn test_namespace_hash_shared() {
        // SHA-256("shared") first 2 bytes should produce 0xa4d2
        let header = MgHeader::new(GrainType::Fact, Some("shared"), 1768471200000);
        assert_eq!(header.ns_hash, 0xa4d2);
    }

    #[test]
    fn test_header_bytes_vector1() {
        // From OMS test vector 1: header should be: 01 00 01 a4 d2 69 68 ba a0
        let header = MgHeader::new(GrainType::Fact, Some("shared"), 1768471200000);
        let bytes = header.to_bytes();
        assert_eq!(
            bytes,
            [0x01, 0x00, 0x01, 0xa4, 0xd2, 0x69, 0x68, 0xba, 0xa0]
        );
    }
}
