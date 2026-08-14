//! Friendly, human-browsable view of run outputs under `results/` — a
//! derived, always-on view layered over the content-addressed store.
//!
//! Invariants (do not break):
//!  - Derived view only: nothing here is authoritative. The SQLite
//!    provenance DB + CAS are truth; any tree can be rebuilt at any time
//!    (`m3flow results sync`).
//!  - Best-effort: materialization failures must never fail a run —
//!    callers log a warning and continue.
//!  - Presentation-only: nothing in this module joins cache keys, spec
//!    hashes, or artifact identity (`defaults.materialize` config).
//!  - Store blobs are written read-only (store.rs), so symlinks here
//!    cannot be used to mutate the CAS in place. A future store GC must
//!    treat `results/` links as GC roots (or use `results sync` as the
//!    repair pass).

use crate::db::{Db, WorkflowRunRecord};
use crate::project::Project;
use crate::store::Store;
use m3flow_core::artifact::Artifact;
use m3flow_core::error::{M3FlowError, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Snapshot of one step for `run.json` (the scheduler maps its private
/// per-node state into these; the rebuild path assembles them from the DB).
pub struct StepView {
    pub order: usize,
    pub node_id: String,
    pub status: String,
    pub task_run: Option<String>,
    pub outputs: BTreeMap<String, String>,
}

// ------------------------------------------------------------------ paths

pub fn results_root(project_root: &Path) -> PathBuf {
    project_root.join("results")
}

/// `results/<group>/<YYYY-MM-DD>_<HH-MM>_<workflow>_<wr_id>/` where
/// `<group>` is the run's user-assigned label (study folder) or, when
/// unlabeled, the workflow name. The stamp is the run's `created_at`
/// (UTC), matching run.json and the DB records; the wr_id suffix keeps
/// same-minute runs of one workflow distinct.
pub fn run_dir(project_root: &Path, run: &WorkflowRunRecord) -> PathBuf {
    let stamp = run
        .created_at
        .get(..16) // "2026-08-13T12:37"
        .unwrap_or(&run.created_at)
        .replace('T', "_")
        .replace(':', "-");
    let group = run.label.as_deref().unwrap_or(&run.name);
    results_root(project_root)
        .join(sanitize(group))
        .join(format!("{stamp}_{}_{}", sanitize(&run.name), run.id))
}

/// Keep workflow-spec characters that are meaningful (incl. `.` from
/// subworkflow node ids); replace anything hostile to filesystems.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Path of `target` relative to `from_dir` (both absolute). Symlinks use
/// this so a moved/archived project keeps working.
fn relative_target(from_dir: &Path, target: &Path) -> PathBuf {
    let from: Vec<Component> = from_dir.components().collect();
    let to: Vec<Component> = target.components().collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut rel = PathBuf::new();
    for _ in common..from.len() {
        rel.push("..");
    }
    for c in &to[common..] {
        rel.push(c.as_os_str());
    }
    rel
}

// ------------------------------------------------------------------ naming

/// Conservative content sniff: only patterns we are sure of. Returns an
/// extension without dot.
fn sniff_ext(head: &[u8]) -> Option<&'static str> {
    let s = std::str::from_utf8(head).ok()?.trim_start();
    if s.starts_with("LAMMPS Description") {
        Some("data")
    } else if s.starts_with('{') || s.starts_with('[') {
        Some("json")
    } else if s.starts_with("set type") {
        Some("inc")
    } else if s.starts_with("units") || s.contains("pair_coeff") {
        Some("lmp")
    } else {
        None
    }
}

/// Deterministic friendly name for an artifact file. Roles may be simple
/// slugs ("data", "charges") or relative paths with a real filename leaf
/// ("lt/monomer_0_0le.lt" from moltemplate bundles) — in the path case
/// the leaf is kept as-is. Total: any role maps to a name, collisions get
/// a `__<role>` suffix before the extension.
fn friendly_file_name(role: &str, target: &Path, used: &mut BTreeSet<String>) -> String {
    let leaf = role.rsplit('/').next().unwrap_or(role);
    let name = if leaf.contains('.') && !leaf.starts_with('.') {
        sanitize(leaf)
    } else {
        let head = std::fs::read(target)
            .map(|b| b[..b.len().min(512)].to_vec())
            .unwrap_or_default();
        let ext = match leaf {
            "data" | "topology" => sniff_ext(&head).unwrap_or("data"),
            "charges" => "inc",
            "init" | "settings" => "lmp",
            "log" => "lammps",
            "dcd" => "dcd",
            "csv" => "csv",
            "restart" => "restart",
            "spec" => "yaml",
            _ => sniff_ext(&head).unwrap_or("dat"),
        };
        let base = match leaf {
            "data" => "system",
            other => other,
        };
        format!("{}.{ext}", sanitize(base))
    };
    if used.insert(name.clone()) {
        return name;
    }
    let tag = sanitize(role);
    let alt = match name.rsplit_once('.') {
        Some((b, e)) => format!("{b}__{tag}.{e}"),
        None => format!("{name}__{tag}"),
    };
    if used.insert(alt.clone()) {
        return alt;
    }
    // pathological: same role twice — number deterministically
    let mut n = 2;
    loop {
        let cand = match name.rsplit_once('.') {
            Some((b, e)) => format!("{b}__{tag}_{n}.{e}"),
            None => format!("{name}__{tag}_{n}"),
        };
        if used.insert(cand.clone()) {
            return cand;
        }
        n += 1;
    }
}

// ------------------------------------------------------------------ symlinks

#[cfg(unix)]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    // Fallback for platforms without symlinks: copy the blob.
    std::fs::copy(target, link).map(|_| ())
}

