//! Scheduler: executes a CompiledWorkflow against the store + provenance DB.
//!
//! Per node lifecycle (plan §35):
//!   PENDING → (deps met) → resolve inputs → cache lookup
//!     hit  → CACHED (outputs linked to the original artifacts)
//!     miss → RUNNING → COMPLETED | FAILED (→ retry) | SKIPPED (condition)
//! Nodes whose dependencies were skipped become SKIPPED; nodes blocked by a
//! failed dependency become CANCELLED when the run ends.

use crate::db::{Db, TaskRunRecord, WorkflowRunRecord};
use crate::ir::{CompiledWorkflow, InputBinding, IrNode};
use crate::project::Project;
use crate::provider::{ExecuteResponse, ProviderHandle};
use crate::store::Store;
use m3flow_core::artifact::{now_rfc3339, Artifact, RunStatus, TaskStatus};
use m3flow_core::canon;
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::expr::{eval_condition, Reference};
use m3flow_core::id::{ArtifactId, TaskRunId};
use m3flow_registry::Registry;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

/// A node restored from a previous (partial) run of the same workflow.
pub struct ResumedNode {
    pub status: TaskStatus,
    pub task_run: Option<TaskRunId>,
    pub outputs: BTreeMap<String, ArtifactId>,
}

/// Everything the scheduler needs; constructed by `run_api`.
pub struct RunContext {
    pub project: Project,
    pub registry: Registry,
    pub db: Db,
    pub store: Store,
    pub compiled: CompiledWorkflow,
    /// workflow input name -> artifact id
    pub workflow_inputs: BTreeMap<String, ArtifactId>,
    pub run: WorkflowRunRecord,
    pub no_cache: bool,
    pub max_concurrency: usize,
    /// Nodes seeded from a previous run (`run resume`): successful nodes
    /// keep their outputs and are not re-executed.
    pub resume: BTreeMap<String, ResumedNode>,
    /// Always-on friendly `results/` tree (presentation-only, best-effort).
    pub materialize: bool,
    pub progress: Option<Box<dyn Fn(&str) + Send>>,
}

/// Per-node mutable execution state.
struct NodeState {
    status: TaskStatus,
    task_run: Option<TaskRunId>,
    attempts: u32,
    cache_key: Option<String>,
    outputs: BTreeMap<String, ArtifactId>,
}

struct Job {
    node: IrNode,
    task_run_id: TaskRunId,
    attempt: u32,
    provider: ProviderHandle,
    request: serde_json::Value,
    workdir: PathBuf,
    store_root: PathBuf,
    expected_validators: Vec<String>,
}

enum OutcomeKind {
    Success {
        outputs: Vec<(String, Artifact, Vec<(String, String, String, u64)>)>,
        validation: Vec<m3flow_core::artifact::ValidationVerdict>,
        engine: Option<serde_json::Value>,
        warnings: Vec<String>,
    },
    Failed {
        error: serde_json::Value,
        recoverable: bool,
        category: String,
    },
}

struct Outcome {
    node_id: String,
    task_run_id: TaskRunId,
    attempt: u32,
    kind: OutcomeKind,
    started_at: String,
    ended_at: String,
}

