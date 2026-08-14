//! M3Flow execution cockpit (plan §46): live run monitoring + control.
//!
//! Layout: run header, node list (status-colored), detail pane.
//! Keys: j/k navigate · l logs · a artifacts · p provenance · g graph ·
//!       r retry node · R resume run · q quit. Auto-refresh while RUNNING.

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use m3flow_core::artifact::{RunStatus, TaskStatus};
use m3flow_core::error::Result;
use m3flow_runtime::project::Project;
use m3flow_runtime::run_api;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Detail,
    Logs,
    Artifacts,
    Provenance,
    Graph,
}

pub struct App {
    project: Project,
    run_id: String,
    pane: Pane,
    selected: usize,
    scroll: u16,
    last_refresh: Instant,
    action_rx: Option<Receiver<String>>,
    notice: Option<String>,
}

impl App {
    #[doc(hidden)]
    pub fn for_test(project: Project, run_id: &str) -> Self {
        Self {
            project,
            run_id: run_id.into(),
            pane: Pane::Detail,
            selected: 0,
            scroll: 0,
            last_refresh: Instant::now(),
            action_rx: None,
            notice: None,
        }
    }
}

pub fn run(project: &Project, run_id: Option<&str>) -> Result<()> {
    let db = run_api::open_db(project)?;
    let run_id = match run_id {
        Some(id) => id.to_string(),
        None => db
            .list_workflow_runs(1)?
            .first()
            .map(|r| r.id.as_str().to_string())
            .ok_or_else(|| m3flow_core::error::M3FlowError::not_found("no runs yet"))?,
    };
    drop(db);

    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut app = App {
        project: project.clone(),
        run_id,
        pane: Pane::Detail,
        selected: 0,
        scroll: 0,
        last_refresh: Instant::now() - Duration::from_secs(10),
        action_rx: None,
        notice: None,
    };
    let res = event_loop(&mut terminal, &mut app);
    disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
    res
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut tasks_cache = Vec::new();
    let mut run_rec = None;
    loop {
        // refresh every 2s (or immediately on state-changing keys)
        if app.last_refresh.elapsed() > Duration::from_secs(2) {
            if let Ok(db) = run_api::open_db(&app.project) {
                run_rec = db.get_workflow_run(&app.run_id).ok();
                tasks_cache = db.task_runs_of(&app.run_id).unwrap_or_default();
            }
            app.last_refresh = Instant::now();
        }
        if let Some(rx) = &app.action_rx {
            if let Ok(msg) = rx.try_recv() {
                app.notice = Some(msg);
                app.action_rx = None;
                app.last_refresh = Instant::now() - Duration::from_secs(10);
            }
        }
        terminal.draw(|f| draw(f, app, &run_rec, &tasks_cache))?;

        if event::poll(Duration::from_millis(300))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('j') | KeyCode::Down => {
                        if app.selected + 1 < tasks_cache.len() {
                            app.selected += 1;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.selected = app.selected.saturating_sub(1);
                    }
                    KeyCode::Char('l') => app.pane = Pane::Logs,
                    KeyCode::Char('a') => app.pane = Pane::Artifacts,
                    KeyCode::Char('p') => app.pane = Pane::Provenance,
                    KeyCode::Char('g') => app.pane = Pane::Graph,
                    KeyCode::Char('d') => app.pane = Pane::Detail,
                    KeyCode::Char('r') => {
                        if let Some(t) = tasks_cache.get(app.selected) {
                            spawn_action(app, format!("retry {}", t.node_id));
                        }
                    }
                    KeyCode::Char('R') => spawn_action(app, "resume".into()),
                    _ => {}
                }
            }
        }
        // keep node selection in range
        if app.selected >= tasks_cache.len() && !tasks_cache.is_empty() {
            app.selected = tasks_cache.len() - 1;
        }
    }
}

fn spawn_action(app: &mut App, action: String) {
    if app.action_rx.is_some() {
        app.notice = Some("an action is already running".into());
        return;
    }
    let (tx, rx) = channel();
    let project = app.project.clone();
    let run_id = app.run_id.clone();
    app.notice = Some(format!("{action} started..."));
    std::thread::spawn(move || {
        let msg = if let Some(step) = action.strip_prefix("retry ") {
            match run_api::retry_step(&project, &run_id, step, None) {
                Ok(r) => format!("retry done: {}", r.status.as_str()),
                Err(e) => format!("retry failed: {e}"),
            }
        } else {
            match run_api::resume_run(&project, &run_id, None) {
                Ok(r) => format!("resume done: {}", r.status.as_str()),
                Err(e) => format!("resume failed: {e}"),
            }
        };
        let _ = tx.send(msg);
    });
    app.action_rx = Some(rx);
}

