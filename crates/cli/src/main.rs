//! m3flow CLI (plan §40–§43). Every command supports --json for agent use.

use clap::{Parser, Subcommand};
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::id::parse_any_id;
use m3flow_registry::Registry;
use m3flow_runtime::project::Project;
use m3flow_runtime::provider::ProviderHandle;
use m3flow_runtime::run_api::{self, InputSource, RunOptions};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "m3flow",
    version,
    about = "M3Flow — task-centric, artifact-driven, provenance-first workflow runtime for molecular simulation"
)]
struct Cli {
    /// Emit machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a new m3flow project directory.
    Init {
        /// Directory to initialize (default: current directory).
        dir: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Task registry operations.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Workflow registry and execution.
    #[command(subcommand)]
    Workflow(WfCmd),
    /// Inspect and control workflow runs.
    #[command(subcommand)]
    Run(RunCmd),
    /// Artifact store operations.
    #[command(subcommand)]
    Artifact(ArtCmd),
    /// Show the JSON schemas documents are validated against.
    #[command(subcommand)]
    Schema(SchemaCmd),
    /// Provider discovery and diagnostics.
    #[command(subcommand)]
    Provider(ProvCmd),
    /// Cache inspection and management.
    #[command(subcommand)]
    Cache(CacheCmd),
    /// Friendly results/ trees (derived views over the store).
    #[command(subcommand)]
    Results(ResultsCmd),
    /// Interactive execution cockpit.
    Tui {
        /// Run id to focus (default: latest).
        run: Option<String>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// List registered tasks.
    List {
        #[arg(long)]
        category: Option<String>,
    },
    /// Full-text search over names, descriptions, tags.
    Search { query: String },
    /// Show one task's full spec.
    Inspect { name: String },
}

#[derive(Subcommand)]
enum WfCmd {
    /// List registered workflows.
    List,
    /// Show one workflow's full spec.
    Inspect { name: String },
    /// Validate a workflow file or registered workflow.
    Validate { name_or_file: String },
    /// Show the compiled execution plan (static expansion, no execution).
    Plan {
        name: String,
        /// Parameter overrides: key=value (value parsed as YAML).
        #[arg(long = "param", value_parser = parse_kv)]
        params: Vec<(String, serde_json::Value)>,
    },
    /// Execute a workflow.
    Run {
        name: String,
        /// Inputs: name=art_xxx (existing artifact) or name=@path (file).
        #[arg(long = "input", value_parser = parse_input)]
        inputs: Vec<(String, InputSource)>,
        /// Parameter overrides: key=value (value parsed as YAML).
        #[arg(long = "param", value_parser = parse_kv)]
        params: Vec<(String, serde_json::Value)>,
        /// Bypass the result cache.
        #[arg(long)]
        no_cache: bool,
        /// Do not materialize the friendly results/ tree for this run.
        #[arg(long)]
        no_materialize: bool,
        /// Study/group folder for the run in results/ (e.g. peo-5k-screen;
        /// [A-Za-z0-9._-], max 64 chars). Unlabeled runs group by workflow.
        #[arg(long)]
        label: Option<String>,
        /// Limit concurrent tasks.
        #[arg(long)]
        max_concurrency: Option<usize>,
        /// Execution backend override for this run (overrides
        /// `executor.type` and per-provider overrides in m3flow.yaml).
        #[arg(long, value_enum)]
        executor: Option<CliExecutor>,
        /// Compile and print the plan without executing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// `--executor` value enum (mirrors `ExecutorKind` in the runtime).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliExecutor {
    Local,
    Slurm,
}

impl From<CliExecutor> for m3flow_runtime::project::ExecutorKind {
    fn from(v: CliExecutor) -> Self {
        match v {
            CliExecutor::Local => m3flow_runtime::project::ExecutorKind::Local,
            CliExecutor::Slurm => m3flow_runtime::project::ExecutorKind::Slurm,
        }
    }
}

#[derive(Subcommand)]
enum RunCmd {
    /// List recent runs.
    List {
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Composite one-call status: run + steps + failure + next command.
    Status { id: String },
    /// Show run details incl. per-step status.
    Inspect { id: String },
    /// Show per-step logs (response documents, engine logs).
    Logs {
        id: String,
        /// Restrict to one step (unique prefix).
        #[arg(long)]
        step: Option<String>,
    },
    /// Show the run's dependency graph with statuses.
    Graph { id: String },
    /// Resume a failed/interrupted run, keeping completed steps.
    Resume { id: String },
    /// Re-execute one step and everything downstream of it.
    Retry { id: String, step: String },
    /// Set or clear a run's study label (moves its results/ folder).
    Label {
        id: String,
        /// New label ([A-Za-z0-9._-], max 64 chars); omit to clear.
        name: Option<String>,
    },
    /// Request cancellation of a running run.
    Cancel { id: String },
}

#[derive(Subcommand)]
enum ArtCmd {
    /// List artifacts, newest first.
    List {
        #[arg(long = "type")]
        type_filter: Option<String>,
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Show one artifact (files, metadata, data, producer).
    Inspect { id: String },
    /// Show the full upstream provenance tree.
    Lineage { id: String },
    /// Check whether an artifact can feed a task's inputs.
    Compatible { id: String, task: String },
    /// Register existing files as an artifact.
    Register {
        #[arg(long = "type")]
        artifact_type: String,
        /// name=path (repeatable).
        #[arg(long = "file", value_parser = parse_file_kv)]
        files: Vec<(String, PathBuf)>,
        /// JSON metadata object.
        #[arg(long)]
        meta: Option<String>,
        /// JSON data payload.
        #[arg(long)]
        data: Option<String>,
    },
}

#[derive(Subcommand)]
enum SchemaCmd {
    /// List known artifact types and available document schemas.
    List,
    /// Print a schema: task|workflow|system|artifact.
    Show { name: String },
}

#[derive(Subcommand)]
enum ProvCmd {
    /// List providers referenced by the registry/config and their status.
    List,
    /// Locate + describe a provider.
    Diagnose { name: String },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Show cache/store statistics.
    Stats,
    /// Drop all cache entries (artifacts are kept).
    Clear,
}

#[derive(Subcommand)]
enum ResultsCmd {
    /// Rebuild friendly results/ tree(s) from the provenance DB.
    Sync {
        /// Rebuild only this run's tree (default: all runs).
        #[arg(long)]
        run: Option<String>,
    },
}

// ------------------------------------------------------------------ parsing

fn parse_kv(s: &str) -> std::result::Result<(String, serde_json::Value), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got '{s}'"))?;
    let parsed: serde_json::Value = serde_yaml::from_str(v)
        .map_err(|e| format!("value for '{k}' is not valid YAML/JSON: {e}"))?;
    Ok((k.to_string(), parsed))
}

fn parse_input(s: &str) -> std::result::Result<(String, InputSource), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected name=art_xxx or name=@path, got '{s}'"))?;
    let src = if let Some(path) = v.strip_prefix('@') {
        InputSource::File(PathBuf::from(shellexpand::tilde(path).into_owned()))
    } else if v.starts_with("art_") {
        InputSource::Artifact(v.to_string())
    } else {
        return Err(format!(
            "input value must be an artifact id (art_...) or @path, got '{v}'"
        ));
    };
    Ok((k.to_string(), src))
}

fn parse_file_kv(s: &str) -> std::result::Result<(String, PathBuf), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected name=path, got '{s}'"))?;
    Ok((
        k.to_string(),
        PathBuf::from(shellexpand::tilde(v).into_owned()),
    ))
}

// ------------------------------------------------------------------ helpers

fn project() -> Result<Project> {
    Project::discover(&std::env::current_dir()?)
}

/// Registry with builtins, extended from the project when inside one.
fn registry() -> Result<Registry> {
    match project() {
        Ok(p) => run_api::open_registry(&p),
        Err(_) => Registry::with_builtins(),
    }
}

fn emit_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn fail(e: M3FlowError, json: bool) -> ! {
    if json {
        let v = serde_json::to_value(&e).unwrap_or_else(
            |_| serde_json::json!({"error_type": "internal", "message": e.to_string()}),
        );
        let mut obj = v.as_object().cloned().unwrap_or_default();
        obj.insert("category".into(), e.category().into());
        obj.insert("recoverable".into(), e.recoverable().into());
        emit_json(&serde_json::Value::Object(obj));
    } else {
        eprintln!("error: {e}");
    }
    std::process::exit(1);
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    match dispatch(cli.cmd, json) {
        Ok(code) => std::process::exit(code),
        Err(e) => fail(e, json),
    }
}

fn dispatch(cmd: Cmd, json: bool) -> Result<i32> {
    match cmd {
        Cmd::Init { dir, name } => {
            let dir = dir.unwrap_or_else(|| PathBuf::from("."));
            let p = Project::init(&dir, name.as_deref())?;
            if json {
                emit_json(&serde_json::json!({
                    "initialized": p.root.display().to_string(),
                }));
            } else {
                println!("initialized m3flow project in {}", p.root.display());
            }
            Ok(0)
        }
        Cmd::Task(c) => task_cmd(c, json),
        Cmd::Workflow(c) => wf_cmd(c, json),
        Cmd::Run(c) => run_cmd(c, json),
        Cmd::Results(c) => results_cmd(c, json),
        Cmd::Artifact(c) => art_cmd(c, json),
        Cmd::Schema(c) => schema_cmd(c, json),
        Cmd::Provider(c) => prov_cmd(c, json),
        Cmd::Cache(c) => cache_cmd(c, json),
        Cmd::Tui { run } => {
            let p = project()?;
            m3flow_tui::run(&p, run.as_deref())?;
            Ok(0)
        }
    }
}

// ------------------------------------------------------------------ task

fn task_json(t: &m3flow_core::specs::TaskSpec, reg: &Registry) -> serde_json::Value {
    let mut v = serde_json::to_value(t).unwrap_or_default();
    v["origin"] = reg
        .origin_of(&format!("{}@{}", t.name, t.version))
        .map(|s| serde_json::Value::String(s.to_string()))
        .unwrap_or(serde_json::Value::Null);
    v
}

fn task_cmd(c: TaskCmd, json: bool) -> Result<i32> {
    let reg = registry()?;
    match c {
        TaskCmd::List { category } => {
            let mut tasks = reg.tasks();
            tasks.sort_by(|a, b| a.name.cmp(&b.name));
            if let Some(cat) = &category {
                tasks.retain(|t| {
                    serde_json::to_value(t.category)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s == cat))
                        .unwrap_or(false)
                });
            }
            if json {
                let v: Vec<_> = tasks.iter().map(|t| task_json(t, &reg)).collect();
                emit_json(&serde_json::json!({"tasks": v}));
            } else {
                println!(
                    "{:<34} {:<10} {:<14} DESCRIPTION",
                    "TASK", "VERSION", "CATEGORY"
                );
                for t in tasks {
                    let cat = serde_json::to_value(t.category)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_default();
                    println!(
                        "{:<34} {:<10} {:<14} {}",
                        t.name,
                        t.version,
                        cat,
                        t.description.lines().next().unwrap_or("")
                    );
                }
            }
        }
        TaskCmd::Search { query } => {
            let hits = reg.search_tasks(&query);
            if json {
                let v: Vec<_> = hits.iter().map(|t| task_json(t, &reg)).collect();
                emit_json(&serde_json::json!({"tasks": v}));
            } else {
                for t in hits {
                    println!(
                        "{}@{} — {}",
                        t.name,
                        t.version,
                        t.description.lines().next().unwrap_or("")
                    );
                }
            }
        }
        TaskCmd::Inspect { name } => {
            let t = reg.task(&name)?;
            emit_json(&task_json(t, &reg));
        }
    }
    Ok(0)
}