pub fn execute(mut ctx: RunContext) -> Result<WorkflowRunRecord> {
    let mut states: BTreeMap<String, NodeState> = ctx
        .compiled
        .nodes
        .iter()
        .map(|n| {
            let resumed = ctx.resume.get(&n.id);
            (
                n.id.clone(),
                NodeState {
                    status: resumed.map(|r| r.status).unwrap_or(TaskStatus::Pending),
                    task_run: resumed.and_then(|r| r.task_run.clone()),
                    attempts: 0,
                    cache_key: None,
                    outputs: resumed.map(|r| r.outputs.clone()).unwrap_or_default(),
                },
            )
        })
        .collect();

    ctx.run.status = RunStatus::Running;
    ctx.run.started_at = Some(now_rfc3339());
    ctx.db.update_workflow_run(&ctx.run)?;

    // friendly results/ tree: inputs + initial run.json (best-effort)
    if ctx.materialize {
        if let Err(e) = crate::materialize::materialize_run_inputs(
            &ctx.project,
            &ctx.store,
            &ctx.db,
            &ctx.run,
            &ctx.workflow_inputs,
        ) {
            ctx.progress(&format!("warning: materialize(inputs) failed: {e}"));
        }
        write_run_json_view(&ctx, &states);
    }

    let (tx, rx): (Sender<Outcome>, Receiver<Outcome>) = channel();
    let mut running = 0usize;
    let cancel_flag = ctx
        .project
        .runs_dir()
        .join(ctx.run.id.as_str())
        .join("CANCEL");
    let mut providers: BTreeMap<String, ProviderHandle> = BTreeMap::new();
    let mut cancelled = false;

    loop {
        // ---- drain finished work
        while let Ok(outcome) = rx.try_recv() {
            running = running.saturating_sub(1);
            handle_outcome(&mut ctx, &mut states, outcome)?;
        }

        // ---- cancellation
        if cancel_flag.exists() && !cancelled {
            cancelled = true;
            ctx.progress("run cancellation requested");
        }

        // ---- find actionable nodes
        let mut dispatched_this_round = false;
        for node in ctx.compiled.nodes.clone() {
            if cancelled {
                break;
            }
            if states[&node.id].status != TaskStatus::Pending {
                continue;
            }
            let dep_statuses: Vec<TaskStatus> = node
                .deps
                .iter()
                .map(|d| {
                    states
                        .get(d)
                        .map(|s| s.status)
                        .unwrap_or(TaskStatus::Pending)
                })
                .collect();
            if dep_statuses.iter().any(|s| *s == TaskStatus::Skipped) {
                set_status(&mut ctx, &mut states, &node.id, TaskStatus::Skipped, None)?;
                ctx.progress(&format!("{}: SKIPPED (upstream skipped)", node.id));
                continue;
            }
            if dep_statuses
                .iter()
                .any(|s| matches!(s, TaskStatus::Failed | TaskStatus::Cancelled))
            {
                continue; // blocked forever; finalized as CANCELLED at run end
            }
            if !dep_statuses.iter().all(|s| s.is_success()) {
                continue; // still waiting on running deps
            }
            if running >= ctx.max_concurrency {
                break;
            }

            // condition gate
            if let Some(cond) = &node.condition {
                if !eval_node_condition(&ctx, &states, cond, &node.id)? {
                    set_status(&mut ctx, &mut states, &node.id, TaskStatus::Skipped, None)?;
                    ctx.progress(&format!("{}: SKIPPED (condition false)", node.id));
                    continue;
                }
            }

            // resolve inputs + params
            let resolved = resolve_node(&ctx, &states, &node)?;
            let task_spec = ctx.registry.task(&node.task)?.clone();

            // provider selection + engine version (cache-key material)
            let provider_name = select_provider(&ctx, &node, &task_spec)?;
            let handle = match providers.entry(provider_name.clone()) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => {
                    match ProviderHandle::locate(
                        &provider_name,
                        ctx.project.provider_config(&provider_name),
                    ) {
                        Ok(h) => e.insert(h),
                        Err(err) => {
                            {
                                let st = states.get_mut(&node.id).unwrap();
                                st.task_run = Some(TaskRunId::new());
                                st.status = TaskStatus::Failed;
                            }
                            persist_task_run(
                                &ctx,
                                &states,
                                &node.id,
                                TaskStatus::Failed,
                                Some(serde_json::json!({
                                    "error_type": "engine_missing",
                                    "category": "environment_error",
                                    "recoverable": false,
                                    "message": err.to_string(),
                                })),
                                None,
                            )?;
                            ctx.progress(&format!(
                                "{}: FAILED (provider '{}' unavailable)",
                                node.id, provider_name
                            ));
                            continue;
                        }
                    }
                }
            };
            let engine_version = handle
                .engine_version()
                .unwrap_or_else(|_| "unknown".to_string());
            let provider_version = handle
                .describe()
                .ok()
                .and_then(|d| {
                    d.pointer("/provider/version")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "unknown".to_string());
            let key = cache_key(
                &node.task,
                &format!("{provider_name}@{provider_version}"),
                &engine_version,
                &resolved.input_hashes,
                &resolved.params,
            );
            states.get_mut(&node.id).unwrap().cache_key = Some(key.clone());

            // cache lookup → CACHED short-circuit
            if !ctx.no_cache {
                if let Some(hit) = ctx.db.cache_lookup(&key)? {
                    apply_cache_hit(&mut ctx, &mut states, &node, &hit, &resolved)?;
                    dispatched_this_round = true;
                    ctx.progress(&format!("{}: CACHED", node.id));
                    continue;
                }
            }

            // dispatch real execution
            let attempt = states[&node.id].attempts + 1;
            let job = build_job(&ctx, &node, &task_spec, &resolved, handle.clone(), attempt)?;
            {
                let st = states.get_mut(&node.id).unwrap();
                st.status = TaskStatus::Running;
                st.attempts = attempt;
                st.task_run = Some(job.task_run_id.clone());
            }
            persist_task_run(&ctx, &states, &node.id, TaskStatus::Running, None, None)?;
            spawn_job(job, tx.clone());
            running += 1;
            dispatched_this_round = true;
            ctx.progress(&format!("{}: RUNNING ({})", node.id, node.label));
        }

        // ---- termination
        let any_pending = states.values().any(|s| s.status == TaskStatus::Pending);
        if !any_pending && running == 0 {
            break;
        }
        if !dispatched_this_round && running == 0 {
            // No dispatch possible and nothing in flight: remaining pending
            // nodes are blocked by failed/cancelled deps (cycles are
            // impossible after topo sort). Finalization marks them CANCELLED.
            break;
        }
        if running > 0 {
            match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(outcome) => {
                    running = running.saturating_sub(1);
                    handle_outcome(&mut ctx, &mut states, outcome)?;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    // ---- finalize: leftover pending/running nodes become CANCELLED
    let mut final_status = RunStatus::Completed;
    let node_ids: Vec<String> = ctx.compiled.nodes.iter().map(|n| n.id.clone()).collect();
    for id in &node_ids {
        match states[id].status {
            TaskStatus::Pending | TaskStatus::Ready | TaskStatus::Running => {
                set_status(&mut ctx, &mut states, id, TaskStatus::Cancelled, None)?;
                final_status = RunStatus::Failed;
            }
            TaskStatus::Failed => final_status = RunStatus::Failed,
            _ => {}
        }
    }
    if cancelled {
        final_status = RunStatus::Cancelled;
    }

    // workflow outputs
    let mut outputs = serde_json::Map::new();
    for (oname, obind) in &ctx.compiled.outputs {
        if let Some(id) = resolve_binding_artifact(&states, &ctx.workflow_inputs, &obind.binding) {
            outputs.insert(oname.clone(), serde_json::Value::String(id.to_string()));
        }
    }
    ctx.run.outputs = Some(serde_json::Value::Object(outputs));
    ctx.run.status = final_status;
    ctx.run.ended_at = Some(now_rfc3339());
    ctx.db.update_workflow_run(&ctx.run)?;
    if ctx.materialize {
        write_run_json_view(&ctx, &states);
    }
    Ok(ctx.run.clone())
}

// ------------------------------------------------------------------ helpers

struct ResolvedNode {
    /// input name -> artifact ids (one, or many for Collect)
    inputs: BTreeMap<String, Vec<ArtifactId>>,
    /// canonicalized task parameters
    params: serde_json::Map<String, serde_json::Value>,
    /// input name -> content hashes (cache key material)
    input_hashes: BTreeMap<String, Vec<String>>,
}

fn resolve_node(
    ctx: &RunContext,
    states: &BTreeMap<String, NodeState>,
    node: &IrNode,
) -> Result<ResolvedNode> {
    let mut inputs = BTreeMap::new();
    let mut input_hashes = BTreeMap::new();
    for (name, binding) in &node.inputs {
        let ids: Vec<ArtifactId> = match binding {
            InputBinding::WorkflowInput { name: wname } => {
                let id = ctx.workflow_inputs.get(wname).ok_or_else(|| {
                    M3FlowError::workflow(
                        format!("workflow input '{wname}' not bound at run time"),
                        Some(node.id.clone()),
                    )
                })?;
                vec![id.clone()]
            }
            InputBinding::NodeOutput { node: dep, output } => {
                let outs = states.get(dep).map(|s| &s.outputs).ok_or_else(|| {
                    M3FlowError::internal(format!("dependency '{dep}' has no state"))
                })?;
                let id = outs.get(output).ok_or_else(|| {
                    M3FlowError::internal(format!("dependency '{dep}' produced no '{output}'"))
                })?;
                vec![id.clone()]
            }
            InputBinding::Collect { base, output } => {
                let mut collected = Vec::new();
                let mut i = 0;
                loop {
                    let member = format!("{base}__{i}");
                    match states.get(&member) {
                        Some(s) => {
                            if let Some(id) = s.outputs.get(output) {
                                collected.push(id.clone());
                            }
                        }
                        None => break,
                    }
                    i += 1;
                }
                if collected.is_empty() {
                    return Err(M3FlowError::workflow(
                        format!("foreach '{base}' produced no outputs to collect"),
                        Some(node.id.clone()),
                    ));
                }
                collected
            }
        };
        let mut hashes = Vec::new();
        for id in &ids {
            let a = ctx.db.get_artifact(id.as_str())?;
            hashes.push(a.content_hash.clone());
        }
        input_hashes.insert(name.clone(), hashes);
        inputs.insert(name.clone(), ids);
    }

    // params: resolve remaining references against artifacts/inputs, then
    // canonicalize against the task's parameter declarations
    let task = ctx.registry.task(&node.task)?;
    let mut params = serde_json::Map::new();
    for (pname, pval) in &node.params {
        let resolved = resolve_value_refs(ctx, states, pval, &node.id)?;
        let canonical =
            task.canonical_param(pname, &resolved)
                .map_err(|e| M3FlowError::Schema {
                    message: format!("node {}: {e}", node.id),
                    details: vec![],
                })?;
        params.insert(pname.clone(), canonical);
    }
    // apply task defaults for unprovided params
    for (pname, decl) in &task.parameters {
        if !params.contains_key(pname) {
            if let Some(default) = &decl.default {
                params.insert(pname.clone(), task.canonical_param(pname, default)?);
            } else if decl.required {
                return Err(M3FlowError::schema(format!(
                    "node {}: required parameter '{pname}' of task '{}' not provided",
                    node.id, task.name
                )));
            }
        }
    }
    Ok(ResolvedNode {
        inputs,
        params,
        input_hashes,
    })
}

/// Resolve `${...}` references in parameter values against runtime data.
fn resolve_value_refs(
    ctx: &RunContext,
    states: &BTreeMap<String, NodeState>,
    v: &serde_json::Value,
    node_id: &str,
) -> Result<serde_json::Value> {
    match v {
        serde_json::Value::String(s) => {
            if let Some(r) = Reference::whole(s) {
                return resolve_reference(ctx, states, &r, node_id);
            }
            // template interpolation inside a longer string
            let mut out = s.clone();
            let mut refs = Vec::new();
            m3flow_core::expr::find_references(v, &mut refs);
            for r in refs {
                let val = resolve_reference(ctx, states, &r, node_id)?;
                out = out.replace(&r.display(), &json_to_display(&val));
            }
            Ok(serde_json::Value::String(out))
        }
        serde_json::Value::Array(a) => Ok(serde_json::Value::Array(
            a.iter()
                .map(|x| resolve_value_refs(ctx, states, x, node_id))
                .collect::<Result<Vec<_>>>()?,
        )),
        serde_json::Value::Object(m) => Ok(serde_json::Value::Object(
            m.iter()
                .map(|(k, x)| Ok((k.clone(), resolve_value_refs(ctx, states, x, node_id)?)))
                .collect::<Result<serde_json::Map<_, _>>>()?,
        )),
        other => Ok(other.clone()),
    }
}

fn resolve_reference(
    ctx: &RunContext,
    states: &BTreeMap<String, NodeState>,
    r: &Reference,
    node_id: &str,
) -> Result<serde_json::Value> {
    let fail = || {
        M3FlowError::workflow(
            format!("cannot resolve {} at runtime", r.display()),
            Some(node_id.to_string()),
        )
    };
    let first = r.path[0].as_str();
    if first == "inputs" {
        let id = ctx.workflow_inputs.get(&r.path[1]).ok_or_else(fail)?;
        if r.path.len() == 2 {
            return Ok(serde_json::Value::String(id.to_string()));
        }
        let a = ctx.db.get_artifact(id.as_str())?;
        return walk_artifact(&a, &r.path[2..]).ok_or_else(fail);
    }
    if first == "params" {
        return ctx
            .compiled
            .params
            .get(r.path[1].as_str())
            .cloned()
            .ok_or_else(fail);
    }
    // step output reference
    let st = states.get(first).ok_or_else(fail)?;
    let out_name = r.path.get(1).ok_or_else(fail)?;
    let id = st.outputs.get(out_name).ok_or_else(fail)?;
    if r.path.len() == 2 {
        return Ok(serde_json::Value::String(id.to_string()));
    }
    let a = ctx.db.get_artifact(id.as_str())?;
    walk_artifact(&a, &r.path[2..]).ok_or_else(fail)
}

/// `${step.out.data.field}` / `${step.out.metadata.field}` / bare field walk.
fn walk_artifact(a: &Artifact, rest: &[String]) -> Option<serde_json::Value> {
    if rest.is_empty() {
        return Some(serde_json::Value::String(a.id.to_string()));
    }
    match rest[0].as_str() {
        "data" => walk_json(a.data.clone()?, &rest[1..]),
        "metadata" => walk_json(a.metadata.clone(), &rest[1..]),
        "id" => Some(serde_json::Value::String(a.id.to_string())),
        "type" => Some(serde_json::Value::String(a.artifact_type.clone())),
        field => {
            // bare field: data first, then metadata
            if let Some(v) = a.data.as_ref().and_then(|d| d.get(field)) {
                walk_json(v.clone(), &rest[1..])
            } else {
                walk_json(a.metadata.get(field)?.clone(), &rest[1..])
            }
        }
    }
}

fn walk_json(mut v: serde_json::Value, path: &[String]) -> Option<serde_json::Value> {
    for p in path {
        v = v.get(p.as_str())?.clone();
    }
    Some(v)
}

fn eval_node_condition(
    ctx: &RunContext,
    states: &BTreeMap<String, NodeState>,
    cond: &str,
    node_id: &str,
) -> Result<bool> {
    eval_condition(cond, &|path: &[String]| {
        resolve_reference(
            ctx,
            states,
            &Reference {
                path: path.to_vec(),
            },
            node_id,
        )
        .ok()
    })
}

fn select_provider(
    ctx: &RunContext,
    node: &IrNode,
    task: &m3flow_core::specs::TaskSpec,
) -> Result<String> {
    if let Some(p) = &node.provider {
        return Ok(p.clone());
    }
    if let Some(p) = ctx.project.preferred_provider(&task.name) {
        return Ok(p.to_string());
    }
    if let Some(d) = task.implementations.iter().find(|i| i.default) {
        return Ok(d.provider.clone());
    }
    task.implementations
        .first()
        .map(|i| i.provider.clone())
        .ok_or_else(|| {
            M3FlowError::workflow(
                format!("task '{}' declares no implementations", task.name),
                Some(node.id.clone()),
            )
        })
}

fn cache_key(
    task_ref: &str,
    provider: &str,
    engine_version: &str,
    input_hashes: &BTreeMap<String, Vec<String>>,
    params: &serde_json::Map<String, serde_json::Value>,
) -> String {
    canon::hash_json(&serde_json::json!({
        "protocol": "m3flow-cache/1",
        "task": task_ref,
        "provider": provider,
        "engine": engine_version,
        "inputs": input_hashes,
        "parameters": params,
    }))
}

fn build_job(
    ctx: &RunContext,
    node: &IrNode,
    task: &m3flow_core::specs::TaskSpec,
    resolved: &ResolvedNode,
    provider: ProviderHandle,
    attempt: u32,
) -> Result<Job> {
    // Reuse the task_run row when re-dispatching after a retry/resume:
    // artifact links already reference it (PK changes would violate FK).
    let task_run_id = ctx
        .db
        .get_task_run(ctx.run.id.as_str(), &node.id)
        .map(|r| r.id)
        .unwrap_or_else(|_| TaskRunId::new());
    let workdir = ctx
        .project
        .runs_dir()
        .join(ctx.run.id.as_str())
        .join(&node.id);
    std::fs::create_dir_all(&workdir).map_err(|e| M3FlowError::io(e, "creating task workdir"))?;

    // request inputs with absolute store paths
    let mut inputs = serde_json::Map::new();
    for (name, ids) in &resolved.inputs {
        if ids.len() == 1 {
            inputs.insert(name.clone(), artifact_request_json(ctx, &ids[0])?);
        } else {
            inputs.insert(
                name.clone(),
                serde_json::Value::Array(
                    ids.iter()
                        .map(|id| artifact_request_json(ctx, id))
                        .collect::<Result<Vec<_>>>()?,
                ),
            );
        }
    }

    let request = serde_json::json!({
        "protocol": m3flow_core::PROVIDER_PROTOCOL,
        "task": {"name": task.name, "version": task.version},
        "workflow_run_id": ctx.run.id.as_str(),
        "task_run_id": task_run_id.as_str(),
        "workdir": workdir,
        "inputs": inputs,
        "parameters": resolved.params,
        "resources": node.resources,
        "config": provider.config.engine.clone().unwrap_or(serde_json::json!({})),
    });
    Ok(Job {
        node: node.clone(),
        task_run_id,
        attempt,
        provider,
        request,
        workdir,
        store_root: ctx.project.artifacts_dir(),
        expected_validators: task.validation.clone(),
    })
}

fn artifact_request_json(ctx: &RunContext, id: &ArtifactId) -> Result<serde_json::Value> {
    let a = ctx.db.get_artifact(id.as_str())?;
    let mut files = serde_json::Map::new();
    for (name, rel) in &a.files {
        files.insert(
            name.clone(),
            serde_json::Value::String(ctx.store.resolve(rel).display().to_string()),
        );
    }
    Ok(serde_json::json!({
        "id": a.id.as_str(),
        "type": a.artifact_type,
        "schema_version": a.schema_version,
        "files": serde_json::Value::Object(files),
        "metadata": a.metadata,
        "data": a.data,
    }))
}

fn spawn_job(job: Job, tx: Sender<Outcome>) {
    std::thread::spawn(move || {
        let started_at = now_rfc3339();
        let kind = run_job(&job);
        let ended_at = now_rfc3339();
        let _ = tx.send(Outcome {
            node_id: job.node.id.clone(),
            task_run_id: job.task_run_id,
            attempt: job.attempt,
            kind,
            started_at,
            ended_at,
        });
    });
}

fn run_job(job: &Job) -> OutcomeKind {
    let request_path = job.workdir.join("request.json");
    if let Err(e) = std::fs::write(
        &request_path,
        serde_json::to_string_pretty(&job.request).unwrap_or_default(),
    ) {
        return failure(
            "io_error",
            "environment_error",
            &format!("writing request: {e}"),
            false,
        );
    }
    let resp: ExecuteResponse = match job.provider.execute(&request_path, &job.workdir) {
        Ok(r) => r,
        Err(e) => {
            return failure(
                "provider_protocol",
                "environment_error",
                &e.to_string(),
                true,
            )
        }
    };
    if resp.status != "success" {
        let err = resp.error.unwrap_or(crate::provider::ProviderError {
            error_type: "unknown".into(),
            category: "provider_error".into(),
            recoverable: false,
            provider: None,
            task: None,
            message: Some("provider reported failure without details".into()),
            details: None,
            raw_log: None,
        });
        let recoverable = err.recoverable;
        let category = err.category.clone();
        return OutcomeKind::Failed {
            error: serde_json::to_value(&err).unwrap_or_default(),
            recoverable,
            category,
        };
    }
    // declared outputs present and correctly typed
    for (oname, otype) in &job.node.declared_outputs {
        match resp.outputs.get(oname) {
            None => {
                return failure(
                    "protocol_error",
                    "protocol_error",
                    &format!("provider did not produce declared output '{oname}'"),
                    false,
                );
            }
            Some(staged) => {
                if !m3flow_core::atypes::is_subtype(&staged.artifact_type, otype) {
                    return failure(
                        "type_check_failed",
                        "protocol_error",
                        &format!(
                            "output '{oname}' declared as {otype} but provider returned {}",
                            staged.artifact_type
                        ),
                        false,
                    );
                }
            }
        }
    }
    // every validator declared by the task spec must be reported
    let reported: std::collections::BTreeSet<&str> =
        resp.validation.iter().map(|v| v.name.as_str()).collect();
    let missing: Vec<String> = job
        .expected_validators
        .iter()
        .filter(|v| !reported.contains(v.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return failure(
            "protocol_error",
            "protocol_error",
            &format!(
                "provider omitted declared validators: {}",
                missing.join(", ")
            ),
            false,
        );
    }
    let failed: Vec<String> = resp
        .validation
        .iter()
        .filter(|v| !v.passed)
        .map(|v| v.name.clone())
        .collect();
    if !failed.is_empty() {
        return OutcomeKind::Failed {
            error: serde_json::json!({
                "error_type": "validation_failed",
                "category": "scientific_validation",
                "recoverable": false,
                "failed_validators": failed,
                "message": format!("scientific validation failed: {}", failed.join(", ")),
            }),
            recoverable: false,
            category: "scientific_validation".into(),
        };
    }
    // ingest outputs into the CAS
    let store = match Store::new(job.store_root.clone()) {
        Ok(s) => s,
        Err(e) => return failure("io_error", "environment_error", &e.to_string(), false),
    };
    let mut outputs = Vec::new();
    for (oname, staged) in &resp.outputs {
        match store.ingest_staged(staged, &job.workdir, Some(&job.task_run_id)) {
            Ok((artifact, rows)) => outputs.push((oname.clone(), artifact, rows)),
            Err(e) => return failure("io_error", "environment_error", &e.to_string(), false),
        }
    }
    OutcomeKind::Success {
        outputs,
        validation: resp.validation,
        engine: resp.engine,
        warnings: resp.warnings,
    }
}

fn failure(error_type: &str, category: &str, message: &str, recoverable: bool) -> OutcomeKind {
    OutcomeKind::Failed {
        error: serde_json::json!({
            "error_type": error_type,
            "category": category,
            "recoverable": recoverable,
            "message": message,
        }),
        recoverable,
        category: category.to_string(),
    }
}

/// Cache hit: this node is satisfied by the outputs of an earlier task run
/// with an identical cache key. Provenance stays honest: a new CACHED
/// task_run row links the resolved inputs and the original artifacts.
fn apply_cache_hit(
    ctx: &mut RunContext,
    states: &mut BTreeMap<String, NodeState>,
    node: &IrNode,
    cached_task_run: &str,
    resolved: &ResolvedNode,
) -> Result<()> {
    let outputs = ctx.db.outputs_of(cached_task_run)?;
    // Reuse the per-(run,node) row id if it exists: artifact links from a
    // previous attempt reference it (changing the PK would violate FK).
    let tr_id = ctx
        .db
        .get_task_run(ctx.run.id.as_str(), &node.id)
        .map(|r| r.id)
        .unwrap_or_else(|_| TaskRunId::new());
    let key = states[&node.id].cache_key.clone();
    // the task_run row must exist before input/output links (FK constraint)
    let (task_name, task_version) = split_task_ref(&node.task);
    let rec = TaskRunRecord {
        id: tr_id.clone(),
        workflow_run_id: ctx.run.id.clone(),
        node_id: node.id.clone(),
        task_name,
        task_version,
        provider: node.provider.clone(),
        status: TaskStatus::Cached,
        cache_key: key,
        attempts: 0,
        created_at: now_rfc3339(),
        started_at: None,
        ended_at: Some(now_rfc3339()),
        params: serde_json::Value::Object(resolved.params.clone()),
        error: None,
        validation: None,
        engine: None,
    };
    ctx.db.upsert_task_run(&rec)?;
    let mut map = BTreeMap::new();
    for (oname, art_id) in outputs {
        if let Some(parsed) = ArtifactId::parse(&art_id) {
            ctx.db.link_output(tr_id.as_str(), &oname, &art_id)?;
            map.insert(oname, parsed);
        }
    }
    for (iname, ids) in &resolved.inputs {
        for id in ids {
            ctx.db.link_input(tr_id.as_str(), iname, id.as_str())?;
        }
    }
    {
        let st = states.get_mut(&node.id).unwrap();
        st.status = TaskStatus::Cached;
        st.task_run = Some(tr_id);
        st.outputs = map;
    }
    if ctx.materialize {
        let arts: Vec<(String, Artifact)> = states[&node.id]
            .outputs
            .iter()
            .filter_map(|(r, aid)| {
                ctx.db
                    .get_artifact(aid.as_str())
                    .ok()
                    .map(|a| (r.clone(), a))
            })
            .collect();
        materialize_step(ctx, states, node, &arts);
    }
    Ok(())
}

// ------------------------------------------------------------------ results view

/// Scheduler-private state → run.json snapshot.
fn step_views(
    compiled: &CompiledWorkflow,
    states: &BTreeMap<String, NodeState>,
) -> Vec<crate::materialize::StepView> {
    compiled
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let st = &states[&n.id];
            crate::materialize::StepView {
                order: i + 1,
                node_id: n.id.clone(),
                status: st.status.as_str().to_string(),
                task_run: st.task_run.as_ref().map(|t| t.to_string()),
                outputs: st
                    .outputs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_string()))
                    .collect(),
            }
        })
        .collect()
}