// ------------------------------------------------------------------ views

/// Write one artifact's view: symlinks for its files, `data.json` for the
/// inline payload, `_artifact.yaml` as the identity pointer.
fn write_artifact_view(
    dir: &Path,
    artifact: &Artifact,
    store: &Store,
    mut extra: serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| M3FlowError::io(e, format!("creating {}", dir.display())))?;

    let mut used = BTreeSet::new();
    for (role, relpath) in &artifact.files {
        let target = store.resolve(relpath);
        let name = friendly_file_name(role, &target, &mut used);
        let link = dir.join(&name);
        let rel = relative_target(dir, &target);
        if link.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&link);
        }
        make_link(&rel, &link)
            .map_err(|e| M3FlowError::io(e, format!("linking {}", link.display())))?;
    }

    if let Some(data) = &artifact.data {
        if !data.is_null() {
            let payload = serde_json::to_string_pretty(data)
                .map_err(|e| M3FlowError::internal(format!("data payload encode: {e}")))?;
            std::fs::write(dir.join("data.json"), payload)
                .map_err(|e| M3FlowError::io(e, "writing data.json"))?;
        }
    }

    extra.insert("id".into(), artifact.id.to_string().into());
    extra.insert("type".into(), artifact.artifact_type.clone().into());
    extra.insert(
        "schema_version".into(),
        artifact.schema_version.clone().into(),
    );
    extra.insert("content_hash".into(), artifact.content_hash.clone().into());
    extra.insert("created_at".into(), artifact.created_at.clone().into());
    extra.insert(
        "producer".into(),
        artifact
            .producer
            .as_ref()
            .map(|p| p.to_string().into())
            .unwrap_or(serde_json::Value::Null),
    );
    extra.insert(
        "note".into(),
        "derived view — identity and provenance live in the m3flow DB; files are symlinks into the content-addressed store".into(),
    );
    let yaml = serde_yaml::to_string(&serde_json::Value::Object(extra))
        .map_err(|e| M3FlowError::internal(format!("yaml encode: {e}")))?;
    std::fs::write(dir.join("_artifact.yaml"), yaml)
        .map_err(|e| M3FlowError::io(e, "writing _artifact.yaml"))?;
    Ok(())
}

/// Materialize the run's workflow inputs under `_inputs/` (run start).
pub fn materialize_run_inputs(
    project: &Project,
    store: &Store,
    db: &Db,
    run: &WorkflowRunRecord,
    inputs: &BTreeMap<String, m3flow_core::id::ArtifactId>,
) -> Result<()> {
    let base = run_dir(&project.root, run);
    for (name, aid) in inputs {
        let art = db.get_artifact(aid.as_str())?;
        let dir = base.join("_inputs").join(sanitize(name));
        let mut extra = serde_json::Map::new();
        extra.insert("run".into(), run.id.to_string().into());
        extra.insert("input".into(), name.clone().into());
        write_artifact_view(&dir, &art, store, extra)?;
    }
    Ok(())
}

