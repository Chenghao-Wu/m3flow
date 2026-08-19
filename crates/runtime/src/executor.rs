//! Execution backends for provider jobs (docs/slurm.md).
//!
//! The local backend runs the provider as a blocking subprocess (the
//! historical behavior). The Slurm backend wraps the same provider call in a
//! generated batch script, submits it with `sbatch`, and polls `squeue` until
//! the job leaves the queue. Completion is signaled by an exit-code marker
//! file the script writes (`.m3flow_exit`), so accounting (`sacct`) is only
//! consulted when the job was killed before it could write the marker — the
//! design therefore also works on clusters with accounting disabled.
//!
//! Executors are a scheduling concern: nothing here joins cache keys,
//! spec hashes, or artifact identity (same rule as `resources`).

use m3flow_core::error::{M3FlowError, Result};
use m3flow_core::specs::Resources;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::project::{ExecutorKind, Project, SlurmConfig};
use crate::provider::{ExecuteResponse, ProviderError, ProviderHandle};

/// Resolved backend for one provider job.
#[derive(Debug, Clone)]
pub enum Executor {
    Local,
    Slurm(SlurmConfig),
}

impl Executor {
    /// Resolve the backend for a provider: `--executor` CLI flag >
    /// `providers.<name>.executor` > `executor.type` > local.
    pub fn resolve(project: &Project, provider: &str, cli_override: Option<ExecutorKind>) -> Self {
        match project.executor_for(provider, cli_override) {
            ExecutorKind::Local => Executor::Local,
            ExecutorKind::Slurm => Executor::Slurm(project.slurm_config()),
        }
    }
}

/// Run `m3flow-<name> execute <request.json>` on the resolved backend.
pub fn execute_provider(
    executor: &Executor,
    handle: &ProviderHandle,
    request_path: &Path,
    workdir: &Path,
    node_id: &str,
    resources: Option<&Resources>,
    cancel_flag: &Path,
) -> Result<ExecuteResponse> {
    match executor {
        Executor::Local => handle.execute(request_path, workdir),
        Executor::Slurm(cfg) => execute_slurm(
            cfg,
            handle,
            request_path,
            workdir,
            node_id,
            resources,
            cancel_flag,
        ),
    }
}

// ------------------------------------------------------------------ slurm

const EXIT_MARKER: &str = ".m3flow_exit";
const PROVIDER_STDOUT: &str = "provider_stdout.json";
const PROVIDER_STDERR: &str = "provider_stderr.log";
const SUBMIT_SCRIPT: &str = "submit.sh";
const JOB_ID_FILE: &str = "slurm_job_id";

/// Consecutive `squeue` failures tolerated before surfacing an error.
const MAX_POLL_FAILURES: u32 = 10;
/// How long to wait for the exit marker after a job leaves the queue
/// (shared-filesystem visibility lag): 30 × 2 s = 60 s.
const MARKER_GRACE_TRIES: u32 = 30;
/// `sacct` retries for a job that vanished without a marker (accounting lag).
const SACCT_GRACE_TRIES: u32 = 5;

fn execute_slurm(
    cfg: &SlurmConfig,
    handle: &ProviderHandle,
    request_path: &Path,
    workdir: &Path,
    node_id: &str,
    resources: Option<&Resources>,
    cancel_flag: &Path,
) -> Result<ExecuteResponse> {
    let default_res = Resources::default();
    let resources = resources.unwrap_or(&default_res);
    let job_id = match submit(cfg, handle, request_path, workdir, node_id, resources) {
        Ok(id) => id,
        Err(err) => return Ok(error_response(handle, workdir, err)),
    };
    let _ = std::fs::write(workdir.join(JOB_ID_FILE), format!("{job_id}\n"));

    let base_secs = cfg.poll_interval_secs.unwrap_or(15).clamp(2, 3600);
    let mut poll_failures = 0u32;
    let mut tick = 0u64;
    loop {
        if cancel_flag.exists() {
            let _ = run_slurm_cmd("scancel", &[job_id.as_str()]);
            return Ok(error_response(
                handle,
                workdir,
                ProviderError {
                    error_type: "cancelled".into(),
                    category: "environment_error".into(),
                    recoverable: false,
                    provider: None,
                    task: None,
                    message: Some(format!("run cancelled; slurm job {job_id} scancelled")),
                    details: None,
                    raw_log: None,
                },
            ));
        }
        match squeue_state(&job_id) {
            Ok(QueueState::Active(_)) => poll_failures = 0,
            Ok(QueueState::Gone) => break,
            Err(e) => {
                poll_failures += 1;
                if poll_failures >= MAX_POLL_FAILURES {
                    return Ok(error_response(
                        handle,
                        workdir,
                        ProviderError {
                            error_type: "slurm_poll_failed".into(),
                            category: "environment_error".into(),
                            recoverable: true,
                            provider: None,
                            task: None,
                            message: format!(
                                "squeue failed {MAX_POLL_FAILURES} times in a row for job {job_id}: {e}"
                            )
                            .into(),
                            details: None,
                            raw_log: None,
                        },
                    ));
                }
            }
        }
        tick += 1;
        jittered_sleep(base_secs, tick, &job_id);
    }

    finish_slurm_job(handle, workdir, &job_id)
}

