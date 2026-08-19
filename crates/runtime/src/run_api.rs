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
    /// Disable the friendly results/ tree for this run (overrides the
    /// project default; presentation-only, never part of any fingerprint).
    pub no_materialize: bool,
    /// Study/group folder for the friendly results/ tree.
    /// Presentation-only: never part of spec_hash, cache keys, or artifact
    /// identity. `None` → the tree groups by workflow name.
    pub label: Option<String>,
    pub max_concurrency: Option<usize>,
    /// `--executor` CLI override for this run. Scheduling-only: never part of
    /// spec_hash, cache keys, or artifact identity.
    pub executor_override: Option<crate::project::ExecutorKind>,
    pub progress: Option<Box<dyn Fn(&str) + Send>>,
}

/// A label becomes a folder name in the friendly results/ tree, so it is
/// validated eagerly rather than sanitized silently: first char
/// `[A-Za-z0-9]`, the rest `[A-Za-z0-9._-]`, max 64 chars. Two different
/// studies must never sanitize-merge into one folder.
pub fn validate_label(label: &str) -> Result<String> {
    let ok = !label.is_empty()
        && label.len() <= 64
        && label
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(label.to_string())
    } else {
        Err(M3FlowError::schema(format!(
            "invalid run label '{label}': use 1-64 chars from [A-Za-z0-9._-], starting with a letter or digit"
        )))
    }
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
    let label = opts.label.as_deref().map(validate_label).transpose()?;

    let run = WorkflowRunRecord {
        id: WorkflowRunId::new(),
        name: compiled.name.clone(),
        version: compiled.version.clone(),
        spec_hash: compiled.spec_hash.clone(),
        status: RunStatus::Pending,
        created_at: now_rfc3339(),
        started_at: None,
        ended_at: None,
        workdir: project.runs_dir().display().to_string(),
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
        label,
    };
    std::fs::create_dir_all(project.runs_dir().join(run.id.as_str()))
        .map_err(|e| M3FlowError::io(e, "creating run dir"))?;
    db.insert_workflow_run(&run)?;

    let ctx = RunContext {
        max_concurrency: opts
            .max_concurrency
            .unwrap_or_else(|| project.max_concurrency()),
        no_cache: opts.no_cache,
        materialize: project.materialize_enabled() && !opts.no_materialize,
        executor_override: opts.executor_override,
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
    finalize_on_error(scheduler::execute(ctx), project)
}

