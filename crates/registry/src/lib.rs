//! Spec registry: TaskSpec / WorkflowSpec / type-pack loading, schema
//! validation, and version resolution (plan §52).
//!
//! Sources, lowest precedence first:
//!   1. the builtin library embedded in the binary (`tasks/`, `workflows/`)
//!   2. project registries (`types/`, `tasks/`, `workflows/` under the project
//!      root, plus paths declared in `m3flow.yaml`)
//!
//! Same `name@version` in a later source replaces the earlier entry.
//! Artifact types behave differently: `types/v1` packs may only add new
//! types under an existing parent — redefining a known type is an error
//! (type names live inside artifact records and provenance chains).
//! Within each source, type packs register before specs, so a spec may
//! reference a type its own batch defines.

use include_dir::{include_dir, Dir};
use m3flow_core::atypes::TypeSet;
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::specs::{parse_ref, TaskSpec, WorkflowSpec};
use semver::Version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

static BUILTIN_TASKS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../tasks");
static BUILTIN_WORKFLOWS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../workflows");
static SCHEMAS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../schemas");

/// One parsed registry document plus its origin (path or "<builtin>/...").
type Doc = (serde_json::Value, String);

/// What project-level sources may add. Builtins are always loaded; this
/// gates only docs arriving via `with_project`/`load_text` (a production-
/// mode project sets kinds to `false`, turning extension into a load error
/// naming the offending file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadPolicy {
    /// `types/v1` packs may register new artifact types.
    pub types: bool,
    /// `task/v1` and `workflow/v1` documents may register specs.
    pub specs: bool,
}

