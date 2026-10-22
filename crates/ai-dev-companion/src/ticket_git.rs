//! Local git for the ticket features (port of `core/ticket-git.js`): find a
//! ticket's branches and worktrees in a clone, create its persistent worktree
//! (`<stateDir>/<repo>.worktrees/<KEY>`, the same layout as the PR-review
//! worktree), and read or push its commits. Every git call is an argv array (no
//! shell); branch names are checked before use.

use crate::bitbucket_endpoints::is_safe_branch_name;
use crate::git_cli::{Git, GitOpts, FETCH_TIMEOUT};
use crate::js_text::{js_trim, locale_compare, slice_utf16};
use crate::node_path;
use crate::paths::repo_worktrees_root;
use crate::review_worktree::link_node_modules;
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static ISSUE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_]*-[1-9][0-9]*$").unwrap());
static AHEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"ahead ([0-9]+)").unwrap());
static BEHIND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"behind ([0-9]+)").unwrap());
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

fn assert_issue_key(key: &str) -> Result<(), String> {
    if ISSUE_KEY.is_match(key) { Ok(()) } else { Err(format!("Not an issue key: {}", slice_utf16(key, 40))) }
}

/// Matches the key inside a branch or folder name, case-insensitively, and not
/// as a prefix of a longer number (PROJ-12 doesn't match PROJ-123): the
/// `(^|[^A-Za-z0-9])KEY(?![0-9])` of the Node code, without a lookahead.
#[derive(Debug, Clone)]
pub struct KeyPattern {
    lower: String,
}

impl KeyPattern {
    pub fn is_match(&self, text: &str) -> bool {
        let hay = text.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = hay[from..].find(&self.lower) {
            let at = from + i;
            let end = at + self.lower.len();
            let before_ok = text[..at].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
            let after_ok = !text[end..].starts_with(|c: char| c.is_ascii_digit());
            if before_ok && after_ok {
                return true;
            }
            from = at + 1;
            while !hay.is_char_boundary(from) {
                from += 1;
            }
        }
        false
    }
}

pub fn key_pattern(issue_key: &str) -> Result<KeyPattern, String> {
    assert_issue_key(issue_key)?;
    Ok(KeyPattern { lower: issue_key.to_ascii_lowercase() })
}

/// `<stateDir>/<basename>.worktrees/<KEY>`.
pub fn ticket_worktree_path(repo_path: &Path, issue_key: &str, home: &Path) -> Result<PathBuf, String> {
    assert_issue_key(issue_key)?;
    Ok(repo_worktrees_root(repo_path, home).join(issue_key))
}

pub const FOR_EACH_REF_FORMAT: &str = "%(refname)%09%(objectname)%09%(upstream:short)%09%(upstream:track)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    pub ahead: u64,
    pub behind: u64,
    pub gone: bool,
}

/// "[ahead 2, behind 1]" / "[gone]" / "" become counts.
pub fn parse_track(track: &str) -> Track {
    let n = |re: &Regex| re.captures(track).and_then(|m| m[1].parse().ok()).unwrap_or(0);
    Track { ahead: n(&AHEAD), behind: n(&BEHIND), gone: track.contains("gone") }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub local: bool,
    pub remote: bool,
    pub sha: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub gone: bool,
}

impl BranchInfo {
    pub fn to_json(&self) -> Value {
        json!({ "name": self.name, "local": self.local, "remote": self.remote, "sha": self.sha, "upstream": self.upstream, "ahead": self.ahead, "behind": self.behind, "gone": self.gone })
    }
}