/// If the scheduler dies with a hard error, the run row must not stay
/// RUNNING — mark it FAILED before propagating.
fn finalize_on_error(
    result: Result<WorkflowRunRecord>,
    project: &Project,
) -> Result<WorkflowRunRecord> {
    match result {
        Ok(rec) => Ok(rec),
        Err(e) => {
            if let Ok(db) = open_db(project) {
                // best-effort: mark non-terminal task rows CANCELLED, run FAILED
                let _ = db.conn().execute(
                    "UPDATE task_run SET status='CANCELLED' WHERE workflow_run_id IN (SELECT id FROM workflow_run WHERE status='RUNNING') AND status IN ('RUNNING','PENDING','READY')",
                    [],
                );
                let _ = db.conn().execute(
                    "UPDATE workflow_run SET status='FAILED', ended_at=?1, error_json=?2 WHERE status='RUNNING'",
                    rusqlite::params![now_rfc3339(), e.to_string()],
                );
            }
            Err(e)
        }
    }
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
    // A RUNNING row with no live process means a crashed run (the CLI is
    // single-writer per project); resume is the recovery path.
    if rec.status == RunStatus::Running {
        eprintln!("warning: run '{run_id}' was marked RUNNING (crashed run); resuming");
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
        materialize: project.materialize_enabled(),
        executor_override: None,
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
    finalize_on_error(scheduler::execute(ctx), project)
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
            format!(
                "step prefix '{prefix}' is ambiguous: {}",
                matches.join(", ")
            ),
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
        return Err(M3FlowError::not_found(format!(
            "input file '{}'",
            path.display()
        )));
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

/// Composite one-call run status (`m3flow run status <wr>`): brief run
/// record + per-step status/outputs + extracted failure + the suggested
/// next command + the results/ dir. Answers "did it fail, where, and
/// what now?" in a single call — the inspect + logs + graph loop's
/// agent-oriented replacement.
pub fn run_status_json(project: &Project, run_id: &str) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let rec = db.get_workflow_run(run_id)?;
    let mut tasks = db.task_runs_of(rec.id.as_str())?;
    tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut steps = Vec::new();
    let mut failure = serde_json::Value::Null;
    for tr in &tasks {
        let outputs: serde_json::Map<String, serde_json::Value> = db
            .outputs_of(tr.id.as_str())?
            .into_iter()
            .map(|(role, aid)| (role, aid.into()))
            .collect();
        steps.push(serde_json::json!({
            "node": tr.node_id,
            "task": format!("{}@{}", tr.task_name, tr.task_version),
            "status": tr.status.as_str(),
            "attempts": tr.attempts,
            "outputs": outputs,
        }));
        if failure.is_null() && matches!(tr.status, TaskStatus::Failed) {
            failure = failure_json(Some(tr.node_id.as_str()), tr.error.as_ref());
        }
    }
    // Run-level failure with no failing task (scheduler crash, cancel race)
    if failure.is_null() && matches!(rec.status, RunStatus::Failed) {
        failure = failure_json(None, rec.error.as_ref());
    }

    let dir = crate::materialize::run_dir(&project.root, &rec);
    Ok(serde_json::json!({
        "run": {
            "id": rec.id.to_string(),
            "workflow": format!("{}@{}", rec.name, rec.version),
            "label": rec.label,
            "status": rec.status.as_str(),
            "created_at": rec.created_at,
            "started_at": rec.started_at,
            "ended_at": rec.ended_at,
        },
        "results_dir": dir.display().to_string(),
        "materialized": dir.is_dir(),
        "steps": steps,
        "failure": failure,
        "next": next_command(&rec, &failure),
    }))
}

/// Pull {step, category, recoverable, message} out of a stored error blob.
/// Task errors land as flat objects ("category"/"recoverable"/"message")
/// from the scheduler's OutcomeKind::Failed; run-level errors may be plain
/// strings (finalize_on_error) or serialized M3FlowError objects.
fn failure_json(step: Option<&str>, error: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(err) = error else {
        return match step {
            Some(s) => serde_json::json!({"step": s, "message": "failed without recorded error"}),
            None => {
                serde_json::json!({"step": null, "message": "run failed without recorded error"})
            }
        };
    };
    if let Some(msg) = err.as_str() {
        return serde_json::json!({"step": step, "message": msg});
    }
    serde_json::json!({
        "step": step,
        "category": err.get("category").cloned().unwrap_or(serde_json::Value::Null),
        "recoverable": err.get("recoverable").and_then(|v| v.as_bool()).unwrap_or(false),
        "message": err.get("message").cloned().unwrap_or_else(|| err.clone()),
    })
}

/// The single most useful follow-up command for the agent, or null when
/// there is nothing to do. Retry for recoverable step failures, logs for
/// permanent ones, resume for cancelled runs; never a destructive action.
fn next_command(rec: &WorkflowRunRecord, failure: &serde_json::Value) -> Option<String> {
    match rec.status {
        RunStatus::Failed => {
            let step = failure.get("step").and_then(|s| s.as_str());
            let recoverable = failure
                .get("recoverable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match (step, recoverable) {
                (Some(s), true) => Some(format!("m3flow run retry {} {s}", rec.id)),
                (Some(s), false) => Some(format!("m3flow run logs {} --step {s}", rec.id)),
                (None, _) => None,
            }
        }
        RunStatus::Cancelled => Some(format!("m3flow run resume {}", rec.id)),
        _ => None,
    }
}

pub fn list_runs_json(project: &Project, limit: usize) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let runs = db.list_workflow_runs(limit)?;
    Ok(serde_json::json!({"runs": runs}))
}

/// Rebuild the friendly results/ tree(s) from the provenance DB
/// (`m3flow results sync [--run wr_…]`). The trees are pure derivatives:
/// wiping and re-deriving them can never lose information.
pub fn results_sync(project: &Project, run_id: Option<&str>) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let store = open_store(project)?;
    let rebuilt = match run_id {
        Some(id) => {
            vec![crate::materialize::sync_run(project, &db, &store, id)?
                .display()
                .to_string()]
        }
        None => crate::materialize::sync_all(project, &db, &store)?,
    };
    Ok(serde_json::json!({"rebuilt": rebuilt}))
}