/// The job left the queue: resolve the outcome via the exit marker when
/// present (normal completion), otherwise via accounting. Ordering matters:
/// a killed job fails fast from `sacct` instead of waiting out the marker
/// grace; on accounting-disabled clusters the grace loop gives the shared
/// filesystem time to flush the marker.
fn finish_slurm_job(
    handle: &ProviderHandle,
    workdir: &Path,
    job_id: &str,
) -> Result<ExecuteResponse> {
    if let Some(rc) = read_marker(workdir) {
        return resolve_marker(handle, workdir, rc);
    }
    // No marker: the job was killed before it could write. Accounting may
    // know why — but lag can also hide a still-finishing job, so only
    // definite failure states short-circuit the marker grace.
    if let Some(state) = sacct_state_with_grace(job_id) {
        let terminal_failure = !matches!(
            state.as_str(),
            "COMPLETED" | "PENDING" | "RUNNING" | "CONFIGURING" | "COMPLETING" | "SUSPENDED"
        );
        if terminal_failure {
            return Ok(error_response(
                handle,
                workdir,
                terminal_error(Some(&state), workdir),
            ));
        }
    }
    // COMPLETED / lagging / accounting disabled: the marker decides.
    for _ in 0..MARKER_GRACE_TRIES {
        std::thread::sleep(Duration::from_secs(2));
        if let Some(rc) = read_marker(workdir) {
            return resolve_marker(handle, workdir, rc);
        }
    }
    Ok(error_response(
        handle,
        workdir,
        terminal_error(None, workdir),
    ))
}