/// `git for-each-ref --format=FOR_EACH_REF_FORMAT refs/heads refs/remotes/origin`
/// output becomes the branches whose name has the key, local and origin merged.
pub fn parse_for_each_ref(stdout: &str, issue_key: &str) -> Result<Vec<BranchInfo>, String> {
    let re = key_pattern(issue_key)?;
    let mut by_name: Vec<BranchInfo> = vec![];
    for line in stdout.split('\n') {
        if js_trim(line).is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let (reference, sha, upstream, track) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let (name, remote) = if let Some(n) = reference.strip_prefix("refs/heads/") {
            (n, false)
        } else if let Some(n) = reference.strip_prefix("refs/remotes/origin/") {
            (n, true)
        } else {
            continue;
        };
        if name == "HEAD" || !re.is_match(name) {
            continue;
        }
        let idx = by_name.iter().position(|b| b.name == name).unwrap_or_else(|| {
            by_name.push(BranchInfo { name: name.into(), local: false, remote: false, sha: None, upstream: None, ahead: 0, behind: 0, gone: false });
            by_name.len() - 1
        });
        let entry = &mut by_name[idx];
        let sha = (!sha.is_empty()).then(|| sha.to_string());
        if remote {
            entry.remote = true;
            if entry.sha.is_none() {
                entry.sha = sha;
            }
        } else {
            entry.local = true;
            entry.sha = sha;
            entry.upstream = (!upstream.is_empty()).then(|| upstream.to_string());
            let t = parse_track(track);
            (entry.ahead, entry.behind, entry.gone) = (t.ahead, t.behind, t.gone);
        }
    }
    by_name.sort_by(|a, b| locale_compare(&a.name, &b.name));
    Ok(by_name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub dir: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
}

/// `git worktree list --porcelain` becomes the worktrees.
pub fn parse_worktree_porcelain(stdout: &str) -> Vec<WorktreeEntry> {
    let mut out: Vec<WorktreeEntry> = vec![];
    for line in stdout.split('\n') {
        if let Some(dir) = line.strip_prefix("worktree ") {
            out.push(WorktreeEntry { dir: dir.to_string(), head: None, branch: None, detached: false });
        } else if let Some(cur) = out.last_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                cur.head = Some(h.to_string());
            } else if let Some(b) = line.strip_prefix("branch ") {
                cur.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if line == "detached" {
                cur.detached = true;
            }
        }
    }
    out
}

/// Changed paths in `git status --porcelain` output.
pub fn count_changed(stdout: &str) -> usize {
    stdout.split('\n').filter(|l| !js_trim(l).is_empty()).count()
}

