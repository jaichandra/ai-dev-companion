//! Address review comments (port of `features/address-review-comments/index.ts`): Claude (or Cursor
//! Agent, when Cursor is the review editor) goes through a PR's open review comments in a worktree
//! of its source branch and makes the edits it agrees with. Nothing is pushed until the user has
//! reviewed the diff and the prepared replies and approves: approve commits (hooks on), pushes
//! exactly the reviewed tree to the PR's branch and posts the replies the user kept.
//!
//! Same worktree discipline as resolve-conflict: the git metadata baseline taken before the agent
//! runs is checked before every later git command; a changed or missing one marks the job
//! compromised. Not ported here: the `start_address_review_comments` MCP tool (it lives with the
//! `/mcp` route).

use super::plan::{
    approve_git_steps, approve_summary, build_address_comments_prompt, collect_open_comments, commit_message, default_replies, parse_address_report, pre_push_error, pull_request_repo_error,
    select_replies, source_sha_error, validate_payload, ApproveInput, ReportEntry,
};
use crate::atlassian::AuthContext;
use crate::claude::{ClaudeRunOptions, ClaudeRunner};
use crate::claude_args::{check_command_rules, normalize_check_commands, Policy};
use crate::config::ConfigFn;
use crate::cursor_agent::{run_cursor_agent, CursorAgentRunOptions};
use crate::diff::compute_file_diffs;
use crate::git_cli::{Git, GitOpts};
use crate::history_record::redact_secrets;
use crate::integrity_baselines::{BaselineStore, Check};
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::main_clone_choice::ask_main_clone_choice;
use crate::prereqs::{resolve_review_editor, GitRemoteRules};
use crate::process_group::is_process_group_alive;
use crate::repo_choice::RepoChoices;
use crate::reviewed_push::{assert_tree_matches_reviewed, can_reject, stage_all, write_tree, JobLocks};
use crate::work_unit_record::{pr_key_for, record_work_unit, record_work_unit_session, Session, Unit};
use crate::work_units::WorkUnits;
use crate::worktree::{AcquireOptions, MainCloneChoice, WorktreeError, WorktreeHandle, Worktrees};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const COMPROMISED_MESSAGE: &str = "Claude modified this worktree's git metadata, so it was not trusted. Discard the job.";
const MISSING_BASELINE_MESSAGE: &str = "This job's worktree-integrity record is missing (it may have been lost in a service restart), so its worktree isn't trusted. Discard the job and start again.";
const CLAUDE_STILL_RUNNING_MESSAGE: &str = "Claude from before the service restarted may still be running in this worktree. Wait a minute and click Refresh diff again, or Discard the job.";

