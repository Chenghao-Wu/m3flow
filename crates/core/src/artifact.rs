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
/// Metadata is deliberately excluded: it is descriptive, not identity.
pub fn content_hash(artifact_type: &str, schema_version: &str, file_hashes: &BTreeMap<String, String>) -> String {
    canon::hash_json(&serde_json::json!({
        "type": artifact_type,
        "schema_version": schema_version,
        "files": file_hashes,
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
