//! M3Flow structured error model.
//!
//! Every failure surfaced by the platform uses one of these categories so that
//! coding agents can branch on `error_type` programmatically (plan §44).

use serde::Serialize;

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum M3FlowError {
    #[error("schema error: {message}")]
    Schema {
        message: String,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        details: Vec<String>,
    },
    #[error("type error: {message}")]
    Type {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        expected: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        received: Option<String>,
    },
    #[error("workflow error: {message}")]
    Workflow {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        step: Option<String>,
    },
    #[error("task error: {message}")]
    Task {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        task: Option<String>,
    },
    #[error("provider error ({provider}): {message}")]
    Provider {
        provider: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        details: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        raw_log: Option<String>,
    },
    #[error("execution error: {message}")]
    Execution { message: String },
    #[error("scientific validation error: {message}")]
    ScientificValidation {
        message: String,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        failed_validators: Vec<String>,
    },
    #[error("artifact compatibility error: {message}")]
    ArtifactCompatibility {
        message: String,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        missing: Vec<String>,
    },
    #[error("not found: {message}")]
    NotFound { message: String },
    #[error("io error: {message}")]
    Io { message: String },
    #[error("internal error: {message}")]
    Internal { message: String },
}

pub type Result<T> = std::result::Result<T, M3FlowError>;

impl M3FlowError {
    /// Machine-readable category for retry policies and agent branching.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Schema { .. } => "input_error",
            Self::Type { .. } => "input_error",
            Self::Workflow { .. } => "protocol_error",
            Self::Task { .. } => "task_error",
            Self::Provider { .. } => "provider_error",
            Self::Execution { .. } => "execution_error",
            Self::ScientificValidation { .. } => "scientific_validation",
            Self::ArtifactCompatibility { .. } => "compatibility_error",
            Self::NotFound { .. } => "not_found",
            Self::Io { .. } => "environment_error",
            Self::Internal { .. } => "internal",
        }
    }

    /// Whether re-running the same task could plausibly succeed.
    pub fn recoverable(&self) -> bool {
        matches!(
            self,
            Self::Provider { .. } | Self::Execution { .. } | Self::Io { .. }
        )
    }

    pub fn schema(message: impl Into<String>) -> Self {
        Self::Schema {
            message: message.into(),
            details: Vec::new(),
        }
    }

    pub fn workflow(message: impl Into<String>, step: Option<String>) -> Self {
        Self::Workflow {
            message: message.into(),
            step,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    pub fn io(err: std::io::Error, context: impl Into<String>) -> Self {
        Self::Io {
            message: format!("{}: {}", context.into(), err),
        }
    }
}

impl From<std::io::Error> for M3FlowError {
    fn from(e: std::io::Error) -> Self {
        Self::Io {
            message: e.to_string(),
        }
    }
}