#[doc(hidden)]
pub fn draw(
    f: &mut Frame,
    app: &App,
    run_rec: &Option<m3flow_runtime::db::WorkflowRunRecord>,
    tasks: &[m3flow_runtime::db::TaskRunRecord],
) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(2),
    ])
    .split(f.area());
    let body = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    // header
    let (title, status_color) = match run_rec {
        Some(r) => (
            format!(
                " {}  {}@{}  [{}]",
                r.id,
                r.name,
                r.version,
                r.status.as_str()
            ),
            match r.status {
                RunStatus::Completed => Color::Green,
                RunStatus::Failed => Color::Red,
                RunStatus::Running => Color::Cyan,
                _ => Color::Yellow,
            },
        ),
        None => (format!(" {}  (not found)", app.run_id), Color::Red),
    };
    f.render_widget(
        Paragraph::new(title)
            .block(Block::default().borders(Borders::ALL))
            .style(Style::default().fg(status_color)),
        chunks[0],
    );

    // node table
    let rows: Vec<Row> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let style = status_style(t.status);
            let marker = if i == app.selected { ">" } else { " " };
            Row::new(vec![
                Cell::from(marker),
                Cell::from(t.node_id.clone()),
                Cell::from(t.status.as_str()).style(style),
                Cell::from(format!("{}", t.attempts)),
                Cell::from(format!("{}@{}", t.task_name, t.task_version)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(34),
            Constraint::Length(11),
            Constraint::Length(4),
            Constraint::Percentage(30),
        ],
    )
    .block(Block::default().title("steps").borders(Borders::ALL));
    f.render_widget(table, body[0]);

    // detail pane
    let detail = detail_text(app, tasks.get(app.selected));
    f.render_widget(
        Paragraph::new(detail)
            .block(
                Block::default()
                    .title(match app.pane {
                        Pane::Detail => "detail [d]",
                        Pane::Logs => "logs [l]",
                        Pane::Artifacts => "artifacts [a]",
                        Pane::Provenance => "provenance [p]",
                        Pane::Graph => "graph [g]",
                    })
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0)),
        body[1],
    );

    // footer
    let foot = match (&app.action_rx, &app.notice) {
        (Some(_), Some(n)) => format!(" {n} (running...)"),
        (_, Some(n)) => format!(" {n}"),
        _ => " [l]ogs [a]rtifacts [p]rovenance [g]raph [r]etry [R]esume [q]uit".to_string(),
    };
    f.render_widget(Paragraph::new(foot), chunks[2]);
}

fn status_style(s: TaskStatus) -> Style {
    let c = match s {
        TaskStatus::Completed => Color::Green,
        TaskStatus::Cached => Color::Blue,
        TaskStatus::Running => Color::Cyan,
        TaskStatus::Failed => Color::Red,
        TaskStatus::Skipped => Color::DarkGray,
        TaskStatus::Cancelled => Color::Yellow,
        _ => Color::White,
    };
    Style::default().fg(c)
}

fn detail_text(app: &App, task: Option<&m3flow_runtime::db::TaskRunRecord>) -> String {
    let Some(t) = task else {
        return "no step selected".into();
    };
    match app.pane {
        Pane::Detail => {
            let mut s = format!(
                "step: {}\ntask: {}@{}\nstatus: {}   attempts: {}\ncreated: {}\nstarted: {}\nended: {}\n",
                t.node_id, t.task_name, t.task_version, t.status.as_str(),
                t.attempts, t.created_at,
                t.started_at.as_deref().unwrap_or("-"),
                t.ended_at.as_deref().unwrap_or("-"),
            );
            s.push_str(&format!("\nparams:\n{}\n", pretty(&t.params)));
            if let Some(v) = &t.validation {
                s.push_str("\nvalidation:\n");
                for v in v {
                    s.push_str(&format!(
                        "  [{}] {}  {}\n",
                        if v.passed { "ok" } else { "XX" },
                        v.name,
                        v.detail.as_deref().unwrap_or("")
                    ));
                }
            }
            if let Some(e) = &t.error {
                s.push_str(&format!("\nerror:\n{}\n", pretty(e)));
            }
            s
        }
        Pane::Logs => {
            let dir = app.project.runs_dir().join(&app.run_id).join(&t.node_id);
            let mut s = String::new();
            if let Ok(resp) = std::fs::read_to_string(dir.join("response.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp) {
                    s.push_str(&format!("response:\n{}\n\n", pretty(&v)));
                }
            }
            for name in ["stdout.log", "log.lammps"] {
                let p = dir.join(name);
                if let Ok(text) = std::fs::read_to_string(&p) {
                    let tail: Vec<&str> = text.lines().rev().take(40).collect();
                    s.push_str(&format!("--- {name} (tail) ---\n"));
                    for ln in tail.into_iter().rev() {
                        s.push_str(ln);
                        s.push('\n');
                    }
                }
            }
            if s.is_empty() {
                s = format!("no logs in {}", dir.display());
            }
            s
        }
        Pane::Artifacts => {
            let db = run_api::open_db(&app.project);
            match db {
                Ok(db) => {
                    let mut s = String::from("outputs:\n");
                    for (name, aid) in db.outputs_of(t.id.as_str()).unwrap_or_default() {
                        s.push_str(&format!("  {name}: {aid}\n"));
                    }
                    s.push_str("inputs:\n");
                    for (name, aid) in db.inputs_of(t.id.as_str()).unwrap_or_default() {
                        s.push_str(&format!("  {name}: {aid}\n"));
                    }
                    s
                }
                Err(e) => format!("db: {e}"),
            }
        }
        Pane::Provenance => match run_api::lineage_json_for_task(&app.project, t.id.as_str()) {
            Ok(v) => pretty(&v),
            Err(e) => format!("provenance: {e}"),
        },
        Pane::Graph => match run_api::run_graph_json(&app.project, &app.run_id) {
            Ok(v) => {
                let mut s = String::new();
                if let Some(edges) = v["edges"].as_array() {
                    for e in edges {
                        let from = e["from"].as_str().unwrap_or("");
                        let to = e["to"].as_str().unwrap_or("");
                        let mark = if to == t.node_id || from == t.node_id {
                            " *"
                        } else {
                            ""
                        };
                        s.push_str(&format!("{} -> {}{}\n", from, to, mark));
                    }
                }
                s
            }
            Err(e) => format!("graph: {e}"),
        },
    }
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}