// ------------------------------------------------------------------ workflow

fn wf_cmd(c: WfCmd, json: bool) -> Result<i32> {
    match c {
        WfCmd::List => {
            let reg = registry()?;
            let mut wfs = reg.workflows();
            wfs.sort_by(|a, b| a.name.cmp(&b.name));
            if json {
                let v: Vec<_> = wfs
                    .iter()
                    .map(|w| serde_json::to_value(w).unwrap_or_default())
                    .collect();
                emit_json(&serde_json::json!({"workflows": v}));
            } else {
                println!("{:<44} DESCRIPTION", "WORKFLOW");
                for w in wfs {
                    println!(
                        "{:<44} {}",
                        format!("{}@{}", w.name, w.version),
                        w.description.lines().next().unwrap_or("")
                    );
                }
            }
        }
        WfCmd::Inspect { name } => {
            let reg = registry()?;
            let w = reg.workflow(&name)?;
            emit_json(&serde_json::to_value(w).unwrap_or_default());
        }
        WfCmd::Validate { name_or_file } => {
            let path = PathBuf::from(&name_or_file);
            if path.is_file() {
                let text = std::fs::read_to_string(&path)?;
                let mut reg = Registry::with_builtins()?;
                reg.load_text(&text, &name_or_file)?;
                if json {
                    emit_json(&serde_json::json!({"valid": true, "file": name_or_file}));
                } else {
                    println!("{name_or_file}: valid");
                }
            } else {
                let reg = registry()?;
                let w = reg.workflow(&name_or_file)?; // resolution = registered + schema-valid at load
                if json {
                    emit_json(&serde_json::json!({
                        "valid": true,
                        "workflow": format!("{}@{}", w.name, w.version),
                    }));
                } else {
                    println!("{}@{}: valid (registered)", w.name, w.version);
                }
            }
        }
        WfCmd::Plan { name, params } => {
            let p = project()?;
            let compiled = run_api::plan_workflow(&p, &name, &params.into_iter().collect())?;
            print_plan(&compiled, json);
        }
        WfCmd::Run {
            name,
            inputs,
            params,
            no_cache,
            no_materialize,
            label,
            max_concurrency,
            executor,
            dry_run,
        } => {
            let p = project()?;
            let params_map = params.into_iter().collect();
            if dry_run {
                let compiled = run_api::plan_workflow(&p, &name, &params_map)?;
                print_plan(&compiled, json);
                return Ok(0);
            }
            let progress: Option<Box<dyn Fn(&str) + Send>> = if json {
                None
            } else {
                Some(Box::new(|msg: &str| eprintln!("  {msg}")))
            };
            let rec = run_api::run_workflow(
                &p,
                &name,
                RunOptions {
                    inputs: inputs.into_iter().collect(),
                    params: params_map,
                    no_cache,
                    no_materialize,
                    label,
                    max_concurrency,
                    executor_override: executor.map(Into::into),
                    progress,
                },
            )?;
            if json {
                emit_json(&serde_json::to_value(&rec).unwrap_or_default());
            } else {
                println!(
                    "run {} finished: {} (workflow {}@{})",
                    rec.id,
                    rec.status.as_str(),
                    rec.name,
                    rec.version
                );
                if let Some(outputs) = &rec.outputs {
                    for (k, v) in outputs.as_object().into_iter().flatten() {
                        println!("  output {k}: {}", v.as_str().unwrap_or(&v.to_string()));
                    }
                }
                print_failures(&p, &rec);
            }
            return Ok(match rec.status {
                m3flow_core::artifact::RunStatus::Completed => 0,
                m3flow_core::artifact::RunStatus::Cancelled => 3,
                _ => 2,
            });
        }
    }
    Ok(0)
}

