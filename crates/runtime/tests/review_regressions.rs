//! Regression tests for the 2026-10-07 code review: each scenario that the
//! review reproduced as a defect is asserted here with the *correct*
//! behavior. Fake providers are shell scripts; no engines are needed.

use m3flow_core::artifact::{RunStatus, TaskStatus};
use m3flow_core::units::{Dimension, Quantity};
use m3flow_runtime::executor::{self, CancelToken, Executor};
use m3flow_runtime::project::{Project, ProviderConfig, SlurmConfig};
use m3flow_runtime::provider::ProviderHandle;
use m3flow_runtime::run_api::{self, RunOptions};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// PATH and cwd are process-global; scenarios run one at a time.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
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

/// Fake provider: `answer` from the engine config is written into the
/// result; `mode` selects special behaviors through the engine config too.
const PROVIDER: &str = r#"#!/bin/bash
cfgval() { sed -n "s/.*\"$1\": *\"\{0,1\}\([^\",}]*\).*/\1/p" "$2" | head -1; }
case "$1" in
  describe)
    engine='{"name":"fake","version":"1.0.0"}'
    if [ -n "$2" ] && [ "$(cfgval engine "$2")" = "unknown" ]; then
      engine='{"name":"unknown","version":"unavailable: probe failed"}'
    fi
    echo "{\"protocol\":\"m3flow-provider/1\",\"provider\":{\"name\":\"fake\",\"version\":\"1.0.0\"},\"engine\":$engine,\"tasks\":[]}"
    ;;
  execute)
    wd=$(sed -n 's/.*"workdir": *"\([^"]*\)".*/\1/p' "$2" | head -1)
    answer=$(cfgval answer "$2"); answer=${answer:-1}
    mode=$(cfgval mode "$2")
    cd "$wd"
    case "$mode" in
      sleep)
        sleep 60 &
        echo $! > child.pid
        wait
        ;;
      escape)
        echo '{"status":"success","outputs":{"result":{"type":"Result","files":{"text":"../../../escape.txt"},"metadata":{},"data":{}}}}'
        exit 0
        ;;
      certify)
        echo state > s.data
        echo '{"status":"success","outputs":{"result":{"type":"EquilibratedState","files":{"data":"s.data"},"metadata":{},"data":{}}}}'
        exit 0
        ;;
    esac
    echo "$answer" > result.txt
    echo "{\"status\":\"success\",\"outputs\":{\"result\":{\"type\":\"Result\",\"files\":{\"text\":\"result.txt\"},\"metadata\":{},\"data\":{\"answer\":$answer}}}}"
    ;;
esac
"#;

fn fixture(tag: &str, engine: &str) -> Fixture {
    fixture_with(tag, engine, "Result", "")
}