/// "PROJECT", "repo" -> the local clone, or why there is none.
pub type RepoResolver = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// What this feature needs from the git host.
#[async_trait]
pub trait ReviewApi: Send + Sync {
    async fn get_pull_request(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Option<Value>, String>;
    async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>, String>;
    async fn list_activities(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Vec<Value>, String>;
    async fn reply_to_comment(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64, parent_id: i64, text: &str) -> Result<(), String>;
}

/// The real git host.
pub struct HostReviewApi(pub Arc<dyn crate::providers::GitHost>);

#[async_trait]
impl ReviewApi for HostReviewApi {
    async fn get_pull_request(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Option<Value>, String> {
        self.0.get_pull_request(auth, project, repo, pr_id).await.map_err(|e| e.to_string())
    }
    async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>, String> {
        self.0.who_am_i(auth).await.map_err(|e| e.to_string())
    }
    async fn list_activities(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Vec<Value>, String> {
        self.0.list_activities(auth, project, repo, pr_id).await.map_err(|e| e.to_string())
    }
    async fn reply_to_comment(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64, parent_id: i64, text: &str) -> Result<(), String> {
        self.0.reply_to_comment(auth, project, repo, pr_id, parent_id, text).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

pub struct AddressDeps<G> {
    pub store: Arc<JobStore>,
    pub git: G,
    pub api: Arc<dyn ReviewApi>,
    pub claude: Arc<dyn ClaudeRunner>,
    pub config: ConfigFn,
    pub repo_path: RepoResolver,
    pub home: PathBuf,
    pub choices: Arc<RepoChoices>,
    pub baselines: Arc<BaselineStore>,
    pub work_units: Arc<WorkUnits>,
    pub rules: GitRemoteRules,
    /// The `cursor-agent` command.
    pub cursor_command: String,
}

struct Running {
    cancel: CancellationToken,
    done: Option<JoinHandle<()>>,
}

struct Inner<G> {
    d: AddressDeps<G>,
    locks: JobLocks,
    running: Mutex<std::collections::HashMap<String, Running>>,
}

pub struct AddressReviewComments<G> {
    inner: Arc<Inner<G>>,
}

pub fn create_address_review_comments_feature<G: Git + 'static>(d: AddressDeps<G>) -> Arc<AddressReviewComments<G>> {
    Arc::new(AddressReviewComments { inner: Arc::new(Inner { d, locks: JobLocks::new(), running: Mutex::new(Default::default()) }) })
}

fn patch(store: &JobStore, id: &str, v: Value) -> Value {
    store.update(id, v.as_object().cloned().unwrap_or_default()).unwrap_or(Value::Null)
}

fn patch_data(store: &JobStore, id: &str, v: Value) {
    let _ = store.patch_data(id, v.as_object().cloned().unwrap_or_default());
}

fn untrusted_message(untrusted: Option<&str>) -> &'static str {
    if untrusted == Some("missing") { MISSING_BASELINE_MESSAGE } else { COMPROMISED_MESSAGE }
}

fn report_to_json(report: &[ReportEntry]) -> Value {
    json!(report.iter().map(|e| json!({ "commentId": e.comment_id, "action": e.action, "note": e.note })).collect::<Vec<_>>())
}

fn report_from_json(v: &Value) -> Option<Vec<ReportEntry>> {
    v.as_array().map(|a| a.iter().filter_map(|e| Some(ReportEntry { comment_id: e["commentId"].as_i64()?, action: e["action"].as_str()?.into(), note: e["note"].as_str().unwrap_or("").into() })).collect())
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

fn report_counts(report: &[ReportEntry]) -> String {
    let n = |a: &str| report.iter().filter(|e| e.action == a).count();
    format!("{} fixed, {} declined, {} need discussion", n("fixed"), n("declined"), n("needs-discussion"))
}

fn review_summary(data: &Value, file_count: usize, refreshed: bool) -> String {
    let files = match file_count {
        0 => "no files changed".to_string(),
        1 => "1 file changed".to_string(),
        n => format!("{n} files changed"),
    };
    let agent = if data["agent"] == "cursor" { "Cursor Agent" } else { "Claude" };
    let count = data["comments"].as_array().map_or(0, Vec::len);
    let report = match report_from_json(&data["report"]) {
        Some(r) => format!("{agent} went through {count} open comment(s): {}; {files}.", report_counts(&r)),
        None => format!("{agent} went through {count} open comment(s) but didn't return a readable report, so no replies were prepared; {files}."),
    };
    format!("{}{report} Review the diff and replies below before approving.", if refreshed { "Refreshed diff. " } else { "" })
}

type MainCloneFuture = Pin<Box<dyn Future<Output = Result<MainCloneChoice, WorktreeError>> + Send>>;

enum RunEnd {
    Done,
}

impl<G: Git + 'static> Inner<G> {
    async fn git_in(&self, args: &[&str], cwd: &Path, allow_failure: bool) -> Result<crate::git_cli::GitOut, String> {
        self.d.git.run(args, cwd, GitOpts::default().with_allow_failure(allow_failure)).await
    }

    fn worktrees(&self) -> Worktrees<'_, G> {
        Worktrees { git: &self.d.git, home: &self.d.home }
    }

    /// Refuses (never kills: the pid may have been reused) while a claude from before a restart may
    /// still be writing this worktree.
    fn assert_no_live_claude(&self, job_id: &str, data: &Value) -> FeatureResult<()> {
        let Some(pid) = data["claudePid"].as_i64() else { return Ok(()) };
        if is_process_group_alive(pid) {
            return Err(FeatureError::JobStatus(CLAUDE_STILL_RUNNING_MESSAGE.into()));
        }
        patch_data(&self.d.store, job_id, json!({ "claudePid": null }));
        Ok(())
    }

    /// Called before every git command the service runs in this worktree after the agent has run:
    /// a mismatch or a missing baseline marks the job compromised, moves it to "failed" (so the
    /// panel offers only Discard) and refuses before any git runs.
    fn assert_git_metadata_intact(&self, job_id: &str, dir: &str) -> FeatureResult<()> {
        let state = match self.d.baselines.check(job_id, Path::new(dir)) {
            Check::Intact => return Ok(()),
            Check::Missing => "missing",
            Check::Changed => "changed",
        };
        patch_data(&self.d.store, job_id, json!({ "gitMetadataCompromised": true, "gitMetadataUntrusted": state }));
        let msg = untrusted_message(Some(state));
        patch(&self.d.store, job_id, json!({ "status": "failed", "error": msg }));
        Err(FeatureError::JobStatus(msg.into()))
    }

    fn intact(&self, job_id: &str, dir: &str) -> bool {
        self.d.baselines.check(job_id, Path::new(dir)) == Check::Intact
    }

    /// Removes the job's worktree, if it got that far. A compromised worktree is removed with plain
    /// fs only: not even `git worktree remove` may run there.
    async fn discard_worktree(&self, job: &Value) -> Result<(), String> {
        let data = &job["data"];
        let Some(handle) = handle_of(&data["worktree"]) else { return Ok(()) };
        let id = job["id"].as_str().unwrap_or("");
        if data["gitMetadataCompromised"] == json!(true) || !self.intact(id, &handle.dir) {
            let reason = data["gitMetadataUntrusted"].as_str().map(String::from).unwrap_or_else(|| {
                if data["gitMetadataCompromised"] == json!(true) || self.d.baselines.check(id, Path::new(&handle.dir)) != Check::Missing { "changed".into() } else { "missing".into() }
            });
            patch_data(&self.d.store, id, json!({ "gitMetadataCompromised": true, "gitMetadataUntrusted": reason }));
            // An adopted worktree is the user's own: never delete it.
            if !handle.adopted {
                let _ = std::fs::remove_dir_all(&handle.dir);
            }
            return Ok(());
        }
        if handle.adopted {
            // Rejected or cancelled: put it back as it was. (After an approved push it is left alone.)
            if let Some(pre) = data["preSha"].as_str() {
                self.worktrees().restore_adopted_worktree(&handle, pre).await?;
            }
            return Ok(());
        }
        self.worktrees().remove_worktree(&handle).await
    }

    /// After an approved push: a job's own worktree goes, an adopted one stays as the push left it.
    async fn release_worktree(&self, job: &Value) -> Result<(), String> {
        match handle_of(&job["data"]["worktree"]) {
            Some(h) if !h.adopted => self.discard_worktree(job).await,
            _ => Ok(()),
        }
    }

    /// Stages everything, fingerprints the tree and diffs it against `pre_sha`.
    async fn render_review(&self, job_id: &str, dir: &str, pre_sha: &str) -> FeatureResult<(String, Vec<Value>)> {
        self.assert_git_metadata_intact(job_id, dir)?;
        let wt = Path::new(dir);
        stage_all(wt).await.map_err(|e| e.to_string())?;
        let tree = write_tree(wt).await.map_err(|e| e.to_string())?;
        let names = self.git_in(&["diff", "--name-only", "-z", "--no-renames", pre_sha, &tree], wt, false).await?.stdout;
        let files: Vec<String> = names.split('\0').filter(|s| !s.is_empty()).map(String::from).collect();
        let diffs = compute_file_diffs(wt, pre_sha, &files).await.map_err(|e| e.to_string())?;
        Ok((tree, diffs.iter().map(|d| serde_json::to_value(d).expect("a diff serializes")).collect()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_job(self: &Arc<Self>, job_id: &str, repo_path: &str, payload: &super::plan::Payload, auth: &AuthContext, check_commands: &[String], check_rules: &[String], cancel: &CancellationToken) -> Result<RunEnd, String> {
        let d = &self.d;
        let store = &d.store;
        let checkpoint = || if cancel.is_cancelled() { Err("cancelled".to_string()) } else { Ok(()) };

        patch(store, job_id, json!({ "progress": { "stepId": "fetch", "label": "Fetching the pull request from Bitbucket\u{2026}" } }));
        let pr = d.api.get_pull_request(auth, &payload.project, &payload.repo, payload.pr_id).await?;
        let Some(pr) = pr.filter(|p| p["fromBranch"].as_str().is_some_and(|b| !b.is_empty())) else {
            return Err(format!("Bitbucket didn't return a usable pull request for {}/{} #{}.", payload.project, payload.repo, payload.pr_id));
        };
        let from_branch = pr["fromBranch"].as_str().unwrap_or("").to_string();
        if let Some(state) = pr["state"].as_str().filter(|s| !s.is_empty() && *s != "OPEN") {
            return Err(format!("Pull request #{} is {} \u{2014} there's nothing to push to.", payload.pr_id, state.to_lowercase()));
        }
        // Before any worktree exists: the worktree comes from, and approve pushes to, this clone's
        // `origin`: only right when the PR's source and target are both the payload's repo.
        if let Some(e) = pull_request_repo_error(&pr, &payload.project, &payload.repo) {
            return Err(e);
        }
        // Only used to skip my own comments nobody has answered; unknown just skips that filter.
        let me = match d.api.who_am_i(auth).await {
            Ok(me) => me,
            Err(e) => {
                eprintln!("[address-review-comments] couldn't tell who's logged in to Bitbucket: {e}");
                None
            }
        };
        patch_data(store, job_id, json!({ "repoPath": repo_path, "fromBranch": from_branch, "pr": pr }));

        checkpoint()?;
        patch(store, job_id, json!({ "progress": { "stepId": "comments", "label": "Collecting open review comments\u{2026}" } }));
        let all = d.api.list_activities(auth, &payload.project, &payload.repo, payload.pr_id).await?;
        let tag = format!("[address-review-comments] {}/{}#{}", payload.project, payload.repo, payload.pr_id);
        let mut on_drop = |c: &Value, reason: &str| {
            let at = if c["anchor"].is_object() { format!(" {}:{} anchor={}", c["anchor"]["path"], c["anchor"]["line"], c["anchor"]) } else { String::new() };
            println!("{tag}: skipped comment {} by {}{at} \u{2014} {reason}", c["id"], c["authorSlug"]);
        };
        let comments = collect_open_comments(&Value::Array(all.clone()), me.as_deref(), Some(&mut on_drop));
        println!("{tag}: {} comment(s) fetched, {} to address", all.len(), comments.len());
        patch_data(store, job_id, json!({ "comments": comments }));
        if comments.is_empty() {
            patch(store, job_id, json!({ "status": "failed", "error": "No open review comments to address \u{2014} resolved threads, your own unanswered comments, and comments on removed or outdated code are skipped." }));
            return Ok(RunEnd::Done);
        }

        // The default editor picks the agent: Cursor Agent when it's Cursor, Claude otherwise.
        let config = (d.config)();
        let editor = resolve_review_editor(config["reviewEditor"].as_str());
        let use_cursor = editor == Some("cursor");
        let agent_name = if use_cursor { "Cursor Agent" } else { "Claude" };
        let model = config["addressReviewComments"]["model"].as_str().filter(|m| !m.is_empty()).map(String::from);
        patch_data(store, job_id, json!({ "editor": editor.unwrap_or("claude-code"), "agent": if use_cursor { "cursor" } else { "claude" } }));

        checkpoint()?;
        let n = comments.len();
        patch(store, job_id, json!({ "progress": { "stepId": "address", "label": format!("Addressing {} with {agent_name} \u{2014} this can take a few minutes\u{2026}", if n == 1 { "1 review comment".to_string() } else { format!("{n} review comments") }) } }));
        // An existing clean checkout of the branch is reused; only when there is none is a
        // disposable worktree made.
        let ask = {
            let (choices, store, job_id, token) = (d.choices.clone(), d.store.clone(), job_id.to_string(), cancel.clone());
            move |info| -> MainCloneFuture {
                let (choices, store, job_id, token) = (choices.clone(), store.clone(), job_id.clone(), token.clone());
                Box::pin(async move {
                    patch(&store, &job_id, json!({ "progress": { "stepId": "fetch", "label": "Waiting for you to choose where to work\u{2026}" } }));
                    ask_main_clone_choice(&choices, &job_id, Some(&token), &info).await
                })
            }
        };
        let (worktree, reused) = self.worktrees().acquire_worktree(Path::new(repo_path), &from_branch, job_id, &AcquireOptions { on_main_clone: Some(&ask) }).await.map_err(|e| e.to_string())?;
        // Recorded before anything else can fail, so cancel/reject can find it.
        patch_data(store, job_id, json!({ "worktree": serde_json::to_value(&worktree).expect("a handle serializes") }));
        let pr_key = pr_key_for(d.rules, &payload.project, &payload.repo, payload.pr_id as u64);
        let unit = Unit { repo_path, branch: &from_branch, dir: reused.then_some(worktree.dir.as_str()), issue_key: None, pr_key: Some(&pr_key) };
        record_work_unit(&d.work_units, &unit);
        if reused {
            patch(store, job_id, json!({ "progress": { "stepId": "fetch", "label": format!("Using the existing checkout of {from_branch} at {}\u{2026}", worktree.dir) } }));
        }
        checkpoint()?;
        // Snapshotted as soon as the worktree exists: from here on every git the service runs in
        // this worktree is preceded by a check against this baseline.
        let wt = Path::new(&worktree.dir);
        let git_dir = self.git_in(&["rev-parse", "--absolute-git-dir"], wt, false).await?.stdout.trim().to_string();
        d.baselines.take(job_id, wt, Path::new(&git_dir))?;
        let pre_sha = self.git_in(&["rev-parse", "HEAD"], wt, false).await?.stdout.trim().to_string();
        // `origin/<fromBranch>` must be the very commit the host just reported as the PR's source.
        if let Some(e) = source_sha_error(&pre_sha, pr["fromSha"].as_str()) {
            return Err(e);
        }
        patch_data(store, job_id, json!({ "preSha": pre_sha }));

        // Sandboxed Bash that may also write the worktree's git dir, minus the four metadata files
        // that could redirect the service's own next git command; the edit guard pinned to the
        // worktree's real path.
        let deny_write: Vec<String> = [wt.join(".git"), PathBuf::from(&git_dir).join("commondir"), PathBuf::from(&git_dir).join("gitdir"), PathBuf::from(&git_dir).join("config.worktree")].iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let edit_root = std::fs::canonicalize(wt).map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        let session_id = crate::ids::uuid_v4();
        checkpoint()?;

        // claudePid is persisted with the job while the agent runs, and cleared however the run
        // ends (it holds Cursor Agent's pid too).
        let prompt = build_address_comments_prompt(&comments, &pr, check_commands);
        let on_spawn: Arc<dyn Fn(u32) + Send + Sync> = {
            let (store, job_id) = (d.store.clone(), job_id.to_string());
            Arc::new(move |pid| patch_data(&store, &job_id, json!({ "claudePid": pid })))
        };
        let ran: Result<(String, Option<String>), String> = if use_cursor {
            run_cursor_agent(&d.cursor_command, CursorAgentRunOptions { cwd: worktree.dir.clone(), prompt, model, cancel: Some(cancel.clone()), on_spawn: Some(on_spawn) }).await.map(|r| (r.text, r.session_id)).map_err(|e| e.to_string())
        } else {
            let mut opts = ClaudeRunOptions::new(&worktree.dir, &prompt, Policy::WORKTREE_WRITE_NARROW_BASH);
            opts.model = model;
            opts.extra_allowed_tools = check_rules.to_vec();
            opts.session_id = Some(session_id.clone());
            opts.sandbox_allow_write = vec![git_dir.clone()];
            opts.sandbox_deny_write = deny_write;
            opts.edit_root = Some(edit_root);
            opts.cancel = Some(cancel.clone());
            opts.on_spawn = Some(on_spawn);
            d.claude.run(opts).await.map(|r| (r.text, r.session_id)).map_err(|e| e.to_string())
        };
        patch_data(store, job_id, json!({ "claudePid": null }));
        let (text, result_session) = ran?;

        if !self.intact(job_id, &worktree.dir) {
            let untrusted = if self.d.baselines.check(job_id, wt) == Check::Missing { "missing" } else { "changed" };
            patch_data(store, job_id, json!({ "gitMetadataCompromised": true, "gitMetadataUntrusted": untrusted }));
            patch(store, job_id, json!({ "status": "failed", "error": untrusted_message(Some(untrusted)) }));
            return Ok(RunEnd::Done);
        }

        // Only a Claude run leaves a session "Continue in Claude Code" can resume.
        let claude_session = (!use_cursor).then(|| json!({ "id": result_session.filter(|s| !s.is_empty()).unwrap_or(session_id), "cwd": worktree.dir, "permissionMode": "default" }));
        let ids: Vec<i64> = comments.iter().filter_map(|c| c["id"].as_i64()).collect();
        let report = parse_address_report(&text, &ids);
        patch_data(store, job_id, json!({
            "claudeSession": claude_session.clone().unwrap_or(Value::Null),
            "report": report.as_deref().map_or(Value::Null, report_to_json),
            "replies": default_replies(report.as_deref()),
        }));
        if let Some(s) = &claude_session {
            record_work_unit_session(&d.work_units, &unit, &Session { id: s["id"].as_str().unwrap_or(""), cwd: Some(&worktree.dir), feature: Some("address-review-comments") });
        }

        checkpoint()?;
        patch(store, job_id, json!({ "progress": { "stepId": "diff", "label": "Preparing diff for review\u{2026}" } }));
        let (tree, files) = self.render_review(job_id, &worktree.dir, &pre_sha).await.map_err(|e| e.to_string())?;
        checkpoint()?;
        patch_data(store, job_id, json!({ "reviewedTree": tree }));
        let data = store.get(job_id).map(|j| j["data"].clone()).unwrap_or(Value::Null);
        patch(store, job_id, json!({ "status": "awaiting-approval", "progress": null, "result": { "summary": review_summary(&data, files.len(), false), "files": files } }));
        Ok(RunEnd::Done)
    }

    async fn approve_inner(&self, job: &Value, ctx: &FeatureContext) -> FeatureResult<()> {
        let id = job["id"].as_str().unwrap_or("").to_string();
        let store = &self.d.store;
        let current = store.get(&id);
        let Some(current) = current.filter(|c| c["status"] == "awaiting-approval") else {
            return Err(FeatureError::Other(format!("Cannot approve job in status \"{}\"", store.get(&id).and_then(|c| c["status"].as_str().map(String::from)).unwrap_or_else(|| "unknown".into()))));
        };
        let mut data = current["data"].clone();
        self.assert_no_live_claude(&id, &data)?;
        if data["gitMetadataCompromised"] == json!(true) {
            return Err(FeatureError::JobStatus(untrusted_message(data["gitMetadataUntrusted"].as_str()).into()));
        }
        let (Some(wt), Some(pre_sha)) = (handle_of(&data["worktree"]), data["preSha"].as_str().map(String::from)) else {
            return Err(FeatureError::JobStatus("This job has no worktree to push.".into()));
        };
        let dir = wt.dir.clone();
        // Validated against this job's own comments before anything runs, so what gets posted is
        // decided up front, not after the push.
        let comments: Vec<Value> = data["comments"].as_array().cloned().unwrap_or_default();
        let replies = select_replies(&ctx.body.as_ref().map_or(Value::Null, |b| b["replies"].clone()), &comments);
        patch(store, &id, json!({ "status": "approving" }));

        let from_branch = data["fromBranch"].as_str().unwrap_or("").to_string();
        let pushed: Result<bool, FeatureError> = async {
            let wtp = Path::new(&dir);
            self.assert_git_metadata_intact(&id, &dir)?;
            stage_all(wtp).await.map_err(|e| e.to_string())?;
            let tree = write_tree(wtp).await.map_err(|e| e.to_string())?;
            assert_tree_matches_reviewed(&tree, data["reviewedTree"].as_str())?;

            let staged_clean = self.git_in(&["diff", "--cached", "--quiet"], wtp, true).await?.code == 0;
            let head = self.git_in(&["rev-parse", "HEAD"], wtp, false).await?.stdout.trim().to_string();
            let parent = self.git_in(&["rev-parse", "-q", "--verify", "HEAD^"], wtp, true).await?;
            let parent_sha = (parent.code == 0).then(|| parent.stdout.trim().to_string());
            let steps = approve_git_steps(&ApproveInput { staged_clean, head: &head, head_parent: parent_sha.as_deref(), pre_sha: &pre_sha, approve_commit: data["approveCommit"].as_str() })?;

            if let Some(reset_to) = &steps.reset_to {
                // Our own unpushed commit plus newly reviewed changes: fold them into one fresh
                // commit on preSha.
                self.assert_git_metadata_intact(&id, &dir)?;
                self.git_in(&["reset", "--soft", reset_to], wtp, false).await?;
            }
            if steps.commit {
                self.assert_git_metadata_intact(&id, &dir)?;
                // Hooks ON (no --no-verify): these are new code changes on the PR, exactly what the
                // repo's pre-commit checks exist for.
                let message = commit_message(&json!({ "id": data["payload"]["prId"] }), report_from_json(&data["report"]).as_deref());
                if let Err(e) = self.git_in(&["commit", "-m", &message], wtp, false).await {
                    return Err(FeatureError::Other(format!("git commit was refused \u{2014} usually a pre-commit hook failing. Nothing was pushed and the worktree is kept: fix it (e.g. Continue in Claude Code), click Refresh diff, then approve again.\n\n{e}")));
                }
                // Recorded before the tree check below, so a retry (after Refresh diff) recognizes
                // this commit as ours.
                let approve_commit = self.git_in(&["rev-parse", "HEAD"], wtp, false).await?.stdout.trim().to_string();
                patch_data(store, &id, json!({ "approveCommit": approve_commit }));
                data["approveCommit"] = json!(approve_commit);
            }
            if steps.push {
                // Re-read the commit about to be pushed, on every path: it must be one commit on
                // preSha whose tree is exactly what Refresh diff last showed. Then push that exact
                // sha, not `HEAD`, so what was checked is what goes out.
                self.assert_git_metadata_intact(&id, &dir)?;
                let out = self.git_in(&["rev-parse", "HEAD", "HEAD^", "HEAD^{tree}"], wtp, false).await?.stdout;
                let lines: Vec<&str> = out.trim().split('\n').map(str::trim).collect();
                let (push_sha, head_parent, head_tree) = (lines.first().copied().unwrap_or(""), lines.get(1).copied().filter(|s| !s.is_empty()), lines.get(2).copied().unwrap_or(""));
                if let Some(e) = pre_push_error(head_parent, head_tree, &pre_sha, data["reviewedTree"].as_str()) {
                    return Err(FeatureError::Other(e));
                }
                self.assert_git_metadata_intact(&id, &dir)?;
                self.git_in(&["push", "origin", &format!("{push_sha}:refs/heads/{from_branch}")], wtp, false).await?;
                return Ok(true);
            }
            Ok(false)
        }
        .await;
        let pushed = match pushed {
            Ok(p) => p,
            Err(e) => {
                // Worktree left in place so the user can inspect or fix it; Refresh diff brings the
                // job back.
                patch(store, &id, json!({ "status": "failed", "error": e.to_string() }));
                return Err(e);
            }
        };

        // After the push: a reply failing no longer un-approves anything. Each reply is recorded the
        // moment it's posted, and ones an earlier approve already posted are skipped.
        let (project, repo, pr_id) = (data["payload"]["project"].as_str().unwrap_or(""), data["payload"]["repo"].as_str().unwrap_or(""), data["payload"]["prId"].as_i64().unwrap_or(0));
        let already: HashSet<i64> = data["repliesPosted"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
        let mut posted: Vec<i64> = data["repliesPosted"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
        let mut failed: Vec<(i64, String)> = vec![];
        for (comment_id, text) in &replies {
            if already.contains(comment_id) {
                continue;
            }
            match self.d.api.reply_to_comment(&ctx.auth, project, repo, pr_id, *comment_id, text).await {
                Ok(()) => {
                    posted.push(*comment_id);
                    patch_data(store, &id, json!({ "repliesPosted": posted }));
                }
                Err(e) => failed.push((*comment_id, e)),
            }
        }

        if let Err(e) = self.release_worktree(&current).await {
            eprintln!("[address-review-comments] removing the worktree after approve failed: {e}");
        }
        let _ = self.d.baselines.remove(&id);
        patch_data(store, &id, json!({ "repliesPosted": posted, "repliesFailed": failed.iter().map(|(c, e)| json!({ "commentId": c, "error": e })).collect::<Vec<_>>() }));
        let mut result = current["result"].as_object().cloned().unwrap_or_else(|| Map::from_iter([("files".to_string(), json!([]))]));
        result.insert("summary".into(), json!(approve_summary(pushed, &from_branch, posted.len(), &failed)));
        patch(store, &id, json!({ "status": "approved", "result": result }));
        Ok(())
    }
}

#[async_trait]
impl<G: Git + 'static> Feature for AddressReviewComments<G> {
    fn id(&self) -> &str {
        "address-review-comments"
    }

    fn label(&self) -> &str {
        "Address review comments"
    }

    async fn start(&self, payload_raw: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let payload = validate_payload(&payload_raw)?;
        let d = &self.inner.d;
        // Validated before a job exists, so a bad config.json entry is an immediate, actionable
        // error rather than a job that fails later. Trimmed once here: the prompt, the
        // --allowedTools rules and the bash guard all see the same list.
        let config = (d.config)();
        let configured = config["addressReviewComments"].get("checkCommands");
        let check_commands = normalize_check_commands(configured)?;
        let check_rules = check_command_rules(Some(&json!(check_commands)))?;
        let repo = (d.repo_path)(&payload.project, &payload.repo)?;
        let job = d.store.create("address-review-comments", Map::from_iter([("payload".to_string(), json!({ "project": payload.project, "repo": payload.repo, "prId": payload.pr_id }))]));
        let id = job["id"].as_str().unwrap_or("").to_string();
        let cancel = CancellationToken::new();
        let (inner, token, job_id) = (self.inner.clone(), cancel.clone(), id.clone());
        let handle = tokio::spawn(async move {
            let outcome = inner.run_job(&job_id, &repo, &payload, &ctx.auth, &check_commands, &check_rules, &token).await;
            if let Err(e) = outcome {
                if !token.is_cancelled() {
                    patch(&inner.d.store, &job_id, json!({ "status": "failed", "progress": null, "error": redact_secrets(&e) }));
                } else {
                    if let Some(j) = inner.d.store.get(&job_id) {
                        if let Err(e) = inner.discard_worktree(&j).await {
                            eprintln!("[address-review-comments] cleanup after cancel failed: {e}");
                        }
                    }
                    let _ = inner.d.baselines.remove(&job_id);
                    patch(&inner.d.store, &job_id, json!({ "status": "rejected", "progress": null }));
                }
            }
            inner.running.lock().unwrap().remove(&job_id);
        });
        self.inner.running.lock().unwrap().insert(id.clone(), Running { cancel, done: Some(handle) });
        Ok(self.inner.d.store.get(&id).unwrap_or(job))
    }

    fn supports_cancel(&self) -> bool {
        true
    }

    async fn cancel(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let id = job["id"].as_str().unwrap_or("").to_string();
        self.inner
            .locks
            .with_job_lock(&id, || async {
                let current = self.inner.d.store.get(&id);
                let status = current.as_ref().and_then(|c| c["status"].as_str()).unwrap_or_else(|| job["status"].as_str().unwrap_or("")).to_string();
                let taken = {
                    let mut running = self.inner.running.lock().unwrap();
                    match running.get_mut(&id) {
                        Some(r) if status == "running" && current.is_some() => Some((r.cancel.clone(), r.done.take())),
                        _ => None,
                    }
                };
                let Some((cancel, done)) = taken else { return Err(FeatureError::Other(format!("Cannot cancel job in status \"{status}\""))) };
                patch(&self.inner.d.store, &id, json!({ "status": "rejecting", "progress": { "stepId": "cancel", "label": "Cancelling and discarding the changes\u{2026}" } }));
                cancel.cancel();
                if let Some(h) = done {
                    let _ = h.await;
                }
                Ok(())
            })
            .await
    }

    async fn approve(&self, job: Value, ctx: FeatureContext) -> FeatureResult<()> {
        let id = job["id"].as_str().unwrap_or("").to_string();
        self.inner.locks.with_job_lock(&id, || self.inner.approve_inner(&job, &ctx)).await
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        // An advisory fast-path, then the lock and a re-read.
        let status = job["status"].as_str().unwrap_or("");
        if matches!(status, "approving" | "rejecting") {
            return Err(FeatureError::JobStatus(format!("This job is already {status}.")));
        }
        let id = job["id"].as_str().unwrap_or("").to_string();
        self.inner
            .locks
            .with_job_lock(&id, || async {
                let Some(current) = self.inner.d.store.get(&id) else { return Err(FeatureError::JobStatus(format!("Unknown job id: {id}"))) };
                let status = current["status"].as_str().unwrap_or("");
                if !can_reject(status) {
                    return Err(FeatureError::JobStatus(format!("This job is already {status}.")));
                }
                patch(&self.inner.d.store, &id, json!({ "status": "rejecting" }));
                self.inner.discard_worktree(&current).await?;
                let _ = self.inner.d.baselines.remove(&id);
                patch(&self.inner.d.store, &id, json!({ "status": "rejected" }));
                Ok(())
            })
            .await
    }

    fn supports_refresh_diff(&self) -> bool {
        true
    }

    async fn refresh_diff(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<Value> {
        let id = job["id"].as_str().unwrap_or("").to_string();
        self.inner
            .locks
            .with_job_lock(&id, || async {
                let store = &self.inner.d.store;
                let Some(current) = store.get(&id) else { return Err(FeatureError::JobStatus(format!("Unknown job id: {id}"))) };
                let status = current["status"].as_str().unwrap_or("");
                if status != "awaiting-approval" && status != "failed" {
                    return Err(FeatureError::JobStatus(format!("Cannot refresh the diff for a job in status \"{status}\"")));
                }
                let data = current["data"].clone();
                self.inner.assert_no_live_claude(&id, &data)?;
                if data["gitMetadataCompromised"] == json!(true) {
                    return Err(FeatureError::JobStatus(untrusted_message(data["gitMetadataUntrusted"].as_str()).into()));
                }
                let (Some(wt), Some(pre_sha)) = (handle_of(&data["worktree"]), data["preSha"].as_str()) else {
                    return Err(FeatureError::JobStatus("There's nothing to refresh \u{2014} this job failed before Claude changed anything.".into()));
                };
                if !Path::new(&wt.dir).exists() {
                    return Err(FeatureError::WorktreeGone("The worktree for this job no longer exists (it may already have been approved or discarded).".into()));
                }
                let (tree, files) = self.inner.render_review(&id, &wt.dir, pre_sha).await?;
                patch_data(store, &id, json!({ "reviewedTree": tree }));
                Ok(patch(store, &id, json!({ "status": "awaiting-approval", "error": null, "result": { "summary": review_summary(&data, files.len(), true), "files": files } })))
            })
            .await
    }

    /// Body `{ repoKey }`: one of the choices offered in `job.data.pendingChoice` when the branch is
    /// checked out in the main clone, or null to cancel.
    fn action_names(&self) -> Vec<&'static str> {
        vec!["confirm-repo"]
    }

    async fn run_action(&self, name: &str, job: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        match name {
            "confirm-repo" => self.inner.d.choices.confirm(&job, ctx.body.as_ref()),
            other => Err(FeatureError::Other(format!("No action \"{other}\"."))),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::claude::testing::FakeClaude;
    use crate::claude::ClaudeRunResult;
    use crate::git_cli::SystemGit;
    use std::process::Command;
    use std::time::Duration;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@e.x").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@e.x").output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[derive(Default)]
    struct FakeApi {
        pr: Mutex<Value>,
        comments: Mutex<Vec<Value>>,
        replies: Mutex<Vec<(i64, String)>>,
        fail_reply: bool,
    }

    #[async_trait]
    impl ReviewApi for FakeApi {
        async fn get_pull_request(&self, _: &AuthContext, _: &str, _: &str, _: i64) -> Result<Option<Value>, String> {
            Ok(Some(self.pr.lock().unwrap().clone()))
        }
        async fn who_am_i(&self, _: &AuthContext) -> Result<Option<String>, String> {
            Ok(Some("me".into()))
        }
        async fn list_activities(&self, _: &AuthContext, _: &str, _: &str, _: i64) -> Result<Vec<Value>, String> {
            Ok(self.comments.lock().unwrap().clone())
        }
        async fn reply_to_comment(&self, _: &AuthContext, _: &str, _: &str, _: i64, parent: i64, text: &str) -> Result<(), String> {
            if self.fail_reply {
                return Err("host said no".into());
            }
            self.replies.lock().unwrap().push((parent, text.into()));
            Ok(())
        }
    }

    struct Fx {
        _d: tempfile::TempDir,
        feature: Arc<AddressReviewComments<SystemGit>>,
        store: Arc<JobStore>,
        api: Arc<FakeApi>,
        origin: PathBuf,
    }

    const REPORT: &str = "Done.\n```json\n[{\"commentId\":1,\"action\":\"fixed\",\"note\":\"fixed the typo\"}]\n```";

    fn comment(id: i64) -> Value {
        json!({ "id": id, "version": 0, "text": "fix the typo", "authorSlug": "bob", "authorName": "Bob", "createdAt": 1, "severity": "NORMAL", "state": "OPEN", "threadResolved": false, "anchor": { "path": "a.txt", "line": 1, "lineType": "ADDED", "fileType": "TO", "diffType": "EFFECTIVE", "orphaned": false }, "replies": [] })
    }

    /// A real origin with a `feature` branch, a clone of it (on main), and a fake host and agent.
    fn fx(comments: Vec<Value>, state: &str, fail_reply: bool, claude: FakeClaude) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let (origin, seed, repo, home) = (root.join("origin.git"), root.join("seed"), root.join("app"), root.join("home"));
        std::fs::create_dir_all(&origin).unwrap();
        sh(&origin, &["init", "-q", "--bare", "-b", "main"]);
        std::fs::create_dir_all(&seed).unwrap();
        sh(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("a.txt"), "teh\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "base"]);
        sh(&seed, &["remote", "add", "origin", origin.to_str().unwrap()]);
        sh(&seed, &["push", "-q", "origin", "main"]);
        sh(&seed, &["checkout", "-qb", "feature"]);
        std::fs::write(seed.join("b.txt"), "b\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "feature work"]);
        sh(&seed, &["push", "-q", "origin", "feature"]);
        let from_sha = sh(&seed, &["rev-parse", "HEAD"]);
        sh(&root, &["clone", "-q", origin.to_str().unwrap(), repo.to_str().unwrap()]);
        sh(&repo, &["config", "user.name", "T"]);
        sh(&repo, &["config", "user.email", "t@e.x"]);
        std::fs::create_dir_all(&home).unwrap();

        let repo_ref = json!({ "projectKey": "ACME", "slug": "app" });
        let api = Arc::new(FakeApi { pr: Mutex::new(json!({ "id": 7, "title": "Feature", "state": state, "fromBranch": "feature", "fromSha": from_sha, "fromRepo": repo_ref, "toBranch": "main", "toRepo": repo_ref })), comments: Mutex::new(comments), fail_reply, ..Default::default() });
        let store = Arc::new(JobStore::new());
        let repo_s = repo.to_string_lossy().into_owned();
        let feature = create_address_review_comments_feature(AddressDeps {
            store: store.clone(),
            git: SystemGit,
            api: api.clone(),
            claude: Arc::new(claude),
            config: Arc::new(|| json!({ "reviewEditor": "claude-code" })),
            repo_path: Arc::new(move |_, _| Ok(repo_s.clone())),
            home,
            choices: Arc::new(RepoChoices::new(store.clone())),
            baselines: Arc::new(BaselineStore::new(Some(root.join("baselines")))),
            work_units: Arc::new(WorkUnits::open(root.join("units.json"))),
            rules: GitRemoteRules::BitbucketDc,
            cursor_command: "cursor-agent".into(),
        });
        Fx { _d: d, feature, store, api, origin }
    }

    /// An agent that edits `a.txt` in its worktree and reports.
    fn fixer() -> FakeClaude {
        FakeClaude::new(|o| {
            std::fs::write(Path::new(&o.cwd).join("a.txt"), "the\n").unwrap();
            Ok(ClaudeRunResult { text: REPORT.into(), raw: String::new(), session_id: Some("sess-9".into()), permission_denials: None })
        })
    }

    async fn settled(f: &Fx, id: &str) -> Value {
        for _ in 0..1000 {
            let j = f.store.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never settled");
    }

    async fn start(f: &Fx) -> Value {
        let job = f.feature.start(json!({ "project": "ACME", "repo": "app", "prId": 7 }), FeatureContext::default()).await.unwrap();
        settled(f, job["id"].as_str().unwrap()).await
    }

    #[tokio::test]
    async fn the_agent_edits_a_worktree_and_the_job_waits_for_approval_with_the_diff_and_default_replies() {
        let f = fx(vec![comment(1)], "OPEN", false, fixer());
        let job = start(&f).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert_eq!(job["result"]["files"][0]["path"], "a.txt");
        assert!(job["result"]["summary"].as_str().unwrap().contains("1 fixed, 0 declined, 0 need discussion; 1 file changed."));
        assert_eq!(job["data"]["claudeSession"]["id"], "sess-9");
        assert_eq!(job["data"]["replies"][0]["commentId"], 1);
        assert_eq!(job["data"]["fromBranch"], "feature");
    }

    #[tokio::test]
    async fn approve_pushes_exactly_the_reviewed_commit_posts_the_kept_replies_and_removes_the_worktree() {
        let f = fx(vec![comment(1)], "OPEN", false, fixer());
        let job = start(&f).await;
        let (id, wt) = (job["id"].as_str().unwrap().to_string(), job["data"]["worktree"]["dir"].as_str().unwrap().to_string());
        let before = sh(&f.origin, &["rev-parse", "feature"]);
        let body = json!({ "replies": [{ "commentId": 1, "text": "Fixed, thanks." }] });
        f.feature.approve(job, FeatureContext { body: Some(body), ..Default::default() }).await.unwrap();
        let done = f.store.get(&id).unwrap();
        assert_eq!(done["status"], "approved", "{done}");
        assert_ne!(sh(&f.origin, &["rev-parse", "feature"]), before);
        assert_eq!(sh(&f.origin, &["show", "feature:a.txt"]), "the");
        assert_eq!(sh(&f.origin, &["rev-parse", "feature^"]), before);
        assert_eq!(*f.api.replies.lock().unwrap(), vec![(1, "Fixed, thanks.".to_string())]);
        assert!(!Path::new(&wt).exists());
        assert!(done["result"]["summary"].as_str().unwrap().contains("feature"), "{done}");
    }

    #[tokio::test]
    async fn a_failed_reply_is_named_in_the_summary_but_does_not_undo_the_push() {
        let f = fx(vec![comment(1)], "OPEN", true, fixer());
        let job = start(&f).await;
        let id = job["id"].as_str().unwrap().to_string();
        let body = json!({ "replies": [{ "commentId": 1, "text": "Fixed." }] });
        f.feature.approve(job, FeatureContext { body: Some(body), ..Default::default() }).await.unwrap();
        let done = f.store.get(&id).unwrap();
        assert_eq!(done["status"], "approved");
        assert_eq!(done["data"]["repliesFailed"][0]["error"], "host said no");
        assert_eq!(sh(&f.origin, &["show", "feature:a.txt"]), "the");
    }

    #[tokio::test]
    async fn a_tree_changed_after_the_review_is_refused_until_the_diff_is_refreshed() {
        let f = fx(vec![comment(1)], "OPEN", false, fixer());
        let job = start(&f).await;
        let (id, wt) = (job["id"].as_str().unwrap().to_string(), job["data"]["worktree"]["dir"].as_str().unwrap().to_string());
        std::fs::write(Path::new(&wt).join("a.txt"), "sneaky\n").unwrap();
        let err = f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap_err().to_string();
        assert!(!err.is_empty());
        let failed = f.store.get(&id).unwrap();
        assert_eq!(failed["status"], "failed");
        let refreshed = f.feature.refresh_diff(failed, FeatureContext::default()).await.unwrap();
        assert_eq!(refreshed["status"], "awaiting-approval");
        assert!(refreshed["result"]["summary"].as_str().unwrap().starts_with("Refreshed diff. "));
        f.feature.approve(refreshed, FeatureContext::default()).await.unwrap();
        assert_eq!(sh(&f.origin, &["show", "feature:a.txt"]), "sneaky");
    }

    #[tokio::test]
    async fn rejecting_discards_the_worktree_and_pushes_nothing() {
        let f = fx(vec![comment(1)], "OPEN", false, fixer());
        let job = start(&f).await;
        let (id, wt) = (job["id"].as_str().unwrap().to_string(), job["data"]["worktree"]["dir"].as_str().unwrap().to_string());
        let before = sh(&f.origin, &["rev-parse", "feature"]);
        f.feature.reject(job, FeatureContext::default()).await.unwrap();
        assert_eq!(f.store.get(&id).unwrap()["status"], "rejected");
        assert!(!Path::new(&wt).exists());
        assert_eq!(sh(&f.origin, &["rev-parse", "feature"]), before);
        assert!(f.feature.reject(f.store.get(&id).unwrap(), FeatureContext::default()).await.is_err());
    }

    #[tokio::test]
    async fn nothing_to_address_a_closed_pr_and_a_fork_pr_fail_before_the_agent_runs() {
        let none = fx(vec![], "OPEN", false, fixer());
        let job = start(&none).await;
        assert_eq!(job["status"], "failed");
        assert!(job["error"].as_str().unwrap().starts_with("No open review comments to address"));

        let closed = fx(vec![comment(1)], "MERGED", false, fixer());
        let job = start(&closed).await;
        assert_eq!(job["error"], "Pull request #7 is merged \u{2014} there's nothing to push to.");

        let fork = fx(vec![comment(1)], "OPEN", false, fixer());
        fork.api.pr.lock().unwrap()["fromRepo"] = json!({ "projectKey": "~ME", "slug": "app" });
        let job = start(&fork).await;
        assert!(job["error"].as_str().unwrap().starts_with("PRs from forks aren't supported yet"), "{job}");
    }

    #[tokio::test]
    async fn a_branch_that_moved_since_the_host_reported_it_is_refused() {
        let f = fx(vec![comment(1)], "OPEN", false, fixer());
        f.api.pr.lock().unwrap()["fromSha"] = json!("0000000000000000000000000000000000000000");
        let job = start(&f).await;
        assert_eq!(job["status"], "failed");
        assert!(job["error"].as_str().unwrap().starts_with("The PR's source branch moved"), "{job}");
    }

    #[tokio::test]
    async fn the_only_action_is_confirm_repo_and_a_bad_payload_is_refused() {
        let f = fx(vec![comment(1)], "OPEN", false, fixer());
        assert_eq!(f.feature.action_names(), ["confirm-repo"]);
        assert!(f.feature.start(json!({ "project": "ACME" }), FeatureContext::default()).await.is_err());
        let e = f.feature.run_action("confirm-repo", json!({ "id": "x" }), FeatureContext::default()).await.unwrap_err();
        assert_eq!(e.to_string(), "Nothing is waiting for a repository answer.");
    }

    #[tokio::test]
    async fn a_job_that_is_not_running_cannot_be_cancelled() {
        let f = fx(vec![comment(1)], "OPEN", false, fixer());
        let job = start(&f).await;
        assert!(f.feature.cancel(job, FeatureContext::default()).await.unwrap_err().to_string().contains("Cannot cancel job in status"));
    }
}
