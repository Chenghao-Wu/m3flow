//! Headless render smoke test for the cockpit (ratatui TestBackend).

use m3flow_core::artifact::{RunStatus, TaskStatus};
use m3flow_core::id::{TaskRunId, WorkflowRunId};
use m3flow_runtime::db::{TaskRunRecord, WorkflowRunRecord};

#[test]
fn renders_without_panic() {
    let dir = std::env::temp_dir().join(format!("m3tui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("m3flow.yaml"), "schema: m3flow-project/v1\n").unwrap();
    let project = m3flow_runtime::project::Project::load(dir.clone()).unwrap();

    let wr = WorkflowRunRecord {
        id: WorkflowRunId::new(),
        name: "demo".into(),
        version: "1.0.0".into(),
        spec_hash: "x".into(),
        status: RunStatus::Running,
        created_at: "now".into(),
        started_at: None,
        ended_at: None,
        workdir: dir.display().to_string(),
        git: None,
        inputs: serde_json::json!({}),
        params: serde_json::json!({}),
        outputs: None,
        error: None,
    };
    let tr = TaskRunRecord {
        id: TaskRunId::new(),
        workflow_run_id: wr.id.clone(),
        node_id: "nvt".into(),
        task_name: "run_nvt".into(),
        task_version: "1.0.0".into(),
        provider: Some("lammps".into()),
        status: TaskStatus::Running,
        cache_key: None,
        attempts: 1,
        created_at: "now".into(),
        started_at: None,
        ended_at: None,
        params: serde_json::json!({"temperature": {"value": 300, "unit": "K"}}),
        error: None,
        validation: None,
        engine: None,
    };

    let app = m3flow_tui::App::for_test(project, wr.id.as_str());
    let backend = ratatui::backend::TestBackend::new(120, 32);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| m3flow_tui::draw(f, &app, &Some(wr.clone()), &[tr.clone()]))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("demo@1.0.0"));
    assert!(text.contains("run_nvt@1.0.0"));
    assert!(text.contains("RUNNING"));
    std::fs::remove_dir_all(&dir).ok();
}
