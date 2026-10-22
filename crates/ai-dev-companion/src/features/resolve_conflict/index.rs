//! Resolve Conflict (port of `features/resolve-conflict/index.ts`): merges the target branch into
//! the PR's branch in an isolated worktree (never a rebase, so the push is a plain, non-force
//! `git push`), lets a headless Claude resolve the conflicts, and shows the result for review.
//! Approve pushes exactly what the panel last showed: the worktree's `git write-tree` fingerprint
//! must still match. The git metadata of the worktree is baselined before Claude runs and compared
//! after it; a change means no git runs there again and only Discard remains.
//!
//! Not ported here: the `start_resolve_conflict` MCP tool (it belongs with the `/mcp` route).

use super::plan::{classify_conflicted_file, intersect_nul_separated, is_nothing_to_push, should_skip_commit, DiffAgainst};
use super::prompt::build_resolve_conflict_prompt;
use crate::claude::{ClaudeRunOptions, ClaudeRunner};
use crate::claude_args::Policy;
use crate::exec::git as exec_git;
use crate::git_cli::SystemGit;
use crate::integrity_baselines::{BaselineStore, Check};
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::main_clone_choice::ask_main_clone_choice;
use crate::prereqs::GitRemoteRules;
use crate::process_group::is_process_group_alive_value;
use crate::repo_choice::RepoChoices;
use crate::reviewed_push::{assert_tree_matches_reviewed, stage_all, write_tree, JobLocks};
use crate::reviewed_push_policy::can_reject;
use crate::work_unit_record::{pr_key_for, record_work_unit, record_work_unit_session, Session, Unit};
use crate::work_units::WorkUnits;
use crate::worktree::{AcquireOptions, MainCloneChoice, MainCloneInfo, WorktreeError, WorktreeHandle, Worktrees};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const COMPROMISED_MESSAGE: &str = "Claude modified this worktree's git metadata, so it was not trusted. Discard the job.";
const MISSING_BASELINE_MESSAGE: &str =
    "This job's worktree-integrity record is missing (it may have been lost in a service restart), so its worktree isn't trusted. Discard the job and start again.";
const CLAUDE_STILL_RUNNING_MESSAGE: &str =
    "Claude from before the service restarted may still be running in this worktree. Wait a minute and click Refresh diff again, or Discard the job.";

/// "PROJECT", "repo" -> the local clone, or why there is none.
pub type RepoResolver = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

pub struct ResolveConflictDeps {
    pub store: Arc<JobStore>,
    pub claude: Arc<dyn ClaudeRunner>,
    pub repo_choices: Arc<RepoChoices>,
    pub baselines: Arc<BaselineStore>,
    pub work_units: Arc<WorkUnits>,
    pub repo_path: RepoResolver,
    pub home: PathBuf,
    pub rules: GitRemoteRules,
}

struct Running {
    cancel: CancellationToken,
    done: Option<JoinHandle<()>>,
}

pub struct ResolveConflict {
    d: ResolveConflictDeps,
    locks: JobLocks,
    running: Arc<Mutex<HashMap<String, Running>>>,
}

pub fn create_resolve_conflict_feature(d: ResolveConflictDeps) -> Arc<ResolveConflict> {
    Arc::new(ResolveConflict { d, locks: JobLocks::new(), running: Arc::new(Mutex::new(HashMap::new())) })
}

fn patch(store: &JobStore, id: &str, v: Value) -> Value {
    store.update(id, v.as_object().cloned().unwrap_or_default()).unwrap_or(Value::Null)
}

fn data_patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.patch_data(id, v.as_object().cloned().unwrap_or_default());
}

fn other(e: impl ToString) -> FeatureError {
    FeatureError::Other(e.to_string())
}

fn untrusted_message(reason: Option<&str>) -> &'static str {
    if reason == Some("missing") { MISSING_BASELINE_MESSAGE } else { COMPROMISED_MESSAGE }
}

fn check_reason(c: Check) -> &'static str {
    if c == Check::Missing { "missing" } else { "changed" }
}

/// Whether `text` has a line starting with a conflict marker (`<<<<<<<`, `=======` not followed by
/// another `=`, or `>>>>>>>`): `/^(<{7}|={7}|>{7})(?!=)/m`.
pub fn has_conflict_marker(text: &str) -> bool {
    text.split(['\n', '\r', '\u{2028}', '\u{2029}']).any(|line| {
        let head: Vec<char> = line.chars().take(8).collect();
        head.len() >= 7 && matches!(head[0], '<' | '=' | '>') && head[..7].iter().all(|c| *c == head[0]) && head.get(7) != Some(&'=')
    })
}

/// Files (relative to the worktree) that still contain conflict markers.
fn files_with_conflict_markers(dir: &Path, files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|f| {
            let full = dir.join(f);
            full.exists() && std::fs::read(&full).map(|b| has_conflict_marker(&String::from_utf8_lossy(&b))).unwrap_or(false)
        })
        .cloned()
        .collect()
}

async fn g(args: &[&str], cwd: &Path, allow_failure: bool, cancel: Option<&CancellationToken>) -> Result<crate::exec::ExecResult, String> {
    exec_git(args, cwd, allow_failure, cancel.cloned()).await.map_err(|e| e.to_string())
}

/// Files whose content in `tree` differs from BOTH merge parents (git's own "combined diff"
/// notion): the intersection of two tree-vs-tree diffs.
async fn review_files(dir: &Path, tree: &str, pre_merge_sha: &str, dest_sha: &str) -> Result<Vec<String>, String> {
    let from_source = g(&["diff", "--no-renames", "--name-only", "-z", pre_merge_sha, tree], dir, false, None).await?.stdout;
    let from_dest = g(&["diff", "--no-renames", "--name-only", "-z", dest_sha, tree], dir, false, None).await?.stdout;
    Ok(intersect_nul_separated(&from_source, &from_dest))
}

enum Render {
    Ok { tree: String, files: Vec<String>, file_diffs: Vec<Value> },
    Markers(Vec<String>),
}

