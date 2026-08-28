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
    /// Execution backend for provider jobs. Scheduling-only: never joins
    /// cache keys or fingerprints (same rule as `resources`).
    #[serde(default)]
    pub executor: Option<ExecutorConfig>,
    /// `development` (default) | `production`. Production locks the
    /// extension surface: project type packs / task specs are rejected and
    /// providers must be declared under `providers:` (see `extensions`).
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Per-kind overrides for the extension surface; each wins over `mode`.
    #[serde(default)]
    pub extensions: Option<Extensions>,
}

/// Project mode shorthand. Guardrail against vocabulary drift, not a
/// security boundary — the config itself is a writable file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Development,
    Production,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllowDeny {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderPolicy {
    /// Any installed provider may be dispatched.
    Any,
    /// Only providers declared under `providers:` in m3flow.yaml may run.
    Pinned,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Extensions {
    /// `types/v1` packs from the project `types/` dir / `registries:` paths.
    #[serde(default)]
    pub types: Option<AllowDeny>,
    /// `task/v1` + `workflow/v1` specs from project dirs / `registries:`.
    #[serde(default)]
    pub tasks: Option<AllowDeny>,
    #[serde(default)]
    pub providers: Option<ProviderPolicy>,
}

/// The extension surface after resolving `mode` + `extensions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionPolicy {
    pub types_allowed: bool,
    pub tasks_allowed: bool,
    pub providers_pinned: bool,
}

impl ExtensionPolicy {
    /// Everything allowed — the default outside production mode.
    pub const PERMISSIVE: Self = Self {
        types_allowed: true,
        tasks_allowed: true,
        providers_pinned: false,
    };
}

fn default_schema() -> String {
    "m3flow-project/v1".to_string()
}

/// Where a provider job runs. Serializes lowercase (`local` | `slurm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutorKind {
    Local,
    Slurm,
}

