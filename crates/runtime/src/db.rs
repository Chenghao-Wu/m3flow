//! SQLite provenance store (plan §55).
//!
//! Single-file database at `.m3flow/m3flow.db`. All writes go through this
//! module; the scheduler owns the only connection during a run.

use m3flow_core::artifact::{Artifact, RunStatus, TaskStatus, ValidationVerdict};
use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::id::{ArtifactId, TaskRunId, WorkflowRunId};
use rusqlite::{params, Connection};
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS workflow_run (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    spec_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    started_at TEXT,
    ended_at TEXT,
    workdir TEXT NOT NULL,
    git_json TEXT,
    inputs_json TEXT NOT NULL,
    params_json TEXT NOT NULL,
    outputs_json TEXT,
    error_json TEXT
);
CREATE TABLE IF NOT EXISTS task_run (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES workflow_run(id),
    node_id TEXT NOT NULL,
    task_name TEXT NOT NULL,
    task_version TEXT NOT NULL,
    provider TEXT,
    status TEXT NOT NULL,
    cache_key TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    ended_at TEXT,
    params_json TEXT NOT NULL,
    error_json TEXT,
    validation_json TEXT,
    engine_json TEXT,
    UNIQUE(workflow_run_id, node_id)
);
CREATE TABLE IF NOT EXISTS artifact (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    producer TEXT,
    created_at TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    data_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_artifact_hash ON artifact(content_hash);
CREATE TABLE IF NOT EXISTS artifact_file (
    artifact_id TEXT NOT NULL REFERENCES artifact(id),
    name TEXT NOT NULL,
    relpath TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (artifact_id, name)
);
CREATE TABLE IF NOT EXISTS artifact_input (
    task_run_id TEXT NOT NULL REFERENCES task_run(id),
    input_name TEXT NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES artifact(id),
    PRIMARY KEY (task_run_id, input_name, artifact_id)
);
CREATE TABLE IF NOT EXISTS artifact_output (
    task_run_id TEXT NOT NULL REFERENCES task_run(id),
    output_name TEXT NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES artifact(id),
    PRIMARY KEY (task_run_id, output_name)
);
CREATE TABLE IF NOT EXISTS cache_entry (
    cache_key TEXT PRIMARY KEY,
    task_run_id TEXT NOT NULL REFERENCES task_run(id),
    created_at TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Connection,
}

// ------------------------------------------------------------------ records

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowRunRecord {
    pub id: WorkflowRunId,
    pub name: String,
    pub version: String,
    pub spec_hash: String,
    pub status: RunStatus,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub workdir: String,
    pub git: Option<serde_json::Value>,
    pub inputs: serde_json::Value,
    pub params: serde_json::Value,
    pub outputs: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskRunRecord {
    pub id: TaskRunId,
    pub workflow_run_id: WorkflowRunId,
    pub node_id: String,
    pub task_name: String,
    pub task_version: String,
    pub provider: Option<String>,
    pub status: TaskStatus,
    pub cache_key: Option<String>,
    pub attempts: u32,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub params: serde_json::Value,
    pub error: Option<serde_json::Value>,
    pub validation: Option<Vec<ValidationVerdict>>,
    pub engine: Option<serde_json::Value>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| M3FlowError::io(e, "creating state dir"))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| M3FlowError::internal(format!("sqlite open: {e}")))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| M3FlowError::internal(format!("sqlite schema: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| M3FlowError::internal(format!("sqlite pragma: {e}")))?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ------------------------------------------------------------ runs

    pub fn insert_workflow_run(&self, r: &WorkflowRunRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO workflow_run
             (id,name,version,spec_hash,status,created_at,started_at,ended_at,workdir,git_json,inputs_json,params_json,outputs_json,error_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                r.id.as_str(),
                r.name,
                r.version,
                r.spec_hash,
                r.status.as_str(),
                r.created_at,
                r.started_at,
                r.ended_at,
                r.workdir,
                r.git.as_ref().map(|g| g.to_string()),
                r.inputs.to_string(),
                r.params.to_string(),
                r.outputs.as_ref().map(|o| o.to_string()),
                r.error.as_ref().map(|e| e.to_string()),
            ],
        ).map_err(|e| M3FlowError::internal(format!("insert workflow_run: {e}")))?;
        Ok(())
    }

    pub fn update_workflow_run(&self, r: &WorkflowRunRecord) -> Result<()> {
        self.conn.execute(
            "UPDATE workflow_run SET status=?2, started_at=?3, ended_at=?4,
             outputs_json=?5, error_json=?6 WHERE id=?1",
            params![
                r.id.as_str(),
                r.status.as_str(),
                r.started_at,
                r.ended_at,
                r.outputs.as_ref().map(|o| o.to_string()),
                r.error.as_ref().map(|e| e.to_string()),
            ],
        ).map_err(|e| M3FlowError::internal(format!("update workflow_run: {e}")))?;
        Ok(())
    }

    pub fn get_workflow_run(&self, id: &str) -> Result<WorkflowRunRecord> {
        self.conn
            .query_row(
                "SELECT id,name,version,spec_hash,status,created_at,started_at,ended_at,workdir,git_json,inputs_json,params_json,outputs_json,error_json
                 FROM workflow_run WHERE id=?1",
                params![id],
                |row| {
                    Ok(WorkflowRunRecord {
                        id: WorkflowRunId::parse(&row.get::<_, String>(0)?)
                            .unwrap_or_else(WorkflowRunId::new),
                        name: row.get(1)?,
                        version: row.get(2)?,
                        spec_hash: row.get(3)?,
                        status: RunStatus::parse(&row.get::<_, String>(4)?)
                            .unwrap_or(RunStatus::Pending),
                        created_at: row.get(5)?,
                        started_at: row.get(6)?,
                        ended_at: row.get(7)?,
                        workdir: row.get(8)?,
                        git: row
                            .get::<_, Option<String>>(9)?
                            .and_then(|s| serde_json::from_str(&s).ok()),
                        inputs: serde_json::from_str(&row.get::<_, String>(10)?)
                            .unwrap_or(serde_json::Value::Null),
                        params: serde_json::from_str(&row.get::<_, String>(11)?)
                            .unwrap_or(serde_json::Value::Null),
                        outputs: row
                            .get::<_, Option<String>>(12)?
                            .and_then(|s| serde_json::from_str(&s).ok()),
                        error: row
                            .get::<_, Option<String>>(13)?
                            .and_then(|s| serde_json::from_str(&s).ok()),
                    })
                },
            )
            .map_err(|_| M3FlowError::not_found(format!("workflow run '{id}'")))
    }

    pub fn list_workflow_runs(&self, limit: usize) -> Result<Vec<WorkflowRunRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM workflow_run ORDER BY created_at DESC LIMIT ?1")
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let ids: Vec<String> = stmt
            .query_map(params![limit as i64], |row| row.get(0))
            .map_err(|e| M3FlowError::internal(e.to_string()))?
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        ids.iter().map(|id| self.get_workflow_run(id)).collect()
    }

    // ------------------------------------------------------------ task runs

    pub fn upsert_task_run(&self, r: &TaskRunRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_run
             (id,workflow_run_id,node_id,task_name,task_version,provider,status,cache_key,attempts,created_at,started_at,ended_at,params_json,error_json,validation_json,engine_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(workflow_run_id,node_id) DO UPDATE SET
               id=excluded.id, provider=excluded.provider, status=excluded.status,
               cache_key=excluded.cache_key, attempts=excluded.attempts,
               started_at=excluded.started_at, ended_at=excluded.ended_at,
               params_json=excluded.params_json, error_json=excluded.error_json,
               validation_json=excluded.validation_json, engine_json=excluded.engine_json",
            params![
                r.id.as_str(),
                r.workflow_run_id.as_str(),
                r.node_id,
                r.task_name,
                r.task_version,
                r.provider,
                r.status.as_str(),
                r.cache_key,
                r.attempts as i64,
                r.created_at,
                r.started_at,
                r.ended_at,
                r.params.to_string(),
                r.error.as_ref().map(|e| e.to_string()),
                r.validation.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default()),
                r.engine.as_ref().map(|e| e.to_string()),
            ],
        ).map_err(|e| M3FlowError::internal(format!("upsert task_run: {e}")))?;
        Ok(())
    }

    pub fn get_task_run(&self, run_id: &str, node_id: &str) -> Result<TaskRunRecord> {
        self.conn.query_row(
            "SELECT id,workflow_run_id,node_id,task_name,task_version,provider,status,cache_key,attempts,created_at,started_at,ended_at,params_json,error_json,validation_json,engine_json
             FROM task_run WHERE workflow_run_id=?1 AND node_id=?2",
            params![run_id, node_id],
            task_run_from_row,
        ).map_err(|_| M3FlowError::not_found(format!("task run '{node_id}' in run '{run_id}'")))
    }

    pub fn task_runs_of(&self, run_id: &str) -> Result<Vec<TaskRunRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,workflow_run_id,node_id,task_name,task_version,provider,status,cache_key,attempts,created_at,started_at,ended_at,params_json,error_json,validation_json,engine_json
             FROM task_run WHERE workflow_run_id=?1 ORDER BY created_at",
        ).map_err(|e| M3FlowError::internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![run_id], task_run_from_row)
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| M3FlowError::internal(format!("reading task runs: {e}")))
    }

    pub fn task_run_by_id(&self, id: &str) -> Result<TaskRunRecord> {
        self.conn.query_row(
            "SELECT id,workflow_run_id,node_id,task_name,task_version,provider,status,cache_key,attempts,created_at,started_at,ended_at,params_json,error_json,validation_json,engine_json
             FROM task_run WHERE id=?1",
            params![id],
            task_run_from_row,
        ).map_err(|_| M3FlowError::not_found(format!("task run '{id}'")))
    }

    // ------------------------------------------------------------ artifacts

    pub fn insert_artifact(&self, a: &Artifact, file_rows: &[(String, String, String, u64)]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO artifact (id,type,schema_version,content_hash,producer,created_at,metadata_json,data_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                a.id.as_str(),
                a.artifact_type,
                a.schema_version,
                a.content_hash,
                a.producer.as_ref().map(|p| p.as_str()),
                a.created_at,
                a.metadata.to_string(),
                a.data.as_ref().map(|d| d.to_string()),
            ],
        ).map_err(|e| M3FlowError::internal(format!("insert artifact: {e}")))?;
        for (name, relpath, sha, size) in file_rows {
            self.conn.execute(
                "INSERT INTO artifact_file (artifact_id,name,relpath,sha256,size) VALUES (?1,?2,?3,?4,?5)",
                params![a.id.as_str(), name, relpath, sha, *size as i64],
            ).map_err(|e| M3FlowError::internal(format!("insert artifact_file: {e}")))?;
        }
        Ok(())
    }

    pub fn get_artifact(&self, id: &str) -> Result<Artifact> {
        self.conn
            .query_row(
                "SELECT id,type,schema_version,content_hash,producer,created_at,metadata_json,data_json
                 FROM artifact WHERE id=?1",
                params![id],
                |row| {
                    Ok(Artifact {
                        id: ArtifactId::parse(&row.get::<_, String>(0)?)
                            .unwrap_or_else(ArtifactId::new),
                        artifact_type: row.get(1)?,
                        schema_version: row.get(2)?,
                        content_hash: row.get(3)?,
                        producer: row
                            .get::<_, Option<String>>(4)?
                            .and_then(|s| TaskRunId::parse(&s)),
                        created_at: row.get(5)?,
                        metadata: serde_json::from_str(&row.get::<_, String>(6)?)
                            .unwrap_or(serde_json::Value::Null),
                        data: row
                            .get::<_, Option<String>>(7)?
                            .and_then(|s| serde_json::from_str(&s).ok()),
                        files: Default::default(),
                    })
                },
            )
            .map_err(|_| M3FlowError::not_found(format!("artifact '{id}'")))
            .map(|mut a| {
                a.files = self.artifact_files(id).unwrap_or_default();
                a
            })
    }

    pub fn artifact_files(&self, id: &str) -> Result<std::collections::BTreeMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, relpath FROM artifact_file WHERE artifact_id=?1")
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let mut out = std::collections::BTreeMap::new();
        for r in rows {
            let (n, p) = r.map_err(|e| M3FlowError::internal(e.to_string()))?;
            out.insert(n, p);
        }
        Ok(out)
    }

    pub fn list_artifacts(&self, type_filter: Option<&str>, limit: usize) -> Result<Vec<Artifact>> {
        let ids: Vec<String> = match type_filter {
            Some(t) => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM artifact WHERE type=?1 ORDER BY created_at DESC LIMIT ?2")
                    .map_err(|e| M3FlowError::internal(e.to_string()))?;
                let collected = stmt
                    .query_map(params![t, limit as i64], |r| r.get(0))
                    .map_err(|e| M3FlowError::internal(e.to_string()))?
                    .collect::<std::result::Result<Vec<String>, _>>()
                    .map_err(|e| M3FlowError::internal(e.to_string()))?;
                collected
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM artifact ORDER BY created_at DESC LIMIT ?1")
                    .map_err(|e| M3FlowError::internal(e.to_string()))?;
                let collected = stmt
                    .query_map(params![limit as i64], |r| r.get(0))
                    .map_err(|e| M3FlowError::internal(e.to_string()))?
                    .collect::<std::result::Result<Vec<String>, _>>()
                    .map_err(|e| M3FlowError::internal(e.to_string()))?;
                collected
            }
        };
        ids.iter().map(|id| self.get_artifact(id)).collect()
    }

    pub fn link_input(&self, task_run: &str, input_name: &str, artifact: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO artifact_input (task_run_id,input_name,artifact_id) VALUES (?1,?2,?3)",
            params![task_run, input_name, artifact],
        ).map_err(|e| M3FlowError::internal(format!("link input: {e}")))?;
        Ok(())
    }

    pub fn link_output(&self, task_run: &str, output_name: &str, artifact: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO artifact_output (task_run_id,output_name,artifact_id) VALUES (?1,?2,?3)",
            params![task_run, output_name, artifact],
        ).map_err(|e| M3FlowError::internal(format!("link output: {e}")))?;
        Ok(())
    }

    /// (input_name, artifact_id) pairs consumed by a task run.
    pub fn inputs_of(&self, task_run: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT input_name, artifact_id FROM artifact_input WHERE task_run_id=?1")
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![task_run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| M3FlowError::internal(e.to_string()))
    }

    /// (output_name, artifact_id) pairs produced by a task run.
    pub fn outputs_of(&self, task_run: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT output_name, artifact_id FROM artifact_output WHERE task_run_id=?1")
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![task_run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| M3FlowError::internal(e.to_string()))
    }

    /// Task runs that consumed an artifact (forward lineage).
    pub fn consumers_of(&self, artifact: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_run_id, input_name FROM artifact_input WHERE artifact_id=?1")
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![artifact], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| M3FlowError::internal(e.to_string()))
    }

    // ------------------------------------------------------------ cache

    pub fn cache_lookup(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_run_id FROM cache_entry WHERE cache_key=?1")
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![key], |r| r.get::<_, String>(0))
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        match rows.next() {
            Some(Ok(tr)) => Ok(Some(tr)),
            Some(Err(e)) => Err(M3FlowError::internal(e.to_string())),
            None => Ok(None),
        }
    }

    pub fn cache_insert(&self, key: &str, task_run: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO cache_entry (cache_key,task_run_id,created_at) VALUES (?1,?2,?3)",
            params![key, task_run, m3flow_core::artifact::now_rfc3339()],
        ).map_err(|e| M3FlowError::internal(format!("cache insert: {e}")))?;
        Ok(())
    }

    pub fn cache_stats(&self) -> Result<(usize, usize)> {
        let entries: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM cache_entry", [], |r| r.get(0))
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        let artifacts: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM artifact", [], |r| r.get(0))
            .map_err(|e| M3FlowError::internal(e.to_string()))?;
        Ok((entries as usize, artifacts as usize))
    }
}

fn task_run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRunRecord> {
    Ok(TaskRunRecord {
        id: TaskRunId::parse(&row.get::<_, String>(0)?).unwrap_or_else(TaskRunId::new),
        workflow_run_id: WorkflowRunId::parse(&row.get::<_, String>(1)?)
            .unwrap_or_else(WorkflowRunId::new),
        node_id: row.get(2)?,
        task_name: row.get(3)?,
        task_version: row.get(4)?,
        provider: row.get(5)?,
        status: TaskStatus::parse(&row.get::<_, String>(6)?).unwrap_or(TaskStatus::Pending),
        cache_key: row.get(7)?,
        attempts: row.get::<_, i64>(8)? as u32,
        created_at: row.get(9)?,
        started_at: row.get(10)?,
        ended_at: row.get(11)?,
        params: serde_json::from_str(&row.get::<_, String>(12)?)
            .unwrap_or(serde_json::Value::Null),
        error: row
            .get::<_, Option<String>>(13)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        validation: row
            .get::<_, Option<String>>(14)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        engine: row
            .get::<_, Option<String>>(15)?
            .and_then(|s| serde_json::from_str(&s).ok()),
    })
}