/// The panel's per-file diffs: a conflicted file resolved wholesale to one side is diffed against
/// the side that shows what was dropped, with a note.
async fn panel_file_diffs(dir: &Path, pre: &str, dest: &str, panel: &[String], conflicted: &[String]) -> Result<Vec<Value>, String> {
    let conflicted: HashSet<&String> = conflicted.iter().collect();
    let mut out = vec![];
    for file in panel {
        let pre_diff = g(&["diff", pre, "--", file], dir, false, None).await?.stdout;
        if !conflicted.contains(file) {
            out.push(json!({ "path": file, "diff": pre_diff }));
            continue;
        }
        let dest_diff = g(&["diff", dest, "--", file], dir, false, None).await?.stdout;
        let decision = classify_conflicted_file(file, pre_diff.is_empty(), dest_diff.is_empty());
        let mut entry = json!({ "path": file, "diff": if decision.diff_against == DiffAgainst::Dest { dest_diff } else { pre_diff } });
        if let Some(note) = decision.note {
            entry["note"] = json!(note);
        }
        out.push(entry);
    }
    Ok(out)
}

/// The one recipe the clean-merge path, the conflict-resolved path and refresh-diff share: stage
/// everything, fingerprint the tree, work out the review set, marker-check the PANEL set (review
/// files plus the files that were conflicted), and render the diffs.
async fn render_review(dir: &Path, pre: &str, dest: &str, conflicted: &[String]) -> Result<Render, String> {
    stage_all(dir).await.map_err(|e| e.to_string())?;
    let tree = write_tree(dir).await.map_err(|e| e.to_string())?;
    let combined = review_files(dir, &tree, pre, dest).await?;
    let mut seen = HashSet::new();
    let panel: Vec<String> = combined.into_iter().chain(conflicted.iter().cloned()).filter(|f| seen.insert(f.clone())).collect();
    let markers = files_with_conflict_markers(dir, &panel);
    if !markers.is_empty() {
        return Ok(Render::Markers(markers));
    }
    let file_diffs = panel_file_diffs(dir, pre, dest, &panel, conflicted).await?;
    Ok(Render::Ok { tree, files: panel, file_diffs })
}