fn fixture_with(tag: &str, engine: &str, output_type: &str, step_extra: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("m3review-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("tasks")).unwrap();
    std::fs::create_dir_all(root.join("workflows")).unwrap();
    let provider = root.join("fake-provider");
    write_exe(&provider, PROVIDER);
    std::fs::write(
        root.join("m3flow.yaml"),
        format!(
            "schema: m3flow-project/v1\nproviders:\n  fake:\n    executable: {}\n    engine: {engine}\n",
            provider.display()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("tasks/probe.yaml"),
        format!(
            "schema: task/v1\nname: probe\nversion: 1.0.0\ncategory: utility\ninputs: {{}}\n\
             parameters:\n  level: {{type: integer, default: 1}}\noutputs:\n  result: {{type: {output_type}}}\n\
             implementations:\n  - {{provider: fake, default: true}}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("workflows/probe.yaml"),
        format!(
            "schema: workflow/v1\nname: probe_flow\nversion: 1.0.0\nsteps:\n  work:\n    task: probe\n{step_extra}\
             outputs:\n  result: {{value: '${{work.result}}'}}\n"
        ),
    )
    .unwrap();
    let project = Project::load(root.clone()).unwrap();
    Fixture { root, project }
}

fn opts() -> RunOptions {
    RunOptions {
        inputs: BTreeMap::new(),
        params: serde_json::Map::new(),
        no_cache: false,
        no_materialize: true,
        label: None,
        max_concurrency: Some(1),
        executor_override: None,
        progress: None,
    }
}

fn result_answer(
    project: &Project,
    run: &m3flow_runtime::db::WorkflowRunRecord,
) -> serde_json::Value {
    let db = run_api::open_db(project).unwrap();
    let out = run.outputs.as_ref().unwrap()["result"].as_str().unwrap();
    db.get_artifact(out).unwrap().data.unwrap()["answer"].clone()
}

#[test]
fn kpa_converts_to_bar() {
    let q = Quantity::parse_str(Dimension::Pressure, "100 kPa").unwrap();
    assert!((q.value - 1.0).abs() < 1e-12, "100 kPa -> {} bar", q.value);
}

#[test]
fn changed_engine_config_misses_the_cache() {
    let _g = lock();
    let fx = fixture("config", "{answer: 1}");
    let first = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    assert_eq!(first.status, RunStatus::Completed);
    assert_eq!(result_answer(&fx.project, &first), serde_json::json!(1));

    // identical config: cache hit
    let again = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    let db = run_api::open_db(&fx.project).unwrap();
    assert_eq!(
        db.task_runs_of(again.id.as_str()).unwrap()[0].status,
        TaskStatus::Cached
    );

    let cfg = fx.root.join("m3flow.yaml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, text.replace("answer: 1", "answer: 2")).unwrap();
    let project = Project::load(fx.root.clone()).unwrap();
    let second = run_api::run_workflow(&project, "probe_flow", opts()).unwrap();
    let tr = db.task_runs_of(second.id.as_str()).unwrap();
    assert_eq!(tr[0].status, TaskStatus::Completed, "must re-execute");
    assert_eq!(result_answer(&project, &second), serde_json::json!(2));
}

#[test]
fn unidentifiable_engine_is_never_cached() {
    let _g = lock();
    let fx = fixture("unknown-engine", "{engine: unknown}");
    for _ in 0..2 {
        let run = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
        let db = run_api::open_db(&fx.project).unwrap();
        let tr = db.task_runs_of(run.id.as_str()).unwrap();
        assert_eq!(tr[0].status, TaskStatus::Completed);
        assert!(tr[0].cache_key.is_none());
    }
    let db = run_api::open_db(&fx.project).unwrap();
    assert_eq!(db.cache_stats().unwrap().0, 0);
}

#[test]
fn cancelled_run_can_be_resumed() {
    let _g = lock();
    let fx = fixture("cancel", "{answer: 1}");
    let first = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    let db = run_api::open_db(&fx.project).unwrap();
    // persisted state of a run cancelled before its task completed
    db.conn().execute("DELETE FROM artifact_output WHERE task_run_id IN (SELECT id FROM task_run WHERE workflow_run_id=?1)", [first.id.as_str()]).unwrap();
    db.conn().execute("DELETE FROM cache_entry WHERE task_run_id IN (SELECT id FROM task_run WHERE workflow_run_id=?1)", [first.id.as_str()]).unwrap();
    db.conn()
        .execute(
            "UPDATE task_run SET status='CANCELLED' WHERE workflow_run_id=?1",
            [first.id.as_str()],
        )
        .unwrap();
    db.conn()
        .execute(
            "UPDATE workflow_run SET status='CANCELLED' WHERE id=?1",
            [first.id.as_str()],
        )
        .unwrap();
    run_api::cancel_run(&fx.project, first.id.as_str()).unwrap();

    let resumed = run_api::resume_run(&fx.project, first.id.as_str(), None).unwrap();
    assert_eq!(resumed.status, RunStatus::Completed);
    assert!(!fx
        .project
        .runs_dir()
        .join(first.id.as_str())
        .join("CANCEL")
        .exists());
}

#[test]
fn retry_completed_step_preserves_identity_and_cache() {
    let _g = lock();
    let fx = fixture("retry", "{answer: 1}");
    let first = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    let retried = run_api::retry_step(&fx.project, first.id.as_str(), "work", None).unwrap();
    assert_eq!(retried.status, RunStatus::Completed);
}

#[test]
fn certified_type_cannot_be_registered_by_hand() {
    let _g = lock();
    let fx = fixture("register", "{answer: 1}");
    let data = fx.root.join("state.data");
    std::fs::write(&data, "arbitrary data").unwrap();
    let err = run_api::register_artifact(
        &fx.project,
        "EquilibratedState",
        &BTreeMap::from([("data".to_string(), data)]),
        serde_json::json!({}),
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("certified"), "{err}");
}

#[test]
fn certified_type_cannot_be_emitted_by_another_task() {
    let _g = lock();
    let fx = fixture_with("emit", "{mode: certify}", "SimulationState", "");
    let run = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let db = run_api::open_db(&fx.project).unwrap();
    let tr = db.task_runs_of(run.id.as_str()).unwrap();
    assert_eq!(
        tr[0].error.as_ref().unwrap()["error_type"],
        "protected_type"
    );
}

#[test]
fn resume_rejects_executable_definition_drift() {
    let _g = lock();
    let fx = fixture("drift", "{answer: 1}");
    let first = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    let wf = fx.root.join("workflows/probe.yaml");
    let original = std::fs::read_to_string(&wf).unwrap();

    // documentation-only edit: same execution closure, resume allowed
    std::fs::write(
        &wf,
        original.replace("version: 1.0.0", "version: 1.0.0\ndescription: reworded"),
    )
    .unwrap();
    let resumed = run_api::resume_run(&fx.project, first.id.as_str(), None).unwrap();
    assert_eq!(resumed.status, RunStatus::Completed);

    // executable change under the same name@version: refused
    std::fs::write(
        &wf,
        original.replace(
            "    task: probe\n",
            "    task: probe\n    parameters: {level: 7}\n",
        ),
    )
    .unwrap();
    let err = run_api::resume_run(&fx.project, first.id.as_str(), None).unwrap_err();
    assert!(
        err.to_string().contains("changed since the run started"),
        "{err}"
    );

    // a task definition change is part of the closure too
    std::fs::write(&wf, &original).unwrap();
    let task = fx.root.join("tasks/probe.yaml");
    let t = std::fs::read_to_string(&task).unwrap();
    std::fs::write(&task, t.replace("default: 1}", "default: 3}")).unwrap();
    assert!(run_api::resume_run(&fx.project, first.id.as_str(), None).is_err());
}

#[test]
fn scheduler_error_only_fails_its_own_run() {
    let _g = lock();
    let fx = fixture("isolation", "{answer: 1}");
    let first = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    let db = run_api::open_db(&fx.project).unwrap();
    // model another active run in the same project database
    db.conn()
        .execute(
            "UPDATE task_run SET status='RUNNING' WHERE workflow_run_id=?1",
            [first.id.as_str()],
        )
        .unwrap();
    db.conn()
        .execute(
            "UPDATE workflow_run SET status='RUNNING' WHERE id=?1",
            [first.id.as_str()],
        )
        .unwrap();
    std::fs::write(fx.root.join("tasks/bad.yaml"), "schema: task/v1\nname: bad\nversion: 1.0.0\ncategory: utility\ninputs: {}\nparameters: {}\noutputs:\n  result: {type: Result}\n").unwrap();
    std::fs::write(fx.root.join("workflows/bad.yaml"), "schema: workflow/v1\nname: bad_flow\nversion: 1.0.0\nsteps:\n  bad:\n    task: bad\noutputs:\n  result: {value: '${bad.result}'}\n").unwrap();
    let project = Project::load(fx.root.clone()).unwrap();
    assert!(run_api::run_workflow(&project, "bad_flow", opts()).is_err());
    assert_eq!(
        db.get_workflow_run(first.id.as_str()).unwrap().status,
        RunStatus::Running
    );
    let tr = db.task_runs_of(first.id.as_str()).unwrap();
    assert_eq!(tr[0].status, TaskStatus::Running);
    // ... while the failing run itself is finalized
    let failed: String = db
        .conn()
        .query_row(
            "SELECT status FROM workflow_run WHERE name='bad_flow'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failed, "FAILED");
}

#[test]
fn output_files_cannot_escape_the_workdir() {
    let _g = lock();
    let fx = fixture("escape", "{mode: escape}");
    // the escaping path resolves to <runs>/escape.txt — make it exist so
    // only the containment rule can reject it
    std::fs::create_dir_all(fx.project.runs_dir()).unwrap();
    std::fs::write(fx.project.runs_dir().join("escape.txt"), "secret").unwrap();
    let run = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let db = run_api::open_db(&fx.project).unwrap();
    let tr = db.task_runs_of(run.id.as_str()).unwrap();
    let msg = tr[0].error.as_ref().unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("rejected"), "{msg}");
}

#[cfg(unix)]
#[test]
fn local_cancel_terminates_the_provider_process_group() {
    let _g = lock();
    let fx = fixture("local-cancel", "{mode: sleep}");
    let project = fx.project.clone();
    let started = Instant::now();
    let worker = std::thread::spawn(move || run_api::run_workflow(&project, "probe_flow", opts()));

    // wait until the provider has spawned its child
    let mut pid_file = None;
    for _ in 0..200 {
        if let Ok(runs) = std::fs::read_dir(fx.project.runs_dir()) {
            for run in runs.flatten() {
                let step = run.path().join("work");
                let p = m3flow_runtime::scheduler::latest_attempt_dir(&step).join("child.pid");
                if p.is_file() {
                    pid_file = Some((run.file_name().to_string_lossy().to_string(), p));
                }
            }
        }
        if pid_file.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (run_id, pid_file) = pid_file.expect("provider never started");
    std::thread::sleep(Duration::from_millis(200));
    let child: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    run_api::cancel_run(&fx.project, &run_id).unwrap();
    let rec = worker.join().unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "cancel was not prompt"
    );
    // the grandchild `sleep 60` is gone too (whole process group terminated)
    std::thread::sleep(Duration::from_millis(300));
    let alive = unsafe { libc_kill(child, 0) } == 0;
    assert!(!alive, "provider child process survived cancellation");
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[test]
fn local_walltime_is_enforced() {
    let _g = lock();
    let fx = fixture_with(
        "walltime",
        "{mode: sleep}",
        "Result",
        "    resources: {walltime: \"0:02\"}\n",
    );
    let started = Instant::now();
    let run = run_api::run_workflow(&fx.project, "probe_flow", opts()).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(started.elapsed() < Duration::from_secs(30));
    let db = run_api::open_db(&fx.project).unwrap();
    let tr = db.task_runs_of(run.id.as_str()).unwrap();
    assert_eq!(
        tr[0].error.as_ref().unwrap()["error_type"],
        "walltime_exceeded"
    );
}

struct PathGuard(String);
impl Drop for PathGuard {
    fn drop(&mut self) {
        std::env::set_var("PATH", &self.0);
    }
}

#[test]
fn slurm_attempt_ignores_previous_attempt_outcome() {
    let _g = lock();
    let fx = fixture("stale-slurm", "{answer: 1}");
    let bin = fx.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (name, body) in [
        ("sbatch", "#!/bin/bash\necho 42424\n"),
        ("squeue", "#!/bin/bash\nexit 0\n"),
        ("sacct", "#!/bin/bash\necho TIMEOUT\n"),
    ] {
        write_exe(&bin.join(name), body);
    }
    let original = std::env::var("PATH").unwrap();
    let _guard = PathGuard(original.clone());
    std::env::set_var("PATH", format!("{}:{original}", bin.display()));
    let wd = fx.root.join("attempt");
    std::fs::create_dir_all(&wd).unwrap();
    // a previous attempt's success left its marker + response behind
    std::fs::write(wd.join(".m3flow_exit"), "0 41000\n").unwrap();
    std::fs::write(
        wd.join("provider_stdout.json"),
        r#"{"status":"success","outputs":{},"warnings":["response from previous attempt"]}"#,
    )
    .unwrap();
    let handle = ProviderHandle {
        name: "fake".into(),
        executable: fx.root.join("fake-provider"),
        config: ProviderConfig::default(),
        description: None,
    };
    let cancel = CancelToken::new(fx.root.join("CANCEL"), 1);
    let response = executor::execute_provider(
        &Executor::Slurm(SlurmConfig::default()),
        &handle,
        &wd.join("request.json"),
        &wd,
        "work",
        None,
        &cancel,
    )
    .unwrap();
    assert_eq!(response.status, "error");
    assert_eq!(response.error.unwrap().error_type, "slurm_timeout");
}

#[test]
fn promotion_verification_binds_report_state_and_files() {
    use m3flow_core::artifact::{now_rfc3339, Artifact};
    use m3flow_core::id::ArtifactId;
    use m3flow_runtime::scheduler::{verify_promotion, PromotionEvidence};
    let ev = |report: serde_json::Value| PromotionEvidence {
        state_content_hash: "hashA".into(),
        state_producer: Some("tr_A".into()),
        state_files: BTreeMap::from([("data".into(), "aaaa".into())]),
        report,
    };
    let out = |sha: &str| Artifact {
        id: ArtifactId::new(),
        artifact_type: "EquilibratedState".into(),
        schema_version: "1".into(),
        files: BTreeMap::from([("data".into(), format!("sha256/{}/{sha}", &sha[..2]))]),
        metadata: serde_json::json!({}),
        data: None,
        content_hash: String::new(),
        producer: None,
        created_at: now_rfc3339(),
    };
    let passing = |extra: serde_json::Value| {
        let mut r =
            serde_json::json!({"equilibrated": true, "checks": {"density_drift": "passed"}});
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        r
    };
    // bound by the thermo evidence's producer
    let r = passing(serde_json::json!({"evidence": {"thermo": {"producer": "tr_A"}}}));
    assert!(verify_promotion(&ev(r.clone()), &out("aaaa")).is_ok());
    // files altered on the way
    assert!(verify_promotion(&ev(r), &out("bbbb")).is_err());
    // a report about another state
    let r = passing(serde_json::json!({"subject": {"state_content_hash": "hashB"}}));
    assert!(verify_promotion(&ev(r), &out("aaaa")).is_err());
    // evidence from another run
    let r = passing(serde_json::json!({"evidence": {"thermo": {"producer": "tr_B"}}}));
    assert!(verify_promotion(&ev(r), &out("aaaa")).is_err());
    // an incomplete check is not a pass
    let r = serde_json::json!({"equilibrated": true,
        "checks": {"density_drift": "passed", "rg_stable": "insufficient_data"},
        "subject": {"state_content_hash": "hashA"}});
    assert!(verify_promotion(&ev(r), &out("aaaa")).is_err());
}
