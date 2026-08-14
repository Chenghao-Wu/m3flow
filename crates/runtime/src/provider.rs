//! Provider subprocess client (plan §39, docs/provider-protocol.md).

use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::PROVIDER_PROTOCOL;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::project::ProviderConfig;

#[derive(Debug, Clone)]
pub struct ProviderHandle {
    pub name: String,
    pub executable: PathBuf,
    pub config: ProviderConfig,
    pub description: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExecuteResponse {
    pub status: String,
    #[serde(default)]
    pub outputs: std::collections::BTreeMap<String, m3flow_core::artifact::StagedArtifact>,
    #[serde(default)]
    pub validation: Vec<m3flow_core::artifact::ValidationVerdict>,
    #[serde(default)]
    pub engine: Option<serde_json::Value>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub error: Option<ProviderError>,
    #[serde(default)]
    pub partial_outputs:
        Option<std::collections::BTreeMap<String, m3flow_core::artifact::StagedArtifact>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderError {
    pub error_type: String,
    pub category: String,
    #[serde(default)]
    pub recoverable: bool,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub details: Option<serde_json::Value>,
    #[serde(default)]
    pub raw_log: Option<String>,
}

impl ProviderHandle {
    /// Locate a provider executable: project config first, then PATH.
    pub fn locate(name: &str, config: Option<&ProviderConfig>) -> Result<Self> {
        let cfg = config.cloned().unwrap_or_default();
        let exe = cfg
            .executable
            .clone()
            .unwrap_or_else(|| format!("m3flow-{name}"));
        let path = which(&exe).ok_or_else(|| M3FlowError::Provider {
            provider: name.to_string(),
            message: format!(
                "provider executable '{exe}' not found on PATH; install it or set providers.{name}.executable in m3flow.yaml"
            ),
            details: None,
            raw_log: None,
        })?;
        Ok(Self {
            name: name.to_string(),
            executable: path,
            config: cfg,
            description: None,
        })
    }

    fn run_json(&self, subcommand: &str, arg: Option<&Path>) -> Result<serde_json::Value> {
        let mut cmd = Command::new(&self.executable);
        cmd.arg(subcommand);
        if let Some(a) = arg {
            cmd.arg(a);
        }
        let output = cmd
            .output()
            .map_err(|e| M3FlowError::io(e, format!("spawning {}", self.executable.display())))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value =
            serde_json::from_str(stdout.trim()).map_err(|e| M3FlowError::Provider {
                provider: self.name.clone(),
                message: format!(
                    "invalid JSON from '{subcommand}' (exit {}): {e}\nstderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                        .lines()
                        .last()
                        .unwrap_or("")
                ),
                details: None,
                raw_log: None,
            })?;
        Ok(parsed)
    }

    pub fn describe(&mut self) -> Result<&serde_json::Value> {
        if self.description.is_none() {
            let v = self.run_json("describe", None)?;
            let proto = v
                .get("protocol")
                .and_then(|p| p.as_str())
                .unwrap_or_default();
            if proto != PROVIDER_PROTOCOL {
                return Err(M3FlowError::Provider {
                    provider: self.name.clone(),
                    message: format!(
                        "protocol mismatch: provider speaks '{proto}', runtime expects '{PROVIDER_PROTOCOL}'"
                    ),
                    details: None,
                    raw_log: None,
                });
            }
            self.description = Some(v);
        }
        Ok(self.description.as_ref().unwrap())
    }

    pub fn engine_version(&mut self) -> Result<String> {
        let d = self.describe()?;
        let engine = d.get("engine").cloned().unwrap_or(serde_json::json!({}));
        let name = engine
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown");
        let version = engine
            .get("version")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown");
        Ok(format!("{name}/{version}"))
    }

    pub fn execute(&self, request_path: &Path, workdir: &Path) -> Result<ExecuteResponse> {
        let v = self.run_json("execute", Some(request_path))?;
        // persist the raw response for inspectability
        let _ = std::fs::write(
            workdir.join("response.json"),
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        );
        serde_json::from_value(v).map_err(|e| M3FlowError::Provider {
            provider: self.name.clone(),
            message: format!("malformed execute response: {e}"),
            details: None,
            raw_log: None,
        })
    }

    pub fn diagnose(&self, request_path: &Path) -> Result<serde_json::Value> {
        self.run_json("diagnose", Some(request_path))
    }
}

fn which(exe: &str) -> Option<PathBuf> {
    let p = Path::new(exe);
    if p.components().count() > 1 || exe.starts_with('.') || exe.starts_with('/') {
        return if p.is_file() {
            Some(p.to_path_buf())
        } else {
            None
        };
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}