fn handle_of(v: &Value) -> Option<WorktreeHandle> {
    Some(WorktreeHandle {
        id: v["id"].as_str()?.into(),
        dir: v["dir"].as_str()?.into(),
        repo_path: v["repoPath"].as_str()?.into(),
        adopted: v["adopted"] == json!(true),
        branch: v["branch"].as_str().map(String::from),
    })
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

enum RunEnd {
    Done,
    Cancelled,
}

impl ResolveConflict {
    fn id_of(job: &Value) -> String {
        job["id"].as_str().unwrap_or("").to_string()
    }

    /// Refuses while a claude that outlived a restart may still be writing the worktree.
    fn assert_no_live_claude(&self, job_id: &str, data: &Value) -> FeatureResult<()> {
        if data["claudePid"].is_null() {
            return Ok(());
        }
        if is_process_group_alive_value(Some(&data["claudePid"])) {
            return Err(FeatureError::JobStatus(CLAUDE_STILL_RUNNING_MESSAGE.into()));
        }
        data_patch(&self.d.store, job_id, json!({ "claudePid": null }));
        Ok(())
    }

    /// A mismatch or missing baseline marks the job compromised and fails it before any git runs.
    fn assert_git_metadata_intact(&self, job_id: &str, dir: &str) -> FeatureResult<()> {
        let state = self.d.baselines.check(job_id, Path::new(dir));
        if state == Check::Intact {
            return Ok(());
        }
        let reason = check_reason(state);
        data_patch(&self.d.store, job_id, json!({ "gitMetadataCompromised": true, "gitMetadataUntrusted": reason }));
        let msg = untrusted_message(Some(reason));
        patch(&self.d.store, job_id, json!({ "status": "failed", "error": msg }));
        Err(FeatureError::JobStatus(msg.into()))
    }

    fn worktrees(&self) -> Worktrees<'_, SystemGit> {
        static GIT: SystemGit = SystemGit;
        Worktrees { git: &GIT, home: &self.d.home }
    }

    /// Merge --abort and remove the job's worktree, if it got that far. The worktree is detached
    /// and never pushed, so this undoes everything.
    async fn discard_worktree(&self, job: &Value) -> Result<(), String> {
        let data = &job["data"];
        let Some(handle) = handle_of(&data["worktree"]) else { return Ok(()) };
        let id = Self::id_of(job);
        let compromised = data["gitMetadataCompromised"] == json!(true);
        let state = self.d.baselines.check(&id, Path::new(&handle.dir));
        if compromised || state != Check::Intact {
            let reason = data["gitMetadataUntrusted"].as_str().map(String::from).unwrap_or_else(|| if compromised { "changed".into() } else { check_reason(state).into() });
            data_patch(&self.d.store, &id, json!({ "gitMetadataCompromised": true, "gitMetadataUntrusted": reason }));
            // No git in or against this worktree at all: its metadata is not trusted. Plain fs
            // removal only (an adopted worktree is the user's own and is left as it is).
            if !handle.adopted {
                let _ = std::fs::remove_dir_all(&handle.dir);
            }
            return Ok(());
        }
        if handle.adopted {
            return self.worktrees().restore_adopted_worktree(&handle, data["preMergeSha"].as_str().unwrap_or("")).await;
        }
        let _ = g(&["merge", "--abort"], Path::new(&handle.dir), true, None).await;
        self.worktrees().remove_worktree(&handle).await
    }

    async fn run_job(&self, job_id: &str, repo: &str, payload: &Value, cancel: &CancellationToken) -> Result<RunEnd, String> {
        let store = &self.d.store;
        let cancelled = || cancel.is_cancelled();
        let source = payload["sourceBranch"].as_str().unwrap_or("").to_string();
        let dest_branch = payload["destBranch"].as_str().unwrap_or("").to_string();

        patch(store, job_id, json!({ "progress": { "stepId": "fetch", "label": "Fetching latest changes and preparing an isolated workspace\u{2026}" } }));
        // An existing clean checkout of the branch is reused; only when there is none is a
        // disposable worktree made.
        let (choices, ask_store, ask_cancel, ask_id) = (self.d.repo_choices.clone(), store.clone(), cancel.clone(), job_id.to_string());
        let ask = move |info: MainCloneInfo| -> BoxFuture<Result<MainCloneChoice, WorktreeError>> {
            let (choices, store, cancel, id) = (choices.clone(), ask_store.clone(), ask_cancel.clone(), ask_id.clone());
            Box::pin(async move {
                patch(&store, &id, json!({ "progress": { "stepId": "fetch", "label": "Waiting for you to choose where to work\u{2026}" } }));
                ask_main_clone_choice(&choices, &id, Some(&cancel), &info).await
            })
        };
        let ask_ref: &(dyn Fn(MainCloneInfo) -> BoxFuture<Result<MainCloneChoice, WorktreeError>> + Send + Sync) = &ask;
        let (handle, reused) = self.worktrees().acquire_worktree(Path::new(repo), &source, job_id, &AcquireOptions { on_main_clone: Some(ask_ref) }).await.map_err(|e| e.to_string())?;
        // Recorded before anything else can fail, so cancel/reject can find it.
        data_patch(store, job_id, json!({ "worktree": serde_json::to_value(&handle).expect("a handle serializes") }));
        let pr_id = payload["prId"].as_u64().or_else(|| payload["prId"].as_str().and_then(|s| s.parse().ok()));
        let pr_key = pr_id.map(|id| pr_key_for(self.d.rules, payload["project"].as_str().unwrap_or(""), payload["repo"].as_str().unwrap_or(""), id));
        // A job's own worktree is gone after the job; only a reused one is worth recording.
        let unit = Unit { repo_path: repo, branch: &source, dir: reused.then_some(handle.dir.as_str()), issue_key: None, pr_key: pr_key.as_deref() };
        record_work_unit(&self.d.work_units, &unit);
        let label = if reused { format!("Using the existing checkout of {source} at {}\u{2026}", handle.dir) } else { "Fetching latest changes and preparing an isolated workspace\u{2026}".to_string() };
        patch(store, job_id, json!({ "progress": { "stepId": "fetch", "label": label } }));
        if cancelled() {
            return Ok(RunEnd::Cancelled);
        }
        let dir = Path::new(&handle.dir);
        // Where git keeps the worktree's index/HEAD (in the main repo): Claude's sandbox may write
        // it, and the baseline snapshots the private metadata that lives there.
        let git_dir = g(&["rev-parse", "--absolute-git-dir"], dir, false, None).await?.stdout.trim().to_string();
        // Snapshotted as soon as the worktree exists, before the merge and any Claude run.
        self.d.baselines.take(job_id, dir, Path::new(&git_dir))?;
        let pre = g(&["rev-parse", "HEAD"], dir, false, None).await?.stdout.trim().to_string();
        data_patch(store, job_id, json!({ "repoPath": repo, "sourceBranch": source, "preMergeSha": pre }));

        if cancelled() {
            return Ok(RunEnd::Cancelled);
        }
        patch(store, job_id, json!({ "progress": { "stepId": "merge", "label": format!("Merging origin/{dest_branch} in\u{2026}") } }));
        // The merge's other parent, recorded right before the merge attempt.
        let dest = g(&["rev-parse", &format!("origin/{dest_branch}")], dir, false, None).await?.stdout.trim().to_string();
        data_patch(store, job_id, json!({ "destSha": dest }));

        // A conflicting merge exits non-zero: the expected path. A failure that leaves no
        // conflicted files is not a conflict, and is checked below.
        let merge = match g(&["merge", "--no-edit", &format!("origin/{dest_branch}")], dir, true, Some(cancel)).await {
            Ok(m) => m,
            Err(_) if cancelled() => return Ok(RunEnd::Cancelled),
            Err(e) => return Err(e),
        };
        let conflicted: Vec<String> = g(&["diff", "--name-only", "--diff-filter=U"], dir, false, Some(cancel)).await?.stdout.split('\n').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        data_patch(store, job_id, json!({ "conflicted": conflicted }));

        if cancelled() {
            return Ok(RunEnd::Cancelled);
        }
        if conflicted.is_empty() && merge.code != 0 {
            // Not a normal conflict (unrelated histories, a bad ref): surface it.
            let detail = if !merge.stderr.trim().is_empty() {
                merge.stderr.trim().to_string()
            } else if !merge.stdout.trim().is_empty() {
                merge.stdout.trim().to_string()
            } else {
                format!("exit code {}", merge.code)
            };
            patch(store, job_id, json!({ "status": "failed", "error": format!("git merge failed (not with the usual conflict markers): {detail}") }));
            return Ok(RunEnd::Done);
        }

        if conflicted.is_empty() {
            // Merged cleanly: nothing for Claude to do; `git merge` already committed it.
            patch(store, job_id, json!({ "progress": { "stepId": "diff", "label": "Preparing diff for review\u{2026}" } }));
            let outcome = render_review(dir, &pre, &dest, &conflicted).await?;
            if cancelled() {
                return Ok(RunEnd::Cancelled);
            }
            match outcome {
                Render::Markers(m) => {
                    patch(store, job_id, json!({ "status": "failed", "error": format!("Conflict markers remain in: {}.", m.join(", ")) }));
                }
                Render::Ok { tree, files, file_diffs } => {
                    data_patch(store, job_id, json!({ "reviewedTree": tree }));
                    let summary = if files.is_empty() {
                        "Merged cleanly \u{2014} nothing needed resolving. Approving pushes the merge commit."
                    } else {
                        "No merge conflicts were found (the merge either succeeded cleanly or was already up to date). Review below and approve to push, or discard to throw it away."
                    };
                    patch(store, job_id, json!({ "status": "awaiting-approval", "result": { "summary": summary, "files": file_diffs } }));
                }
            }
            return Ok(RunEnd::Done);
        }

        let label = if conflicted.len() == 1 {
            format!("Resolving \"{}\" with Claude \u{2014} this can take a few minutes\u{2026}", conflicted[0])
        } else {
            format!("Resolving {} conflicted files with Claude \u{2014} this can take a few minutes\u{2026}", conflicted.len())
        };
        patch(store, job_id, json!({ "progress": { "stepId": "resolve", "label": label } }));
        // Our own session id, so claudeSession.id is always populated and a later phase can resume it.
        let session_id = crate::ids::uuid_v4();
        // The sandbox allows writes under cwd and `git_dir`; the files that can redirect git's own
        // next invocation are denied on top of that.
        let deny: Vec<String> = [dir.join(".git"), PathBuf::from(&git_dir).join("commondir"), PathBuf::from(&git_dir).join("gitdir"), PathBuf::from(&git_dir).join("config.worktree")].iter().map(|p| p.to_string_lossy().into_owned()).collect();
        // Edit/Write are confined to the real path of the worktree by the edit guard.
        let edit_root = std::fs::canonicalize(dir).map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        let mut opts = ClaudeRunOptions::new(&handle.dir, &build_resolve_conflict_prompt(&conflicted), Policy::WORKTREE_WRITE);
        opts.session_id = Some(session_id.clone());
        opts.sandbox_allow_write = vec![git_dir.clone()];
        opts.sandbox_deny_write = deny;
        opts.edit_root = Some(edit_root);
        opts.cancel = Some(cancel.clone());
        // claudePid is persisted with the job while claude runs and cleared however the run ends.
        let (spawn_store, spawn_id) = (store.clone(), job_id.to_string());
        opts.on_spawn = Some(Arc::new(move |pid| data_patch(&spawn_store, &spawn_id, json!({ "claudePid": pid }))));
        let (progress_store, progress_id) = (store.clone(), job_id.to_string());
        opts.on_progress = Some(Arc::new(move |label| progress_store.set_progress_if_running(&progress_id, "resolve", label)));
        let result = self.d.claude.run(opts).await;
        data_patch(store, job_id, json!({ "claudePid": null }));
        let result = match result {
            Ok(r) => r,
            Err(e) if e.is_aborted() || cancelled() => return Ok(RunEnd::Cancelled),
            Err(e) => return Err(e.to_string()),
        };

        // Before anything else touches this worktree with git: a changed (or missing) baseline
        // means how git behaves here is no longer trustworthy.
        let state = self.d.baselines.check(job_id, dir);
        if state != Check::Intact {
            let reason = check_reason(state);
            data_patch(store, job_id, json!({ "gitMetadataCompromised": true, "gitMetadataUntrusted": reason }));
            patch(store, job_id, json!({ "status": "failed", "error": untrusted_message(Some(reason)) }));
            return Ok(RunEnd::Done);
        }

        let claude_id = result.session_id.clone().filter(|s| !s.is_empty()).unwrap_or(session_id);
        data_patch(store, job_id, json!({ "claudeSession": { "id": claude_id, "cwd": handle.dir, "permissionMode": "default" } }));
        record_work_unit_session(&self.d.work_units, &unit, &Session { id: &claude_id, cwd: Some(&handle.dir), feature: Some("resolve-conflict") });

        if cancelled() {
            return Ok(RunEnd::Cancelled);
        }
        patch(store, job_id, json!({ "progress": { "stepId": "verify", "label": "Verifying the resolution\u{2026}" } }));
        patch(store, job_id, json!({ "progress": { "stepId": "diff", "label": "Preparing diff for review\u{2026}" } }));
        let outcome = render_review(dir, &pre, &dest, &conflicted).await?;
        if cancelled() {
            return Ok(RunEnd::Cancelled);
        }
        match outcome {
            Render::Markers(m) => {
                patch(store, job_id, json!({ "status": "failed", "error": format!("Claude did not fully resolve conflicts in: {}. The worktree at {} is left in place for manual inspection \u{2014} discard the job to clean it up.", m.join(", "), handle.dir) }));
            }
            Render::Ok { tree, file_diffs, .. } => {
                data_patch(store, job_id, json!({ "reviewedTree": tree }));
                patch(store, job_id, json!({ "status": "awaiting-approval", "result": { "summary": format!("Resolved {} conflicted file(s) against origin/{dest_branch}. Review the diffs below before approving.", conflicted.len()), "files": file_diffs } }));
            }
        }
        Ok(RunEnd::Done)
    }

    async fn approve_locked(&self, job_id: &str) -> FeatureResult<()> {
        let store = &self.d.store;
        let current = store.get(job_id);
        let Some(current) = current.filter(|c| c["status"] == "awaiting-approval") else {
            let status = store.get(job_id).and_then(|c| c["status"].as_str().map(String::from)).unwrap_or_else(|| "unknown".into());
            return Err(other(format!("Cannot approve job in status \"{status}\"")));
        };
        let data = current["data"].clone();
        self.assert_no_live_claude(job_id, &data)?;
        if data["gitMetadataCompromised"] == json!(true) {
            return Err(FeatureError::JobStatus(untrusted_message(data["gitMetadataUntrusted"].as_str()).into()));
        }
        patch(store, job_id, json!({ "status": "approving" }));
        let outcome: FeatureResult<()> = async {
            let handle = handle_of(&data["worktree"]).ok_or_else(|| other("This job has no worktree."))?;
            let dir = Path::new(&handle.dir);
            let (pre, dest) = (data["preMergeSha"].as_str().unwrap_or(""), data["destSha"].as_str().unwrap_or(""));
            self.assert_git_metadata_intact(job_id, &handle.dir)?;
            stage_all(dir).await.map_err(other)?;
            let tree = write_tree(dir).await.map_err(other)?;
            // Approve must push exactly what the panel last showed.
            assert_tree_matches_reviewed(&tree, data["reviewedTree"].as_str()).map_err(other)?;
            // Belt and suspenders: the same content was marker-checked clean when stamped.
            let review_set = review_files(dir, &tree, pre, dest).await.map_err(other)?;
            let mut seen = HashSet::new();
            let all: Vec<String> = review_set.into_iter().chain(strings(&data["conflicted"])).filter(|f| seen.insert(f.clone())).collect();
            let still = files_with_conflict_markers(dir, &all);
            if !still.is_empty() {
                return Err(other(format!("Conflict markers remain in: {}. Resolve them (e.g. in the worktree at {}) and try again \u{2014} Refresh diff picks up the fix.", still.join(", "), handle.dir)));
            }
            let staged_clean = g(&["diff", "--cached", "--quiet"], dir, true, None).await.map_err(other)?.code == 0;
            let ahead: i64 = g(&["rev-list", "--count", &format!("{pre}..HEAD")], dir, false, None).await.map_err(other)?.stdout.trim().parse().unwrap_or(0);

            if is_nothing_to_push(staged_clean, ahead) {
                // "Already up to date", and nothing else changed: nothing to commit or push.
                self.assert_git_metadata_intact(job_id, &handle.dir)?;
                self.worktrees().remove_worktree(&handle).await.map_err(other)?;
                let _ = self.d.baselines.remove(job_id);
                let mut result = current["result"].clone();
                if !result.is_object() {
                    result = json!({ "files": [] });
                }
                result["summary"] = json!("Nothing to push \u{2014} the branch is already up to date.");
                patch(store, job_id, json!({ "status": "approved", "result": result }));
                return Ok(());
            }

            // Nothing staged, HEAD moved past preMergeSha by real ancestry: there is no pending
            // commit to make (git merge auto-committed, or the user committed in a terminal).
            let is_ancestor = g(&["merge-base", "--is-ancestor", pre, "HEAD"], dir, true, None).await.map_err(other)?.code == 0;
            if !should_skip_commit(staged_clean, ahead, is_ancestor) {
                self.assert_git_metadata_intact(job_id, &handle.dir)?;
                let has_merge_head = g(&["rev-parse", "-q", "--verify", "MERGE_HEAD"], dir, true, None).await.map_err(other)?.code == 0;
                // --no-verify: the human review that matters already happened (approving the diff
                // in the panel); hooks still run for any commit the user makes themselves.
                if has_merge_head {
                    g(&["commit", "--no-edit", "--no-verify"], dir, false, None).await.map_err(other)?;
                } else {
                    // The merge was already committed in a resumed terminal session and these are
                    // further review edits on top of it: name the commit explicitly.
                    let msg = format!("Resolve review edits for PR #{}", crate::js_text::js_to_string(&data["payload"]["prId"]));
                    g(&["commit", "-m", &msg, "--no-verify"], dir, false, None).await.map_err(other)?;
                }
            }
            self.assert_git_metadata_intact(job_id, &handle.dir)?;
            g(&["push", "origin", &format!("HEAD:{}", data["sourceBranch"].as_str().unwrap_or(""))], dir, false, None).await.map_err(other)?;
            self.worktrees().remove_worktree(&handle).await.map_err(other)?;
            let _ = self.d.baselines.remove(job_id);
            patch(store, job_id, json!({ "status": "approved" }));
            Ok(())
        }
        .await;
        if let Err(e) = &outcome {
            // The worktree stays on failure (push rejected, markers still there) so the user can
            // inspect or finish it by hand.
            patch(store, job_id, json!({ "status": "failed", "error": e.to_string() }));
        }
        outcome
    }

    /// A handle on the parts a spawned run needs.
    fn parts(&self) -> Arc<ResolveConflict> {
        let d = &self.d;
        Arc::new(ResolveConflict {
            d: ResolveConflictDeps { store: d.store.clone(), claude: d.claude.clone(), repo_choices: d.repo_choices.clone(), baselines: d.baselines.clone(), work_units: d.work_units.clone(), repo_path: d.repo_path.clone(), home: d.home.clone(), rules: d.rules },
            locks: JobLocks::new(),
            running: self.running.clone(),
        })
    }
}

