//! Spec registry: TaskSpec / WorkflowSpec loading, schema validation, and
//! version resolution (plan §52).
//!
//! Sources, lowest precedence first:
//!   1. the builtin library embedded in the binary (`tasks/`, `workflows/`)
//!   2. project registries (`tasks/`, `workflows/` under the project root,
//!      plus paths declared in `m3flow.yaml`)
//! Same `name@version` in a later source replaces the earlier entry.

use include_dir::{include_dir, Dir};
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::specs::{parse_ref, TaskSpec, WorkflowSpec};
use semver::Version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

static BUILTIN_TASKS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../tasks");
static BUILTIN_WORKFLOWS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../workflows");
static SCHEMAS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../schemas");

#[derive(Debug, Default)]
pub struct Registry {
    tasks: BTreeMap<String, BTreeMap<Version, TaskSpec>>,
    workflows: BTreeMap<String, BTreeMap<Version, WorkflowSpec>>,
    /// source path (or "<builtin>") per qualified name, for diagnostics
    origins: BTreeMap<String, String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with the embedded builtin library loaded.
    pub fn with_builtins() -> Result<Self> {
        let mut r = Self::new();
        r.load_dir_embedded(&BUILTIN_TASKS, "<builtin>/tasks")?;
        r.load_dir_embedded(&BUILTIN_WORKFLOWS, "<builtin>/workflows")?;
        Ok(r)
    }

    /// Add project-local registries on top of builtins.
    pub fn with_project(mut self, project_root: &Path, extra: &[PathBuf]) -> Result<Self> {
        for sub in ["tasks", "workflows"] {
            let dir = project_root.join(sub);
            if dir.is_dir() {
                self.load_dir_fs(&dir)?;
            }
        }
        for dir in extra {
            if dir.is_dir() {
                self.load_dir_fs(dir)?;
            }
        }
        Ok(self)
    }

    fn load_dir_embedded(&mut self, dir: &Dir, origin: &str) -> Result<()> {
        // Dir::files() is not recursive — walk explicitly.
        let mut stack: Vec<&Dir> = vec![dir];
        while let Some(d) = stack.pop() {
            for sub in d.dirs() {
                stack.push(sub);
            }
            for f in d.files() {
                let is_yaml = matches!(
                    f.path().extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                );
                if !is_yaml {
                    continue;
                }
                let text = f
                    .contents_utf8()
                    .ok_or_else(|| M3FlowError::internal("builtin spec is not UTF-8"))?;
                self.load_text(text, &format!("{origin}/{}", f.path().display()))?;
            }
        }
        Ok(())
    }

    pub fn load_dir_fs(&mut self, dir: &Path) -> Result<()> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d)
                .map_err(|e| M3FlowError::io(e, format!("reading {}", d.display())))?
            {
                let p = entry?.path();
                if p.is_dir() {
                    stack.push(p);
                } else if matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                ) {
                    let text = std::fs::read_to_string(&p)
                        .map_err(|e| M3FlowError::io(e, format!("reading {}", p.display())))?;
                    self.load_text(&text, &p.display().to_string())?;
                }
            }
        }
        Ok(())
    }

    /// Validate + register one spec document (task or workflow, sniffed by `schema:`).
    pub fn load_text(&mut self, text: &str, origin: &str) -> Result<()> {
        let json: serde_json::Value = serde_yaml::from_str(text)
            .map_err(|e| M3FlowError::schema(format!("{origin}: YAML parse failed: {e}")))?;
        let schema_tag = json
            .get("schema")
            .and_then(|s| s.as_str())
            .ok_or_else(|| M3FlowError::schema(format!("{origin}: missing 'schema' key")))?;
        match schema_tag {
            "task/v1" => {
                validate_against("task", &json).map_err(|e| prefix_err(origin, e))?;
                let spec = TaskSpec::from_json(&json).map_err(|e| prefix_err(origin, e))?;
                self.check_task_types(&spec)
                    .map_err(|e| prefix_err(origin, e))?;
                self.register_task(spec, origin);
            }
            "workflow/v1" => {
                validate_against("workflow", &json).map_err(|e| prefix_err(origin, e))?;
                let spec = WorkflowSpec::from_json(&json).map_err(|e| prefix_err(origin, e))?;
                self.check_workflow_types(&spec)
                    .map_err(|e| prefix_err(origin, e))?;
                self.register_workflow(spec, origin);
            }
            other => {
                return Err(M3FlowError::schema(format!(
                    "{origin}: unknown schema tag '{other}' (expected task/v1 or workflow/v1)"
                )))
            }
        }
        Ok(())
    }

    fn check_task_types(&self, spec: &TaskSpec) -> Result<()> {
        for decl in spec.inputs.values() {
            ensure_type(&decl.artifact_type, &spec.name)?;
        }
        for decl in spec.outputs.values() {
            ensure_type(&decl.artifact_type, &spec.name)?;
        }
        Ok(())
    }

    fn check_workflow_types(&self, spec: &WorkflowSpec) -> Result<()> {
        for decl in spec.inputs.values() {
            ensure_type(&decl.artifact_type, &spec.name)?;
        }
        Ok(())
    }

    fn register_task(&mut self, spec: TaskSpec, origin: &str) {
        let version = Version::parse(&spec.version).unwrap_or_else(|_| Version::new(0, 0, 0));
        self.origins
            .insert(format!("task:{}", spec.qualified()), origin.to_string());
        self.tasks
            .entry(spec.name.clone())
            .or_default()
            .insert(version, spec);
    }

    fn register_workflow(&mut self, spec: WorkflowSpec, origin: &str) {
        let version = Version::parse(&spec.version).unwrap_or_else(|_| Version::new(0, 0, 0));
        self.origins
            .insert(format!("workflow:{}", spec.qualified()), origin.to_string());
        self.workflows
            .entry(spec.name.clone())
            .or_default()
            .insert(version, spec);
    }

    /// Resolve `name` or `name@x.y.z`; bare names resolve to the highest version.
    pub fn task(&self, reference: &str) -> Result<&TaskSpec> {
        let (name, ver) = parse_ref(reference);
        let versions = self.tasks.get(&name).ok_or_else(|| {
            M3FlowError::not_found(format!(
                "task '{name}' is not registered (try `m3flow task list`)"
            ))
        })?;
        pick_version(versions, ver.as_deref(), "task", &name)
    }

    pub fn workflow(&self, reference: &str) -> Result<&WorkflowSpec> {
        let (name, ver) = parse_ref(reference);
        let versions = self.workflows.get(&name).ok_or_else(|| {
            M3FlowError::not_found(format!(
                "workflow '{name}' is not registered (try `m3flow workflow list`)"
            ))
        })?;
        pick_version(versions, ver.as_deref(), "workflow", &name)
    }

    pub fn has_workflow(&self, reference: &str) -> bool {
        self.workflow(reference).is_ok()
    }

    pub fn tasks(&self) -> Vec<&TaskSpec> {
        self.tasks
            .values()
            .filter_map(|vs| vs.values().next_back())
            .collect()
    }

    pub fn workflows(&self) -> Vec<&WorkflowSpec> {
        self.workflows
            .values()
            .filter_map(|vs| vs.values().next_back())
            .collect()
    }

    pub fn task_versions(&self, name: &str) -> Vec<&Version> {
        self.tasks
            .get(name)
            .map(|vs| vs.keys().collect())
            .unwrap_or_default()
    }

    pub fn workflow_versions(&self, name: &str) -> Vec<&Version> {
        self.workflows
            .get(name)
            .map(|vs| vs.keys().collect())
            .unwrap_or_default()
    }

    pub fn origin_of(&self, qualified: &str) -> Option<&str> {
        self.origins.get(qualified).map(|s| s.as_str())
    }

    pub fn search_tasks(&self, needle: &str) -> Vec<&TaskSpec> {
        let n = needle.to_lowercase();
        self.tasks()
            .into_iter()
            .filter(|t| {
                t.name.to_lowercase().contains(&n)
                    || t.description.to_lowercase().contains(&n)
                    || t.tags.iter().any(|tag| tag.to_lowercase().contains(&n))
            })
            .collect()
    }
}