fn read_marker(workdir: &Path) -> Option<i32> {
    std::fs::read_to_string(workdir.join(EXIT_MARKER))
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

/// Marker present: trust the provider's own protocol output when it exists,
/// matching local-execution semantics exactly.
fn resolve_marker(handle: &ProviderHandle, workdir: &Path, rc: i32) -> Result<ExecuteResponse> {
    match read_provider_response(handle, workdir) {
        Some(Ok(resp)) => Ok(resp),
        Some(Err(e)) => Ok(error_response(
            handle,
            workdir,
            ProviderError {
                error_type: "provider_protocol".into(),
                category: "environment_error".into(),
                recoverable: true,
                provider: None,
                task: None,
                message: Some(format!("{e}")),
                details: None,
                raw_log: Some(log_tails(workdir)),
            },
        )),
        None => Ok(error_response(
            handle,
            workdir,
            ProviderError {
                error_type: "provider_protocol".into(),
                category: "environment_error".into(),
                recoverable: true,
                provider: None,
                task: None,
                message: format!(
                    "provider exited {rc} without protocol output (see {PROVIDER_STDOUT}/{PROVIDER_STDERR})"
                )
                .into(),
                details: None,
                raw_log: Some(log_tails(workdir)),
            },
        )),
    }
}

/// Submit the generated batch script. Errors are structured (not `Err`) so
/// they flow through the provider-failure branch with the right taxonomy.
fn submit(
    cfg: &SlurmConfig,
    handle: &ProviderHandle,
    request_path: &Path,
    workdir: &Path,
    node_id: &str,
    resources: &Resources,
) -> std::result::Result<String, ProviderError> {
    let script = render_batch_script(cfg, handle, request_path, workdir, node_id, resources)
        .map_err(|e| ProviderError {
            error_type: "invalid_slurm_config".into(),
            category: "input_error".into(),
            recoverable: false,
            provider: None,
            task: None,
            message: Some(e.to_string()),
            details: None,
            raw_log: None,
        })?;
    let script_path = workdir.join(SUBMIT_SCRIPT);
    std::fs::write(&script_path, script).map_err(|e| ProviderError {
        error_type: "io_error".into(),
        category: "environment_error".into(),
        recoverable: false,
        provider: None,
        task: None,
        message: Some(format!("writing {}: {e}", script_path.display())),
        details: None,
        raw_log: None,
    })?;

    let out = Command::new("sbatch")
        .arg("--parsable")
        .arg(&script_path)
        .output()
        .map_err(|e| ProviderError {
            error_type: "sbatch_unavailable".into(),
            category: "environment_error".into(),
            recoverable: false,
            provider: None,
            task: None,
            message: Some(format!(
                "spawning sbatch failed: {e} (the slurm executor must run on a Slurm login node)"
            )),
            details: None,
            raw_log: None,
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let lower = stderr.to_lowercase();
        // Misconfiguration is permanent; queue/QoS limits are transient.
        let permanent = [
            "invalid partition",
            "invalid qos",
            "invalid account",
            "invalid gres",
            "invalid feature",
            "unrecognized",
        ]
        .iter()
        .any(|k| lower.contains(k));
        return Err(ProviderError {
            error_type: "slurm_submit_failed".into(),
            category: if permanent {
                "environment_error"
            } else {
                "resource_error"
            }
            .into(),
            recoverable: !permanent,
            provider: None,
            task: None,
            message: Some(format!("sbatch rejected the submission: {stderr}")),
            details: None,
            raw_log: Some(stderr),
        });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id = stdout
        .lines()
        .next()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        return Err(ProviderError {
            error_type: "slurm_submit_failed".into(),
            category: "environment_error".into(),
            recoverable: true,
            provider: None,
            task: None,
            message: Some(format!(
                "could not parse a job id from sbatch output: {}",
                stdout.trim()
            )),
            details: None,
            raw_log: None,
        });
    }
    Ok(id)
}

enum QueueState {
    #[allow(dead_code)]
    Active(String),
    Gone,
}

fn squeue_state(job_id: &str) -> Result<QueueState> {
    let out = run_slurm_cmd("squeue", &["-h", "-j", job_id, "-o", "%T"])?;
    if out.status.success() {
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Ok(if state.is_empty() {
            QueueState::Gone
        } else {
            QueueState::Active(state)
        });
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Invalid job id") {
        return Ok(QueueState::Gone);
    }
    Err(M3FlowError::internal(format!(
        "squeue -j {job_id} exited {}: {}",
        out.status,
        stderr.trim()
    )))
}

fn sacct_state_with_grace(job_id: &str) -> Option<String> {
    for attempt in 0..SACCT_GRACE_TRIES {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(3));
        }
        if let Ok(out) = run_slurm_cmd("sacct", &["-X", "-n", "-P", "-j", job_id, "-o", "State"]) {
            if out.status.success() {
                if let Some(state) = parse_sacct_state(&String::from_utf8_lossy(&out.stdout)) {
                    return Some(state);
                }
            }
        }
    }
    None
}

/// First state token from `sacct -X -n -P -o State` output, uppercased.
/// Tolerates trailing pipes, `.batch`/`.extern` suffixes, and annotations
/// like `CANCELLED by 30159`.
pub fn parse_sacct_state(stdout: &str) -> Option<String> {
    let line = stdout.lines().find(|l| !l.trim().is_empty())?;
    let field = line.split('|').next()?.trim();
    let token = field.split('.').next()?.split_whitespace().next()?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_uppercase())
    }
}