fn write_run_json_view(ctx: &RunContext, states: &BTreeMap<String, NodeState>) {
    let inputs: BTreeMap<String, String> = ctx
        .workflow_inputs
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    if let Err(e) = crate::materialize::write_run_json(
        &ctx.project,
        &ctx.run,
        &inputs,
        &step_views(&ctx.compiled, states),
    ) {
        ctx.progress(&format!("warning: run.json failed: {e}"));
    }
}

/// Materialize one step's outputs + refresh run.json. Best-effort by
/// contract: the results/ tree is a derived view, never run-critical.
fn materialize_step(
    ctx: &RunContext,
    states: &BTreeMap<String, NodeState>,
    node: &IrNode,
    arts: &[(String, Artifact)],
) {
    let order = ctx
        .compiled
        .nodes
        .iter()
        .position(|n| n.id == node.id)
        .map(|i| i + 1)
        .unwrap_or(0);
    if let Err(e) = crate::materialize::materialize_step_outputs(
        &ctx.project,
        &ctx.store,
        &ctx.run,
        order,
        &node.id,
        arts,
    ) {
        ctx.progress(&format!("{}: warning: materialize failed: {e}", node.id));
    }
    write_run_json_view(ctx, states);
}

fn handle_outcome(
    ctx: &mut RunContext,
    states: &mut BTreeMap<String, NodeState>,
    outcome: Outcome,
) -> Result<()> {
    let node = ctx
        .compiled
        .node(&outcome.node_id)
        .cloned()
        .ok_or_else(|| M3FlowError::internal("outcome for unknown node"))?;
    match outcome.kind {
        OutcomeKind::Success {
            outputs,
            validation,
            engine,
            warnings,
        } => {
            for w in &warnings {
                ctx.progress(&format!("{}: warning: {w}", node.id));
            }
            let mut map = BTreeMap::new();
            let mut mat_artifacts: Vec<(String, Artifact)> = Vec::new();
            for (oname, artifact, rows) in outputs {
                let aid = artifact.id.clone();
                ctx.db.insert_artifact(&artifact, &rows)?;
                ctx.db
                    .link_output(outcome.task_run_id.as_str(), &oname, aid.as_str())?;
                map.insert(oname.clone(), aid);
                mat_artifacts.push((oname, artifact));
            }
            for (iname, ids) in collect_input_ids(ctx, states, &node) {
                for id in ids {
                    ctx.db
                        .link_input(outcome.task_run_id.as_str(), &iname, id.as_str())?;
                }
            }
            {
                let st = states.get_mut(&node.id).unwrap();
                st.status = TaskStatus::Completed;
                st.outputs = map;
            }
            persist_task_run(
                ctx,
                states,
                &node.id,
                TaskStatus::Completed,
                None,
                Some(validation),
            )?;
            // precise timestamps + engine provenance from the worker
            let _ = ctx.db.conn().execute(
                "UPDATE task_run SET started_at=?1, ended_at=?2, engine_json=?3 WHERE id=?4",
                rusqlite::params![
                    outcome.started_at,
                    outcome.ended_at,
                    engine.as_ref().map(|e| e.to_string()),
                    outcome.task_run_id.as_str(),
                ],
            );
            // record the cache entry under the key stored at dispatch time
            if let Ok(rec) = ctx.db.get_task_run(ctx.run.id.as_str(), &node.id) {
                if let Some(key) = &rec.cache_key {
                    ctx.db.cache_insert(key, outcome.task_run_id.as_str())?;
                }
            }
            ctx.progress(&format!("{}: COMPLETED", node.id));
            if ctx.materialize {
                materialize_step(ctx, states, &node, &mat_artifacts);
            }
        }
        OutcomeKind::Failed {
            error,
            recoverable,
            category,
        } => {
            let attempts = states[&node.id].attempts;
            let should_retry = node.retry.max_attempts > attempts
                && ((node.retry.on.is_empty() && recoverable)
                    || node.retry.on.iter().any(|c| *c == category));
            let _ = ctx.db.conn().execute(
                "UPDATE task_run SET started_at=?1, ended_at=?2 WHERE id=?3",
                rusqlite::params![
                    outcome.started_at,
                    outcome.ended_at,
                    outcome.task_run_id.as_str(),
                ],
            );
            if should_retry {
                states.get_mut(&node.id).unwrap().status = TaskStatus::Pending;
                persist_task_run(
                    ctx,
                    states,
                    &node.id,
                    TaskStatus::Pending,
                    Some(error),
                    None,
                )?;
                ctx.progress(&format!(
                    "{}: attempt {} failed ({}); retrying",
                    node.id, outcome.attempt, category
                ));
            } else {
                states.get_mut(&node.id).unwrap().status = TaskStatus::Failed;
                persist_task_run(ctx, states, &node.id, TaskStatus::Failed, Some(error), None)?;
                ctx.progress(&format!("{}: FAILED ({})", node.id, category));
            }
        }
    }
    Ok(())
}