const MAX_WORKTREES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketWorktreeInfo {
    pub dir: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub exists: bool,
    pub dirty: bool,
    pub changed: usize,
    pub ahead: u64,
    pub behind: u64,
    pub is_ticket_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketScan {
    pub branches: Vec<BranchInfo>,
    pub worktrees: Vec<TicketWorktreeInfo>,
}

/// What one clone has for this ticket: worktrees whose branch or folder name
/// has the key, each with `dirty`, `changed` and, when its branch is known,
/// `ahead`/`behind`. The clone's own main checkout is never listed.
pub async fn scan_repo_for_ticket(repo_path: &Path, issue_key: &str, home: &Path, git: &impl Git) -> Result<TicketScan, String> {
    let re = key_pattern(issue_key)?;
    let format = format!("--format={FOR_EACH_REF_FORMAT}");
    let refs = git.run(&["for-each-ref", &format, "refs/heads", "refs/remotes/origin"], repo_path, GitOpts::default()).await?;
    let branches = parse_for_each_ref(&refs.stdout, issue_key)?;
    let list = git.run(&["worktree", "list", "--porcelain"], repo_path, GitOpts::default()).await?;
    let main = node_path::resolve(&repo_path.to_string_lossy());
    let ticket_dir = node_path::resolve(&ticket_worktree_path(repo_path, issue_key, home)?.to_string_lossy());
    let mut worktrees = vec![];
    for wt in parse_worktree_porcelain(&list.stdout) {
        if node_path::resolve(&wt.dir) == main {
            continue;
        }
        let by_branch = wt.branch.as_deref().is_some_and(|b| re.is_match(b));
        if !by_branch && !re.is_match(&node_path::basename(&wt.dir)) {
            continue;
        }
        if worktrees.len() >= MAX_WORKTREES {
            break;
        }
        let exists = Path::new(&wt.dir).exists();
        let mut changed = 0;
        if exists {
            let st = git.run(&["status", "--porcelain"], Path::new(&wt.dir), GitOpts::allow_failure()).await?;
            changed = if st.code == 0 { count_changed(&st.stdout) } else { 0 };
        }
        let b = branches.iter().find(|x| Some(&x.name) == wt.branch.as_ref());
        worktrees.push(TicketWorktreeInfo {
            is_ticket_worktree: node_path::resolve(&wt.dir) == ticket_dir,
            dir: wt.dir,
            branch: wt.branch,
            head: wt.head,
            exists,
            dirty: changed > 0,
            changed,
            ahead: b.map_or(0, |b| b.ahead),
            behind: b.map_or(0, |b| b.behind),
        });
    }
    Ok(TicketScan { branches, worktrees })
}

/// origin/HEAD's branch if the clone knows it, else master, else main.
pub async fn default_branch_from_git(repo_path: &Path, git: &impl Git) -> Result<String, String> {
    let sym = git.run(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"], repo_path, GitOpts::allow_failure()).await?;
    let name = js_trim(&sym.stdout);
    if sym.code == 0 && !name.is_empty() {
        return Ok(name.strip_prefix("refs/remotes/origin/").unwrap_or(name).to_string());
    }
    for candidate in ["master", "main"] {
        let r = git.run(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{candidate}")], repo_path, GitOpts::allow_failure()).await?;
        if r.code == 0 {
            return Ok(candidate.to_string());
        }
    }
    Err(format!("Couldn't tell the default branch of {} (no origin/HEAD, origin/master or origin/main).", repo_path.display()))
}

async fn ref_exists(repo_path: &Path, reference: &str, git: &impl Git) -> Result<bool, String> {
    Ok(git.run(&["rev-parse", "--verify", "--quiet", reference], repo_path, GitOpts::allow_failure()).await?.code == 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsuredWorktree {
    pub dir: PathBuf,
    pub branch: String,
    pub created: bool,
}

fn real_or_resolved(p: &str) -> String {
    fs::canonicalize(p).ok().and_then(|c| c.to_str().map(str::to_string)).unwrap_or_else(|| node_path::resolve(p))
}

/// Makes sure `<stateDir>/<repo>.worktrees/<KEY>` exists as a worktree of
/// `repo_path`. Idempotent: an existing worktree there is reused on whatever
/// branch it is on. Otherwise it fetches `origin/<base>` and checks out
/// `branch`: the existing local branch, the origin branch of that name, or a
/// new branch from `origin/<base>` (`--no-track`, so the upstream becomes
/// origin/<branch> on the first push). A folder at that path that isn't a
/// worktree of this repo is never touched: that is an error.
pub async fn ensure_ticket_worktree(repo_path: &Path, issue_key: &str, branch: &str, base: &str, home: &Path, git: &impl Git) -> Result<EnsuredWorktree, String> {
    let dir = ticket_worktree_path(repo_path, issue_key, home)?;
    let dir_str = dir.to_string_lossy().into_owned();
    let list = git.run(&["worktree", "list", "--porcelain"], repo_path, GitOpts::default()).await?;
    let target = if dir.exists() { real_or_resolved(&dir_str) } else { node_path::resolve(&dir_str) };
    if let Some(existing) = parse_worktree_porcelain(&list.stdout).into_iter().find(|wt| real_or_resolved(&wt.dir) == target) {
        return Ok(EnsuredWorktree { dir, branch: existing.branch.unwrap_or_else(|| branch.to_string()), created: false });
    }
    if dir.exists() {
        return Err(format!("{dir_str} already exists but isn't a worktree of {}. Move it aside and try again.", repo_path.display()));
    }
    if !is_safe_branch_name(branch) {
        return Err(format!("Not a safe branch name: {}", slice_utf16(branch, 80)));
    }
    if !is_safe_branch_name(base) {
        return Err(format!("Not a safe base branch: {}", slice_utf16(base, 80)));
    }
    git.run(&["check-ref-format", "--branch", branch], repo_path, GitOpts::default()).await?;
    git.run(&["fetch", "origin", &format!("refs/heads/{base}:refs/remotes/origin/{base}")], repo_path, GitOpts::timeout(FETCH_TIMEOUT)).await?;
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if ref_exists(repo_path, &format!("refs/heads/{branch}"), git).await? {
        git.run(&["worktree", "add", &dir_str, branch], repo_path, GitOpts::default()).await?;
    } else if ref_exists(repo_path, &format!("refs/remotes/origin/{branch}"), git).await? {
        git.run(&["worktree", "add", "--track", "-b", branch, &dir_str, &format!("origin/{branch}")], repo_path, GitOpts::default()).await?;
    } else {
        git.run(&["worktree", "add", "--no-track", "-b", branch, &dir_str, &format!("origin/{base}")], repo_path, GitOpts::default()).await?;
    }
    link_node_modules(repo_path, &dir);
    Ok(EnsuredWorktree { dir, branch: branch.to_string(), created: true })
}

fn assert_base(base: &str) -> Result<(), String> {
    if is_safe_branch_name(base) { Ok(()) } else { Err("Not a safe base branch.".into()) }
}

/// Refresh `refs/remotes/origin/<base>` so the ahead count, log and diff below
/// aren't measured against a stale ref (a forced refspec, so a rewritten base
/// doesn't fail the fetch). Errors if origin can't be reached.
pub async fn fetch_base(dir: &Path, base: &str, git: &impl Git) -> Result<(), String> {
    assert_base(base)?;
    git.run(&["fetch", "origin", &format!("+refs/heads/{base}:refs/remotes/origin/{base}")], dir, GitOpts::timeout(FETCH_TIMEOUT)).await.map(|_| ())
}

/// The branch checked out in `dir` ("HEAD" when detached).
pub async fn current_branch(dir: &Path, git: &impl Git) -> Result<String, String> {
    Ok(js_trim(&git.run(&["rev-parse", "--abbrev-ref", "HEAD"], dir, GitOpts::default()).await?.stdout).to_string())
}

/// Commits on HEAD that origin/<base> doesn't have.
pub async fn commits_ahead(dir: &Path, base: &str, git: &impl Git) -> Result<u64, String> {
    assert_base(base)?;
    let r = git.run(&["rev-list", "--count", &format!("refs/remotes/origin/{base}..HEAD")], dir, GitOpts::default()).await?;
    Ok(js_trim(&r.stdout).parse().unwrap_or(0))
}

/// Subjects and bodies of those commits (newest first), capped.
pub async fn commit_log(dir: &Path, base: &str, max: usize, max_chars: usize, git: &impl Git) -> Result<String, String> {
    assert_base(base)?;
    let r = git.run(&["log", "--no-merges", &format!("--max-count={max}"), "--format=- %s%n%b", &format!("refs/remotes/origin/{base}..HEAD")], dir, GitOpts::default()).await?;
    Ok(slice_utf16(js_trim(&BLANK_LINES.replace_all(&r.stdout, "\n\n")), max_chars))
}

/// `git diff --stat` of the branch against the merge base with origin/<base>, capped.
pub async fn diff_stat(dir: &Path, base: &str, max_chars: usize, git: &impl Git) -> Result<String, String> {
    assert_base(base)?;
    let r = git.run(&["diff", "--stat", &format!("refs/remotes/origin/{base}...HEAD")], dir, GitOpts::default()).await?;
    Ok(slice_utf16(js_trim(&r.stdout), max_chars))
}

/// `git push -u origin <branch>` from the worktree (hooks run as usual).
pub async fn push_branch(dir: &Path, branch: &str, git: &impl Git) -> Result<(), String> {
    if !is_safe_branch_name(branch) {
        return Err("Not a safe branch name.".into());
    }
    git.run(&["push", "-u", "origin", branch], dir, GitOpts::timeout(FETCH_TIMEOUT)).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::{git as sh, git_fails, make_repos, out, FakeGit};
    use crate::git_cli::SystemGit;

    #[test]
    fn key_pattern_matches_the_key_at_a_boundary_case_insensitively_and_not_a_longer_number() {
        let re = key_pattern("PROJ-12").unwrap();
        for yes in ["bugfix/PROJ-12-login", "proj-12", "feature/x_PROJ-12", "PROJ-12", "é-PROJ-12"] {
            assert!(re.is_match(yes), "{yes}");
        }
        for no in ["bugfix/PROJ-123", "XPROJ-12", "PROJ-1", "", "PROJ-12x1PROJ-123"] {
            assert!(!re.is_match(no) || no == "PROJ-12x1PROJ-123", "{no}");
        }
        assert!(re.is_match("PROJ-12x1PROJ-123"), "the first occurrence is a match even if a later one is not");
        assert!(re.is_match("PROJ-123 and PROJ-12"), "a later occurrence can match");
        assert!(key_pattern("proj-12").is_err());
        assert!(key_pattern("PROJ-12)|(.*").is_err());
    }

    #[test]
    fn ticket_worktree_path_is_state_dir_repo_worktrees_key() {
        let p = ticket_worktree_path(Path::new("/Users/x/gitviews/sample-app"), "PROJ-7", Path::new("/h")).unwrap();
        assert_eq!(p, Path::new("/h").join(format!(".{}", crate::app_identity::app_slug())).join("sample-app.worktrees").join("PROJ-7"));
        assert!(ticket_worktree_path(Path::new("/Users/x/sample-app"), "../../etc", Path::new("/h")).is_err());
    }

    #[test]
    fn parse_for_each_ref_merges_local_and_origin_branches_that_carry_the_key() {
        let out = [
            "refs/heads/bugfix/PROJ-7-login\taaa\torigin/bugfix/PROJ-7-login\t[ahead 2, behind 1]",
            "refs/heads/master\tbbb\torigin/master\t",
            "refs/remotes/origin/bugfix/PROJ-7-login\tccc\t\t",
            "refs/remotes/origin/PROJ-7-spike\tddd\t\t",
            "refs/remotes/origin/HEAD\teee\t\t",
            "refs/heads/PROJ-70\tfff\t\t",
        ]
        .join("\n");
        let got: Vec<Value> = parse_for_each_ref(&out, "PROJ-7").unwrap().iter().map(BranchInfo::to_json).collect();
        assert_eq!(
            got,
            [
                json!({ "name": "bugfix/PROJ-7-login", "local": true, "remote": true, "sha": "aaa", "upstream": "origin/bugfix/PROJ-7-login", "ahead": 2, "behind": 1, "gone": false }),
                json!({ "name": "PROJ-7-spike", "local": false, "remote": true, "sha": "ddd", "upstream": null, "ahead": 0, "behind": 0, "gone": false }),
            ]
        );
        assert_eq!(parse_track("[gone]"), Track { ahead: 0, behind: 0, gone: true });
        assert_eq!(parse_track(""), Track { ahead: 0, behind: 0, gone: false });
        // A remote line first, then the local one: the local sha wins.
        let flipped = parse_for_each_ref("refs/remotes/origin/PROJ-7-a\tr\t\t\nrefs/heads/PROJ-7-a\tl\t\t", "PROJ-7").unwrap();
        assert_eq!(flipped[0].sha.as_deref(), Some("l"));
    }

    #[test]
    fn parse_worktree_porcelain_reads_dirs_heads_branches_and_detached_entries() {
        let out = "worktree /r/sample-app\nHEAD 111\nbranch refs/heads/master\n\nworktree /r/sample-app.worktrees/pr-review\nHEAD 222\ndetached\n\n";
        assert_eq!(
            parse_worktree_porcelain(out),
            [
                WorktreeEntry { dir: "/r/sample-app".into(), head: Some("111".into()), branch: Some("master".into()), detached: false },
                WorktreeEntry { dir: "/r/sample-app.worktrees/pr-review".into(), head: Some("222".into()), branch: None, detached: true },
            ]
        );
        assert!(parse_worktree_porcelain("HEAD 1\nbranch x").is_empty(), "lines before the first worktree are ignored");
        assert_eq!(count_changed(" M a\n?? b\n\n"), 2);
    }

    #[tokio::test]
    async fn ensure_ticket_worktree_creates_the_worktree_on_a_new_branch_from_origin_base_then_reuses_it() {
        let r = make_repos();
        let first = ensure_ticket_worktree(&r.clone, "PROJ-7", "bugfix/PROJ-7-login", "master", &r.home, &SystemGit).await.unwrap();
        assert!(first.created);
        assert_eq!(first.dir, r.wt_root.join("PROJ-7"));
        assert_eq!(sh(&first.dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "bugfix/PROJ-7-login");
        // --no-track: no upstream until the first push.
        assert!(git_fails(&first.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]));
        let again = ensure_ticket_worktree(&r.clone, "PROJ-7", "other-name", "master", &r.home, &SystemGit).await.unwrap();
        assert_eq!(again, EnsuredWorktree { dir: first.dir.clone(), branch: "bugfix/PROJ-7-login".into(), created: false });
    }

    #[tokio::test]
    async fn ensure_ticket_worktree_checks_out_an_existing_origin_branch_and_refuses_a_stray_folder_or_a_bad_name() {
        let r = make_repos();
        sh(&r.clone, &["push", "-q", "origin", "master:refs/heads/bugfix/PROJ-8-x"]);
        sh(&r.clone, &["fetch", "-q", "origin"]);
        let wt = ensure_ticket_worktree(&r.clone, "PROJ-8", "bugfix/PROJ-8-x", "master", &r.home, &SystemGit).await.unwrap();
        assert_eq!(sh(&wt.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/bugfix/PROJ-8-x");

        fs::create_dir_all(r.wt_root.join("PROJ-9")).unwrap();
        fs::write(r.wt_root.join("PROJ-9").join("keep.txt"), "mine").unwrap();
        let e = ensure_ticket_worktree(&r.clone, "PROJ-9", "bugfix/PROJ-9", "master", &r.home, &SystemGit).await.unwrap_err();
        assert!(e.contains("isn't a worktree"), "{e}");
        assert_eq!(fs::read_to_string(r.wt_root.join("PROJ-9").join("keep.txt")).unwrap(), "mine");

        assert!(ensure_ticket_worktree(&r.clone, "PROJ-10", "-rf", "master", &r.home, &SystemGit).await.unwrap_err().contains("safe branch"));
        assert!(ensure_ticket_worktree(&r.clone, "PROJ-10", "ok", "a..b", &r.home, &SystemGit).await.unwrap_err().contains("safe base"));
    }

    #[tokio::test]
    async fn scan_repo_for_ticket_lists_the_tickets_branches_and_worktrees_with_dirty_and_ahead_counts() {
        let r = make_repos();
        let wt = ensure_ticket_worktree(&r.clone, "PROJ-7", "bugfix/PROJ-7-login", "master", &r.home, &SystemGit).await.unwrap();
        fs::write(wt.dir.join("a.txt"), "a\n").unwrap();
        sh(&wt.dir, &["add", "a.txt"]);
        sh(&wt.dir, &["commit", "-q", "-m", "PROJ-7: add a"]);
        push_branch(&wt.dir, "bugfix/PROJ-7-login", &SystemGit).await.unwrap();
        fs::write(wt.dir.join("b.txt"), "b\n").unwrap();
        sh(&wt.dir, &["add", "b.txt"]);
        sh(&wt.dir, &["commit", "-q", "-m", "PROJ-7: add b"]);
        fs::write(wt.dir.join("c.txt"), "dirty\n").unwrap();

        let scan = scan_repo_for_ticket(&r.clone, "PROJ-7", &r.home, &SystemGit).await.unwrap();
        assert_eq!(scan.branches.len(), 1);
        let b = &scan.branches[0];
        assert_eq!((b.name.as_str(), b.local, b.remote, b.ahead), ("bugfix/PROJ-7-login", true, true, 1));
        assert_eq!(scan.worktrees.len(), 1);
        let w = &scan.worktrees[0];
        assert_eq!((w.dir.as_str(), w.dirty, w.changed, w.is_ticket_worktree), (wt.dir.to_str().unwrap(), true, 1, true));

        assert_eq!(commits_ahead(&wt.dir, "master", &SystemGit).await.unwrap(), 2);
        let log = commit_log(&wt.dir, "master", 30, 8000, &SystemGit).await.unwrap();
        assert!(log.find("- PROJ-7: add b").unwrap() < log.find("- PROJ-7: add a").unwrap(), "{log}");
        let stat = diff_stat(&wt.dir, "master", 4000, &SystemGit).await.unwrap();
        assert!(stat.find("a.txt").unwrap() < stat.find("b.txt").unwrap(), "{stat}");
        assert_eq!(current_branch(&wt.dir, &SystemGit).await.unwrap(), "bugfix/PROJ-7-login");

        let none = scan_repo_for_ticket(&r.clone, "PROJ-99", &r.home, &SystemGit).await.unwrap();
        assert_eq!(none, TicketScan { branches: vec![], worktrees: vec![] });
    }

    #[tokio::test]
    async fn default_branch_from_git_uses_origin_head_then_master_or_main() {
        let r = make_repos();
        assert_eq!(default_branch_from_git(&r.clone, &SystemGit).await.unwrap(), "master");
        sh(&r.clone, &["remote", "set-head", "origin", "--delete"]);
        assert_eq!(default_branch_from_git(&r.clone, &SystemGit).await.unwrap(), "master");
        sh(&r.clone, &["update-ref", "-d", "refs/remotes/origin/master"]);
        assert!(default_branch_from_git(&r.clone, &SystemGit).await.unwrap_err().contains("Couldn't tell the default branch"));
    }

    #[tokio::test]
    async fn fetch_base_brings_origin_base_up_to_date_so_the_ahead_count_isnt_measured_against_a_stale_ref() {
        let r = make_repos();
        let wt = ensure_ticket_worktree(&r.clone, "PROJ-5", "bugfix/PROJ-5-x", "master", &r.home, &SystemGit).await.unwrap();
        // Someone else lands a commit on origin/master; our origin/master ref is now stale.
        let other = r.root.join("other");
        sh(&r.root, &["clone", "-q", r.origin.to_str().unwrap(), "other"]);
        sh(&other, &["config", "user.email", "t@example.com"]);
        sh(&other, &["config", "user.name", "T"]);
        fs::write(other.join("n.txt"), "n\n").unwrap();
        sh(&other, &["add", "n.txt"]);
        sh(&other, &["commit", "-q", "-m", "new on master"]);
        sh(&other, &["push", "-q", "origin", "master"]);
        assert_eq!(sh(&wt.dir, &["rev-list", "--count", "HEAD..refs/remotes/origin/master"]), "0");
        fetch_base(&wt.dir, "master", &SystemGit).await.unwrap();
        assert_eq!(sh(&wt.dir, &["rev-list", "--count", "HEAD..refs/remotes/origin/master"]), "1");
        assert!(fetch_base(&wt.dir, "bad;name", &SystemGit).await.unwrap_err().contains("safe base"));
    }

    #[tokio::test]
    async fn fetch_base_uses_a_forced_refspec_so_a_rewritten_base_doesnt_fail_the_fetch() {
        let fake = FakeGit::new(|_, _| out("", 0));
        fetch_base(Path::new("/x"), "master", &fake).await.unwrap();
        assert_eq!(fake.args()[0], ["fetch", "origin", "+refs/heads/master:refs/remotes/origin/master"]);
    }

    #[tokio::test]
    async fn the_read_helpers_refuse_an_unsafe_base_or_branch_before_running_git() {
        let fake = FakeGit::new(|_, _| out("", 0));
        let d = Path::new("/x");
        assert!(commits_ahead(d, "a b", &fake).await.is_err());
        assert!(commit_log(d, "-x", 1, 1, &fake).await.is_err());
        assert!(diff_stat(d, "a..b", 1, &fake).await.is_err());
        assert!(push_branch(d, "--evil", &fake).await.is_err());
        assert!(fake.args().is_empty());
        let log = FakeGit::new(|_, _| out("- a\n\n\n\n- b\n  \n", 0));
        assert_eq!(commit_log(d, "main", 30, 8000, &log).await.unwrap(), "- a\n\n- b");
        assert_eq!(commit_log(d, "main", 30, 3, &log).await.unwrap(), "- a");
    }
}
