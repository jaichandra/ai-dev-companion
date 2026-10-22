//! What `companion worktrees` shows and removes (port of
//! `core/worktree-report.js`): every worktree of the configured repos, whether
//! this tool made it, and what is safe to clear. A worktree is the tool's only
//! when it sits under `<repo>.worktrees/` or the job-worktree root; your own
//! (and the clone itself) are listed and never removed.

use crate::git_cli::{Git, GitOpts};
use crate::js_text::js_trim;
use crate::paths::repo_worktrees_root;
use crate::session_cwd::strictly_inside;
use crate::ticket_git::parse_worktree_porcelain;
use crate::work_units::WorkUnits;
use crate::workspace_resolver::inspect_worktree;
use crate::worktree_registry::real;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// How many worktrees the tool may have in one repo before Start fix says to clear some first.
pub const WORKTREE_SOFT_LIMIT: usize = 5;

fn owned_by(repo_path: &Path, dir: &str, job_worktrees_root: Option<&str>, home: &Path) -> bool {
    let root = repo_worktrees_root(repo_path, home).to_string_lossy().into_owned();
    strictly_inside(Some(&root), Some(dir)) || job_worktrees_root.is_some_and(|r| !r.is_empty() && strictly_inside(Some(r), Some(dir)))
}

/// origin's default branch ref (`origin/master`), or `None`.
async fn default_ref(repo_path: &Path, git: &impl Git) -> Result<Option<String>, String> {
    let head = git.run(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"], repo_path, GitOpts::allow_failure()).await?;
    let name = js_trim(&head.stdout);
    if head.code == 0 && !name.is_empty() {
        return Ok(Some(name.strip_prefix("refs/remotes/").unwrap_or(name).to_string()));
    }
    for name in ["master", "main"] {
        if git.run(&["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{name}")], repo_path, GitOpts::allow_failure()).await?.code == 0 {
            return Ok(Some(format!("origin/{name}")));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeRow {
    pub repo_key: String,
    pub repo_path: PathBuf,
    pub dir: String,
    pub branch: Option<String>,
    pub is_main_clone: bool,
    pub owned: bool,
    pub exists: bool,
    pub dirty: bool,
    pub changed: usize,
    pub in_progress: Option<&'static str>,
    pub unpushed: u64,
    pub merged: bool,
    pub upstream_gone: bool,
    pub issue_key: Option<String>,
    pub pr_key: Option<String>,
    pub sessions: usize,
    pub last_used: Option<f64>,
    pub prunable: bool,
    /// Why it isn't safe to remove; `None` when it is (if also `owned`).
    pub why: Option<String>,
}

impl WorktreeRow {
    pub fn to_json(&self) -> Value {
        json!({
            "repoKey": self.repo_key, "repoPath": self.repo_path, "dir": self.dir, "branch": self.branch,
            "isMainClone": self.is_main_clone, "owned": self.owned, "exists": self.exists, "dirty": self.dirty,
            "changed": self.changed, "inProgress": self.in_progress, "unpushed": self.unpushed, "merged": self.merged,
            "upstreamGone": self.upstream_gone, "issueKey": self.issue_key, "prKey": self.pr_key, "sessions": self.sessions,
            "lastUsed": self.last_used.map(crate::js_number::json_number), "prunable": self.prunable, "why": self.why,
        })
    }
}

/// null when the worktree is safe to remove, otherwise the reason it isn't.
pub fn prune_verdict(row: &WorktreeRow) -> Option<String> {
    let plural = |n: u64, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
    if row.is_main_clone {
        return Some("your clone".into());
    }
    if !row.owned {
        return Some("yours, not made by the tool".into());
    }
    if !row.exists {
        return Some("folder is gone".into());
    }
    if let Some(p) = row.in_progress {
        return Some(format!("a {p} is in progress"));
    }
    if row.dirty {
        return Some(format!("{} uncommitted change{}", row.changed, if row.changed == 1 { "" } else { "s" }));
    }
    // Removing a worktree never deletes its branch, so commits are not lost either way. A branch whose
    // upstream is gone was pushed and then deleted (the usual end of a merged PR, squash included), and one
    // that is an ancestor of the default branch is merged; anything else is still work in progress.
    if !row.merged && !row.upstream_gone {
        return Some(if row.unpushed > 0 { format!("{} not pushed", plural(row.unpushed, "commit")) } else { "branch not merged".into() });
    }
    None
}

/// The worktrees of `repos` (key and clone path, in order), each with its
/// state. `units` are the work-unit records, which supply the ticket, PR,
/// sessions and last use.
pub async fn list_worktrees(repos: &[(String, PathBuf)], units: &[Value], job_worktrees_root: Option<&str>, home: &Path, git: &impl Git) -> Result<Vec<WorktreeRow>, String> {
    let mut rows = vec![];
    for (repo_key, repo_path) in repos {
        if !repo_path.join(".git").exists() {
            continue;
        }
        let list = git.run(&["worktree", "list", "--porcelain"], repo_path, GitOpts::allow_failure()).await?;
        if list.code != 0 {
            continue;
        }
        let base = default_ref(repo_path, git).await?;
        let repo_real = real(&repo_path.to_string_lossy());
        for wt in parse_worktree_porcelain(&list.stdout) {
            let is_main_clone = real(&wt.dir) == repo_real;
            let exists = Path::new(&wt.dir).exists();
            let owned = !is_main_clone && owned_by(repo_path, &wt.dir, job_worktrees_root, home);
            let unit = units.iter().find(|u| u["dir"].as_str().is_some_and(|d| !d.is_empty() && real(d) == real(&wt.dir)));
            let mtime = || fs::metadata(&wt.dir).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64() * 1000.0);
            let mut row = WorktreeRow {
                repo_key: repo_key.clone(),
                repo_path: repo_path.clone(),
                dir: wt.dir.clone(),
                branch: wt.branch.clone(),
                is_main_clone,
                owned,
                exists,
                dirty: false,
                changed: 0,
                in_progress: None,
                unpushed: 0,
                merged: false,
                upstream_gone: false,
                issue_key: unit.and_then(|u| u["issueKey"].as_str()).map(String::from),
                pr_key: unit.and_then(|u| u["prKey"].as_str()).map(String::from),
                sessions: unit.and_then(|u| u["sessions"].as_array()).map_or(0, Vec::len),
                last_used: match unit {
                    Some(u) => u["updatedAt"].as_f64(),
                    None if exists => mtime(),
                    None => None,
                },
                prunable: false,
                why: None,
            };
            if exists {
                let dir = Path::new(&wt.dir);
                let state = inspect_worktree(dir, git).await?;
                (row.dirty, row.changed, row.in_progress) = (state.dirty, state.changed, state.in_progress);
                let local = git.run(&["rev-list", "--count", "HEAD", "--not", "--remotes"], dir, GitOpts::allow_failure()).await?;
                row.unpushed = if local.code == 0 { js_trim(&local.stdout).parse().unwrap_or(0) } else { 0 };
                if let Some(base) = &base {
                    row.merged = git.run(&["merge-base", "--is-ancestor", "HEAD", base], dir, GitOpts::allow_failure()).await?.code == 0;
                }
                if let Some(branch) = &wt.branch {
                    let track = git.run(&["for-each-ref", "--format=%(upstream:track)", &format!("refs/heads/{branch}")], repo_path, GitOpts::allow_failure()).await?;
                    row.upstream_gone = track.stdout.contains("gone");
                }
            }
            row.why = prune_verdict(&row);
            row.prunable = row.why.is_none() && owned;
            rows.push(row);
        }
    }
    Ok(rows)
}

#[derive(Debug, Default)]
pub struct Pruned {
    pub removed: Vec<WorktreeRow>,
    /// The row and the first line of git's complaint.
    pub failed: Vec<(WorktreeRow, String)>,
}

/// Removes the prunable worktrees (git refuses a dirty one itself, so there is
/// no `--force`) and drops stale registrations.
pub async fn prune_worktrees(rows: &[WorktreeRow], git: &impl Git, work_units: Option<&WorkUnits>) -> Result<Pruned, String> {
    let mut result = Pruned::default();
    for row in rows.iter().filter(|r| r.prunable) {
        let r = git.run(&["worktree", "remove", &row.dir], &row.repo_path, GitOpts::allow_failure()).await?;
        if r.code == 0 {
            result.removed.push(row.clone());
        } else {
            let text = if r.stderr.is_empty() { &r.stdout } else { &r.stderr };
            result.failed.push((row.clone(), js_trim(text).split('\n').next().unwrap_or("").to_string()));
        }
    }
    let mut seen: Vec<&PathBuf> = vec![];
    for row in rows {
        if !seen.contains(&&row.repo_path) {
            seen.push(&row.repo_path);
            git.run(&["worktree", "prune"], &row.repo_path, GitOpts::allow_failure()).await?;
        }
    }
    if let Some(units) = work_units {
        units.prune(|u| u["dir"].as_str().is_none_or(str::is_empty) || Path::new(u["dir"].as_str().unwrap_or("")).exists() || u["sessions"].as_array().is_some_and(|s| !s.is_empty()))?;
    }
    Ok(result)
}

/// How many worktrees of `repo_path` the tool owns now.
pub async fn count_owned(repo_path: &Path, job_worktrees_root: Option<&str>, home: &Path, git: &impl Git) -> Result<usize, String> {
    let list = git.run(&["worktree", "list", "--porcelain"], repo_path, GitOpts::allow_failure()).await?;
    if list.code != 0 {
        return Ok(0);
    }
    let me = real(&repo_path.to_string_lossy());
    Ok(parse_worktree_porcelain(&list.stdout).iter().filter(|wt| real(&wt.dir) != me && owned_by(repo_path, &wt.dir, job_worktrees_root, home)).count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::{git as sh, make_repos, Repos};
    use crate::git_cli::SystemGit;
    use crate::work_units::UnitFields;

    fn repos_of(r: &Repos) -> Vec<(String, PathBuf)> {
        vec![("ACME/app".to_string(), r.clone.clone())]
    }

    /// A tool-owned worktree on a new branch (made from origin/master).
    fn tool_tree(r: &Repos, name: &str) -> PathBuf {
        let dir = r.wt_root.join(name);
        fs::create_dir_all(&r.wt_root).unwrap();
        sh(&r.clone, &["worktree", "add", "-q", "-b", name, dir.to_str().unwrap(), "origin/master"]);
        dir
    }

    fn by_branch<'a>(rows: &'a [WorktreeRow], b: &str) -> &'a WorktreeRow {
        rows.iter().find(|x| x.branch.as_deref() == Some(b)).unwrap()
    }

    async fn list(r: &Repos, units: &[Value]) -> Vec<WorktreeRow> {
        list_worktrees(&repos_of(r), units, None, &r.home, &SystemGit).await.unwrap()
    }

    #[tokio::test]
    async fn it_lists_the_clone_yours_and_the_tools_and_says_which_can_be_pruned() {
        let r = make_repos();
        let merged = tool_tree(&r, "PROJ-1"); // nothing new on it: merged into origin/master
        let dirty = tool_tree(&r, "PROJ-2");
        fs::write(dirty.join("wip.txt"), "x").unwrap();
        let unpushed = tool_tree(&r, "PROJ-3");
        fs::write(unpushed.join("f.txt"), "x").unwrap();
        sh(&unpushed, &["add", "."]);
        sh(&unpushed, &["commit", "-q", "-m", "local"]);
        let mine = r.root.join("my-tree");
        sh(&r.clone, &["worktree", "add", "-q", "-b", "mine", mine.to_str().unwrap(), "origin/master"]);

        let rows = list(&r, &[]).await;
        assert_eq!(rows.len(), 5);
        assert!(by_branch(&rows, "master").is_main_clone && !by_branch(&rows, "master").prunable);
        assert!(!by_branch(&rows, "mine").owned);
        assert_eq!(by_branch(&rows, "mine").why.as_deref(), Some("yours, not made by the tool"));
        assert!(by_branch(&rows, "PROJ-1").prunable && by_branch(&rows, "PROJ-1").merged);
        assert_eq!(by_branch(&rows, "PROJ-2").why.as_deref(), Some("1 uncommitted change"));
        assert_eq!(by_branch(&rows, "PROJ-3").why.as_deref(), Some("1 commit not pushed"));
        assert_eq!(rows.iter().filter(|x| x.prunable).map(|x| x.dir.clone()).collect::<Vec<_>>(), [merged.to_string_lossy()]);
    }

    #[tokio::test]
    async fn a_branch_pushed_and_merged_away_upstream_gone_is_prunable_one_not_merged_is_kept() {
        let r = make_repos();
        let work = tool_tree(&r, "PROJ-4");
        fs::write(work.join("f.txt"), "x").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-q", "-m", "change"]);
        sh(&work, &["push", "-q", "-u", "origin", "PROJ-4"]);
        let rows = list(&r, &[]).await;
        assert_eq!(by_branch(&rows, "PROJ-4").why.as_deref(), Some("branch not merged"), "pushed but not merged and not deleted");

        sh(&r.clone, &["push", "-q", "origin", "--delete", "PROJ-4"]); // as after merging the PR
        sh(&r.clone, &["fetch", "-q", "--prune", "origin"]);
        let rows = list(&r, &[]).await;
        assert!(by_branch(&rows, "PROJ-4").upstream_gone && by_branch(&rows, "PROJ-4").prunable);
    }

    #[tokio::test]
    async fn the_registry_supplies_ticket_pr_sessions_and_last_use() {
        let r = make_repos();
        let dir = tool_tree(&r, "PROJ-5");
        let reg = WorkUnits::open(r.root.join("units.json")).with_clock(|| 5000);
        let unit = reg.upsert(r.clone.to_str().unwrap(), "PROJ-5", &UnitFields { dir: dir.to_str(), issue_key: Some("PROJ-5"), pr_key: Some("bitbucket:ACME/app#9") }).unwrap();
        reg.add_session(unit["key"].as_str().unwrap(), "s-1", dir.to_str(), None).unwrap();
        let rows = list(&r, &reg.list()).await;
        let row = by_branch(&rows, "PROJ-5");
        assert_eq!((row.issue_key.as_deref(), row.pr_key.as_deref(), row.sessions, row.last_used), (Some("PROJ-5"), Some("bitbucket:ACME/app#9"), 1, Some(5000.0)));
        assert!(by_branch(&rows, "master").last_used.is_some(), "a worktree with no unit falls back to its folder's modification time");
    }

    #[tokio::test]
    async fn pruning_removes_only_the_prunable_worktrees_keeps_the_branches_and_clears_stale_units() {
        let r = make_repos();
        let gone = tool_tree(&r, "PROJ-6");
        let kept = tool_tree(&r, "PROJ-7");
        fs::write(kept.join("wip.txt"), "x").unwrap();
        let reg = WorkUnits::open(r.root.join("units.json"));
        reg.upsert(r.clone.to_str().unwrap(), "PROJ-6", &UnitFields { dir: gone.to_str(), ..Default::default() }).unwrap();
        reg.upsert(r.clone.to_str().unwrap(), "PROJ-7", &UnitFields { dir: kept.to_str(), ..Default::default() }).unwrap();

        let rows = list(&r, &reg.list()).await;
        let result = prune_worktrees(&rows, &SystemGit, Some(&reg)).await.unwrap();
        assert_eq!(result.removed.iter().map(|x| x.dir.clone()).collect::<Vec<_>>(), [gone.to_string_lossy()]);
        assert!(result.failed.is_empty());
        assert!(!gone.exists() && kept.exists());
        assert!(sh(&r.clone, &["branch", "--list", "PROJ-6"]).contains("PROJ-6"), "the branch stays");
        assert_eq!(reg.list().iter().map(|u| u["branch"].as_str().unwrap().to_string()).collect::<Vec<_>>(), ["PROJ-7"]);
    }

    #[tokio::test]
    async fn count_owned_counts_only_the_tools_worktrees() {
        let r = make_repos();
        tool_tree(&r, "PROJ-8");
        tool_tree(&r, "PROJ-9");
        sh(&r.clone, &["worktree", "add", "-q", "-b", "mine", r.root.join("mine").to_str().unwrap(), "origin/master"]);
        assert_eq!(count_owned(&r.clone, None, &r.home, &SystemGit).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_repo_that_isnt_a_clone_is_skipped() {
        let rows = list_worktrees(&[("ACME/none".into(), PathBuf::from("/nonexistent/path"))], &[], None, Path::new("/h"), &SystemGit).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn job_worktrees_count_as_the_tools_too() {
        let r = make_repos();
        let jobs_root = r.root.join("jobs");
        let dir = jobs_root.join("app").join("11111111-1111-1111-1111-111111111111");
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        sh(&r.clone, &["worktree", "add", "-q", "--detach", dir.to_str().unwrap(), "origin/master"]);
        let rows = list_worktrees(&repos_of(&r), &[], jobs_root.to_str(), &r.home, &SystemGit).await.unwrap();
        let row = rows.iter().find(|x| x.dir == dir.to_string_lossy()).unwrap();
        assert!(row.owned && row.branch.is_none());
        assert_eq!(count_owned(&r.clone, jobs_root.to_str(), &r.home, &SystemGit).await.unwrap(), 1);
    }

    #[test]
    fn the_verdict_wording_matches_the_node_report() {
        let base = WorktreeRow {
            repo_key: "k".into(), repo_path: "/r".into(), dir: "/r.worktrees/x".into(), branch: Some("x".into()), is_main_clone: false, owned: true, exists: true,
            dirty: false, changed: 0, in_progress: None, unpushed: 0, merged: true, upstream_gone: false, issue_key: None, pr_key: None, sessions: 0, last_used: None, prunable: false, why: None,
        };
        assert_eq!(prune_verdict(&base), None);
        assert_eq!(prune_verdict(&WorktreeRow { exists: false, ..base.clone() }).as_deref(), Some("folder is gone"));
        assert_eq!(prune_verdict(&WorktreeRow { in_progress: Some("rebase"), ..base.clone() }).as_deref(), Some("a rebase is in progress"));
        assert_eq!(prune_verdict(&WorktreeRow { dirty: true, changed: 3, ..base.clone() }).as_deref(), Some("3 uncommitted changes"));
        assert_eq!(prune_verdict(&WorktreeRow { merged: false, unpushed: 2, ..base.clone() }).as_deref(), Some("2 commits not pushed"));
        assert_eq!(prune_verdict(&WorktreeRow { is_main_clone: true, ..base }).as_deref(), Some("your clone"));
    }
}