fn collect_input_ids(
    ctx: &RunContext,
    states: &BTreeMap<String, NodeState>,
    node: &IrNode,
) -> BTreeMap<String, Vec<ArtifactId>> {
    let mut out = BTreeMap::new();
    for (name, binding) in &node.inputs {
        let ids: Vec<ArtifactId> = match binding {
            InputBinding::WorkflowInput { name } => {
                ctx.workflow_inputs.get(name).cloned().into_iter().collect()
            }
            InputBinding::NodeOutput { node, output } => states
                .get(node)
                .and_then(|s| s.outputs.get(output))
                .cloned()
                .into_iter()
                .collect(),
            InputBinding::Collect { base, output } => {
                let mut v = Vec::new();
                let mut i = 0;
                while let Some(s) = states.get(&format!("{base}__{i}")) {
                    if let Some(id) = s.outputs.get(output) {
                        v.push(id.clone());
                    }
                    i += 1;
                }
                v
            }
        };
        out.insert(name.clone(), ids);
    }
    out
}

fn persist_task_run(
    ctx: &RunContext,
    states: &BTreeMap<String, NodeState>,
    node_id: &str,
    status: TaskStatus,
    error: Option<serde_json::Value>,
    validation: Option<Vec<m3flow_core::artifact::ValidationVerdict>>,
) -> Result<()> {
    let st = states
        .get(node_id)
        .ok_or_else(|| M3FlowError::internal("missing node state"))?;
    let node = ctx
        .compiled
        .node(node_id)
        .ok_or_else(|| M3FlowError::internal("missing node"))?;
    let existing = ctx.db.get_task_run(ctx.run.id.as_str(), node_id).ok();
    let (task_name, task_version) = split_task_ref(&node.task);
    let cache_key = existing
        .as_ref()
        .and_then(|r| r.cache_key.clone())
        .or_else(|| st.cache_key.clone());
    let rec = TaskRunRecord {
        id: st.task_run.clone().unwrap_or_default(),
        workflow_run_id: ctx.run.id.clone(),
        node_id: node_id.to_string(),
        task_name,
        task_version,
        provider: node.provider.clone(),
        status,
        cache_key,
        attempts: st.attempts,
        created_at: existing
            .as_ref()
            .map(|r| r.created_at.clone())
            .unwrap_or_else(now_rfc3339),
        started_at: existing
            .as_ref()
            .and_then(|r| r.started_at.clone())
            .or_else(|| (status == TaskStatus::Running).then(now_rfc3339)),
        ended_at: existing
            .as_ref()
            .and_then(|r| r.ended_at.clone())
            .or_else(|| status.is_terminal().then(now_rfc3339)),
        params: existing
            .as_ref()
            .map(|r| r.params.clone())
            .unwrap_or(serde_json::Value::Null),
        error,
        validation,
        engine: existing.and_then(|r| r.engine),
    };
    ctx.db.upsert_task_run(&rec)
}

fn split_task_ref(task_ref: &str) -> (String, String) {
    task_ref
        .split_once('@')
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .unwrap_or((task_ref.to_string(), String::new()))
}

fn set_status(
    ctx: &mut RunContext,
    states: &mut BTreeMap<String, NodeState>,
    node_id: &str,
    status: TaskStatus,
    error: Option<serde_json::Value>,
) -> Result<()> {
    states.get_mut(node_id).unwrap().status = status;
    persist_task_run(ctx, states, node_id, status, error, None)
}

fn resolve_binding_artifact(
    states: &BTreeMap<String, NodeState>,
    workflow_inputs: &BTreeMap<String, ArtifactId>,
    binding: &InputBinding,
) -> Option<ArtifactId> {
    match binding {
        InputBinding::WorkflowInput { name } => workflow_inputs.get(name).cloned(),
        InputBinding::NodeOutput { node, output } => states.get(node)?.outputs.get(output).cloned(),
        InputBinding::Collect { .. } => None, // list outputs not exposed as workflow outputs
    }
}

fn json_to_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl RunContext {
    fn progress(&self, msg: &str) {
        if let Some(cb) = &self.progress {
            cb(msg);
        }
    }
}