/// Materialize one completed step's outputs under `NN_<node_id>/`
/// (called from the scheduler on every task success → tree grows live).
pub fn materialize_step_outputs(
    project: &Project,
    store: &Store,
    run: &WorkflowRunRecord,
    order: usize,
    node_id: &str,
    outputs: &[(String, Artifact)],
) -> Result<()> {
    let step_dir = run_dir(&project.root, run).join(format!("{order:02}_{}", sanitize(node_id)));
    let mut used_types = BTreeSet::new();
    for (role, art) in outputs {
        let mut dir_name = sanitize(&art.artifact_type);
        if !used_types.insert(dir_name.clone()) {
            dir_name = format!("{}__{}", sanitize(&art.artifact_type), sanitize(role));
            used_types.insert(dir_name.clone());
        }
        let mut extra = serde_json::Map::new();
        extra.insert("run".into(), run.id.to_string().into());
        extra.insert("step".into(), node_id.to_string().into());
        extra.insert("output".into(), role.clone().into());
        write_artifact_view(&step_dir.join(dir_name), art, store, extra)?;
    }
    Ok(())
}

/// (Re)write `run.json` — the run-level pointer + step map. Cheap enough
/// to rewrite after every step; the final write carries the terminal status.
pub fn write_run_json(
    project: &Project,
    run: &WorkflowRunRecord,
    inputs: &BTreeMap<String, String>,
    steps: &[StepView],
) -> Result<()> {
    let doc = serde_json::json!({
        "run": {
            "id": run.id.to_string(),
            "workflow": format!("{}@{}", run.name, run.version),
            "label": run.label,
            "status": run.status.as_str(),
            "created_at": run.created_at,
            "started_at": run.started_at,
            "ended_at": run.ended_at,
        },
        "inputs": inputs,
        "steps": steps.iter().map(|s| serde_json::json!({
            "order": s.order,
            "node_id": s.node_id,
            "status": s.status,
            "task_run": s.task_run,
            "outputs": s.outputs,
        })).collect::<Vec<_>>(),
        "note": "derived view — truth is the m3flow DB (.m3flow/m3flow.db) and the content-addressed store; rebuild with `m3flow results sync`",
    });
    let text = serde_json::to_string_pretty(&doc)
        .map_err(|e| M3FlowError::internal(format!("run.json encode: {e}")))?;
    std::fs::write(run_dir(&project.root, run).join("run.json"), text)
        .map_err(|e| M3FlowError::io(e, "writing run.json"))?;
    Ok(())
}

// ------------------------------------------------------------------ rebuild

/// Rebuild one run's tree from the DB (wipe + re-derive). Step order uses
/// task `created_at` (dispatch order follows dependency order); only the
/// scheduler path knows the exact declaration index, and both orders agree
/// for sequential execution.
pub fn sync_run(project: &Project, db: &Db, store: &Store, run_id: &str) -> Result<PathBuf> {
    let run = db.get_workflow_run(run_id)?;
    let base = run_dir(&project.root, &run);
    if base.exists() {
        std::fs::remove_dir_all(&base)
            .map_err(|e| M3FlowError::io(e, format!("wiping {}", base.display())))?;
    }
    std::fs::create_dir_all(&base)
        .map_err(|e| M3FlowError::io(e, format!("creating {}", base.display())))?;

    // inputs from the run row
    let mut inputs = BTreeMap::new();
    if let Some(obj) = run.inputs.as_object() {
        for (name, v) in obj {
            if let Some(aid) = v.as_str() {
                inputs.insert(name.clone(), aid.to_string());
            }
        }
    }
    for (name, aid) in &inputs {
        if let Ok(art) = db.get_artifact(aid) {
            let mut extra = serde_json::Map::new();
            extra.insert("run".into(), run.id.to_string().into());
            extra.insert("input".into(), name.clone().into());
            write_artifact_view(
                &base.join("_inputs").join(sanitize(name)),
                &art,
                store,
                extra,
            )?;
        }
    }

    // steps in dispatch order
    let mut tasks = db.task_runs_of(run.id.as_str())?;
    tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    let mut views = Vec::new();
    for (i, tr) in tasks.iter().enumerate() {
        let outs = db.outputs_of(tr.id.as_str())?;
        let mut artifacts = Vec::new();
        let mut out_map = BTreeMap::new();
        for (oname, aid) in outs {
            if let Ok(art) = db.get_artifact(&aid) {
                out_map.insert(oname.clone(), aid.clone());
                artifacts.push((oname, art));
            }
        }
        if matches!(
            tr.status,
            m3flow_core::artifact::TaskStatus::Completed
                | m3flow_core::artifact::TaskStatus::Cached
        ) && !artifacts.is_empty()
        {
            materialize_step_outputs(project, store, &run, i + 1, &tr.node_id, &artifacts)?;
        }
        views.push(StepView {
            order: i + 1,
            node_id: tr.node_id.clone(),
            status: tr.status.as_str().to_string(),
            task_run: Some(tr.id.to_string()),
            outputs: out_map,
        });
    }
    write_run_json(project, &run, &inputs, &views)?;
    Ok(base)
}