#[async_trait]
impl Feature for ResolveConflict {
    fn id(&self) -> &str {
        "resolve-conflict"
    }

    fn label(&self) -> &str {
        "Resolve Conflict"
    }

    async fn start(&self, payload_raw: Value, _ctx: FeatureContext) -> FeatureResult<Value> {
        let field = |k: &str| payload_raw[k].as_str().map(String::from);
        let (Some(project), Some(repo_name), Some(_), Some(_)) = (field("project"), field("repo"), field("sourceBranch"), field("destBranch")) else {
            return Err(other("resolve-conflict payload must include project, repo, sourceBranch, destBranch (strings)"));
        };
        let repo = (self.d.repo_path)(&project, &repo_name).map_err(other)?;
        let payload = json!({ "project": project, "repo": repo_name, "prId": payload_raw["prId"], "sourceBranch": payload_raw["sourceBranch"], "destBranch": payload_raw["destBranch"] });
        let job = self.d.store.create("resolve-conflict", Map::from_iter([("payload".to_string(), payload.clone())]));
        let id = Self::id_of(&job);
        let cancel = CancellationToken::new();
        // Not awaited by the caller: the HTTP handler returns the running job and the extension polls.
        let (this, running) = (self.parts(), self.running.clone());
        let (job_id, token) = (id.clone(), cancel.clone());
        let handle = tokio::spawn(async move {
            let end = this.run_job(&job_id, &repo, &payload, &token).await;
            match end {
                Ok(RunEnd::Done) => {}
                Err(e) if !token.is_cancelled() => {
                    patch(&this.d.store, &job_id, json!({ "status": "failed", "error": e }));
                }
                Ok(RunEnd::Cancelled) | Err(_) => {
                    if let Some(job) = this.d.store.get(&job_id) {
                        if let Err(e) = this.discard_worktree(&job).await {
                            eprintln!("[resolve-conflict] cleanup after cancel failed: {e}");
                        }
                    }
                    let _ = this.d.baselines.remove(&job_id);
                    patch(&this.d.store, &job_id, json!({ "status": "rejected", "progress": null }));
                }
            }
            running.lock().unwrap().remove(&job_id);
        });
        self.running.lock().unwrap().insert(id.clone(), Running { cancel, done: Some(handle) });
        Ok(self.d.store.get(&id).unwrap_or(job))
    }

