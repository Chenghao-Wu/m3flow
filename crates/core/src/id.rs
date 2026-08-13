//! Typed identifiers: `art_`, `tr_`, `wr_` prefixed 8-hex-char ids (plan §7).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fresh_hex(seed: &str) -> String {
    // Ids are randomness-free on purpose: reproducible within a run log and
    // unique in practice via (time, pid, counter, seed) mixing.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(seed.as_bytes());
    h.update(now.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(n.to_le_bytes());
    hex::encode(h.finalize())[..8].to_string()
}

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, fresh_hex(stringify!($name))))
            }
            pub fn parse(s: &str) -> Option<Self> {
                let body = s.strip_prefix($prefix)?;
                if body.len() == 8 && body.chars().all(|c| c.is_ascii_hexdigit()) {
                    Some(Self(s.to_string()))
                } else {
                    None
                }
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(ArtifactId, "art_");
id_type!(TaskRunId, "tr_");
id_type!(WorkflowRunId, "wr_");

/// Parse any of the known id kinds.
pub fn parse_any_id(s: &str) -> Option<(&'static str, String)> {
    if ArtifactId::parse(s).is_some() {
        Some(("artifact", s.to_string()))
    } else if TaskRunId::parse(s).is_some() {
        Some(("task_run", s.to_string()))
    } else if WorkflowRunId::parse(s).is_some() {
        Some(("workflow_run", s.to_string()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        let a = ArtifactId::new();
        assert!(ArtifactId::parse(a.as_str()).is_some());
        assert!(TaskRunId::parse(a.as_str()).is_none());
        assert_eq!(parse_any_id(a.as_str()).unwrap().0, "artifact");
    }

    #[test]
    fn ids_unique() {
        let a = ArtifactId::new();
        let b = ArtifactId::new();
        assert_ne!(a, b);
    }
}
