//! Unified diff for a set of files, each against a fixed base ref, run inside
//! the worktree (port of `core/diff.ts`). `git diff` already produces a
//! perfectly good unified diff, so no diffing library sits on top of it.

use crate::exec::{git, ExecError};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub diff: String,
}

pub async fn compute_file_diffs(worktree_dir: &Path, base_ref: &str, files: &[String]) -> Result<Vec<FileDiff>, ExecError> {
    let mut diffs = Vec::with_capacity(files.len());
    for file in files {
        let result = git(&["diff", base_ref, "--", file], worktree_dir, false, None).await?;
        diffs.push(FileDiff { path: file.clone(), diff: result.stdout });
    }
    Ok(diffs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::git as run_git;
    use std::fs;

    #[tokio::test]
    async fn each_file_gets_its_own_diff_against_the_base_and_an_unchanged_one_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        run_git(p, &["init", "-q", "-b", "main"]);
        run_git(p, &["config", "user.email", "t@example.com"]);
        run_git(p, &["config", "user.name", "T"]);
        fs::write(p.join("a.txt"), "one\n").unwrap();
        fs::write(p.join("b.txt"), "same\n").unwrap();
        run_git(p, &["add", "."]);
        run_git(p, &["commit", "-q", "-m", "base"]);
        fs::write(p.join("a.txt"), "one\ntwo\n").unwrap();
        let diffs = compute_file_diffs(p, "HEAD", &["a.txt".into(), "b.txt".into()]).await.unwrap();
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].path, "a.txt");
        assert!(diffs[0].diff.contains("+two") && diffs[0].diff.starts_with("diff --git a/a.txt b/a.txt"));
        assert_eq!(diffs[1], FileDiff { path: "b.txt".into(), diff: String::new() });
    }

    #[tokio::test]
    async fn a_bad_base_ref_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        run_git(d.path(), &["init", "-q"]);
        assert!(compute_file_diffs(d.path(), "nope", &["a.txt".into()]).await.is_err());
    }
}
