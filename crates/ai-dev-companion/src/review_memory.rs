//! What Review PR remembers about each PR it has reviewed in a Claude Code
//! terminal (port of `core/review-memory.js`): the session id, the folder it ran
//! in and the head commit it reviewed, so a second click can reopen that session
//! (head unchanged) or review only the new commits. One small JSON file in the
//! state dir, keyed by the review-in-editor plan's PR key.

use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "review-memory.json";
const MAX_AGE_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

fn file_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

fn read_all(dir: &Path) -> Map<String, Value> {
    match fs::read_to_string(file_in(dir)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// The remembered review for `key`, or `None`.
pub fn read_review(key: &str, dir: &Path) -> Option<Value> {
    read_all(dir).get(key).filter(|r| r.is_object()).cloned()
}

/// Remembers `record` (`{sessionId, cwd, reviewedSha, reviewedAt}`) for `key`,
/// dropping entries older than 30 days.
pub fn write_review(key: &str, record: &Value, dir: &Path, now_ms: i64) -> std::io::Result<()> {
    let mut all = read_all(dir);
    all.retain(|_, v| v.get("reviewedAt").and_then(Value::as_f64).is_some_and(|at| now_ms as f64 - at <= MAX_AGE_MS));
    all.insert(key.to_string(), record.clone());
    fs::create_dir_all(dir)?;
    let file = file_in(dir);
    let tmp = format!("{}.{}.tmp", file.display(), std::process::id());
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(&Value::Object(all)).expect("a JSON map serializes").as_bytes())?;
    drop(f);
    fs::rename(tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_written_review_reads_back_and_an_unknown_key_or_missing_file_is_none() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_review("P/r#1", d.path()), None);
        let rec = json!({ "sessionId": "s", "cwd": "/w", "reviewedSha": "abc1234", "reviewedAt": 100 });
        write_review("P/r#1", &rec, d.path(), 100).unwrap();
        assert_eq!(read_review("P/r#1", d.path()), Some(rec));
        assert_eq!(read_review("P/r#2", d.path()), None);
        assert_eq!(fs::metadata(file_in(d.path())).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn writing_replaces_that_prs_entry_keeps_others_and_drops_stale_ones() {
        let d = tempfile::tempdir().unwrap();
        let day = 24 * 60 * 60 * 1000;
        let rec = |s: &str, sha: &str, at: i64| json!({ "sessionId": s, "cwd": "/w", "reviewedSha": sha, "reviewedAt": at });
        write_review("old", &rec("s", "a", 0), d.path(), 0).unwrap();
        write_review("keep", &rec("s", "b", 10 * day), d.path(), 10 * day).unwrap();
        write_review("keep", &rec("t", "c", 40 * day), d.path(), 40 * day).unwrap();
        assert_eq!(read_review("old", d.path()), None);
        assert_eq!(read_review("keep", d.path()).unwrap()["sessionId"], "t");
    }

    #[test]
    fn a_corrupt_or_non_object_file_reads_as_empty() {
        let d = tempfile::tempdir().unwrap();
        fs::write(file_in(d.path()), "{nope").unwrap();
        assert_eq!(read_review("x", d.path()), None);
        fs::write(file_in(d.path()), "[1]").unwrap();
        assert_eq!(read_review("x", d.path()), None);
        write_review("x", &json!({ "reviewedAt": 1 }), d.path(), 1).unwrap();
        assert!(read_review("x", d.path()).is_some(), "a write over a bad file starts afresh");
    }
}