/// Map a terminal accounting state onto the failure taxonomy. Only used when
/// the job vanished without writing its exit marker (killed by Slurm).
fn terminal_error(state: Option<&str>, workdir: &Path) -> ProviderError {
    let (error_type, category, recoverable, msg) = match state {
        Some("TIMEOUT") => (
            "slurm_timeout",
            "resource_error",
            true,
            "job hit the walltime limit",
        ),
        Some("OUT_OF_MEMORY") => (
            "slurm_out_of_memory",
            "resource_error",
            true,
            "job exceeded its memory allocation",
        ),
        Some("NODE_FAIL") | Some("BOOT_FAIL") => (
            "slurm_node_failure",
            "environment_error",
            true,
            "compute node failure",
        ),
        Some("PREEMPTED") | Some("REVOKED") => (
            "slurm_preempted",
            "environment_error",
            true,
            "job preempted by the scheduler",
        ),
        Some("CANCELLED") => (
            "slurm_cancelled",
            "environment_error",
            false,
            "job was cancelled externally",
        ),
        Some(other) => (
            "slurm_job_failed",
            "execution_error",
            false,
            match other {
                "FAILED" => "job failed before producing output",
                _ => "job terminated without completing",
            },
        ),
        None => (
            "slurm_job_lost",
            "resource_error",
            true,
            "job left the queue without a completion marker and accounting \
             has no record (killed: walltime/OOM/preemption?)",
        ),
    };
    let label = state.unwrap_or("UNKNOWN");
    ProviderError {
        error_type: error_type.into(),
        category: category.into(),
        recoverable,
        provider: None,
        task: None,
        message: Some(format!("slurm job {label}: {msg}")),
        details: None,
        raw_log: Some(log_tails(workdir)),
    }
}

/// Build a `status: "error"` response so Slurm-level failures flow through
/// the scheduler's provider-failure branch with the right taxonomy. The raw
/// response is persisted like a local run for inspectability.
fn error_response(
    handle: &ProviderHandle,
    workdir: &Path,
    mut err: ProviderError,
) -> ExecuteResponse {
    err.provider = err.provider.take().or_else(|| Some(handle.name.clone()));
    let _ = std::fs::write(
        workdir.join("response.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "status": "error",
            "error": err,
        }))
        .unwrap_or_default(),
    );
    ExecuteResponse {
        status: "error".into(),
        outputs: Default::default(),
        validation: Vec::new(),
        engine: None,
        warnings: Vec::new(),
        error: Some(err),
        partial_outputs: None,
    }
}

/// Read and parse the provider's protocol doc captured in the workdir.
/// `None` means no usable JSON (crash before/while printing).
fn read_provider_response(
    handle: &ProviderHandle,
    workdir: &Path,
) -> Option<Result<ExecuteResponse>> {
    let text = std::fs::read_to_string(workdir.join(PROVIDER_STDOUT)).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    Some(ExecuteResponse::persist_and_parse(v, workdir, &handle.name))
}

/// Render the batch script. Directive values are emitted raw (no shell
/// quoting — sbatch parses them itself), so project paths must not contain
/// spaces; shell lines are quoted.
pub fn render_batch_script(
    cfg: &SlurmConfig,
    handle: &ProviderHandle,
    request_path: &Path,
    workdir: &Path,
    node_id: &str,
    resources: &Resources,
) -> Result<String> {
    // Validate time strings up front so a typo fails before submission.
    let directives = resources_to_directives(resources, cfg)?;

    let mut s = String::from("#!/bin/bash\n");
    s.push_str(&format!(
        "#SBATCH --job-name={}\n",
        sanitize_job_name(node_id)
    ));
    s.push_str(&format!(
        "#SBATCH --output={}/slurm-%j.out\n",
        workdir.display()
    ));
    for d in &directives {
        s.push_str(&format!("#SBATCH {d}\n"));
    }
    for extra in &cfg.extra_sbatch {
        s.push_str(&format!("#SBATCH {extra}\n"));
    }
    s.push_str("\nset -euo pipefail\n");
    s.push_str(&format!("cd {}\n", shell_quote(workdir)));
    for cmd in &cfg.setup_commands {
        s.push_str(cmd);
        s.push('\n');
    }
    // Capture the provider exit code without errexit aborting the marker write.
    s.push_str("set +e\n");
    s.push_str(&format!(
        "{} execute {} > {} 2> {}\n",
        shell_quote(&handle.executable),
        shell_quote(request_path),
        shell_quote(&workdir.join(PROVIDER_STDOUT)),
        shell_quote(&workdir.join(PROVIDER_STDERR)),
    ));
    s.push_str("rc=$?\n");
    s.push_str(&format!(
        "echo \"$rc\" > {}\n",
        shell_quote(&workdir.join(EXIT_MARKER))
    ));
    s.push_str("exit \"$rc\"\n");
    Ok(s)
}

