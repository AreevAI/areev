//! LR-13 — Anthropic memory-tool backend adapter.
//!
//! Implements the client-side operation contract of Anthropic's memory
//! tool ("you implement the storage backend"): `view`, `create`,
//! `str_replace`, `insert`, `delete`, `rename` over a `/memories`
//! path space — but backed by grains instead of naive files, so every
//! edit is a supersession (full history), reads are `entity_latest`
//! point lookups, provenance links survive renames, and crypto-erasure
//! applies to the agent's memory directory.
//!
//! Mapping: one file = one supersession chain of Fact grains with
//! `subject = <path>`, `relation = "memory_file"`, `object = <content
//! digest>` (kept short so the dictionary never stores file bodies),
//! and the body in the grain's `context.content` field.

use areev_core::error::{Hash, AreevError, Result};
use areev_core::types::Fact;
use serde_json::{json, Value};

use crate::Areev;

pub const MEMORY_FILE_RELATION: &str = "memory_file";
const ROOT: &str = "/memories";

pub struct MemoryTool<'a> {
    m: &'a mut Areev,
    ns: String,
}

impl<'a> MemoryTool<'a> {
    pub fn new(m: &'a mut Areev, ns: &str) -> Self {
        MemoryTool { m, ns: ns.to_string() }
    }

    /// Dispatch one memory-tool command object (the shape Claude sends):
    /// `{"command": "view", "path": "/memories", ...}` → result text.
    pub fn execute(&mut self, cmd: &Value) -> Result<String> {
        let command = cmd.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let path = cmd.get("path").and_then(|v| v.as_str()).unwrap_or("");
        match command {
            "view" => self.view(path),
            "create" => self.create(
                path,
                cmd.get("file_text").and_then(|v| v.as_str()).unwrap_or(""),
            ),
            "str_replace" => self.str_replace(
                path,
                cmd.get("old_str").and_then(|v| v.as_str()).unwrap_or(""),
                cmd.get("new_str").and_then(|v| v.as_str()).unwrap_or(""),
            ),
            "insert" => self.insert(
                path,
                cmd.get("insert_line").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                cmd.get("insert_text").and_then(|v| v.as_str()).unwrap_or(""),
            ),
            "delete" => self.delete(path),
            "rename" => self.rename(
                cmd.get("old_path").and_then(|v| v.as_str()).unwrap_or(path),
                cmd.get("new_path").and_then(|v| v.as_str()).unwrap_or(""),
            ),
            other => Err(AreevError::Validation(format!(
                "unknown memory command: {other}"
            ))),
        }
    }

    fn check_path(path: &str) -> Result<()> {
        if !path.starts_with(ROOT) || path.contains("..") || path.contains("//") {
            return Err(AreevError::Validation(format!(
                "path must live under {ROOT} (got '{path}')"
            )));
        }
        Ok(())
    }

