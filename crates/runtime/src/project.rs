//! Project discovery and configuration (plan §47).
//!
//! A project is any directory containing `m3flow.yaml`; its state lives in
//! `.m3flow/`. Commands search upward from the cwd like git.

use m3flow_core::error::{M3FlowError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PROJECT_FILE: &str = "m3flow.yaml";
pub const STATE_DIR: &str = ".m3flow";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default = "default_schema")]
    pub schema: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub registries: Option<BTreeMap<String, Vec<PathBuf>>>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub defaults: Option<Defaults>,
}

fn default_schema() -> String {
    "m3flow-project/v1".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Executable name or path (default: `m3flow-<name>` on PATH).
    #[serde(default)]
    pub executable: Option<String>,
    /// Python interpreter for Python providers.
    #[serde(default)]
    pub python: Option<String>,
    /// Engine configuration, provider-specific (e.g. LAMMPS binary path).
    #[serde(default)]
    pub engine: Option<serde_json::Value>,
    /// Free-form extra config forwarded to the provider in `config`.
    #[serde(default)]
    pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    /// task name -> provider name
    #[serde(default)]
    pub provider_selection: BTreeMap<String, String>,
    #[serde(default)]
    pub max_concurrency: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
}

impl Project {
    /// Find the enclosing project by walking up from `start`.
    pub fn discover(start: &Path) -> Result<Self> {
        let mut dir = start
            .canonicalize()
            .map_err(|e| M3FlowError::io(e, "canonicalizing cwd"))?;
        loop {
            if dir.join(PROJECT_FILE).is_file() {
                return Self::load(dir);
            }
            if !dir.pop() {
                return Err(M3FlowError::not_found(format!(
                    "no {PROJECT_FILE} found in {} or any parent; run `m3flow init`",
                    start.display()
                )));
            }
        }
    }

    pub fn load(root: PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(root.join(PROJECT_FILE))
            .map_err(|e| M3FlowError::io(e, "reading project file"))?;
        let config: ProjectConfig = serde_yaml::from_str(&text)
            .map_err(|e| M3FlowError::schema(format!("{PROJECT_FILE}: {e}")))?;
        Ok(Self { root, config })
    }

    /// Initialize a new project directory.
    pub fn init(root: &Path, name: Option<&str>) -> Result<Self> {
        std::fs::create_dir_all(root.join(STATE_DIR))
            .map_err(|e| M3FlowError::io(e, "creating state dir"))?;
        for sub in ["systems", "workflows", "results"] {
            std::fs::create_dir_all(root.join(sub))
                .map_err(|e| M3FlowError::io(e, format!("creating {sub}")))?;
        }
        let cfg = ProjectConfig {
            schema: default_schema(),
            name: name.map(|s| s.to_string()),
            registries: None,
            providers: BTreeMap::new(),
            defaults: None,
        };
        let text = serde_yaml::to_string(&cfg)
            .map_err(|e| M3FlowError::internal(format!("yaml encode: {e}")))?;
        std::fs::write(root.join(PROJECT_FILE), text)
            .map_err(|e| M3FlowError::io(e, "writing project file"))?;
        Self::load(root.to_path_buf())
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    pub fn db_path(&self) -> PathBuf {
        self.state_dir().join("m3flow.db")
    }

    pub fn artifacts_dir(&self) -> PathBuf {
        self.state_dir().join("artifacts")
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.state_dir().join("runs")
    }

    /// Extra registry directories from config, resolved against the root.
    pub fn extra_registry_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(regs) = &self.config.registries {
            for dirs in regs.values() {
                for d in dirs {
                    out.push(if d.is_absolute() { d.clone() } else { self.root.join(d) });
                }
            }
        }
        out
    }

    pub fn provider_config(&self, name: &str) -> Option<&ProviderConfig> {
        self.config.providers.get(name)
    }

    pub fn max_concurrency(&self) -> usize {
        self.config
            .defaults
            .as_ref()
            .and_then(|d| d.max_concurrency)
            .unwrap_or_else(|| default_concurrency())
    }

    pub fn preferred_provider(&self, task: &str) -> Option<&str> {
        self.config
            .defaults
            .as_ref()
            .and_then(|d| d.provider_selection.get(task))
            .map(|s| s.as_str())
    }
}

fn default_concurrency() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// Git context for reproducibility metadata (plan §54). All fields optional:
/// a run must never fail because git is absent.
pub fn git_context(dir: &Path) -> serde_json::Value {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let commit = run(&["rev-parse", "HEAD"]);
    let dirty = run(&["status", "--porcelain"]).map(|s| !s.is_empty());
    serde_json::json!({
        "commit": commit,
        "dirty_worktree": dirty,
    })
}
