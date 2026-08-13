//! High-level run/inspection API used by the CLI and the TUI (plan §40–§43).

use crate::compile::Compiler;
use crate::db::{Db, WorkflowRunRecord};
use crate::ir::CompiledWorkflow;
use crate::project::Project;
use crate::scheduler::{self, ResumedNode, RunContext};
use crate::store::Store;
use m3flow_core::artifact::{now_rfc3339, RunStatus, TaskStatus};
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::id::{ArtifactId, WorkflowRunId};
use m3flow_registry::Registry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// How a workflow input is supplied on the command line.
#[derive(Debug, Clone)]
pub enum InputSource {
    /// Existing artifact id.
    Artifact(String),
    /// File to register as a new artifact.
    File(PathBuf),
}

pub struct RunOptions {
    pub inputs: BTreeMap<String, InputSource>,
    pub params: serde_json::Map<String, serde_json::Value>,
    pub no_cache: bool,
    pub max_concurrency: Option<usize>,
    pub progress: Option<Box<dyn Fn(&str) + Send>>,
}

// ------------------------------------------------------------------ context

pub fn open_registry(project: &Project) -> Result<Registry> {
    Registry::with_builtins()?.with_project(&project.root, &project.extra_registry_dirs())
}

pub fn open_db(project: &Project) -> Result<Db> {
    Db::open(&project.db_path())
}

pub fn open_store(project: &Project) -> Result<Store> {
    Store::new(project.artifacts_dir())
}

// ------------------------------------------------------------------ running