/// Map declared resources + Slurm config onto `#SBATCH` option strings.
pub fn resources_to_directives(resources: &Resources, cfg: &SlurmConfig) -> Result<Vec<String>> {
    let mut out = vec![
        "--nodes=1".to_string(),
        "--ntasks=1".to_string(),
        format!("--cpus-per-task={}", resources.cpu.unwrap_or(1).max(1)),
    ];
    if let Some(mem) = &resources.memory {
        out.push(format!("--mem={}", normalize_mem(mem)));
    }
    let walltime = resources.walltime.as_deref().or(cfg.time.as_deref());
    if let Some(w) = walltime {
        out.push(format!("--time={}", slurm_time(w)?));
    }
    if let Some(gres) = &cfg.gres {
        out.push(format!("--gres={gres}"));
    } else if let Some(gpu) = resources.gpu.filter(|n| *n > 0) {
        match &cfg.gpu_type {
            Some(t) => out.push(format!("--gres=gpu:{t}:{gpu}")),
            None => out.push(format!("--gres=gpu:{gpu}")),
        }
    }
    if let Some(p) = &cfg.partition {
        out.push(format!("--partition={p}"));
    }
    if let Some(a) = &cfg.account {
        out.push(format!("--account={a}"));
    }
    if let Some(q) = &cfg.qos {
        out.push(format!("--qos={q}"));
    }
    Ok(out)
}

/// Normalize a memory spec for `--mem`: `8GB`→`8G`, `512mb`→`512M`,
/// bare `8192`→`8192M` (Slurm's default unit is MiB).
pub fn normalize_mem(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return "1G".to_string();
    }
    let (digits, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let suffix = suffix.trim().to_uppercase();
    let unit = match suffix.as_str() {
        "" => "M".to_string(),
        u => u.trim_end_matches('B').to_string(),
    };
    format!("{digits}{unit}")
}

/// Normalize a walltime for `--time`. Slurm-native forms pass through
/// (bare minutes, `MM:SS`, `HH:MM:SS`, `D-HH:MM:SS`, `UNLIMITED`/`INFINITE`);
/// m3flow-style durations (`30 min`, `2 h`, `1.5 d`) are converted to bare
/// minutes, rounded up (Slurm's granularity).
pub fn slurm_time(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(M3FlowError::schema("empty walltime"));
    }
    let lower = s.to_lowercase();
    if lower == "unlimited" || lower == "infinite" {
        return Ok("INFINITE".to_string());
    }
    // Slurm-native: bare minutes, MM:SS / HH:MM:SS, D-HH:MM:SS
    if s.chars().all(|c| c.is_ascii_digit())
        || s.split([':', '-'])
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            && s.contains([':', '-'])
    {
        return Ok(s.to_string());
    }
    // m3flow-style: "<number>[ ]<unit>"
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num_s, unit_s) = s.split_at(split);
    let value: f64 = num_s.trim().parse().map_err(|_| {
        M3FlowError::schema(format!(
            "invalid walltime '{s}': use '30 min', '2 h', '1 d', or Slurm-native 'HH:MM:SS'"
        ))
    })?;
    let unit = unit_s.trim().to_lowercase();
    let minutes = match unit.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => value / 60.0,
        "m" | "min" | "mins" | "minute" | "minutes" => value,
        "h" | "hr" | "hrs" | "hour" | "hours" => value * 60.0,
        "d" | "day" | "days" => value * 1440.0,
        "w" | "week" | "weeks" => value * 10080.0,
        _ => {
            return Err(M3FlowError::schema(format!(
                "invalid walltime '{s}': unknown unit '{unit}' (use min/h/d or Slurm-native 'HH:MM:SS')"
            )))
        }
    };
    Ok(format!("{}", minutes.ceil().max(1.0) as u64))
}