    fn supports_cancel(&self) -> bool {
        true
    }

    async fn cancel(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let id = Self::id_of(&job);
        self.locks
            .with_job_lock(&id, || async {
                let current = self.d.store.get(&id);
                let status = current.as_ref().and_then(|c| c["status"].as_str()).or(job["status"].as_str()).unwrap_or("").to_string();
                let taken = {
                    let mut running = self.running.lock().unwrap();
                    match running.get_mut(&id) {
                        Some(r) if current.is_some() && status == "running" => Some((r.cancel.clone(), r.done.take())),
                        _ => None,
                    }
                };
                let Some((cancel, done)) = taken else { return Err(other(format!("Cannot cancel job in status \"{status}\""))) };
                patch(&self.d.store, &id, json!({ "status": "rejecting", "progress": { "stepId": "cancel", "label": "Cancelling and discarding the merge\u{2026}" } }));
                cancel.cancel();
                if let Some(h) = done {
                    let _ = h.await;
                }
                Ok(())
            })
            .await
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let id = Self::id_of(&job);
        self.locks.with_job_lock(&id, || self.approve_locked(&id)).await
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let status = job["status"].as_str().unwrap_or("");
        // Advisory fast refusal before queuing for the lock; re-checked inside it.
        if status == "approving" || status == "rejecting" {
            return Err(FeatureError::JobStatus(format!("This job is already {status}.")));
        }
        let id = Self::id_of(&job);
        self.locks
            .with_job_lock(&id, || async {
                let Some(current) = self.d.store.get(&id) else { return Err(FeatureError::JobStatus(format!("Unknown job id: {id}"))) };
                let status = current["status"].as_str().unwrap_or("");
                if !can_reject(status) {
                    return Err(FeatureError::JobStatus(format!("This job is already {status}.")));
                }
                patch(&self.d.store, &id, json!({ "status": "rejecting" }));
                self.discard_worktree(&current).await.map_err(other)?;
                let _ = self.d.baselines.remove(&id);
                patch(&self.d.store, &id, json!({ "status": "rejected" }));
                Ok(())
            })
            .await
    }