/// Compile a workflow without executing it (`workflow plan`, `run --dry-run`).
pub fn plan_workflow(
    project: &Project,
    workflow_ref: &str,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<CompiledWorkflow> {
    let registry = open_registry(project)?;
    let spec = registry.workflow(workflow_ref)?.clone();
    Compiler::new(&registry).compile(&spec, params)
}

pub fn run_workflow(
    project: &Project,
    workflow_ref: &str,
    opts: RunOptions,
) -> Result<WorkflowRunRecord> {
    let registry = open_registry(project)?;
    let db = open_db(project)?;
    let store = open_store(project)?;
    let spec = registry.workflow(workflow_ref)?.clone();
    let compiled = Compiler::new(&registry).compile(&spec, &opts.params)?;

    let workflow_inputs = bind_inputs(&compiled, &opts.inputs, &db, &store)?;

    let run = WorkflowRunRecord {
        id: WorkflowRunId::new(),
        name: compiled.name.clone(),
        version: compiled.version.clone(),
        spec_hash: compiled.spec_hash.clone(),
        status: RunStatus::Pending,
        created_at: now_rfc3339(),
        started_at: None,
        ended_at: None,
        workdir: project
            .runs_dir()
            .display()
            .to_string(),
        git: Some(crate::project::git_context(&project.root)),
        inputs: serde_json::to_value(
            workflow_inputs
                .iter()
                .map(|(k, v)| (k.clone(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
        .unwrap_or_default(),
        params: compiled.params.clone(),
        outputs: None,
        error: None,
    };
    std::fs::create_dir_all(project.runs_dir().join(run.id.as_str()))
        .map_err(|e| M3FlowError::io(e, "creating run dir"))?;
    db.insert_workflow_run(&run)?;

    let ctx = RunContext {
        max_concurrency: opts.max_concurrency.unwrap_or_else(|| project.max_concurrency()),
        no_cache: opts.no_cache,
        progress: opts.progress,
        resume: BTreeMap::new(),
        project: project.clone(),
        registry,
        db,
        store,
        compiled,
        workflow_inputs,
        run,
    };
    scheduler::execute(ctx)
}

/// Resume an interrupted/failed run: successful nodes are kept, everything
/// else re-executes (cache still applies to re-dispatched nodes).
pub fn resume_run(
    project: &Project,
    run_id: &str,
    progress: Option<Box<dyn Fn(&str) + Send>>,
) -> Result<WorkflowRunRecord> {
    resume_impl(project, run_id, &BTreeSet::new(), progress)
}

/// Retry one node (by unique id prefix) and everything downstream of it.
pub fn retry_step(
    project: &Project,
    run_id: &str,
    node_prefix: &str,
    progress: Option<Box<dyn Fn(&str) + Send>>,
) -> Result<WorkflowRunRecord> {
    let registry = open_registry(project)?;
    let db = open_db(project)?;
    let rec = db.get_workflow_run(run_id)?;
    let compiled = recompile(&registry, &rec)?;

    let target = unique_node(&compiled, node_prefix)?;
    // target + transitive dependents must re-run
    let mut reset = BTreeSet::from([target.clone()]);
    let mut changed = true;
    while changed {
        changed = false;
        for n in &compiled.nodes {
            if !reset.contains(&n.id) && n.deps.iter().any(|d| reset.contains(d)) {
                reset.insert(n.id.clone());
                changed = true;
            }
        }
    }
    resume_impl(project, run_id, &reset, progress)
}

fn resume_impl(
    project: &Project,
    run_id: &str,
    force_reset: &BTreeSet<String>,
    progress: Option<Box<dyn Fn(&str) + Send>>,
) -> Result<WorkflowRunRecord> {
    let registry = open_registry(project)?;
    let db = open_db(project)?;
    let store = open_store(project)?;
    let rec = db.get_workflow_run(run_id)?;
    if rec.status == RunStatus::Running {
        return Err(M3FlowError::workflow(
            format!("run '{run_id}' is still marked RUNNING; cancel it first"),
            None,
        ));
    }
    let compiled = recompile(&registry, &rec)?;

    let mut workflow_inputs = BTreeMap::new();
    if let Some(obj) = rec.inputs.as_object() {
        for (k, v) in obj {
            let id = v.as_str().and_then(ArtifactId::parse).ok_or_else(|| {
                M3FlowError::workflow(format!("run '{run_id}' has malformed input '{k}'"), None)
            })?;
            workflow_inputs.insert(k.clone(), id);
        }
    }

    let mut resume = BTreeMap::new();
    for tr in db.task_runs_of(run_id)? {
        if force_reset.contains(&tr.node_id) {
            continue;
        }
        if matches!(
            tr.status,
            TaskStatus::Completed | TaskStatus::Cached | TaskStatus::Skipped
        ) {
            let mut outputs = BTreeMap::new();
            for (oname, art_id) in db.outputs_of(tr.id.as_str())? {
                if let Some(parsed) = ArtifactId::parse(&art_id) {
                    outputs.insert(oname, parsed);
                }
            }
            resume.insert(
                tr.node_id.clone(),
                ResumedNode {
                    status: tr.status,
                    task_run: Some(tr.id.clone()),
                    outputs,
                },
            );
        }
    }

    let mut run = rec;
    run.status = RunStatus::Pending;
    run.ended_at = None;
    run.error = None;
    let ctx = RunContext {
        max_concurrency: project.max_concurrency(),
        no_cache: false,
        progress,
        resume,
        project: project.clone(),
        registry,
        db,
        store,
        compiled,
        workflow_inputs,
        run,
    };
    scheduler::execute(ctx)
}

/// Signal cancellation: the scheduler polls for this flag file.
pub fn cancel_run(project: &Project, run_id: &str) -> Result<()> {
    let dir = project.runs_dir().join(run_id);
    if !dir.is_dir() {
        return Err(M3FlowError::not_found(format!("run '{run_id}'")));
    }
    std::fs::write(dir.join("CANCEL"), "cancel requested\n")
        .map_err(|e| M3FlowError::io(e, "writing CANCEL flag"))
}

fn recompile(registry: &Registry, rec: &WorkflowRunRecord) -> Result<CompiledWorkflow> {
    let spec = registry
        .workflow(&format!("{}@{}", rec.name, rec.version))
        .map_err(|e| {
            M3FlowError::workflow(
                format!(
                    "cannot resume: workflow '{}@{}' no longer in registry ({e})",
                    rec.name, rec.version
                ),
                None,
            )
        })?
        .clone();
    let params = rec.params.as_object().cloned().unwrap_or_default();
    Compiler::new(registry).compile(&spec, &params)
}

fn unique_node(compiled: &CompiledWorkflow, prefix: &str) -> Result<String> {
    let matches: Vec<&str> = compiled
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .filter(|id| *id == prefix || id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(M3FlowError::not_found(format!(
            "no step matching '{prefix}' in workflow '{}'",
            compiled.name
        ))),
        1 => Ok(matches[0].to_string()),
        _ => Err(M3FlowError::workflow(
            format!("step prefix '{prefix}' is ambiguous: {}", matches.join(", ")),
            None,
        )),
    }
}

// ------------------------------------------------------------------ inputs

fn bind_inputs(
    compiled: &CompiledWorkflow,
    sources: &BTreeMap<String, InputSource>,
    db: &Db,
    store: &Store,
) -> Result<BTreeMap<String, ArtifactId>> {
    // reject undeclared input names early
    for name in sources.keys() {
        if !compiled.inputs_decl.contains_key(name) {
            return Err(M3FlowError::schema(format!(
                "unknown input '{name}'; workflow '{}' declares: {}",
                compiled.name,
                compiled
                    .inputs_decl
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut bound = BTreeMap::new();
    for (name, decl) in &compiled.inputs_decl {
        match sources.get(name) {
            None => {
                if decl.required {
                    return Err(M3FlowError::schema(format!(
                        "required input '{name}' (type {}) not provided",
                        decl.artifact_type
                    )));
                }
            }
            Some(InputSource::Artifact(id)) => {
                let parsed = ArtifactId::parse(id)
                    .ok_or_else(|| M3FlowError::schema(format!("malformed artifact id '{id}'")))?;
                let artifact = db.get_artifact(id)?;
                if !m3flow_core::atypes::is_subtype(&artifact.artifact_type, &decl.artifact_type) {
                    return Err(M3FlowError::ArtifactCompatibility {
                        message: format!(
                            "input '{name}': artifact {} has type {} but the workflow requires {}",
                            id, artifact.artifact_type, decl.artifact_type
                        ),
                        missing: vec![],
                    });
                }
                bound.insert(name.clone(), parsed);
            }
            Some(InputSource::File(path)) => {
                let id = register_input_file(store, db, path, &decl.artifact_type)?;
                bound.insert(name.clone(), id);
            }
        }
    }
    Ok(bound)
}

/// Register a file as an input artifact. Spec-family artifacts carry the
/// document under the conventional `spec` file key plus its parsed form as
/// `data`; other artifacts keep the file under its basename.
fn register_input_file(
    store: &Store,
    db: &Db,
    path: &PathBuf,
    decl_type: &str,
) -> Result<ArtifactId> {
    if !path.is_file() {
        return Err(M3FlowError::not_found(format!("input file '{}'", path.display())));
    }
    if m3flow_core::atypes::is_subtype(decl_type, "Spec") {
        let text = std::fs::read_to_string(path)
            .map_err(|e| M3FlowError::io(e, format!("reading {}", path.display())))?;
        let json: serde_json::Value = serde_yaml::from_str(&text).map_err(|e| {
            M3FlowError::schema(format!("{}: not valid YAML/JSON: {e}", path.display()))
        })?;
        if decl_type == "SystemSpec" {
            m3flow_registry::validate_system_spec(&json)?;
        }
        let name = json
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("unnamed")
            .to_string();
        let mut files = BTreeMap::new();
        files.insert("spec".to_string(), path.clone());
        let (artifact, rows) = store.register_files(
            decl_type,
            &files,
            serde_json::json!({"name": name}),
            Some(json),
            None,
        )?;
        let id = artifact.id.clone();
        db.insert_artifact(&artifact, &rows)?;
        Ok(id)
    } else {
        let basename = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let mut files = BTreeMap::new();
        files.insert(basename, path.clone());
        let (artifact, rows) =
            store.register_files(decl_type, &files, serde_json::json!({}), None, None)?;
        let id = artifact.id.clone();
        db.insert_artifact(&artifact, &rows)?;
        Ok(id)
    }
}

/// Register a free-standing artifact (`artifact register`).
pub fn register_artifact(
    project: &Project,
    artifact_type: &str,
    paths: &BTreeMap<String, PathBuf>,
    metadata: serde_json::Value,
    data: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    if !m3flow_core::atypes::is_known_type(artifact_type) {
        return Err(M3FlowError::schema(format!(
            "unknown artifact type '{artifact_type}'"
        )));
    }
    let db = open_db(project)?;
    let store = open_store(project)?;
    let (artifact, rows) = store.register_files(artifact_type, paths, metadata, data, None)?;
    let out = artifact.summary();
    db.insert_artifact(&artifact, &rows)?;
    Ok(out)
}

// ------------------------------------------------------------------ inspect

pub fn run_json(project: &Project, run_id: &str) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let rec = db.get_workflow_run(run_id)?;
    let tasks = db.task_runs_of(rec.id.as_str())?;
    Ok(serde_json::json!({
        "run": rec,
        "tasks": tasks,
    }))
}

pub fn list_runs_json(project: &Project, limit: usize) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let runs = db.list_workflow_runs(limit)?;
    Ok(serde_json::json!({"runs": runs}))
}

/// Graph of a run: nodes + dependency edges + live statuses.
pub fn run_graph_json(project: &Project, run_id: &str) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let rec = db.get_workflow_run(run_id)?;
    let tasks = db.task_runs_of(rec.id.as_str())?;
    let status_of: BTreeMap<String, String> = tasks
        .iter()
        .map(|t| (t.node_id.clone(), t.status.as_str().to_string()))
        .collect();

    let registry = open_registry(project)?;
    let (nodes, edges) = match recompile(&registry, &rec) {
        Ok(compiled) => {
            let nodes: Vec<_> = compiled
                .nodes
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.id,
                        "task": n.task,
                        "label": n.label,
                        "status": status_of.get(&n.id).cloned().unwrap_or_else(|| "PENDING".into()),
                    })
                })
                .collect();
            let edges: Vec<_> = compiled
                .nodes
                .iter()
                .flat_map(|n| {
                    n.deps
                        .iter()
                        .map(|d| serde_json::json!({"from": d, "to": n.id}))
                        .collect::<Vec<_>>()
                })
                .collect();
            (nodes, edges)
        }
        Err(_) => {
            // workflow no longer in registry: report nodes without edges
            let nodes = tasks
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.node_id,
                        "task": format!("{}@{}", t.task_name, t.task_version),
                        "label": t.node_id,
                        "status": t.status.as_str(),
                    })
                })
                .collect();
            (nodes, Vec::new())
        }
    };
    Ok(serde_json::json!({
        "run": run_id,
        "workflow": format!("{}@{}", rec.name, rec.version),
        "nodes": nodes,
        "edges": edges,
    }))
}

