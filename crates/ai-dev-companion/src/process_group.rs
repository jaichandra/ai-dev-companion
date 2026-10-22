//! Whether a headless `claude` from an earlier run of this service is still
//! alive (port of `core/process-group.js`). Claude is started detached, so it
//! leads its own process group, which also means it outlives this process
//! when that dies without cleaning up. The worktree features record that
//! group id while Claude runs, and after a restart Refresh diff and Approve
//! use this to refuse to stage, commit and push a worktree an orphaned Claude
//! may still be writing.
//!
//! Only ever a probe (signal 0), never a kill: a pid read back from a job
//! file after a restart may by now belong to an unrelated process, so the
//! fail-closed answer is to refuse and let the user wait or Discard.

use serde_json::Value;

/// True if process group `pgid` still has a live member. `kill(-pgid, 0)`
/// succeeding, or failing with EPERM (it exists but belongs to someone else),
/// means alive; ESRCH means no such group. A pgid that isn't a positive
/// integer is always false: 0 or a negative number would make kill(2) address
/// this process's own group or every process instead.
#[cfg(unix)]
pub fn is_process_group_alive(pgid: i64) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else { return false };
    if pgid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence and permission.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn is_process_group_alive(_pgid: i64) -> bool {
    false
}

/// The same over an untyped value read back from a job file: only an integral
/// JSON number can be alive.
pub fn is_process_group_alive_value(pgid: Option<&Value>) -> bool {
    let Some(Value::Number(n)) = pgid else { return false };
    let id = match n.as_i64() {
        Some(i) => i,
        None => match n.as_f64() {
            Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 => f as i64,
            _ => return false,
        },
    };
    is_process_group_alive(id)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn a_detached_childs_process_group_is_alive_while_it_runs_and_dead_once_its_killed() {
        // process_group(0) makes the child a group leader (pgid == pid),
        // exactly how claude is spawned.
        let mut child =
            Command::new("sleep").arg("30").process_group(0).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        let pgid = i64::from(child.id());
        let alive = is_process_group_alive(pgid);
        // SAFETY: kills the group we just created.
        unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
        child.wait().unwrap();
        assert!(alive);
        assert!(!is_process_group_alive(pgid));
    }

    #[test]
    fn a_non_positive_or_non_integer_pgid_is_never_alive() {
        // 0 and -1 would mean "my own group" / "every process" to kill(2).
        for pgid in [0, -1, -42, i64::MAX, i64::MIN] {
            assert!(!is_process_group_alive(pgid), "{pgid}");
        }
        for v in [json!(0), json!(-1), json!(1.5), json!("123"), json!(null), json!({}), json!(f64::NAN), json!(1e300)] {
            assert!(!is_process_group_alive_value(Some(&v)), "{v}");
        }
        assert!(!is_process_group_alive_value(None));
    }
}