    fn head(&mut self, path: &str) -> Result<Option<(Hash, String)>> {
        match self.m.latest(&self.ns, path, MEMORY_FILE_RELATION)? {
            Some(g) => {
                let content = g
                    .fields
                    .get("context")
                    .and_then(|c| c.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(Some((g.hash, content)))
            }
            None => Ok(None),
        }
    }

    fn write_version(&mut self, path: &str, content: &str, prev: Option<Hash>) -> Result<Hash> {
        use sha2::{Digest, Sha256};
        // Ingress boundary (proposal §4.2): memory-file bodies are agent
        // free text; transform before the version digest and the blob commit
        // to it (D8).
        let ingressed = self.m.ingress_transform_text(&self.ns.clone(), content)?;
        let content: &str = ingressed.as_deref().unwrap_or(content);
        let digest = hex::encode(&Sha256::digest(content.as_bytes())[..6]);
        let mut f = Fact::new(path, MEMORY_FILE_RELATION, &format!("v:{digest}"));
        f.common.namespace = Some(self.ns.clone());
        f.common.context = Some(json!({ "content": content }));
        // Index the body, not just "path memory_file v:digest" — otherwise
        // memory files are invisible to BM25/vector recall.
        f.common.embedding_text = Some(crate::migrate::clip_et(content));
        f.common.source_type = Some("agent".to_string());
        match prev {
            Some(old) => self.m.supersede(&old, &mut f),
            None => self.m.add(&f),
        }
    }

    fn view(&mut self, path: &str) -> Result<String> {
        Self::check_path(path)?;
        let dirish = path == ROOT || path.ends_with('/');
        if dirish {
            let prefix = if path.ends_with('/') { path.to_string() } else { format!("{path}/") };
            let files = self.m.subjects_with_relation(&self.ns, MEMORY_FILE_RELATION)?;
            let listing: Vec<String> = files
                .into_iter()
                .filter(|f| f.starts_with(&prefix))
                .collect();
            if listing.is_empty() {
                return Ok(format!("Directory: {path}\n(empty)"));
            }
            return Ok(format!("Directory: {path}\n{}", listing.join("\n")));
        }
        match self.head(path)? {
            Some((_, content)) => {
                let numbered: Vec<String> = content
                    .lines()
                    .enumerate()
                    .map(|(i, l)| format!("{:>4}: {l}", i + 1))
                    .collect();
                Ok(numbered.join("\n"))
            }
            None => Err(AreevError::Validation(format!("file not found: {path}"))),
        }
    }

    fn create(&mut self, path: &str, file_text: &str) -> Result<String> {
        Self::check_path(path)?;
        if path == ROOT || path.ends_with('/') {
            return Err(AreevError::Validation("create requires a file path".into()));
        }
        let prev = self.head(path)?.map(|(h, _)| h);
        let existed = prev.is_some();
        self.write_version(path, file_text, prev)?;
        Ok(format!(
            "{} {path}",
            if existed { "Overwrote (new version of)" } else { "Created" }
        ))
    }

    fn str_replace(&mut self, path: &str, old_str: &str, new_str: &str) -> Result<String> {
        Self::check_path(path)?;
        let (head, content) = self
            .head(path)?
            .ok_or_else(|| AreevError::Validation(format!("file not found: {path}")))?;
        let occurrences = content.matches(old_str).count();
        if old_str.is_empty() || occurrences == 0 {
            return Err(AreevError::Validation("old_str not found in file".into()));
        }
        if occurrences > 1 {
            return Err(AreevError::Validation(format!(
                "old_str appears {occurrences} times — must be unique"
            )));
        }
        let updated = content.replacen(old_str, new_str, 1);
        self.write_version(path, &updated, Some(head))?;
        Ok(format!("Replaced text in {path}"))
    }

    fn insert(&mut self, path: &str, insert_line: usize, insert_text: &str) -> Result<String> {
        Self::check_path(path)?;
        let (head, content) = self
            .head(path)?
            .ok_or_else(|| AreevError::Validation(format!("file not found: {path}")))?;
        let mut lines: Vec<&str> = content.lines().collect();
        let at = insert_line.min(lines.len());
        lines.insert(at, insert_text);
        let updated = lines.join("\n");
        self.write_version(path, &updated, Some(head))?;
        Ok(format!("Inserted at line {at} in {path}"))
    }

    fn delete(&mut self, path: &str) -> Result<String> {
        Self::check_path(path)?;
        if path == ROOT {
            return Err(AreevError::Validation("refusing to delete the root".into()));
        }
        // Forget the whole chain (host-level erasure; tombstoned in op-log).
        let versions = self.m.history(&self.ns, path, MEMORY_FILE_RELATION)?;
        if versions.is_empty() {
            return Err(AreevError::Validation(format!("file not found: {path}")));
        }
        let n = versions.len();
        for v in versions {
            self.m.forget(&v.hash)?;
        }
        Ok(format!("Deleted {path} ({n} versions erased)"))
    }

    fn rename(&mut self, old_path: &str, new_path: &str) -> Result<String> {
        Self::check_path(old_path)?;
        Self::check_path(new_path)?;
        let (old_head, content) = self
            .head(old_path)?
            .ok_or_else(|| AreevError::Validation(format!("file not found: {old_path}")))?;
        if self.head(new_path)?.is_some() {
            return Err(AreevError::Validation(format!("target exists: {new_path}")));
        }
        // New chain at the new path with provenance to the old head; then
        // erase the old chain. (v1: history does not carry across renames —
        // the provenance link preserves the connection.)
        let mut f = {
            use sha2::{Digest, Sha256};
            let ingressed = self.m.ingress_transform_text(&self.ns.clone(), &content)?;
            let content: String = ingressed.unwrap_or_else(|| content.clone());
            let digest = hex::encode(&Sha256::digest(content.as_bytes())[..6]);
            let mut f = Fact::new(new_path, MEMORY_FILE_RELATION, &format!("v:{digest}"));
            f.common.namespace = Some(self.ns.clone());
            f.common.context = Some(json!({ "content": content }));
            f.common.embedding_text = Some(crate::migrate::clip_et(&content));
            f.common.derived_from = Some(old_head.to_hex());
            f
        };
        self.m.add(&f)?;
        let versions = self.m.history(&self.ns, old_path, MEMORY_FILE_RELATION)?;
        for v in versions {
            self.m.forget(&v.hash)?;
        }
        let _ = &mut f;
        Ok(format!("Renamed {old_path} → {new_path}"))
    }
}