fn sanitize_job_name(node_id: &str) -> String {
    let clean: String = node_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(56)
        .collect();
    format!("m3f_{clean}")
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

fn run_slurm_cmd(prog: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(prog).args(args).output().map_err(|e| {
        M3FlowError::io(
            e,
            format!("running {prog} (is Slurm installed and on PATH?)"),
        )
    })
}

/// Poll cadence with ±30% jitter (protects the controller when many tasks
/// run concurrently). Tiny xorshift seeded from time + job id; no `rand` dep.
fn jittered_sleep(base_secs: u64, tick: u64, job_id: &str) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let id_hash = job_id
        .bytes()
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64));
    let mut x = (nanos ^ id_hash ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)).max(1);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let pct = (x % 61) as i64 - 30; // -30..=30
    let millis = (base_secs as i64 * 1000 * (100 + pct) / 100).max(500) as u64;
    std::thread::sleep(Duration::from_millis(millis));
}

/// Tails of the provider stderr and Slurm output for error reports.
fn log_tails(workdir: &Path) -> String {
    let mut out = String::new();
    let mut tail = |path: PathBuf, label: &str| {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(40);
            if start < lines.len() {
                out.push_str(&format!("--- {label} (tail) ---\n"));
                out.push_str(&lines[start..].join("\n"));
                out.push('\n');
            }
        }
    };
    tail(workdir.join(PROVIDER_STDERR), PROVIDER_STDERR);
    if let Ok(entries) = std::fs::read_dir(workdir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("slurm-") && name.ends_with(".out") {
                tail(e.path(), &name);
            }
        }
    }
    out.chars().take(4000).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slurm_time_passthrough_native() {
        assert_eq!(slurm_time("45").unwrap(), "45");
        assert_eq!(slurm_time("10:00").unwrap(), "10:00");
        assert_eq!(slurm_time("1:30:00").unwrap(), "1:30:00");
        assert_eq!(slurm_time("2-00:00:00").unwrap(), "2-00:00:00");
        assert_eq!(slurm_time("UNLIMITED").unwrap(), "INFINITE");
    }

    #[test]
    fn slurm_time_m3flow_style() {
        assert_eq!(slurm_time("30 min").unwrap(), "30");
        assert_eq!(slurm_time("2 h").unwrap(), "120");
        assert_eq!(slurm_time("1 d").unwrap(), "1440");
        assert_eq!(slurm_time("1.5 h").unwrap(), "90");
        assert_eq!(slurm_time("90 s").unwrap(), "2");
        assert_eq!(slurm_time("30min").unwrap(), "30");
        assert!(slurm_time("soon").is_err());
        assert!(slurm_time("2 fortnights").is_err());
        assert!(slurm_time("").is_err());
    }

    #[test]
    fn mem_normalization() {
        assert_eq!(normalize_mem("8GB"), "8G");
        assert_eq!(normalize_mem("8192MB"), "8192M");
        assert_eq!(normalize_mem("512M"), "512M");
        assert_eq!(normalize_mem("64g"), "64G");
        assert_eq!(normalize_mem("1024"), "1024M");
    }

    #[test]
    fn sacct_state_parsing() {
        assert_eq!(parse_sacct_state("COMPLETED\n"), Some("COMPLETED".into()));
        assert_eq!(
            parse_sacct_state("CANCELLED by 30159\n"),
            Some("CANCELLED".into())
        );
        assert_eq!(parse_sacct_state("FAILED|\n"), Some("FAILED".into()));
        assert_eq!(parse_sacct_state("\n\nTIMEOUT\n"), Some("TIMEOUT".into()));
        assert_eq!(parse_sacct_state(""), None);
        assert_eq!(parse_sacct_state("||\n"), None);
    }

    #[test]
    fn terminal_state_taxonomy() {
        let wd = Path::new("/nonexistent");
        let e = terminal_error(Some("TIMEOUT"), wd);
        assert_eq!(e.category, "resource_error");
        assert!(e.recoverable);
        let e = terminal_error(Some("NODE_FAIL"), wd);
        assert_eq!(e.category, "environment_error");
        assert!(e.recoverable);
        let e = terminal_error(Some("CANCELLED"), wd);
        assert!(!e.recoverable);
        let e = terminal_error(Some("FAILED"), wd);
        assert_eq!(e.category, "execution_error");
        let e = terminal_error(None, wd);
        assert_eq!(e.error_type, "slurm_job_lost");
        assert!(e.recoverable);
    }

    fn test_handle() -> ProviderHandle {
        ProviderHandle {
            name: "fake".into(),
            executable: PathBuf::from("/opt/bin/m3flow-fake"),
            config: Default::default(),
            description: None,
        }
    }

    #[test]
    fn directives_from_resources() {
        let res = Resources {
            cpu: Some(8),
            gpu: Some(2),
            memory: Some("16GB".into()),
            walltime: Some("2 h".into()),
        };
        let cfg = SlurmConfig {
            partition: Some("gpua800".into()),
            qos: Some("4gpus".into()),
            gpu_type: Some("a800".into()),
            ..Default::default()
        };
        let d = resources_to_directives(&res, &cfg).unwrap();
        assert!(d.contains(&"--cpus-per-task=8".to_string()));
        assert!(d.contains(&"--mem=16G".to_string()));
        assert!(d.contains(&"--time=120".to_string()));
        assert!(d.contains(&"--gres=gpu:a800:2".to_string()));
        assert!(d.contains(&"--partition=gpua800".to_string()));
        assert!(d.contains(&"--qos=4gpus".to_string()));
    }

    #[test]
    fn directives_verbatim_gres_wins() {
        let res = Resources {
            gpu: Some(1),
            ..Default::default()
        };
        let cfg = SlurmConfig {
            gres: Some("gpu:4090:1,license:foo:1".into()),
            gpu_type: Some("a800".into()),
            ..Default::default()
        };
        let d = resources_to_directives(&res, &cfg).unwrap();
        assert!(d.contains(&"--gres=gpu:4090:1,license:foo:1".to_string()));
        assert!(!d.iter().any(|x| x == "--gres=gpu:a800:1"));
    }

    #[test]
    fn directives_defaults() {
        let d = resources_to_directives(&Resources::default(), &SlurmConfig::default()).unwrap();
        assert!(d.contains(&"--nodes=1".to_string()));
        assert!(d.contains(&"--ntasks=1".to_string()));
        assert!(d.contains(&"--cpus-per-task=1".to_string()));
        assert!(!d.iter().any(|x| x.starts_with("--gres")));
        assert!(!d.iter().any(|x| x.starts_with("--time")));
    }

    #[test]
    fn invalid_walltime_fails_before_submission() {
        let res = Resources {
            walltime: Some("whenever".into()),
            ..Default::default()
        };
        assert!(resources_to_directives(&res, &SlurmConfig::default()).is_err());
    }

    #[test]
    fn batch_script_shape() {
        let cfg = SlurmConfig {
            partition: Some("gpua800".into()),
            setup_commands: vec![
                "module load anaconda3".into(),
                "source activate autopoly".into(),
            ],
            extra_sbatch: vec!["--mail-type=NONE".into()],
            ..Default::default()
        };
        let res = Resources {
            cpu: Some(4),
            ..Default::default()
        };
        let wd = Path::new("/gpfs/home/u/proj/.m3flow/runs/run_x/node_y");
        let script = render_batch_script(
            &cfg,
            &test_handle(),
            Path::new("/gpfs/home/u/proj/.m3flow/runs/run_x/node_y/request.json"),
            wd,
            "node y",
            &res,
        )
        .unwrap();
        assert!(script.starts_with("#!/bin/bash\n"));
        assert!(script.contains("#SBATCH --job-name=m3f_node_y\n"));
        assert!(script.contains("#SBATCH --cpus-per-task=4\n"));
        assert!(script.contains("#SBATCH --partition=gpua800\n"));
        assert!(script.contains("#SBATCH --mail-type=NONE\n"));
        assert!(script.contains("module load anaconda3\nsource activate autopoly\n"));
        assert!(script.contains("'/opt/bin/m3flow-fake' execute '"));
        assert!(script.contains("rc=$?\n"));
        assert!(script.contains(".m3flow_exit"));
        assert!(script.contains("provider_stdout.json"));
        assert!(script.contains("exit \"$rc\"\n"));
    }

    #[test]
    fn job_name_sanitized() {
        assert_eq!(sanitize_job_name("run nvt/1"), "m3f_run_nvt_1");
        assert!(sanitize_job_name(&"x".repeat(200)).len() <= 60);
    }
}