pub fn artifact_json(project: &Project, artifact_id: &str) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let store = open_store(project)?;
    let a = db.get_artifact(artifact_id)?;
    let mut files = serde_json::Map::new();
    for (name, rel) in &a.files {
        files.insert(
            name.clone(),
            serde_json::json!({
                "path": store.resolve(rel).display().to_string(),
                "store_relpath": rel,
            }),
        );
    }
    let producer = match &a.producer {
        Some(tr) => db
            .task_run_by_id(tr.as_str())
            .ok()
            .map(|t| serde_json::json!({
                "task_run_id": t.id.as_str(),
                "workflow_run_id": t.workflow_run_id.as_str(),
                "node_id": t.node_id,
                "task": format!("{}@{}", t.task_name, t.task_version),
            })),
        None => None,
    };
    Ok(serde_json::json!({
        "id": a.id.as_str(),
        "type": a.artifact_type,
        "schema_version": a.schema_version,
        "content_hash": a.content_hash,
        "created_at": a.created_at,
        "files": files,
        "metadata": a.metadata,
        "data": a.data,
        "producer": producer,
    }))
}

pub fn list_artifacts_json(
    project: &Project,
    type_filter: Option<&str>,
    limit: usize,
) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let arts = db.list_artifacts(type_filter, limit)?;
    let summaries: Vec<_> = arts.iter().map(|a| a.summary()).collect();
    Ok(serde_json::json!({"artifacts": summaries}))
}