/// Set or clear a run's study label (`m3flow runs label <wr_id> [name]`).
/// Pure metadata update — fingerprints, cache keys, and statuses are
/// untouched. If the run has a materialized tree, it is moved to the new
/// group folder (remove + re-derive; never hand-`mv`, the DB is truth).
pub fn label_run(
    project: &Project,
    run_id: &str,
    label: Option<&str>,
) -> Result<serde_json::Value> {
    let label = label.map(validate_label).transpose()?;
    let db = open_db(project)?;
    let before = db.get_workflow_run(run_id)?;
    let old_dir = crate::materialize::run_dir(&project.root, &before);

    db.set_workflow_run_label(run_id, label.as_deref())?;

    let mut moved = serde_json::Value::Null;
    if old_dir.is_dir() {
        let store = open_store(project)?;
        std::fs::remove_dir_all(&old_dir)
            .map_err(|e| M3FlowError::io(e, format!("removing {}", old_dir.display())))?;
        // Drop the old group folder if this was its last run (fails
        // harmlessly when siblings remain).
        if let Some(parent) = old_dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        let new_dir = crate::materialize::sync_run(project, &db, &store, run_id)?;
        moved = serde_json::json!({
            "from": old_dir.display().to_string(),
            "to": new_dir.display().to_string(),
        });
    }
    Ok(serde_json::json!({
        "run": run_id,
        "label": label,
        "moved": moved,
    }))
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
        Some(tr) => db.task_run_by_id(tr.as_str()).ok().map(|t| {
            serde_json::json!({
                "task_run_id": t.id.as_str(),
                "workflow_run_id": t.workflow_run_id.as_str(),
                "node_id": t.node_id,
                "task": format!("{}@{}", t.task_name, t.task_version),
            })
        }),
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

/// Lineage for every output of a task run (TUI provenance pane).
pub fn lineage_json_for_task(project: &Project, task_run_id: &str) -> Result<serde_json::Value> {
    let db = open_db(project)?;
    let tr = db.task_run_by_id(task_run_id)?;
    let mut outputs = Vec::new();
    for (oname, aid) in db.outputs_of(task_run_id)? {
        outputs.push(serde_json::json!({
            "output": oname,
            "artifact": lineage_of(&db, &aid, 0)?,
        }));
    }
    let mut inputs = Vec::new();
    for (iname, aid) in db.inputs_of(task_run_id)? {
        let a = db.get_artifact(&aid)?;
        inputs.push(serde_json::json!({
            "input": iname,
            "artifact": {"id": aid, "type": a.artifact_type},
        }));
    }
    Ok(serde_json::json!({
        "task_run": {
            "id": tr.id.as_str(),
            "node_id": tr.node_id,
            "task": format!("{}@{}", tr.task_name, tr.task_version),
            "status": tr.status.as_str(),
        },
        "inputs": inputs,
        "outputs": outputs,
    }))
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

#[cfg(test)]
mod tests {
    use super::validate_label;

    #[test]
    fn label_charset_is_folder_safe() {
        assert!(validate_label("peo-5k-screen").is_ok());
        assert!(validate_label("study_01.v2").is_ok());
        assert!(validate_label("A").is_ok());
    }

    #[test]
    fn label_rejects_hostile_input() {
        // path separators, spaces, leading dot, dot-dots, empty, overlong —
        // anything that could escape or merge folders in results/
        for bad in [
            "",
            "a/b",
            "a\\b",
            "a b",
            ".hidden",
            "..",
            "a..b/..",
            "émile",
            &"x".repeat(65),
        ] {
            assert!(validate_label(bad).is_err(), "accepted {bad:?}");
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::db::TaskRunRecord;
    use m3flow_core::artifact::{RunStatus, TaskStatus};
    use m3flow_core::id::TaskRunId;

    fn tmp_project(tag: &str) -> Project {
        let dir = std::env::temp_dir().join(format!("m3status-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m3flow.yaml"), "schema: m3flow-project/v1\n").unwrap();
        Project::load(dir).unwrap()
    }

    fn run_rec(status: RunStatus, error: Option<serde_json::Value>) -> WorkflowRunRecord {
        WorkflowRunRecord {
            id: WorkflowRunId::new(),
            name: "construct_system".into(),
            version: "1.0.0".into(),
            spec_hash: "x".into(),
            status,
            created_at: "2026-08-14T09:00:00.000Z".into(),
            started_at: None,
            ended_at: None,
            workdir: String::new(),
            git: None,
            inputs: serde_json::json!({}),
            params: serde_json::json!({}),
            outputs: None,
            error,
            label: Some("peo-5k-screen".into()),
        }
    }

    fn task(
        rec: &WorkflowRunRecord,
        node: &str,
        order: usize,
        status: TaskStatus,
        error: Option<serde_json::Value>,
    ) -> TaskRunRecord {
        TaskRunRecord {
            id: TaskRunId::new(),
            workflow_run_id: rec.id.clone(),
            node_id: node.into(),
            task_name: "build_system".into(),
            task_version: "1.0.0".into(),
            provider: Some("autopoly".into()),
            status,
            cache_key: None,
            attempts: 2,
            created_at: format!("2026-08-14T09:0{order}:00.000Z"),
            started_at: None,
            ended_at: None,
            params: serde_json::json!({}),
            error,
            validation: None,
            engine: None,
        }
    }

    #[test]
    fn status_extracts_failure_and_suggests_retry() {
        let project = tmp_project("failed");
        let db = open_db(&project).unwrap();
        let rec = run_rec(RunStatus::Failed, None);
        db.insert_workflow_run(&rec).unwrap();
        let ok = task(&rec, "build", 1, TaskStatus::Completed, None);
        let bad = task(
            &rec,
            "pack",
            2,
            TaskStatus::Failed,
            Some(serde_json::json!({
                "category": "provider_error", "recoverable": true, "message": "lammps exited 1"
            })),
        );
        let waiting = task(&rec, "equilibrate", 3, TaskStatus::Pending, None);
        for t in [&ok, &bad, &waiting] {
            db.upsert_task_run(t).unwrap();
        }
        db.conn()
            .execute(
                "INSERT INTO artifact (id, type, schema_version, content_hash, producer, created_at, metadata_json)
                 VALUES ('art_00112233', 'LammpsData', '1.0', 'h', NULL, '2026-08-14T09:00:00Z', '{}')",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO artifact_output (task_run_id, output_name, artifact_id) VALUES (?1, 'system', 'art_00112233')",
                rusqlite::params![ok.id.as_str()],
            )
            .unwrap();

        let v = run_status_json(&project, rec.id.as_str()).unwrap();
        assert_eq!(v["run"]["status"], "FAILED");
        assert_eq!(v["run"]["label"], "peo-5k-screen");
        assert_eq!(v["steps"].as_array().unwrap().len(), 3);
        // dispatch order, not insertion order
        assert_eq!(v["steps"][0]["node"], "build");
        assert_eq!(v["steps"][0]["outputs"]["system"], "art_00112233");
        assert_eq!(v["failure"]["step"], "pack");
        assert_eq!(v["failure"]["category"], "provider_error");
        assert_eq!(v["failure"]["message"], "lammps exited 1");
        assert_eq!(
            v["next"].as_str().unwrap(),
            format!("m3flow run retry {} pack", rec.id)
        );
        assert!(v["results_dir"].as_str().unwrap().contains("peo-5k-screen"));
        let _ = std::fs::remove_dir_all(&project.root);
    }

    #[test]
    fn permanent_failure_points_at_logs_not_retry() {
        let project = tmp_project("permanent");
        let db = open_db(&project).unwrap();
        let rec = run_rec(RunStatus::Failed, None);
        db.insert_workflow_run(&rec).unwrap();
        let bad = task(
            &rec,
            "pack",
            1,
            TaskStatus::Failed,
            Some(serde_json::json!({
                "category": "scientific_validation", "recoverable": false,
                "message": "scientific validation failed: charge_neutrality"
            })),
        );
        db.upsert_task_run(&bad).unwrap();

        let v = run_status_json(&project, rec.id.as_str()).unwrap();
        assert_eq!(
            v["next"].as_str().unwrap(),
            format!("m3flow run logs {} --step pack", rec.id)
        );
        let _ = std::fs::remove_dir_all(&project.root);
    }

    #[test]
    fn cancelled_run_suggests_resume_completed_suggests_nothing() {
        let project = tmp_project("cancelled");
        let db = open_db(&project).unwrap();
        let cancelled = run_rec(RunStatus::Cancelled, None);
        db.insert_workflow_run(&cancelled).unwrap();
        let v = run_status_json(&project, cancelled.id.as_str()).unwrap();
        assert_eq!(
            v["next"].as_str().unwrap(),
            format!("m3flow run resume {}", cancelled.id)
        );
        assert!(v["failure"].is_null());

        let done = run_rec(RunStatus::Completed, None);
        db.insert_workflow_run(&done).unwrap();
        let v = run_status_json(&project, done.id.as_str()).unwrap();
        assert!(v["next"].is_null());
        let _ = std::fs::remove_dir_all(&project.root);
    }
}
