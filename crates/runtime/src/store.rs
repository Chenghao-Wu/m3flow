//! Content-addressable artifact store (plan §56).
//!
//! Files live at `.m3flow/artifacts/sha256/<2-hex>/<full-hash>`; identical
//! bytes are stored once. Artifact records reference files by store-relative
//! path, which never changes — artifacts are immutable.

use m3flow_core::artifact::{now_rfc3339, Artifact, StagedArtifact};
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
        self.ingest_reader(&mut std::io::Cursor::new(bytes), "<bytes>")
    }

    /// Ingest a file from disk: streamed through the hasher in chunks (large
    /// trajectories never sit in memory) into a temporary file that is
    /// atomically renamed onto its hash-named path.
    pub fn ingest_file(&self, src: &Path) -> Result<(String, String, u64)> {
        let mut f = std::fs::File::open(src)
            .map_err(|e| M3FlowError::io(e, format!("reading {}", src.display())))?;
        self.ingest_reader(&mut f, &src.display().to_string())
    }

    fn ingest_reader(
        &self,
        reader: &mut dyn std::io::Read,
        what: &str,
    ) -> Result<(String, String, u64)> {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        let tmp_dir = self.root.join("tmp");
        std::fs::create_dir_all(&tmp_dir)
            .map_err(|e| M3FlowError::io(e, "creating CAS temp dir"))?;
        let tmp = tmp_dir.join(format!(
            "ingest-{}-{}",
            std::process::id(),
            m3flow_core::id::ArtifactId::new().as_str()
        ));
        let result = (|| {
            let mut out = std::fs::File::create(&tmp)
                .map_err(|e| M3FlowError::io(e, format!("creating {}", tmp.display())))?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 1 << 20];
            let mut size = 0u64;
            loop {
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| M3FlowError::io(e, format!("reading {what}")))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])
                    .map_err(|e| M3FlowError::io(e, format!("writing {}", tmp.display())))?;
                size += n as u64;
            }
            out.sync_all()
                .map_err(|e| M3FlowError::io(e, format!("syncing {}", tmp.display())))?;
            Ok::<_, M3FlowError>((hex::encode(hasher.finalize()), size))
        })();
        let (sha, size) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        let rel = format!("sha256/{}/{}", &sha[..2], sha);
        let dest = self.cas_path(&sha);
        if self.blob_is_valid(&dest, &sha) {
            let _ = std::fs::remove_file(&tmp);
            return Ok((rel, sha, size));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| M3FlowError::io(e, "creating CAS shard"))?;
        }
        // CAS blobs are immutable: read-only from birth. This is what makes
        // symlinks in the friendly `results/` trees safe — a write through a
        // link fails instead of silently poisoning a hash-named blob.
        // Best-effort: odd filesystems must not fail registration.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444));
        }
        // rename is atomic within the store filesystem: readers see either
        // no blob or the complete blob, never a partial write
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            if !self.blob_is_valid(&dest, &sha) {
                return Err(M3FlowError::io(e, format!("publishing {}", dest.display())));
            }
        }
        Ok((rel, sha, size))
    }

    /// An existing blob is reused only if its bytes still hash to its name
    /// (a torn write or corruption is replaced, not trusted).
    fn blob_is_valid(&self, path: &Path, sha: &str) -> bool {
        use sha2::{Digest, Sha256};
        let Ok(mut f) = std::fs::File::open(path) else {
            return false;
        };
        let mut hasher = Sha256::new();
        if std::io::copy(&mut f, &mut hasher).is_err() {
            return false;
        }
        hex::encode(hasher.finalize()) == sha
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
            let src = contained_output_path(workdir, rel)?;
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

/// Resolve a provider-declared output file, refusing anything that escapes
/// the task workdir: absolute paths, `..` components, and symlinks whose
/// target lies outside the workdir.
pub fn contained_output_path(workdir: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let reject = |why: &str| M3FlowError::Provider {
        provider: String::new(),
        message: format!("output file '{rel}' rejected: {why}"),
        details: None,
        raw_log: None,
    };
    let p = Path::new(rel);
    if rel.is_empty() || p.is_absolute() {
        return Err(reject(
            "must be a non-empty path relative to the task workdir",
        ));
    }
    if p.components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(reject("must not contain '..' or root components"));
    }
    let joined = workdir.join(p);
    if let (Ok(real), Ok(base)) = (joined.canonicalize(), workdir.canonicalize()) {
        if !real.starts_with(&base) {
            return Err(reject("resolves (via a symlink) outside the task workdir"));
        }
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use m3flow_core::canon;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("m3store-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn streamed_ingest_matches_bytes_and_dedups() {
        let d = tmp("ingest");
        let store = Store::new(d.join("store")).unwrap();
        let src = d.join("f.bin");
        let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &bytes).unwrap();
        let (rel, sha, size) = store.ingest_file(&src).unwrap();
        assert_eq!(sha, canon::hash_bytes(&bytes));
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(std::fs::read(store.resolve(&rel)).unwrap(), bytes);
        assert_eq!(store.ingest_bytes(&bytes).unwrap().1, sha);
        // no temp files left behind
        assert_eq!(std::fs::read_dir(d.join("store/tmp")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupted_blob_is_replaced_not_trusted() {
        let d = tmp("corrupt");
        let store = Store::new(d.join("store")).unwrap();
        let (rel, _, _) = store.ingest_bytes(b"payload").unwrap();
        let blob = store.resolve(&rel);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::write(&blob, b"torn").unwrap();
        store.ingest_bytes(b"payload").unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), b"payload");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn output_paths_must_stay_in_the_workdir() {
        let d = tmp("contain");
        let wd = d.join("wd");
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(d.join("secret"), "x").unwrap();
        std::fs::write(wd.join("ok.txt"), "x").unwrap();
        assert!(contained_output_path(&wd, "ok.txt").is_ok());
        assert!(contained_output_path(&wd, "sub/./ok.txt").is_ok());
        assert!(contained_output_path(&wd, "../secret").is_err());
        assert!(contained_output_path(&wd, "/etc/passwd").is_err());
        assert!(contained_output_path(&wd, "").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("secret"), wd.join("link")).unwrap();
            assert!(contained_output_path(&wd, "link").is_err());
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