/// Full upward lineage: artifact → producer task run → its inputs → ...
pub fn lineage_json(project: &Project, artifact_id: &str) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    lineage_of(&db, artifact_id, 0)
}

fn lineage_of(db: &Db, artifact_id: &str, depth: usize) -> Result<serde_json::Value> {
    if depth > 32 {
        return Ok(serde_json::json!({"truncated": true}));
    }
    let a = db.get_artifact(artifact_id)?;
    let mut out = serde_json::json!({
        "id": a.id.as_str(),
        "type": a.artifact_type,
        "content_hash": a.content_hash,
    });
    if let Some(tr_id) = &a.producer {
        if let Ok(tr) = db.task_run_by_id(tr_id.as_str()) {
            let mut inputs = Vec::new();
            for (iname, aid) in db.inputs_of(tr.id.as_str())? {
                inputs.push(serde_json::json!({
                    "input": iname,
                    "artifact": lineage_of(db, &aid, depth + 1)?,
                }));
            }
            out["producer"] = serde_json::json!({
                "task_run_id": tr.id.as_str(),
                "workflow_run_id": tr.workflow_run_id.as_str(),
                "node_id": tr.node_id,
                "task": format!("{}@{}", tr.task_name, tr.task_version),
                "status": tr.status.as_str(),
                "inputs": inputs,
            });
        }
    }
    Ok(out)
}

/// Which inputs of `task_ref` could accept this artifact?
pub fn compatible_json(
    project: &Project,
    artifact_id: &str,
    task_ref: &str,
) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let registry = open_registry(project)?;
    let a = db.get_artifact(artifact_id)?;
    let task = registry.task(task_ref)?;
    let mut matches = Vec::new();
    for (iname, decl) in &task.inputs {
        if m3flow_core::atypes::is_subtype(&a.artifact_type, &decl.artifact_type) {
            matches.push(serde_json::json!({
                "input": iname,
                "declared_type": decl.artifact_type,
            }));
        }
    }
    Ok(serde_json::json!({
        "artifact": artifact_id,
        "artifact_type": a.artifact_type,
        "task": format!("{}@{}", task.name, task.version),
        "compatible": !matches.is_empty(),
        "matching_inputs": matches,
    }))
}

// ------------------------------------------------------------------ cache

pub fn cache_stats_json(project: &Project) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let (entries, artifacts) = db.cache_stats()?;
    Ok(serde_json::json!({
        "cache_entries": entries,
        "artifacts": artifacts,
    }))
}

pub fn cache_clear(project: &Project) -> Result<usize> {
    let db = open_db(project)?;
    let n = db
        .conn()
        .execute("DELETE FROM cache_entry", [])
        .map_err(|e| M3FlowError::internal(format!("cache clear: {e}")))?;
    Ok(n)
}
