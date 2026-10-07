//! Run ownership: one live executor per workflow run.
//!
//! The process that executes a run holds an exclusive OS lock on
//! `runs/<id>/OWNER` (`File::try_lock`: flock on Unix, LockFileEx on
//! Windows) for the duration of the execution. The lock dies with the
//! process, so a crashed owner never needs stale-file detection, and the
//! file itself is never deleted (deleting a lock file reopens the race it
//! guards against). Its content only names the current/last holder for
//! error messages. Each acquisition bumps `runs/<id>/GENERATION`, which
//! scopes cancellation flags: a CANCEL written for an earlier execution
//! never cancels a later one.

use m3flow_core::artifact::now_rfc3339;
use m3flow_core::error::{M3FlowError, Result};
use std::io::{Seek, Write};
use std::path::Path;

const OWNER_FILE: &str = "OWNER";
const GENERATION_FILE: &str = "GENERATION";

/// Held for the duration of one execution; the OS lock is released when
/// the lease (and its file handle) is dropped or the process exits.
#[derive(Debug)]
pub struct RunLease {
    _owner: std::fs::File,
    pub generation: u64,
}

/// Current execution generation of a run (0 if it never executed).
pub fn current_generation(run_dir: &Path) -> u64 {
    std::fs::read_to_string(run_dir.join(GENERATION_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Become the run's single executor (new generation), or fail if another
/// live process — or another handle in this process — owns it.
pub fn acquire(run_dir: &Path) -> Result<RunLease> {
    std::fs::create_dir_all(run_dir).map_err(|e| M3FlowError::io(e, "creating run dir"))?;
    let path = run_dir.join(OWNER_FILE);
    let mut owner = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| M3FlowError::io(e, "opening run owner file"))?;
    match owner.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let holder: Option<serde_json::Value> = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            let who = holder
                .map(|h| {
                    format!(
                        "pid {} on host {} (since {})",
                        h["pid"],
                        h["host"].as_str().unwrap_or("?"),
                        h["acquired_at"].as_str().unwrap_or("?")
                    )
                })
                .unwrap_or_else(|| "another process".into());
            return Err(M3FlowError::workflow(
                format!(
                    "run is being executed by {who}; wait for it to finish or cancel it \
                     (the ownership lock is released automatically when that process exits)"
                ),
                None,
            ));
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(M3FlowError::io(e, "locking run owner file"))
        }
    }
    // Exclusive from here on: bump the generation, then record ourselves.
    let generation = current_generation(run_dir) + 1;
    std::fs::write(run_dir.join(GENERATION_FILE), format!("{generation}\n"))
        .map_err(|e| M3FlowError::io(e, "writing run generation"))?;
    let doc = serde_json::json!({
        "pid": std::process::id(),
        "generation": generation,
        "host": hostname(),
        "acquired_at": now_rfc3339(),
    });
    owner
        .set_len(0)
        .and_then(|_| owner.rewind())
        .and_then(|_| owner.write_all(doc.to_string().as_bytes()))
        .map_err(|e| M3FlowError::io(e, "writing run owner"))?;
    Ok(RunLease {
        _owner: owner,
        generation,
    })
}

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buffer and length are valid; result is NUL-terminated on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("m3lease-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn exclusive_and_generational() {
        let d = dir("excl");
        let a = acquire(&d).unwrap();
        assert_eq!(a.generation, 1);
        // held (even by this same process): a second acquisition is refused
        let err = acquire(&d).unwrap_err().to_string();
        assert!(err.contains(&format!("pid {}", std::process::id())), "{err}");
        drop(a);
        let b = acquire(&d).unwrap();
        assert_eq!(b.generation, 2);
        drop(b);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn leftover_owner_file_of_a_dead_process_is_not_a_lock() {
        let d = dir("stale");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(OWNER_FILE), r#"{"pid": 999999999, "generation": 4}"#).unwrap();
        std::fs::write(d.join(GENERATION_FILE), "4\n").unwrap();
        let lease = acquire(&d).unwrap();
        assert_eq!(lease.generation, 5);
        drop(lease);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn concurrent_acquisitions_yield_exactly_one_owner() {
        let d = dir("race");
        std::fs::create_dir_all(&d).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (d, barrier) = (d.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    let lease = acquire(&d).ok();
                    // hold the lease until every contender has tried
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    lease.map(|l| l.generation)
                })
            })
            .collect();
        let won: Vec<u64> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(won, vec![1], "exactly one contender may own the run");
        let _ = std::fs::remove_dir_all(&d);
    }
}