impl Default for LoadPolicy {
    fn default() -> Self {
        Self {
            types: true,
            specs: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Registry {
    tasks: BTreeMap<String, BTreeMap<Version, TaskSpec>>,
    workflows: BTreeMap<String, BTreeMap<Version, WorkflowSpec>>,
    /// source path (or "<builtin>") per qualified name, for diagnostics
    origins: BTreeMap<String, String>,
    /// builtin artifact types + any loaded `types/v1` packs
    types: TypeSet,
    /// gates project-sourced documents (builtins are exempt)
    policy: LoadPolicy,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with the embedded builtin library loaded.
    pub fn with_builtins() -> Result<Self> {
        let mut r = Self::new();
        let mut docs = Vec::new();
        collect_embedded(&BUILTIN_TASKS, "<builtin>/tasks", &mut docs)?;
        collect_embedded(&BUILTIN_WORKFLOWS, "<builtin>/workflows", &mut docs)?;
        r.load_docs(docs)?;
        Ok(r)
    }

    /// Add project-local registries on top of builtins, under `policy`.
    /// A doc whose kind the policy denies is a load error naming the file —
    /// loud at `open_registry` time rather than a silent skip that surfaces
    /// later as "unknown task".
    pub fn with_project(
        mut self,
        project_root: &Path,
        extra: &[PathBuf],
        policy: LoadPolicy,
    ) -> Result<Self> {
        self.policy = policy;
        let mut docs = Vec::new();
        for sub in ["types", "tasks", "workflows"] {
            let dir = project_root.join(sub);
            if dir.is_dir() {
                collect_fs(&dir, &mut docs)?;
            }
        }
        for dir in extra {
            if dir.is_dir() {
                collect_fs(dir, &mut docs)?;
            }
        }
        self.load_docs(docs)?;
        Ok(self)
    }

    /// Validate + register one document (task, workflow, or type pack,
    /// sniffed by `schema:`). For batches use the dir loaders — they register
    /// type packs before specs regardless of file order.
    pub fn load_text(&mut self, text: &str, origin: &str) -> Result<()> {
        let json = parse_doc(text, origin)?;
        self.load_docs(vec![(json, origin.to_string())])
    }

    /// Register a batch of documents in two phases: type packs first, so a
    /// spec may reference a type its own batch defines.
    fn load_docs(&mut self, docs: Vec<Doc>) -> Result<()> {
        for (json, origin) in &docs {
            if schema_tag(json) == Some("types/v1") {
                if !self.policy.types {
                    return Err(M3FlowError::schema(format!(
                        "{origin}: type packs are disabled by project policy \
                         (extensions.types: deny, e.g. mode: production)"
                    )));
                }
                self.load_type_pack(json, origin)?;
            }
        }
        for (json, origin) in docs {
            match schema_tag(&json) {
                Some("types/v1") => {} // registered in phase one
                Some("task/v1") | Some("workflow/v1") if !self.policy.specs => {
                    return Err(M3FlowError::schema(format!(
                        "{origin}: project task/workflow specs are disabled by project policy \
                         (extensions.tasks: deny, e.g. mode: production)"
                    )));
                }
                Some("task/v1") => {
                    validate_against("task", &json).map_err(|e| prefix_err(&origin, e))?;
                    let spec = TaskSpec::from_json(&json).map_err(|e| prefix_err(&origin, e))?;
                    self.check_task_types(&spec)
                        .map_err(|e| prefix_err(&origin, e))?;
                    self.register_task(spec, &origin);
                }
                Some("workflow/v1") => {
                    validate_against("workflow", &json).map_err(|e| prefix_err(&origin, e))?;
                    let spec =
                        WorkflowSpec::from_json(&json).map_err(|e| prefix_err(&origin, e))?;
                    self.check_workflow_types(&spec)
                        .map_err(|e| prefix_err(&origin, e))?;
                    self.register_workflow(spec, &origin);
                }
                Some(other) => {
                    return Err(M3FlowError::schema(format!(
                        "{origin}: unknown schema tag '{other}' (expected task/v1, workflow/v1, or types/v1)"
                    )))
                }
                None => {
                    return Err(M3FlowError::schema(format!(
                        "{origin}: missing 'schema' key"
                    )))
                }
            }
        }
        Ok(())
    }

    fn load_type_pack(&mut self, json: &serde_json::Value, origin: &str) -> Result<()> {
        validate_against("types", json).map_err(|e| prefix_err(origin, e))?;
        let entries = parse_type_pack(json).map_err(|e| prefix_err(origin, e))?;
        // add_pack errors already carry the origin
        self.types.add_pack(&entries, origin)
    }

    /// The merged artifact-type view: builtins plus loaded packs.
    pub fn types(&self) -> &TypeSet {
        &self.types
    }

    fn check_task_types(&self, spec: &TaskSpec) -> Result<()> {
        for decl in spec.inputs.values() {
            self.ensure_type(&decl.artifact_type, &spec.name)?;
        }
        for decl in spec.outputs.values() {
            self.ensure_type(&decl.artifact_type, &spec.name)?;
        }
        Ok(())
    }

    fn check_workflow_types(&self, spec: &WorkflowSpec) -> Result<()> {
        for decl in spec.inputs.values() {
            self.ensure_type(&decl.artifact_type, &spec.name)?;
        }
        Ok(())
    }

    fn ensure_type(&self, t: &str, owner: &str) -> Result<()> {
        if self.types.is_known_type(t) {
            Ok(())
        } else {
            Err(M3FlowError::schema(format!(
                "'{owner}' references unknown artifact type '{t}' (see `m3flow schema list`)"
            )))
        }
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

fn schema_tag(json: &serde_json::Value) -> Option<&str> {
    json.get("schema").and_then(|s| s.as_str())
}

fn parse_doc(text: &str, origin: &str) -> Result<serde_json::Value> {
    serde_yaml::from_str(text)
        .map_err(|e| M3FlowError::schema(format!("{origin}: YAML parse failed: {e}")))
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}

fn collect_embedded(dir: &Dir, origin: &str, out: &mut Vec<Doc>) -> Result<()> {
    // Dir::files() is not recursive — walk explicitly.
    let mut stack: Vec<&Dir> = vec![dir];
    while let Some(d) = stack.pop() {
        for sub in d.dirs() {
            stack.push(sub);
        }
        for f in d.files() {
            if !is_yaml_file(f.path()) {
                continue;
            }
            let text = f
                .contents_utf8()
                .ok_or_else(|| M3FlowError::internal("builtin spec is not UTF-8"))?;
            let origin = format!("{origin}/{}", f.path().display());
            out.push((parse_doc(text, &origin)?, origin));
        }
    }
    Ok(())
}

fn collect_fs(dir: &Path, out: &mut Vec<Doc>) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)
            .map_err(|e| M3FlowError::io(e, format!("reading {}", d.display())))?
        {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if is_yaml_file(&p) {
                let origin = p.display().to_string();
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| M3FlowError::io(e, format!("reading {}", p.display())))?;
                out.push((parse_doc(&text, &origin)?, origin));
            }
        }
    }
    Ok(())
}

/// Parse the `types:` mapping of a `types/v1` pack into (name, parent) pairs.
fn parse_type_pack(json: &serde_json::Value) -> Result<Vec<(String, String)>> {
    let map = json
        .get("types")
        .and_then(|t| t.as_object())
        .ok_or_else(|| M3FlowError::schema("types/v1: missing 'types' mapping".to_string()))?;
    let mut out = Vec::new();
    for (name, parent) in map {
        let p = parent.as_str().ok_or_else(|| {
            M3FlowError::schema(format!(
                "types/v1: parent of '{name}' must be a type name string"
            ))
        })?;
        out.push((name.clone(), p.to_string()));
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    const PACK: &str = r#"
schema: types/v1
pack: widget
types:
  WidgetSystem: System
  WidgetResult: Result
"#;

    fn task_doc(input_type: &str, output_type: &str) -> String {
        format!(
            r#"
schema: task/v1
name: use_widget
version: 1.0.0
description: test task
category: analysis
inputs:
  sys:
    type: {input_type}
parameters: {{}}
outputs:
  out:
    type: {output_type}
"#
        )
    }

    #[test]
    fn pack_then_task_registers_new_types() {
        let mut reg = Registry::with_builtins().unwrap();
        reg.load_text(PACK, "widget/types.yaml").unwrap();
        reg.load_text(
            &task_doc("WidgetSystem", "WidgetResult"),
            "widget/task.yaml",
        )
        .unwrap();
        assert!(reg.types().is_known_type("WidgetSystem"));
        assert!(reg.types().is_subtype("WidgetSystem", "System"));
    }

    #[test]
    fn task_referencing_pack_type_without_pack_fails() {
        let mut reg = Registry::with_builtins().unwrap();
        let e = reg
            .load_text(&task_doc("WidgetSystem", "WidgetResult"), "task.yaml")
            .unwrap_err();
        assert!(e
            .to_string()
            .contains("unknown artifact type 'WidgetSystem'"));
    }

    #[test]
    fn two_phase_load_order_independent() {
        // task doc appears BEFORE the pack that defines its types
        let docs = vec![
            (
                parse_doc(&task_doc("WidgetSystem", "WidgetResult"), "task.yaml").unwrap(),
                "task.yaml".to_string(),
            ),
            (
                parse_doc(PACK, "types.yaml").unwrap(),
                "types.yaml".to_string(),
            ),
        ];
        let mut reg = Registry::with_builtins().unwrap();
        reg.load_docs(docs).unwrap();
        assert!(reg.task("use_widget").is_ok());
    }

    #[test]
    fn duplicate_type_across_packs_fails() {
        let mut reg = Registry::with_builtins().unwrap();
        reg.load_text(PACK, "a/types.yaml").unwrap();
        let e = reg.load_text(PACK, "b/types.yaml").unwrap_err();
        assert!(e.to_string().contains("already defined by a/types.yaml"));
    }

    #[test]
    fn pack_with_unknown_parent_fails() {
        let mut reg = Registry::with_builtins().unwrap();
        let bad = "schema: types/v1\ntypes:\n  Orphan: NoSuchType\n";
        let e = reg.load_text(bad, "bad.yaml").unwrap_err();
        assert!(e.to_string().contains("unresolvable parent types"));
    }

    #[test]
    fn builtin_types_unchanged_without_packs() {
        let reg = Registry::with_builtins().unwrap();
        assert!(reg.types().is_subtype("EquilibratedState", "State"));
        assert!(!reg.types().is_known_type("WidgetSystem"));
        // 28 builtin types + root
        assert_eq!(reg.types().all_types().len(), 29);
    }

    fn locked_registry(types: bool, specs: bool) -> Registry {
        Registry::with_builtins()
            .unwrap()
            .with_project(
                Path::new("/nonexistent-project-root"),
                &[],
                LoadPolicy { types, specs },
            )
            .unwrap()
    }

    #[test]
    fn policy_denies_type_packs() {
        let mut reg = locked_registry(false, true);
        let e = reg.load_text(PACK, "proj/types.yaml").unwrap_err();
        assert!(e.to_string().contains("type packs are disabled"));
        // specs still allowed
        reg.load_text(&task_doc("SystemSpec", "DensityResult"), "t.yaml")
            .unwrap();
    }

    #[test]
    fn policy_denies_specs() {
        let mut reg = locked_registry(true, false);
        let e = reg
            .load_text(&task_doc("SystemSpec", "DensityResult"), "t.yaml")
            .unwrap_err();
        assert!(e.to_string().contains("specs are disabled"));
        // type packs still allowed
        reg.load_text(PACK, "types.yaml").unwrap();
    }

    #[test]
    fn default_policy_allows_everything() {
        let mut reg = locked_registry(true, true);
        reg.load_text(PACK, "types.yaml").unwrap();
        reg.load_text(&task_doc("WidgetSystem", "WidgetResult"), "t.yaml")
            .unwrap();
    }
}