fn ensure_type(t: &str, owner: &str) -> Result<()> {
    if m3flow_core::atypes::is_known_type(t) {
        Ok(())
    } else {
        Err(M3FlowError::schema(format!(
            "'{owner}' references unknown artifact type '{t}' (see `m3flow schema list`)"
        )))
    }
}

fn pick_version<'a, T>(
    versions: &'a BTreeMap<Version, T>,
    want: Option<&str>,
    kind: &str,
    name: &str,
) -> Result<&'a T> {
    match want {
        Some(v) => {
            let v = Version::parse(v)
                .map_err(|_| M3FlowError::schema(format!("bad version '{v}' in {kind} ref")))?;
            versions.get(&v).ok_or_else(|| {
                let have: Vec<String> = versions.keys().map(|k| k.to_string()).collect();
                M3FlowError::not_found(format!(
                    "{kind} '{name}@{v}' not registered; available: {}",
                    have.join(", ")
                ))
            })
        }
        None => versions.values().next_back().ok_or_else(|| {
            M3FlowError::not_found(format!("no versions of {kind} '{name}' registered"))
        }),
    }
}

fn prefix_err(origin: &str, e: M3FlowError) -> M3FlowError {
    match e {
        M3FlowError::Schema { message, details } => M3FlowError::Schema {
            message: format!("{origin}: {message}"),
            details,
        },
        other => other,
    }
}

// ------------------------------------------------------------- validation

fn schema_json(name: &str) -> serde_json::Value {
    let f = SCHEMAS
        .get_file(format!("{name}.schema.json"))
        .unwrap_or_else(|| panic!("missing embedded schema {name}.schema.json"));
    serde_json::from_str(f.contents_utf8().unwrap()).expect("embedded schema must be valid JSON")
}

pub fn validate_against(name: &str, doc: &serde_json::Value) -> Result<()> {
    let schema = schema_json(name);
    let validator = jsonschema::validator_for(&schema)
        .map_err(|e| M3FlowError::internal(format!("schema compile: {e}")))?;
    let mut details = Vec::new();
    for err in validator.iter_errors(doc) {
        details.push(format!("{}: {}", err.instance_path, err));
        if details.len() >= 8 {
            break;
        }
    }
    if details.is_empty() {
        Ok(())
    } else {
        Err(M3FlowError::Schema {
            message: format!("document failed {name}.schema.json validation"),
            details,
        })
    }
}

/// Validate a SystemSpec document (used by `workflow validate` and project tooling).
pub fn validate_system_spec(doc: &serde_json::Value) -> Result<()> {
    validate_against("system", doc)
}

pub fn schema_text(name: &str) -> Option<String> {
    SCHEMAS
        .get_file(format!("{name}.schema.json"))
        .and_then(|f| f.contents_utf8())
        .map(|s| s.to_string())
}
