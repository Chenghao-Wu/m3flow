//! Artifact records (plan §7) and the provider-output staging model.

use crate::canon;
use crate::id::{ArtifactId, TaskRunId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A stored artifact as recorded in the provenance DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: ArtifactId,
    #[serde(rename = "type")]
    pub artifact_type: String,
    pub schema_version: String,
    /// name -> store-relative file path
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub producer: Option<TaskRunId>,
    pub created_at: String,
}

impl Artifact {
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": self.artifact_type,
            "schema_version": self.schema_version,
            "files": self.files.keys().collect::<Vec<_>>(),
            "content_hash": self.content_hash,
            "producer": self.producer,
            "created_at": self.created_at,
        })
    }
}

/// An output produced by a provider before ingestion into the store
/// (files are workdir-relative at this stage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedArtifact {
    #[serde(rename = "type")]
    pub artifact_type: String,
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// Compute the content fingerprint from type + per-file content hashes.
/// This is the *file identity* of an artifact (deduplication, lineage);
/// it deliberately ignores metadata and data. What a task computes from an
/// artifact is captured by [`execution_fingerprint`] instead.
pub fn content_hash(
    artifact_type: &str,
    schema_version: &str,
    file_hashes: &BTreeMap<String, String>,
) -> String {
    canon::hash_json(&serde_json::json!({
        "type": artifact_type,
        "schema_version": schema_version,
        "files": file_hashes,
    }))
}

/// Top-level metadata keys that are presentation only (never read by a
/// task to compute anything). Everything else in metadata — time axes,
/// unit systems, topology/selection descriptors, sampling strides — is
/// semantic and joins the execution fingerprint.
pub const PRESENTATION_METADATA_KEYS: &[&str] = &[
    "description",
    "display_name",
    "label",
    "labels",
    "notes",
    "tags",
];

/// Identity of an artifact *as a task input*: file identity plus the data
/// payload and the semantic metadata. Two artifacts with identical bytes
/// but different `frame_interval_fs`, `units` or data payloads give
/// different results downstream, so they must never share a cache entry.
pub fn execution_fingerprint(a: &Artifact) -> String {
    let semantic_metadata = match &a.metadata {
        serde_json::Value::Object(m) => serde_json::Value::Object(
            m.iter()
                .filter(|(k, _)| !PRESENTATION_METADATA_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        serde_json::Value::Null => serde_json::json!({}),
        other => other.clone(),
    };
    canon::hash_json(&serde_json::json!({
        "fingerprint": "m3flow-input/1",
        "content_hash": a.content_hash,
        "metadata": semantic_metadata,
        "data": a.data,
    }))
}

/// Lifecycle of a task inside a workflow run (plan §35).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Completed,
    Failed,
    Cached,
    Skipped,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Ready => "READY",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Cached => "CACHED",
            Self::Skipped => "SKIPPED",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PENDING" => Some(Self::Pending),
            "READY" => Some(Self::Ready),
            "RUNNING" => Some(Self::Running),
            "COMPLETED" => Some(Self::Completed),
            "FAILED" => Some(Self::Failed),
            "CACHED" => Some(Self::Cached),
            "SKIPPED" => Some(Self::Skipped),
            "CANCELLED" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cached | Self::Skipped | Self::Cancelled
        )
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Completed | Self::Cached | Self::Skipped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PENDING" => Some(Self::Pending),
            "RUNNING" => Some(Self::Running),
            "COMPLETED" => Some(Self::Completed),
            "FAILED" => Some(Self::Failed),
            "CANCELLED" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A validation verdict attached to a task run (plan §45).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationVerdict {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ArtifactId;

    fn art(metadata: serde_json::Value, data: Option<serde_json::Value>) -> Artifact {
        Artifact {
            id: ArtifactId::new(),
            artifact_type: "Trajectory".into(),
            schema_version: "1".into(),
            files: BTreeMap::new(),
            metadata,
            data,
            content_hash: "same-bytes".into(),
            producer: None,
            created_at: now_rfc3339(),
        }
    }

    #[test]
    fn semantic_metadata_and_data_change_the_execution_fingerprint() {
        let a = art(serde_json::json!({"frame_interval_fs": 100}), None);
        let b = art(serde_json::json!({"frame_interval_fs": 1000}), None);
        assert_eq!(a.content_hash, b.content_hash);
        assert_ne!(execution_fingerprint(&a), execution_fingerprint(&b));
        let c = art(serde_json::json!({}), Some(serde_json::json!({"value": 1})));
        let d = art(serde_json::json!({}), Some(serde_json::json!({"value": 2})));
        assert_ne!(execution_fingerprint(&c), execution_fingerprint(&d));
    }

    #[test]
    fn presentation_metadata_does_not() {
        let a = art(serde_json::json!({"units": "real", "label": "run A"}), None);
        let b = art(
            serde_json::json!({"units": "real", "label": "run B", "notes": "x"}),
            None,
        );
        assert_eq!(execution_fingerprint(&a), execution_fingerprint(&b));
        let c = art(serde_json::json!({"units": "lj", "label": "run A"}), None);
        assert_ne!(execution_fingerprint(&a), execution_fingerprint(&c));
    }
}
