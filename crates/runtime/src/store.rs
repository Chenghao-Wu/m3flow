//! Content-addressable artifact store (plan §56).
//!
//! Files live at `.m3flow/artifacts/sha256/<2-hex>/<full-hash>`; identical
//! bytes are stored once. Artifact records reference files by store-relative
//! path, which never changes — artifacts are immutable.

use m3flow_core::artifact::{now_rfc3339, Artifact, StagedArtifact};
use m3flow_core::canon;
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::id::{ArtifactId, TaskRunId};
use m3flow_core::ARTIFACT_SCHEMA_VERSION;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(root.join("sha256"))
            .map_err(|e| M3FlowError::io(e, "creating artifact store"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn cas_path(&self, sha: &str) -> PathBuf {
        self.root.join("sha256").join(&sha[..2]).join(sha)
    }

    /// Absolute path of a store-relative file reference.
    pub fn resolve(&self, relpath: &str) -> PathBuf {
        self.root.join(relpath)
    }

    /// Ingest bytes into the CAS; returns (store-relative path, sha256, size).
    pub fn ingest_bytes(&self, bytes: &[u8]) -> Result<(String, String, u64)> {
        let sha = canon::hash_bytes(bytes);
        let rel = format!("sha256/{}/{}", &sha[..2], sha);
        let dest = self.cas_path(&sha);
        if !dest.exists() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| M3FlowError::io(e, "creating CAS shard"))?;
            }
            std::fs::write(&dest, bytes)
                .map_err(|e| M3FlowError::io(e, format!("writing {}", dest.display())))?;
        }
        Ok((rel, sha, bytes.len() as u64))
    }

    /// Ingest a file from disk (streaming copy through the hasher).
    pub fn ingest_file(&self, src: &Path) -> Result<(String, String, u64)> {
        let bytes = std::fs::read(src)
            .map_err(|e| M3FlowError::io(e, format!("reading {}", src.display())))?;
        self.ingest_bytes(&bytes)
    }

    /// Ingest all files of a staged provider output and build the record.
    pub fn ingest_staged(
        &self,
        staged: &StagedArtifact,
        workdir: &Path,
        producer: Option<&TaskRunId>,
    ) -> Result<(Artifact, Vec<(String, String, String, u64)>)> {
        let mut files = BTreeMap::new();
        let mut rows = Vec::new();
        let mut file_hashes = BTreeMap::new();
        for (name, rel) in &staged.files {
            let src = workdir.join(rel);
            if !src.is_file() {
                return Err(M3FlowError::Provider {
                    provider: String::new(),
                    message: format!(
                        "declared output file '{}' for '{}' not found in workdir",
                        rel, name
                    ),
                    details: None,
                    raw_log: None,
                });
            }
            let (relpath, sha, size) = self.ingest_file(&src)?;
            file_hashes.insert(name.clone(), sha.clone());
            files.insert(name.clone(), relpath.clone());
            rows.push((name.clone(), relpath, sha, size));
        }
        let artifact = Artifact {
            id: ArtifactId::new(),
            artifact_type: staged.artifact_type.clone(),
            schema_version: ARTIFACT_SCHEMA_VERSION.to_string(),
            content_hash: m3flow_core::artifact::content_hash(
                &staged.artifact_type,
                ARTIFACT_SCHEMA_VERSION,
                &file_hashes,
            ),
            files,
            metadata: if staged.metadata.is_null() {
                serde_json::json!({})
            } else {
                staged.metadata.clone()
            },
            data: staged.data.clone(),
            producer: producer.cloned(),
            created_at: now_rfc3339(),
        };
        Ok((artifact, rows))
    }

    /// Register a free-standing artifact from explicit files (CLI
    /// `artifact register`, workflow file inputs).
    pub fn register_files(
        &self,
        artifact_type: &str,
        paths: &BTreeMap<String, PathBuf>,
        metadata: serde_json::Value,
        data: Option<serde_json::Value>,
        producer: Option<&TaskRunId>,
    ) -> Result<(Artifact, Vec<(String, String, String, u64)>)> {
        let mut files = BTreeMap::new();
        let mut rows = Vec::new();
        let mut file_hashes = BTreeMap::new();
        for (name, src) in paths {
            let (relpath, sha, size) = self.ingest_file(src)?;
            file_hashes.insert(name.clone(), sha.clone());
            files.insert(name.clone(), relpath.clone());
            rows.push((name.clone(), relpath, sha, size));
        }
        let artifact = Artifact {
            id: ArtifactId::new(),
            artifact_type: artifact_type.to_string(),
            schema_version: ARTIFACT_SCHEMA_VERSION.to_string(),
            content_hash: m3flow_core::artifact::content_hash(
                artifact_type,
                ARTIFACT_SCHEMA_VERSION,
                &file_hashes,
            ),
            files,
            metadata,
            data,
            producer: producer.cloned(),
            created_at: now_rfc3339(),
        };
        Ok((artifact, rows))
    }
}