fn print_plan(compiled: &m3flow_runtime::ir::CompiledWorkflow, json: bool) {
    if json {
        emit_json(&serde_json::to_value(compiled).unwrap_or_default());
        return;
    }
    println!(
        "workflow {}@{} — {} steps",
        compiled.name,
        compiled.version,
        compiled.nodes.len()
    );
    for (i, n) in compiled.nodes.iter().enumerate() {
        let deps = if n.deps.is_empty() {
            String::new()
        } else {
            format!("  (after {})", n.deps.join(", "))
        };
        println!(
            "  {:>3}. {:<28} {:<34}{}{}",
            i + 1,
            n.id,
            n.task,
            deps,
            if n.label.is_empty() {
                String::new()
            } else {
                format!("  [{}]", n.label)
            }
        );
    }
}

// ------------------------------------------------------------------ run

fn run_cmd(c: RunCmd, json: bool) -> Result<i32> {
    let p = project()?;
    match c {
        RunCmd::List { limit } => {
            let v = run_api::list_runs_json(&p, limit)?;
            if json {
                emit_json(&v);
            } else {
                println!(
                    "{:<14} {:<36} {:<22} {:<10} CREATED",
                    "RUN", "WORKFLOW", "LABEL", "STATUS"
                );
                for r in v["runs"].as_array().into_iter().flatten() {
                    let label = r["label"].as_str().unwrap_or("-");
                    let label = if label.chars().count() > 20 {
                        format!("{}…", label.chars().take(19).collect::<String>())
                    } else {
                        label.to_string()
                    };
                    println!(
                        "{:<14} {:<36} {:<22} {:<10} {}",
                        r["id"].as_str().unwrap_or(""),
                        format!(
                            "{}@{}",
                            r["name"].as_str().unwrap_or(""),
                            r["version"].as_str().unwrap_or("")
                        ),
                        label,
                        r["status"].as_str().unwrap_or(""),
                        r["created_at"].as_str().unwrap_or("")
                    );
                }
            }
        }
        RunCmd::Status { id } => {
            let v = run_api::run_status_json(&p, &id)?;
            if json {
                emit_json(&v);
            } else {
                let r = &v["run"];
                let label = r["label"]
                    .as_str()
                    .map(|l| format!("  [{l}]"))
                    .unwrap_or_default();
                println!(
                    "run {} — {} — {}{}",
                    r["id"].as_str().unwrap_or(""),
                    r["workflow"].as_str().unwrap_or(""),
                    r["status"].as_str().unwrap_or(""),
                    label
                );
                println!(
                    "{:<30} {:<10} {:<8} OUTPUTS",
                    "STEP", "STATUS", "ATTEMPTS"
                );
                for s in v["steps"].as_array().into_iter().flatten() {
                    let outs = s["outputs"].as_object().map(|o| o.len()).unwrap_or(0);
                    println!(
                        "{:<30} {:<10} {:<8} {}",
                        s["node"].as_str().unwrap_or(""),
                        s["status"].as_str().unwrap_or(""),
                        s["attempts"].as_i64().unwrap_or(0),
                        if outs == 0 {
                            "-".into()
                        } else {
                            format!("{outs} artifact(s)")
                        }
                    );
                }
                if let Some(f) = v["failure"].as_object() {
                    println!(
                        "failure: step {} — {} — {}",
                        f["step"].as_str().unwrap_or("<run>"),
                        f["category"].as_str().unwrap_or("unknown"),
                        f["message"].as_str().unwrap_or("")
                    );
                }
                if let Some(next) = v["next"].as_str() {
                    println!("next: {next}");
                }
                if v["materialized"].as_bool() == Some(true) {
                    println!("results: {}", v["results_dir"].as_str().unwrap_or(""));
                }
            }
        }
        RunCmd::Inspect { id } => {
            let v = run_api::run_json(&p, &id)?;
            if json {
                emit_json(&v);
            } else {
                let r = &v["run"];
                println!(
                    "run {} — {}@{} — {}",
                    r["id"].as_str().unwrap_or(""),
                    r["name"].as_str().unwrap_or(""),
                    r["version"].as_str().unwrap_or(""),
                    r["status"].as_str().unwrap_or("")
                );
                println!("{:<30} {:<10} {:<8} TASK", "STEP", "STATUS", "ATTEMPTS");
                for t in v["tasks"].as_array().into_iter().flatten() {
                    println!(
                        "{:<30} {:<10} {:<8} {}@{}",
                        t["node_id"].as_str().unwrap_or(""),
                        t["status"].as_str().unwrap_or(""),
                        t["attempts"].as_i64().unwrap_or(0),
                        t["task_name"].as_str().unwrap_or(""),
                        t["task_version"].as_str().unwrap_or("")
                    );
                    // A FAILED row without its reason sends the user digging
                    // through workdirs; the error JSON is already on the row.
                    if t["status"].as_str() == Some("FAILED") {
                        let msg = t["error"]
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("no message recorded");
                        println!("   error: {msg}");
                        if let Some(d) = t["error"].get("details") {
                            if !d.is_null() {
                                println!("   details: {d}");
                            }
                        }
                    }
                }
            }
        }
        RunCmd::Logs { id, step } => {
            logs_cmd(&p, &id, step.as_deref(), json)?;
        }
        RunCmd::Graph { id } => {
            let v = run_api::run_graph_json(&p, &id)?;
            if json {
                emit_json(&v);
            } else {
                println!(
                    "run {} — {}",
                    v["run"],
                    v["workflow"].as_str().unwrap_or("")
                );
                for n in v["nodes"].as_array().into_iter().flatten() {
                    println!(
                        "  {:<30} {:<10} {}",
                        n["id"].as_str().unwrap_or(""),
                        n["status"].as_str().unwrap_or(""),
                        n["task"].as_str().unwrap_or("")
                    );
                }
                println!("edges:");
                for e in v["edges"].as_array().into_iter().flatten() {
                    println!(
                        "  {} -> {}",
                        e["from"].as_str().unwrap_or(""),
                        e["to"].as_str().unwrap_or("")
                    );
                }
            }
        }
        RunCmd::Resume { id } => {
            let progress = progress_cb(json);
            let rec = run_api::resume_run(&p, &id, progress)?;
            return finish_run(&p, rec, json);
        }
        RunCmd::Retry { id, step } => {
            let progress = progress_cb(json);
            let rec = run_api::retry_step(&p, &id, &step, progress)?;
            return finish_run(&p, rec, json);
        }
        RunCmd::Label { id, name } => {
            let v = run_api::label_run(&p, &id, name.as_deref())?;
            if json {
                emit_json(&v);
            } else {
                match v["label"].as_str() {
                    Some(l) => println!("run {id} labeled '{l}'"),
                    None => println!("run {id} label cleared"),
                }
                if let Some(m) = v["moved"].as_object() {
                    println!(
                        "  {} -> {}",
                        m["from"].as_str().unwrap_or(""),
                        m["to"].as_str().unwrap_or("")
                    );
                }
            }
        }
        RunCmd::Cancel { id } => {
            run_api::cancel_run(&p, &id)?;
            if json {
                emit_json(&serde_json::json!({"cancel_requested": id}));
            } else {
                println!("cancellation requested for run {id}");
            }
        }
    }
    Ok(0)
}