    fn supports_refresh_diff(&self) -> bool {
        true
    }

    async fn refresh_diff(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<Value> {
        let id = Self::id_of(&job);
        self.locks
            .with_job_lock(&id, || async {
                let store = &self.d.store;
                let Some(current) = store.get(&id) else { return Err(FeatureError::JobStatus(format!("Unknown job id: {id}"))) };
                let status = current["status"].as_str().unwrap_or("");
                if status != "awaiting-approval" && status != "failed" {
                    return Err(FeatureError::JobStatus(format!("Cannot refresh the diff for a job in status \"{status}\"")));
                }
                let data = current["data"].clone();
                // A job reconciled to "failed" after a crash is accepted here, and its claude may
                // have outlived the old process.
                self.assert_no_live_claude(&id, &data)?;
                if data["gitMetadataCompromised"] == json!(true) {
                    return Err(FeatureError::JobStatus(untrusted_message(data["gitMetadataUntrusted"].as_str()).into()));
                }
                let handle = handle_of(&data["worktree"]).filter(|h| Path::new(&h.dir).exists());
                let Some(handle) = handle else { return Err(FeatureError::WorktreeGone("The worktree for this job no longer exists (it may already have been approved or discarded).".into())) };
                let dir = Path::new(&handle.dir);
                let conflicted = strings(&data["conflicted"]);
                self.assert_git_metadata_intact(&id, &handle.dir)?;
                let head = g(&["rev-parse", "HEAD"], dir, false, None).await.map_err(other)?.stdout.trim().to_string();
                let has_merge_head = g(&["rev-parse", "-q", "--verify", "MERGE_HEAD"], dir, true, None).await.map_err(other)?.code == 0;
                // "Nothing to review" is specifically the merge-failed-outright case.
                if status == "failed" && conflicted.is_empty() && !has_merge_head && Some(head.as_str()) == data["preMergeSha"].as_str() {
                    return Err(FeatureError::JobStatus("There's nothing to refresh \u{2014} the merge failed before any conflict was ever found, so no diff was produced.".into()));
                }
                match render_review(dir, data["preMergeSha"].as_str().unwrap_or(""), data["destSha"].as_str().unwrap_or(""), &conflicted).await.map_err(other)? {
                    Render::Markers(m) => Ok(patch(store, &id, json!({ "status": "failed", "error": format!("Conflict markers remain in: {}.", m.join(", ")) }))),
                    Render::Ok { tree, files, file_diffs } => {
                        data_patch(store, &id, json!({ "reviewedTree": tree }));
                        let summary = match files.len() {
                            0 => "Refreshed \u{2014} nothing needed resolving.".to_string(),
                            1 => "Refreshed diff \u{2014} 1 file to review. Review below before approving.".to_string(),
                            n => format!("Refreshed diff \u{2014} {n} files to review. Review below before approving."),
                        };
                        Ok(patch(store, &id, json!({ "status": "awaiting-approval", "error": null, "result": { "summary": summary, "files": file_diffs } })))
                    }
                }
            })
            .await
    }

    fn action_names(&self) -> Vec<&'static str> {
        vec!["confirm-repo"]
    }