/// Rebuild every run's tree (`m3flow results sync` without `--run`).
/// Wipes the results/ root first: the trees are pure derivatives of the
/// DB, so orphans from deleted runs, relabeled studies, or an old layout
/// should disappear instead of lingering next to the rebuild.
pub fn sync_all(project: &Project, db: &Db, store: &Store) -> Result<Vec<String>> {
    let root = results_root(&project.root);
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .map_err(|e| M3FlowError::io(e, format!("wiping {}", root.display())))?;
    }
    std::fs::create_dir_all(&root)
        .map_err(|e| M3FlowError::io(e, format!("creating {}", root.display())))?;

    let runs = db.list_workflow_runs(10000)?;
    let mut rebuilt = Vec::new();
    for run in &runs {
        // A run whose row is broken should not block the others.
        match sync_run(project, db, store, run.id.as_str()) {
            Ok(dir) => rebuilt.push(dir.display().to_string()),
            Err(e) => eprintln!("warning: sync skipped {}: {e}", run.id),
        }
    }
    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_meaningful_chars() {
        assert_eq!(sanitize("equilibrate.promote"), "equilibrate.promote");
        assert_eq!(sanitize("a/b\\c d"), "a_b_c_d");
    }

    fn test_run(label: Option<&str>) -> WorkflowRunRecord {
        WorkflowRunRecord {
            id: m3flow_core::id::WorkflowRunId::parse("wr_6ac43a45").unwrap(),
            name: "construct_system".into(),
            version: "1.0.0".into(),
            spec_hash: "x".into(),
            status: m3flow_core::artifact::RunStatus::Completed,
            created_at: "2026-08-13T12:37:53.246Z".into(),
            started_at: None,
            ended_at: None,
            workdir: String::new(),
            git: None,
            inputs: serde_json::json!({}),
            params: serde_json::json!({}),
            outputs: None,
            error: None,
            label: label.map(str::to_string),
        }
    }

    #[test]
    fn unlabeled_run_groups_by_workflow() {
        let dir = run_dir(Path::new("/p"), &test_run(None));
        assert_eq!(
            dir,
            PathBuf::from(
                "/p/results/construct_system/2026-08-13_12-37_construct_system_wr_6ac43a45"
            )
        );
    }

    #[test]
    fn labeled_run_groups_by_label() {
        let dir = run_dir(Path::new("/p"), &test_run(Some("peo-5k-screen")));
        assert_eq!(
            dir,
            PathBuf::from("/p/results/peo-5k-screen/2026-08-13_12-37_construct_system_wr_6ac43a45")
        );
    }

    #[test]
    fn relative_target_walks_up() {
        let from = Path::new("/p/results/wf/run/01_step/Type");
        let to = Path::new("/p/.m3flow/artifacts/sha256/ab/abcdef");
        let rel = relative_target(from, to);
        assert_eq!(
            rel,
            PathBuf::from("../../../../../.m3flow/artifacts/sha256/ab/abcdef")
        );
    }

    #[test]
    fn sniff_known_formats() {
        assert_eq!(
            sniff_ext(b"LAMMPS Description\n\n 1620 atoms"),
            Some("data")
        );
        assert_eq!(sniff_ext(b"{\"a\": 1}"), Some("json"));
        assert_eq!(sniff_ext(b"set type 1 charge -0.18"), Some("inc"));
        assert_eq!(sniff_ext(b"units real\natom_style full"), Some("lmp"));
        assert_eq!(sniff_ext(b"  pair_coeff 1 1 0.07 3.4"), Some("lmp"));
        assert_eq!(sniff_ext(b"\x00\x01binary"), None);
    }

    #[test]
    fn collision_names_are_deterministic() {
        let mut used = BTreeSet::new();
        let missing = Path::new("/nonexistent");
        let a = friendly_file_name("data", missing, &mut used);
        let b = friendly_file_name("data", missing, &mut used);
        assert_eq!(a, "system.data");
        assert_eq!(b, "system__data.data");
    }

    #[test]
    fn path_roles_keep_their_leaf_filename() {
        let mut used = BTreeSet::new();
        let missing = Path::new("/nonexistent");
        // moltemplate bundles register roles like "lt/monomer_0_0le.lt"
        assert_eq!(
            friendly_file_name("lt/monomer_0_0le.lt", missing, &mut used),
            "monomer_0_0le.lt"
        );
        // leaf collision against a simple role mapping
        assert_eq!(
            friendly_file_name("data", missing, &mut used),
            "system.data"
        );
        assert_eq!(
            friendly_file_name("sub/system.data", missing, &mut used),
            "system__sub_system.data.data"
        );
    }
}