fn progress_cb(json: bool) -> Option<Box<dyn Fn(&str) + Send>> {
    if json {
        None
    } else {
        Some(Box::new(|msg: &str| eprintln!("  {msg}")))
    }
}

fn finish_run(p: &Project, rec: m3flow_runtime::db::WorkflowRunRecord, json: bool) -> Result<i32> {
    if json {
        emit_json(&serde_json::to_value(&rec).unwrap_or_default());
    } else {
        println!("run {} finished: {}", rec.id, rec.status.as_str());
        print_failures(p, &rec);
    }
    Ok(match rec.status {
        m3flow_core::artifact::RunStatus::Completed => 0,
        m3flow_core::artifact::RunStatus::Cancelled => 3,
        _ => 2,
    })
}

/// Print why a run failed: each failed step's recorded error message plus
/// provider-supplied details (e.g. gate checks/metrics). The provenance DB
/// already stores all of this on the task_run row — surface it so a failure
/// is actionable without digging through workdirs.
fn print_failures(p: &Project, rec: &m3flow_runtime::db::WorkflowRunRecord) {
    if !matches!(rec.status, m3flow_core::artifact::RunStatus::Failed) {
        return;
    }
    if let Some(e) = &rec.error {
        let msg = e
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| e.to_string());
        if !msg.is_empty() {
            println!("  run error: {msg}");
        }
    }
    let Ok(db) = run_api::open_db(p) else { return };
    let Ok(tasks) = db.task_runs_of(rec.id.as_str()) else {
        return;
    };
    for t in tasks
        .iter()
        .filter(|t| matches!(t.status, m3flow_core::artifact::TaskStatus::Failed))
    {
        let msg = t
            .error
            .as_ref()
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("no message recorded");
        println!(
            "  failed: {} ({}@{}): {msg}",
            t.node_id, t.task_name, t.task_version
        );
        if let Some(details) = t.error.as_ref().and_then(|e| e.get("details")) {
            if !details.is_null() {
                println!("          details: {details}");
            }
        }
    }
}