impl Default for ExecutorKind {
    fn default() -> Self {
        ExecutorKind::Local
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutorConfig {
    /// Backend selection: `local` (default) | `slurm`.
    #[serde(rename = "type", default)]
    pub kind: Option<ExecutorKind>,
    /// Slurm backend options; all optional, see docs/slurm.md.
    #[serde(default)]
    pub slurm: Option<SlurmConfig>,
}

/// Options for the Slurm executor. Everything is optional: an empty section
/// submits to the cluster default partition with site defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlurmConfig {
    #[serde(default)]
    pub partition: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub qos: Option<String>,
    /// GPU model for `--gres=gpu:<type>:<N>` when a step requests GPUs.
    #[serde(default)]
    pub gpu_type: Option<String>,
    /// Verbatim `--gres` string; wins over `gpu_type`.
    #[serde(default)]
    pub gres: Option<String>,
    /// Default `--time` when a step declares no `resources.walltime`.
    #[serde(default)]
    pub time: Option<String>,
    /// Base poll cadence for job state (±30% jitter). Default: 15 s.
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
    /// Shell lines run in the batch script before the provider call
    /// (module loads, conda activation, …).
    #[serde(default)]
    pub setup_commands: Vec<String>,
    /// Verbatim extra `#SBATCH` lines (site-specific directives).
    #[serde(default)]
    pub extra_sbatch: Vec<String>,
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
    /// Executor override for this provider (`local` | `slurm`); overrides the
    /// global `executor.type` but not a `--executor` CLI flag.
    #[serde(default)]
    pub executor: Option<ExecutorKind>,
    /// Version pin: the provider's self-reported version must match exactly.
    /// Enforced whenever set (any mode); in `providers: pinned` mode an
    /// entry here is also what makes the provider dispatchable at all.
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    /// task name -> provider name
    #[serde(default)]
    pub provider_selection: BTreeMap<String, String>,
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    /// Friendly `results/` tree materialization (presentation-only:
    /// never joins cache keys or fingerprints).
    #[serde(default)]
    pub materialize: Option<MaterializeConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaterializeConfig {
    /// Default: true.
    #[serde(default)]
    pub enabled: Option<bool>,
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
            executor: None,
            mode: None,
            extensions: None,
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
                    out.push(if d.is_absolute() {
                        d.clone()
                    } else {
                        self.root.join(d)
                    });
                }
            }
        }
        out
    }

    pub fn provider_config(&self, name: &str) -> Option<&ProviderConfig> {
        self.config.providers.get(name)
    }

    /// Effective extension policy: `extensions.<kind>` wins where set,
    /// otherwise the `mode` default (production locks all three kinds).
    pub fn extension_policy(&self) -> ExtensionPolicy {
        let production = matches!(self.config.mode, Some(Mode::Production));
        let ext = self.config.extensions.as_ref();
        let allowed = |kind: Option<AllowDeny>| match kind {
            Some(AllowDeny::Allow) => true,
            Some(AllowDeny::Deny) => false,
            None => !production,
        };
        ExtensionPolicy {
            types_allowed: allowed(ext.and_then(|e| e.types)),
            tasks_allowed: allowed(ext.and_then(|e| e.tasks)),
            providers_pinned: match ext.and_then(|e| e.providers) {
                Some(ProviderPolicy::Pinned) => true,
                Some(ProviderPolicy::Any) => false,
                None => production,
            },
        }
    }

    /// Executor for a provider job. Precedence: `--executor` CLI flag >
    /// `providers.<name>.executor` > `executor.type` > local.
    pub fn executor_for(&self, provider: &str, cli_override: Option<ExecutorKind>) -> ExecutorKind {
        if let Some(k) = cli_override {
            return k;
        }
        if let Some(k) = self.config.providers.get(provider).and_then(|p| p.executor) {
            return k;
        }
        self.config
            .executor
            .as_ref()
            .and_then(|e| e.kind)
            .unwrap_or_default()
    }

    /// Slurm backend options (empty defaults when the section is absent).
    pub fn slurm_config(&self) -> SlurmConfig {
        self.config
            .executor
            .as_ref()
            .and_then(|e| e.slurm.clone())
            .unwrap_or_default()
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

    /// Always-on friendly `results/` tree (default: enabled).
    pub fn materialize_enabled(&self) -> bool {
        self.config
            .defaults
            .as_ref()
            .and_then(|d| d.materialize.as_ref())
            .and_then(|m| m.enabled)
            .unwrap_or(true)
    }

    pub fn results_dir(&self) -> PathBuf {
        self.root.join("results")
    }
}

fn default_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(yaml: &str) -> Project {
        let config: ProjectConfig = serde_yaml::from_str(yaml).unwrap();
        Project {
            root: PathBuf::from("/nonexistent"),
            config,
        }
    }

    #[test]
    fn default_is_permissive() {
        let p = project_with("schema: m3flow-project/v1\n");
        assert_eq!(p.extension_policy(), ExtensionPolicy::PERMISSIVE);
    }

    #[test]
    fn production_locks_everything() {
        let p = project_with("mode: production\n");
        let pol = p.extension_policy();
        assert!(!pol.types_allowed);
        assert!(!pol.tasks_allowed);
        assert!(pol.providers_pinned);
    }

    #[test]
    fn extensions_override_mode_per_kind() {
        let p = project_with("mode: production\nextensions:\n  types: allow\n  providers: any\n");
        let pol = p.extension_policy();
        assert!(pol.types_allowed); // explicitly re-enabled
        assert!(!pol.tasks_allowed); // still locked by mode
        assert!(!pol.providers_pinned); // explicitly re-enabled
    }

    #[test]
    fn development_mode_can_deny_selectively() {
        let p = project_with("extensions:\n  tasks: deny\n");
        let pol = p.extension_policy();
        assert!(pol.types_allowed);
        assert!(!pol.tasks_allowed);
        assert!(!pol.providers_pinned);
    }

    #[test]
    fn provider_version_pin_parses() {
        let p = project_with("providers:\n  lammps:\n    version: 0.4.0\n");
        assert_eq!(
            p.provider_config("lammps").unwrap().version.as_deref(),
            Some("0.4.0")
        );
    }
}