    /// Body `{ repoKey }`: one of the choices offered in `data.pendingChoice` when the branch is
    /// checked out in the main clone, or null to cancel.
    async fn run_action(&self, name: &str, job: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        match name {
            "confirm-repo" => self.d.repo_choices.confirm(&job, ctx.body.as_ref()),
            unknown => Err(FeatureError::Other(format!("No action \"{unknown}\"."))),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::claude::testing::FakeClaude;
    use crate::claude::{ClaudeError, ClaudeRunResult};
    use crate::git_cli::testing::{git as sh, make_repos_with, Repos};
    use std::time::Duration;

    #[test]
    fn conflict_markers_are_found_at_line_starts_only_and_a_longer_equals_run_is_not_one() {
        assert!(has_conflict_marker("a\n<<<<<<< HEAD\nb\n=======\nc\n>>>>>>> x\n"));
        assert!(has_conflict_marker("=======\n"));
        assert!(has_conflict_marker("x\r>>>>>>>"));
        assert!(!has_conflict_marker("========\n"));
        assert!(!has_conflict_marker("  <<<<<<< indented\n<<<<<< six\n"));
        assert!(!has_conflict_marker("<<<<<<=\n<=<<<<<\n"));
        assert!(!has_conflict_marker(""));
    }

    struct Fx {
        r: Repos,
        feature: Arc<ResolveConflict>,
        store: Arc<JobStore>,
        baselines: Arc<BaselineStore>,
        claude: Arc<FakeClaude>,
    }

    fn deps(r: &Repos, store: &Arc<JobStore>, baselines: &Arc<BaselineStore>, claude: Arc<dyn ClaudeRunner>) -> ResolveConflictDeps {
        let clone = r.clone.to_string_lossy().into_owned();
        ResolveConflictDeps {
            store: store.clone(),
            claude,
            repo_choices: Arc::new(RepoChoices::new(store.clone())),
            baselines: baselines.clone(),
            work_units: Arc::new(WorkUnits::open(r.root.join(format!("work-units-{}.json", crate::ids::uuid_v4())))),
            repo_path: Arc::new(move |p, n| if (p, n) == ("ACME", "sample-app") { Ok(clone.clone()) } else { Err(format!("No local clone of {p}/{n}.")) }),
            home: r.home.clone(),
            rules: GitRemoteRules::BitbucketDc,
        }
    }

    /// Origin has `feat` (the PR branch) and master; when `diverge`, both change README.md
    /// differently (`conflict`) or different files.
    fn fx(conflict: bool, diverge: bool, claude: FakeClaude) -> Fx {
        let r = make_repos_with(&["feat"]);
        if diverge {
            let c = &r.clone;
            sh(c, &["checkout", "-q", "feat"]);
            std::fs::write(c.join(if conflict { "README.md" } else { "feat.txt" }), "feature side\n").unwrap();
            sh(c, &["add", "."]);
            sh(c, &["commit", "-q", "-m", "feat change"]);
            sh(c, &["push", "-q", "origin", "feat"]);
            sh(c, &["checkout", "-q", "master"]);
            std::fs::write(c.join(if conflict { "README.md" } else { "master.txt" }), "master side\n").unwrap();
            sh(c, &["add", "."]);
            sh(c, &["commit", "-q", "-m", "master change"]);
            sh(c, &["push", "-q", "origin", "master"]);
        }
        // The user's clone sits on master, so the PR branch is checked out nowhere and the job
        // makes its own worktree.
        let store = Arc::new(JobStore::new());
        let baselines = Arc::new(BaselineStore::new(None));
        let claude = Arc::new(claude);
        let feature = create_resolve_conflict_feature(deps(&r, &store, &baselines, claude.clone()));
        Fx { r, feature, store, baselines, claude }
    }

    fn payload() -> Value {
        json!({ "project": "ACME", "repo": "sample-app", "prId": 7, "sourceBranch": "feat", "destBranch": "master" })
    }

    /// A "Claude" that resolves README.md by writing a merged version into its cwd.
    fn resolver() -> FakeClaude {
        FakeClaude::new(|o| {
            std::fs::write(Path::new(&o.cwd).join("README.md"), "merged by claude\n").unwrap();
            Ok(ClaudeRunResult { text: "done".into(), raw: String::new(), session_id: o.session_id.clone(), permission_denials: None })
        })
    }

    async fn settled(f: &Fx, id: &str) -> Value {
        for _ in 0..2000 {
            let j = f.store.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the job never settled: {}", f.store.get(id).unwrap());
    }

    async fn start(f: &Fx) -> Value {
        let job = f.feature.start(payload(), FeatureContext::default()).await.unwrap();
        settled(f, job["id"].as_str().unwrap()).await
    }

    fn origin_log(f: &Fx, branch: &str) -> String {
        sh(&f.r.origin, &["log", "--format=%P|%s", branch])
    }

    #[tokio::test]
    async fn a_bad_payload_or_an_unknown_repo_is_refused() {
        let f = fx(true, true, resolver());
        assert!(f.feature.start(json!({ "project": "ACME" }), FeatureContext::default()).await.unwrap_err().to_string().contains("must include"));
        let mut p = payload();
        p["repo"] = json!("nope");
        assert_eq!(f.feature.start(p, FeatureContext::default()).await.unwrap_err().to_string(), "No local clone of ACME/nope.");
        assert!(f.claude.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_conflict_is_resolved_for_review_and_approving_pushes_the_merge_to_the_source_branch() {
        let f = fx(true, true, resolver());
        let job = start(&f).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert_eq!(job["result"]["summary"], "Resolved 1 conflicted file(s) against origin/master. Review the diffs below before approving.");
        assert_eq!(job["result"]["files"][0]["path"], "README.md");
        assert!(job["result"]["files"][0]["diff"].as_str().unwrap().contains("merged by claude"));
        assert_eq!(job["data"]["conflicted"], json!(["README.md"]));
        assert_eq!(job["data"]["claudeSession"]["permissionMode"], "default");
        assert!(job["data"]["reviewedTree"].as_str().is_some_and(|t| t.len() >= 40));
        assert_eq!(f.claude.calls.lock().unwrap()[0].2, "worktreeWrite");
        let dir = job["data"]["worktree"]["dir"].as_str().unwrap().to_string();
        assert!(Path::new(&dir).exists());

        f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap();
        let done = f.store.get(job["id"].as_str().unwrap()).unwrap();
        assert_eq!(done["status"], "approved", "{done}");
        assert!(!Path::new(&dir).exists());
        assert!(f.baselines.get(job["id"].as_str().unwrap()).is_none());
        // A plain, non-force push of a merge commit (two parents) onto the source branch.
        let top = origin_log(&f, "feat").lines().next().unwrap().to_string();
        assert_eq!(top.split('|').next().unwrap().split(' ').count(), 2, "{top}");
        assert_eq!(sh(&f.r.origin, &["show", "feat:README.md"]), "merged by claude");
    }

    #[tokio::test]
    async fn a_merge_without_conflicts_skips_claude_and_approving_pushes_it() {
        let f = fx(false, true, resolver());
        let job = start(&f).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert!(job["result"]["summary"].as_str().unwrap().starts_with("Merged cleanly"));
        assert!(f.claude.calls.lock().unwrap().is_empty());
        f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap();
        assert_eq!(f.store.get(job["id"].as_str().unwrap()).unwrap()["status"], "approved");
        assert_eq!(sh(&f.r.origin, &["show", "feat:master.txt"]), "master side");
    }

    #[tokio::test]
    async fn a_branch_that_is_already_up_to_date_has_nothing_to_push() {
        let f = fx(false, false, resolver());
        let job = start(&f).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert!(job["result"]["summary"].as_str().unwrap().starts_with("Merged cleanly"));
        let before = sh(&f.r.origin, &["rev-parse", "feat"]);
        f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap();
        let done = f.store.get(job["id"].as_str().unwrap()).unwrap();
        assert_eq!((done["status"].clone(), done["result"]["summary"].clone()), (json!("approved"), json!("Nothing to push \u{2014} the branch is already up to date.")));
        assert_eq!(sh(&f.r.origin, &["rev-parse", "feat"]), before);
    }

    #[tokio::test]
    async fn markers_left_by_claude_fail_the_job_keep_the_worktree_and_discard_cleans_up() {
        let f = fx(true, true, FakeClaude::replying("gave up"));
        let job = start(&f).await;
        assert_eq!(job["status"], "failed", "{job}");
        assert!(job["error"].as_str().unwrap().starts_with("Claude did not fully resolve conflicts in: README.md."));
        let dir = job["data"]["worktree"]["dir"].as_str().unwrap().to_string();
        assert!(Path::new(&dir).exists());
        f.feature.reject(job.clone(), FeatureContext::default()).await.unwrap();
        assert_eq!(f.store.get(job["id"].as_str().unwrap()).unwrap()["status"], "rejected");
        assert!(!Path::new(&dir).exists());
        // Nothing was pushed.
        assert!(!origin_log(&f, "feat").contains("Merge"));
    }

    #[tokio::test]
    async fn approve_refuses_a_worktree_that_changed_since_it_was_reviewed_until_the_diff_is_refreshed() {
        let f = fx(true, true, resolver());
        let job = start(&f).await;
        let id = job["id"].as_str().unwrap().to_string();
        let dir = job["data"]["worktree"]["dir"].as_str().unwrap().to_string();
        std::fs::write(Path::new(&dir).join("README.md"), "edited after review\n").unwrap();
        let err = f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap_err().to_string();
        assert!(err.contains("changed since you reviewed it"), "{err}");
        assert_eq!(f.store.get(&id).unwrap()["status"], "failed");
        let refreshed = f.feature.refresh_diff(f.store.get(&id).unwrap(), FeatureContext::default()).await.unwrap();
        assert_eq!(refreshed["status"], "awaiting-approval", "{refreshed}");
        assert_eq!(refreshed["result"]["summary"], "Refreshed diff \u{2014} 1 file to review. Review below before approving.");
        f.feature.approve(f.store.get(&id).unwrap(), FeatureContext::default()).await.unwrap();
        assert_eq!(sh(&f.r.origin, &["show", "feat:README.md"]), "edited after review");
    }

    #[tokio::test]
    async fn a_claude_that_touches_the_worktrees_git_metadata_gets_the_job_failed_and_no_git_run_again() {
        let f = fx(
            true,
            true,
            FakeClaude::new(|o| {
                let gitlink = Path::new(&o.cwd).join(".git");
                let text = std::fs::read_to_string(&gitlink).unwrap();
                std::fs::write(&gitlink, format!("{text}# tampered\n")).unwrap();
                Ok(ClaudeRunResult { text: "ok".into(), raw: String::new(), session_id: None, permission_denials: None })
            }),
        );
        let job = start(&f).await;
        assert_eq!((job["status"].clone(), job["error"].clone()), (json!("failed"), json!(COMPROMISED_MESSAGE)));
        assert_eq!(job["data"]["gitMetadataCompromised"], true);
        let err = f.feature.refresh_diff(job.clone(), FeatureContext::default()).await.unwrap_err();
        assert!(matches!(err, FeatureError::JobStatus(m) if m == COMPROMISED_MESSAGE));
        // Discard removes the folder with plain fs, no git.
        let dir = job["data"]["worktree"]["dir"].as_str().unwrap().to_string();
        f.feature.reject(job.clone(), FeatureContext::default()).await.unwrap();
        assert!(!Path::new(&dir).exists());
    }

    struct Hanging;

    #[async_trait]
    impl ClaudeRunner for Hanging {
        async fn run(&self, opts: ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError> {
            opts.cancel.expect("cancellable").cancelled().await;
            Err(ClaudeError::Exec(crate::exec::ExecError::Aborted))
        }
    }

    #[tokio::test]
    async fn cancelling_a_running_job_discards_the_worktree_and_rejects_it() {
        let mut f = fx(true, true, resolver());
        // Swap the runner for one that waits for the cancel.
        f.feature = create_resolve_conflict_feature(deps(&f.r, &f.store, &f.baselines, Arc::new(Hanging)));
        let job = f.feature.start(payload(), FeatureContext::default()).await.unwrap();
        let id = job["id"].as_str().unwrap().to_string();
        for _ in 0..2000 {
            if f.store.get(&id).unwrap()["progress"]["stepId"] == "resolve" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let dir = f.store.get(&id).unwrap()["data"]["worktree"]["dir"].as_str().unwrap().to_string();
        f.feature.cancel(f.store.get(&id).unwrap(), FeatureContext::default()).await.unwrap();
        let done = f.store.get(&id).unwrap();
        assert_eq!(done["status"], "rejected", "{done}");
        assert!(!Path::new(&dir).exists());
        assert!(f.feature.cancel(done, FeatureContext::default()).await.is_err());
    }

    #[tokio::test]
    async fn a_finished_job_cannot_be_rejected_again_and_an_unknown_action_is_refused() {
        let f = fx(false, false, resolver());
        let job = start(&f).await;
        f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap();
        let done = f.store.get(job["id"].as_str().unwrap()).unwrap();
        assert!(matches!(f.feature.reject(done.clone(), FeatureContext::default()).await, Err(FeatureError::JobStatus(m)) if m == "This job is already approved."));
        assert!(f.feature.approve(done.clone(), FeatureContext::default()).await.is_err());
        assert!(f.feature.run_action("nope", done.clone(), FeatureContext::default()).await.is_err());
        assert!(f.feature.run_action("confirm-repo", done, FeatureContext::default()).await.is_err());
        assert_eq!(f.feature.action_names(), ["confirm-repo"]);
    }
}