fn logs_cmd(p: &Project, run_id: &str, step: Option<&str>, json: bool) -> Result<i32> {
    let run_dir = p.runs_dir().join(run_id);
    if !run_dir.is_dir() {
        return Err(M3FlowError::not_found(format!(
            "run directory for '{run_id}'"
        )));
    }
    let mut steps: Vec<PathBuf> = std::fs::read_dir(&run_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    steps.sort();
    if let Some(want) = step {
        let matches: Vec<_> = steps
            .iter()
            .filter(|s| {
                s.file_name()
                    .map(|n| n.to_string_lossy().starts_with(want))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if matches.len() != 1 {
            return Err(M3FlowError::not_found(format!(
                "step prefix '{want}' matched {} step directories",
                matches.len()
            )));
        }
        steps = matches;
    }
    let mut out = Vec::new();
    for dir in &steps {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let mut files: Vec<String> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        files.sort();
        let response = std::fs::read_to_string(dir.join("response.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        out.push(serde_json::json!({
            "step": name,
            "workdir": dir.display().to_string(),
            "files": files,
            "response": response,
        }));
    }
    if json {
        emit_json(&serde_json::json!({"run": run_id, "steps": out}));
    } else {
        for s in &out {
            println!("== {} ({})", s["step"], s["workdir"].as_str().unwrap_or(""));
            if let Some(r) = s["response"].as_object() {
                println!(
                    "   status: {}   engine: {}",
                    r.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                    r.get("engine")
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "?".into())
                );
                if let Some(err) = r.get("error") {
                    if !err.is_null() {
                        println!("   error: {}", err);
                    }
                }
            }
            let files: Vec<&str> = s["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str())
                .collect();
            println!("   files: {}", files.join(", "));
        }
    }
    Ok(0)
}

// ------------------------------------------------------------------ artifact

fn art_cmd(c: ArtCmd, json: bool) -> Result<i32> {
    let p = project()?;
    match c {
        ArtCmd::List { type_filter, limit } => {
            let v = run_api::list_artifacts_json(&p, type_filter.as_deref(), limit)?;
            if json {
                emit_json(&v);
            } else {
                println!("{:<16} {:<24} {:<16} CREATED", "ARTIFACT", "TYPE", "HASH");
                for a in v["artifacts"].as_array().into_iter().flatten() {
                    println!(
                        "{:<16} {:<24} {:<16} {}",
                        a["id"].as_str().unwrap_or(""),
                        a["type"].as_str().unwrap_or(""),
                        &a["content_hash"].as_str().unwrap_or("")
                            [..12.min(a["content_hash"].as_str().unwrap_or("").len())],
                        a["created_at"].as_str().unwrap_or("")
                    );
                }
            }
        }
        ArtCmd::Inspect { id } => emit_json(&run_api::artifact_json(&p, &id)?),
        ArtCmd::Lineage { id } => emit_json(&run_api::lineage_json(&p, &id)?),
        ArtCmd::Compatible { id, task } => {
            let v = run_api::compatible_json(&p, &id, &task)?;
            if json {
                emit_json(&v);
            } else if v["compatible"].as_bool().unwrap_or(false) {
                println!("{} can feed {} via:", id, task);
                for m in v["matching_inputs"].as_array().into_iter().flatten() {
                    println!(
                        "  input '{}' (declared {})",
                        m["input"].as_str().unwrap_or(""),
                        m["declared_type"].as_str().unwrap_or("")
                    );
                }
            } else {
                println!("{id} is NOT compatible with any input of {task}");
            }
        }
        ArtCmd::Register {
            artifact_type,
            files,
            meta,
            data,
        } => {
            if files.is_empty() {
                return Err(M3FlowError::schema(
                    "artifact register needs at least one --file name=path",
                ));
            }
            let meta = match meta {
                Some(m) => serde_json::from_str(&m)
                    .map_err(|e| M3FlowError::schema(format!("--meta: {e}")))?,
                None => serde_json::json!({}),
            };
            let data = match data {
                Some(d) => Some(
                    serde_json::from_str(&d)
                        .map_err(|e| M3FlowError::schema(format!("--data: {e}")))?,
                ),
                None => None,
            };
            let v = run_api::register_artifact(
                &p,
                &artifact_type,
                &files.into_iter().collect::<BTreeMap<_, _>>(),
                meta,
                data,
            )?;
            emit_json(&v);
        }
    }
    Ok(0)
}

// ------------------------------------------------------------------ schema

fn schema_cmd(c: SchemaCmd, json: bool) -> Result<i32> {
    match c {
        SchemaCmd::List => {
            let types = m3flow_core::atypes::all_types();
            if json {
                emit_json(&serde_json::json!({
                    "document_schemas": ["task", "workflow", "system", "artifact"],
                    "artifact_types": types,
                }));
            } else {
                println!("document schemas: task, workflow, system, artifact");
                println!("artifact types:");
                for t in types {
                    let parent = m3flow_core::atypes::parent_of(t).unwrap_or("-");
                    println!("  {t:<28} <: {parent}");
                }
            }
        }
        SchemaCmd::Show { name } => {
            let text = m3flow_registry::schema_text(&name).ok_or_else(|| {
                M3FlowError::not_found(format!(
                    "schema '{name}' (choose: task, workflow, system, artifact)"
                ))
            })?;
            println!("{text}");
        }
    }
    Ok(0)
}

// ------------------------------------------------------------------ provider

fn prov_cmd(c: ProvCmd, json: bool) -> Result<i32> {
    match c {
        ProvCmd::List => {
            let reg = registry()?;
            let proj = project().ok();
            let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for t in reg.tasks() {
                for i in &t.implementations {
                    names.insert(i.provider.clone());
                }
            }
            if let Some(p) = &proj {
                for k in p.config.providers.keys() {
                    names.insert(k.clone());
                }
            }
            let mut rows = Vec::new();
            for n in &names {
                let cfg = proj.as_ref().and_then(|p| p.provider_config(n));
                match ProviderHandle::locate(n, cfg) {
                    Ok(mut h) => {
                        let version = h.engine_version().unwrap_or_else(|_| "unknown".into());
                        rows.push(serde_json::json!({
                            "name": n,
                            "executable": h.executable.display().to_string(),
                            "available": true,
                            "engine": version,
                        }));
                    }
                    Err(_) => {
                        rows.push(serde_json::json!({
                            "name": n,
                            "available": false,
                        }));
                    }
                }
            }
            if json {
                emit_json(&serde_json::json!({"providers": rows}));
            } else {
                println!(
                    "{:<16} {:<10} {:<24} EXECUTABLE",
                    "PROVIDER", "STATUS", "ENGINE"
                );
                for r in &rows {
                    println!(
                        "{:<16} {:<10} {:<24} {}",
                        r["name"].as_str().unwrap_or(""),
                        if r["available"].as_bool().unwrap_or(false) {
                            "ok"
                        } else {
                            "MISSING"
                        },
                        r["engine"].as_str().unwrap_or("-"),
                        r["executable"].as_str().unwrap_or("-")
                    );
                }
            }
        }
        ProvCmd::Diagnose { name } => {
            let proj = project().ok();
            let cfg = proj.as_ref().and_then(|p| p.provider_config(&name));
            let mut h = ProviderHandle::locate(&name, cfg)?;
            let desc = h.describe()?.clone();
            emit_json(&serde_json::json!({
                "provider": name,
                "executable": h.executable.display().to_string(),
                "describe": desc,
            }));
        }
    }
    Ok(0)
}

// ------------------------------------------------------------------ cache

fn cache_cmd(c: CacheCmd, json: bool) -> Result<i32> {
    let p = project()?;
    match c {
        CacheCmd::Stats => {
            let v = run_api::cache_stats_json(&p)?;
            if json {
                emit_json(&v);
            } else {
                println!(
                    "cache entries: {}   artifacts: {}",
                    v["cache_entries"], v["artifacts"]
                );
            }
        }
        CacheCmd::Clear => {
            let n = run_api::cache_clear(&p)?;
            if json {
                emit_json(&serde_json::json!({"cleared": n}));
            } else {
                println!("cleared {n} cache entries");
            }
        }
    }
    Ok(0)
}

fn results_cmd(c: ResultsCmd, json: bool) -> Result<i32> {
    let p = project()?;
    match c {
        ResultsCmd::Sync { run } => {
            let v = run_api::results_sync(&p, run.as_deref())?;
            if json {
                emit_json(&v);
            } else {
                let dirs = v["rebuilt"].as_array().cloned().unwrap_or_default();
                println!("rebuilt {} results tree(s)", dirs.len());
                for d in dirs {
                    println!("  {}", d.as_str().unwrap_or_default());
                }
            }
        }
    }
    Ok(0)
}

// keep parse_any_id referenced for future id-taking commands
#[allow(dead_code)]
fn _id_kind(s: &str) -> Option<&'static str> {
    parse_any_id(s).map(|(k, _)| k)
}
