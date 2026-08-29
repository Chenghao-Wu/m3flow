//! Regression test: resolved task params must persist to the task_run row
//! for *executed* tasks (not only cache hits), and both workflow-level and
//! per-step params must appear in the materialized `results/<...>/run.json`
//! — including after a `results sync` rebuild from the DB.
//!
//! Uses a fake local provider (shell script on PATH); no engines needed.

use m3flow_core::artifact::{RunStatus, TaskStatus};
use m3flow_runtime::project::Project;
use m3flow_runtime::run_api::{self, RunOptions};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// PATH is process-global; scenarios must not run concurrently.
static LOCK: Mutex<()> = Mutex::new(());

struct PathGuard {
    original: String,
}

impl PathGuard {
    fn prepend(dir: &Path) -> Self {
        let original = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", dir.display(), original));
        Self { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        std::env::set_var("PATH", &self.original);
    }
}

struct Fixture {
    root: PathBuf,
    project: Project,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn fixture(tag: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("m3params-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    let proj_dir = root.join("proj");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&proj_dir).unwrap();

    write_exe(
        &bin.join("m3flow-fake"),
        r#"#!/bin/bash
case "$1" in
  describe)
    echo '{"protocol":"m3flow-provider/1","provider":{"name":"fake","version":"0.1.0"},"engine":{"name":"fake","version":"0.1"},"tasks":[]}'
    ;;
  execute)
    wd=$(grep -oP '"workdir":\s*"\K[^"]+' "$2")
    echo "fake result" > "$wd/result.txt"
    echo '{"status":"success","outputs":{"result":{"type":"Result","files":{"summary":"result.txt"},"metadata":{},"data":{"value":42}}},"validation":[],"engine":{"name":"fake","version":"0.1"},"warnings":[]}'
    ;;
esac
"#,
    );

    // no `executor:` block → local executor
    std::fs::write(
        proj_dir.join("m3flow.yaml"),
        format!(
            r#"schema: m3flow-project/v1
providers:
  fake:
    executable: {}
"#,
            bin.join("m3flow-fake").display()
        ),
    )
    .unwrap();

    std::fs::create_dir_all(proj_dir.join("tasks")).unwrap();
    std::fs::write(
        proj_dir.join("tasks").join("param_task.yaml"),
        r#"schema: task/v1
name: param_task
version: 1.0.0
description: task with required + defaulted params
category: utility
inputs: {}
parameters:
  temperature: {type: temperature, required: true}
  timestep: {type: time, default: 1 fs}
  seed: {type: integer, default: 7}
outputs:
  result: {type: Result}
implementations:
  - provider: fake
    default: true
"#,
    )
    .unwrap();

    std::fs::create_dir_all(proj_dir.join("workflows")).unwrap();
    std::fs::write(
        proj_dir.join("workflows").join("param_flow.yaml"),
        r#"schema: workflow/v1
name: param_flow
version: 1.0.0
parameters:
  temperature: {type: temperature, required: true}
  note: {type: string, default: "wf-default-note"}
steps:
  run:
    task: param_task
    parameters:
      temperature: "${params.temperature}"
outputs:
  result: {value: "${run.result}"}
"#,
    )
    .unwrap();

    let project = Project::load(proj_dir).unwrap();
    Fixture { root, project }
}

fn run_opts() -> RunOptions {
    let mut params = serde_json::Map::new();
    params.insert("temperature".into(), serde_json::json!("350 K"));
    RunOptions {
        inputs: BTreeMap::new(),
        params,
        no_cache: false,
        no_materialize: false,
        label: Some("params-test".to_string()),
        max_concurrency: Some(1),
        executor_override: None,
        progress: None,
    }
}

fn run_json_path(project: &Project, run_id: &str) -> PathBuf {
    let dir = project.root.join("results").join("params-test");
    let entry = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().contains(run_id))
        .expect("run results dir missing");
    entry.path().join("run.json")
}

#[test]
fn executed_task_params_persist_and_appear_in_run_json() {
    let _lock = LOCK.lock().unwrap();
    let fx = fixture("exec");
    let _path = PathGuard::prepend(&fx.root.join("bin"));

    let rec = run_api::run_workflow(&fx.project, "param_flow", run_opts()).unwrap();
    assert_eq!(rec.status, RunStatus::Completed);

    // ---- DB: executed (not cached) task must carry resolved params
    let db = run_api::open_db(&fx.project).unwrap();
    let runs = db.task_runs_of(rec.id.as_str()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, TaskStatus::Completed);
    let p = &runs[0].params;
    assert_eq!(p["temperature"], serde_json::json!({"value": 350.0, "unit": "K"}));
    // defaults applied by the scheduler
    assert_eq!(p["timestep"], serde_json::json!({"value": 1.0, "unit": "fs"}));
    assert_eq!(p["seed"], serde_json::json!(7));

    // ---- workflow-level params on the run row
    let wf = db.get_workflow_run(rec.id.as_str()).unwrap();
    assert_eq!(
        wf.params["temperature"],
        serde_json::json!({"value": 350.0, "unit": "K"})
    );
    assert_eq!(wf.params["note"], serde_json::json!("wf-default-note"));

    // ---- run.json: workflow params + per-step params
    let rj: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run_json_path(&fx.project, rec.id.as_str())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        rj["params"]["temperature"],
        serde_json::json!({"value": 350.0, "unit": "K"})
    );
    assert_eq!(rj["params"]["note"], serde_json::json!("wf-default-note"));
    let step = &rj["steps"][0];
    assert_eq!(step["node_id"], serde_json::json!("run"));
    assert_eq!(
        step["params"]["temperature"],
        serde_json::json!({"value": 350.0, "unit": "K"})
    );
    assert_eq!(
        step["params"]["timestep"],
        serde_json::json!({"value": 1.0, "unit": "fs"})
    );
    assert_eq!(step["params"]["seed"], serde_json::json!(7));

    // ---- cache-hit rerun keeps params on the row too
    let rec2 = run_api::run_workflow(&fx.project, "param_flow", run_opts()).unwrap();
    assert_eq!(rec2.status, RunStatus::Completed);
    let runs2 = db.task_runs_of(rec2.id.as_str()).unwrap();
    assert_eq!(runs2[0].status, TaskStatus::Cached);
    assert_eq!(
        runs2[0].params["temperature"],
        serde_json::json!({"value": 350.0, "unit": "K"})
    );

    // ---- rebuild from DB (`results sync`) preserves params in run.json
    run_api::results_sync(&fx.project, Some(rec.id.as_str())).unwrap();
    let rj2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run_json_path(&fx.project, rec.id.as_str())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        rj2["params"]["temperature"],
        serde_json::json!({"value": 350.0, "unit": "K"})
    );
    assert_eq!(
        rj2["steps"][0]["params"]["seed"],
        serde_json::json!(7)
    );
}
