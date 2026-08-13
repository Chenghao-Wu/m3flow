//! M3Flow core: domain model for the scientific workflow runtime.
//!
//! - [`specs`] — TaskSpec / WorkflowSpec / SystemSpec documents
//! - [`artifact`] — Artifact records, run/task statuses, validation verdicts
//! - [`atypes`] — artifact type hierarchy and compatibility
//! - [`units`] — dimensioned quantities with canonical units
//! - [`expr`] — `${...}` references and the condition mini-language
//! - [`canon`] — canonical JSON + content hashing (cache keys, fingerprints)
//! - [`error`] — the structured error model
//! - [`id`] — typed ids (`art_`, `tr_`, `wr_`)

pub mod artifact;
pub mod atypes;
pub mod canon;
pub mod error;
pub mod expr;
pub mod id;
pub mod specs;
pub mod units;

pub use artifact::{Artifact, RunStatus, StagedArtifact, TaskStatus, ValidationVerdict};
pub use error::{M3FlowError, Result};
pub use id::{ArtifactId, TaskRunId, WorkflowRunId};
pub use specs::{SystemSpec, TaskSpec, WorkflowSpec};
pub use units::{Dimension, Quantity};

/// Current schema versions emitted by this build.
pub const ARTIFACT_SCHEMA_VERSION: &str = "1";
pub const PROVIDER_PROTOCOL: &str = "m3flow-provider/1";
pub const PLATFORM_VERSION: &str = env!("CARGO_PKG_VERSION");
