//! End-to-end test of the Slurm executor against a fake cluster: shell
//! scripts impersonating `sbatch`/`squeue`/`sacct`/`scancel` on a prepended
//! PATH, plus a fake provider executable. No real Slurm required.
//!
//! The fake sbatch launches the generated batch script in the background and
//! reports the child PID as the job id, so `squeue` (kill -0), `scancel`
//! (kill), and the `.m3flow_exit` marker all behave like the real thing.
//! `FAKE_MODE=timeout` in the batch script (injected via `setup_commands`)
//! simulates a job killed by the scheduler: never launched, `sacct` reports
//! TIMEOUT.

use m3flow_core::artifact::{RunStatus, TaskStatus};
use m3flow_runtime::project::Project;
use m3flow_runtime::run_api::{self, RunOptions};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// PATH is process-global; scenarios must not run concurrently.
static SCENARIO_LOCK: Mutex<()> = Mutex::new(());

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
    state: PathBuf,
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

/// Build a fake cluster + project for one scenario. `provider` selects the
/// provider behavior (ok | fail | slow), `setup_lines` lands in the
/// executor's `setup_commands` (where the fake sbatch finds FAKE_MODE).
fn fixture(tag: &str, provider: &str, setup_lines: &[&str]) -> Fixture {
    let root = std::env::temp_dir().join(format!("m3slurm-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    let state = root.join("state");
    let proj_dir = root.join("proj");
    for d in [&bin, &state, &proj_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    let st = state.display().to_string();

    write_exe(
        &bin.join("sbatch"),
        &format!(
            r#"#!/bin/bash
# fake sbatch --parsable <script>
for script; do :; done
mode=$(grep -oP 'FAKE_MODE=\K\w+' "$script" | head -1)
mode=${{mode:-success}}
if [ "$mode" = "timeout" ]; then
  jobid=$((RANDOM + 20000))
  echo "$jobid TIMEOUT" >> "{st}/jobs"
  printf '%s\n' "$jobid"
  exit 0
fi
bash "$script" &
jobid=$!
echo "$jobid RUNNING" >> "{st}/jobs"
printf '%s\n' "$jobid"
"#,
        ),
    );
    write_exe(
        &bin.join("squeue"),
        &format!(
            r#"#!/bin/bash
# fake squeue -h -j <id> -o %T
while [ $# -gt 0 ]; do
  if [ "$1" = "-j" ]; then jobid="$2"; shift 2; continue; fi
  shift
done
if kill -0 "$jobid" 2>/dev/null; then
  echo "RUNNING"
  exit 0
fi
echo "slurm_load_jobs error: Invalid job id specified" >&2
exit 1
# state dir: {st}
"#,
        ),
    );
    write_exe(
        &bin.join("sacct"),
        &format!(
            r#"#!/bin/bash
# fake sacct -X -n -P -j <id> -o State
while [ $# -gt 0 ]; do
  if [ "$1" = "-j" ]; then jobid="$2"; shift 2; continue; fi
  shift
done
grep "^$jobid " "{st}/jobs" 2>/dev/null | awk '{{print $2}}' | head -1
exit 0
"#,
        ),
    );
    write_exe(
        &bin.join("scancel"),
        &format!(
            r#"#!/bin/bash
echo "$1" >> "{st}/scancelled"
kill "$1" 2>/dev/null
exit 0
"#,
        ),
    );

    // fake provider: describe + execute per scenario flavor
    let execute_body = match provider {
        "fail" => {
            r#"
    echo '{"status":"error","error":{"error_type":"engine_blew_up","category":"execution_error","recoverable":false,"message":"kaboom"}}'
    exit 1
"#
        }
        "slow" => {
            r#"
    sleep 30
    wd=$(grep -oP '"workdir":\s*"\K[^"]+' "$2")
    echo "fake result" > "$wd/result.txt"
    echo '{"status":"success","outputs":{"result":{"type":"Result","files":{"summary":"result.txt"},"metadata":{},"data":{"value":42}}},"validation":[],"engine":{"name":"fake","version":"0.1"},"warnings":[]}'
"#
        }
        _ => {
            r#"
    wd=$(grep -oP '"workdir":\s*"\K[^"]+' "$2")
    echo "fake result" > "$wd/result.txt"
    echo '{"status":"success","outputs":{"result":{"type":"Result","files":{"summary":"result.txt"},"metadata":{},"data":{"value":42}}},"validation":[],"engine":{"name":"fake","version":"0.1"},"warnings":[]}'
"#
        }
    };
    write_exe(
        &bin.join("m3flow-fake"),
        &format!(
            r#"#!/bin/bash
case "$1" in
  describe)
    echo '{{"protocol":"m3flow-provider/1","provider":{{"name":"fake","version":"0.1.0"}},"engine":{{"name":"fake","version":"0.1"}},"tasks":[]}}'
    ;;
  execute){execute_body}
    ;;
esac
"#,
        ),
    );

    let setup_yaml = if setup_lines.is_empty() {
        " []\n".to_string()
    } else {
        format!(
            "\n{}",
            setup_lines
                .iter()
                .map(|l| format!("      - \"{l}\"\n"))
                .collect::<String>()
        )
    };
    std::fs::write(
        proj_dir.join("m3flow.yaml"),
        format!(
            r#"schema: m3flow-project/v1
executor:
  type: slurm
  slurm:
    partition: fakepart
    qos: fakeqos
    poll_interval_secs: 2
    setup_commands:{setup_yaml}providers:
  fake:
    executable: {}
"#,
            bin.join("m3flow-fake").display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(proj_dir.join("tasks")).unwrap();
    std::fs::write(
        proj_dir.join("tasks").join("fake_task.yaml"),
        r#"schema: task/v1
name: fake_task
version: 1.0.0
description: fake task for executor tests
category: utility
inputs: {}
parameters: {}
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
        proj_dir.join("workflows").join("fake_flow.yaml"),
        r#"schema: workflow/v1
name: fake_flow
version: 1.0.0
steps:
  make:
    task: fake_task
outputs:
  result: {value: "${make.result}"}
"#,
    )
    .unwrap();

    let project = Project::load(proj_dir).unwrap();
    Fixture {
        root,
        state,
        project,
    }
}

fn run_opts() -> RunOptions {
    RunOptions {
        inputs: BTreeMap::new(),
        params: serde_json::Map::new(),
        no_cache: false,
        no_materialize: true,
        label: None,
        max_concurrency: Some(2),
        executor_override: None,
        progress: None,
    }
}

fn node_workdir(project: &Project, run_id: &str) -> PathBuf {
    project.runs_dir().join(run_id).join("make")
}

#[test]
fn slurm_success_path_matches_local_contract() {
    let _lock = SCENARIO_LOCK.lock().unwrap();
    let fx = fixture("success", "ok", &[]);
    let _path = PathGuard::prepend(&fx.root.join("bin"));

    let rec = run_api::run_workflow(&fx.project, "fake_flow", run_opts()).unwrap();
    assert_eq!(rec.status, RunStatus::Completed);
    assert!(rec
        .outputs
        .as_ref()
        .and_then(|o| o.as_object())
        .unwrap()
        .contains_key("result"));

    // workdir contract: everything a local run leaves, plus slurm artifacts
    let wd = node_workdir(&fx.project, rec.id.as_str());
    for f in [
        "request.json",
        "response.json",
        "submit.sh",
        "slurm_job_id",
        ".m3flow_exit",
        "provider_stdout.json",
        "provider_stderr.log",
    ] {
        assert!(wd.join(f).is_file(), "missing {}", wd.join(f).display());
    }
    assert_eq!(
        std::fs::read_to_string(wd.join(".m3flow_exit"))
            .unwrap()
            .trim(),
        "0"
    );
    let script = std::fs::read_to_string(wd.join("submit.sh")).unwrap();
    assert!(script.contains("#SBATCH --partition=fakepart"));
    assert!(script.contains("#SBATCH --qos=fakeqos"));
    assert!(script.contains("m3flow-fake' execute '"));

    // artifacts ingested → rerun is a cache hit
    let rec2 = run_api::run_workflow(&fx.project, "fake_flow", run_opts()).unwrap();
    assert_eq!(rec2.status, RunStatus::Completed);
    let db = run_api::open_db(&fx.project).unwrap();
    let runs = db.task_runs_of(rec2.id.as_str()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, TaskStatus::Cached);
}

#[test]
fn slurm_provider_failure_flows_through_marker() {
    let _lock = SCENARIO_LOCK.lock().unwrap();
    let fx = fixture("provfail", "fail", &[]);
    let _path = PathGuard::prepend(&fx.root.join("bin"));

    let rec = run_api::run_workflow(&fx.project, "fake_flow", run_opts()).unwrap();
    assert_eq!(rec.status, RunStatus::Failed);

    let wd = node_workdir(&fx.project, rec.id.as_str());
    assert_eq!(
        std::fs::read_to_string(wd.join(".m3flow_exit"))
            .unwrap()
            .trim(),
        "1"
    );
    let db = run_api::open_db(&fx.project).unwrap();
    let runs = db.task_runs_of(rec.id.as_str()).unwrap();
    assert_eq!(runs[0].status, TaskStatus::Failed);
    let err = runs[0].error.as_ref().unwrap();
    assert_eq!(err["error_type"], "engine_blew_up");
    assert_eq!(err["category"], "execution_error");
    assert_eq!(err["recoverable"], false);
}

#[test]
fn slurm_killed_job_maps_to_recoverable_resource_error() {
    let _lock = SCENARIO_LOCK.lock().unwrap();
    let fx = fixture("timeout", "ok", &["# FAKE_MODE=timeout"]);
    let _path = PathGuard::prepend(&fx.root.join("bin"));

    let rec = run_api::run_workflow(&fx.project, "fake_flow", run_opts()).unwrap();
    assert_eq!(rec.status, RunStatus::Failed);

    let wd = node_workdir(&fx.project, rec.id.as_str());
    assert!(wd.join("slurm_job_id").is_file());
    assert!(!wd.join(".m3flow_exit").exists());
    let db = run_api::open_db(&fx.project).unwrap();
    let runs = db.task_runs_of(rec.id.as_str()).unwrap();
    assert_eq!(runs[0].status, TaskStatus::Failed);
    let err = runs[0].error.as_ref().unwrap();
    assert_eq!(err["error_type"], "slurm_timeout");
    assert_eq!(err["category"], "resource_error");
    assert_eq!(err["recoverable"], true);
}

#[test]
fn slurm_run_cancel_scancels_the_job() {
    let _lock = SCENARIO_LOCK.lock().unwrap();
    let fx = fixture("cancel", "slow", &[]);
    let _path = PathGuard::prepend(&fx.root.join("bin"));

    let project = fx.project.clone();
    let handle = std::thread::spawn(move || {
        run_api::run_workflow(&project, "fake_flow", run_opts()).unwrap()
    });

    // wait for dispatch: the run dir + slurm job id appear
    let mut run_id = None;
    for _ in 0..100 {
        if let Ok(entries) = std::fs::read_dir(fx.project.runs_dir()) {
            run_id = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .find(|n| n != "CANCEL");
            if let Some(id) = &run_id {
                if node_workdir(&fx.project, id).join("slurm_job_id").is_file() {
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let run_id = run_id.expect("run dir never appeared");
    run_api::cancel_run(&fx.project, &run_id).unwrap();

    let rec = handle.join().unwrap();
    assert_eq!(rec.status, RunStatus::Cancelled);
    // scancel was called with the dispatched job id
    let cancelled = std::fs::read_to_string(fx.state.join("scancelled")).unwrap();
    let job_id = std::fs::read_to_string(node_workdir(&fx.project, &run_id).join("slurm_job_id"))
        .unwrap()
        .trim()
        .to_string();
    assert!(cancelled.lines().any(|l| l.trim() == job_id));
}
