//! Review PR (port of `features/review-in-editor/index.ts`): checks the pull request's branch out in
//! the repo's review worktree (or reviews it where it is already checked out) and opens the
//! configured editor on it, with an AI review started where the editor can do that. It runs to
//! completion inside `start` (there is nothing to approve: you review the checkout yourself, in
//! your own editor), and never rejects: any failure lands the job in "failed" so the extension's
//! one-shot flow gets `job.error` back through the normal 200 answer.
//!
//! The exception is a review still live in the repo's single review worktree: the same PR switches
//! to it (its job, terminal brought to the front), and a different one is refused with a 409 unless
//! the request names that review as the one to cancel first, since checking out would change the
//! files under its agent. Starts are serialized per worktree, so two quick clicks can't both check
//! out.

use super::plan::{
    assert_payload, build_conflict, build_review_prompt, build_review_result, build_summary, build_switched_result, build_terminal_header, build_terminal_title, decide_reuse, decide_start,
    parse_dirty_files, parse_replace_job_id, pr_key, HeaderInput, ReviewPromptInput, ReviewResultInput, Reuse,
};
use crate::config::ConfigFn;
use crate::editor::{EditorId, Editors, ReviewSession};
use crate::git_cli::{Git, GitOpts};
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore, SessionStatus};
use crate::prereqs::{resolve_review_editor, GitRemoteRules};
use crate::review_memory::{read_review, write_review};
use crate::review_session as rs;
use crate::review_worktree::{is_registered_worktree, provision_review_worktree, review_worktree_path};
use crate::terminal::TerminalOptions;
use crate::terminal_focus::{focus_terminal, terminal_location_from_value};
use crate::work_unit_record::{pr_key_for, record_work_unit, Unit};
use crate::work_units::WorkUnits;
use crate::workspace_resolver::{resolve_workspace, Workspace};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// "PROJECT", "repo" -> the local clone, or why there is none.
pub type RepoResolver = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// How long a stopped review gets to wind down before a replace gives up.
const STOP_GRACE: Duration = Duration::from_millis(2000);
const REPLACE_WAIT: Duration = Duration::from_millis(5000);
const REPLACE_POLL: Duration = Duration::from_millis(200);

pub struct ReviewDeps<G> {
    pub store: Arc<JobStore>,
    pub git: G,
    pub config: ConfigFn,
    pub repo_path: RepoResolver,
    pub home: PathBuf,
    /// Where status files and the worktrees' session records live (`review_session::state_dir`).
    pub sessions_dir: PathBuf,
    /// Where the remembered reviews live (`review_memory`'s folder).
    pub memory_dir: PathBuf,
    pub work_units: Arc<WorkUnits>,
    pub rules: GitRemoteRules,
    pub editors: Editors,
    /// The profile's permission mode for this feature's Claude Code sessions.
    pub permission_mode: String,
    /// The `osascript` used to bring a terminal to the front.
    pub osascript: String,
}

pub struct ReviewInEditor<G> {
    d: ReviewDeps<G>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub fn create_review_in_editor_feature<G: Git + 'static>(d: ReviewDeps<G>) -> Arc<ReviewInEditor<G>> {
    Arc::new(ReviewInEditor { d, locks: Mutex::new(HashMap::new()) })
}

fn patch(store: &JobStore, id: &str, v: Value) -> Value {
    store.update(id, v.as_object().cloned().unwrap_or_default()).unwrap_or(Value::Null)
}

fn editor_label(editor: EditorId) -> &'static str {
    match editor {
        EditorId::Vscode => "VS Code",
        EditorId::Cursor => "Cursor",
        EditorId::ClaudeCode => "Claude Code",
    }
}

fn session_status_of(s: &rs::Status) -> SessionStatus {
    SessionStatus { state: s.state.to_string(), exit_code: s.exit_code }
}

fn session_status_from_json(v: &Value) -> SessionStatus {
    SessionStatus { state: v["state"].as_str().unwrap_or("ended").to_string(), exit_code: v["exitCode"].as_i64() }
}

impl<G: Git + 'static> ReviewInEditor<G> {
    fn lock_for(&self, dir: &str) -> Arc<tokio::sync::Mutex<()>> {
        let key = crate::node_path::resolve(dir);
        self.locks.lock().unwrap().entry(key).or_default().clone()
    }

    async fn git_out(&self, args: &[&str], cwd: &Path, allow_failure: bool) -> Result<crate::git_cli::GitOut, String> {
        self.d.git.run(args, cwd, GitOpts::default().with_allow_failure(allow_failure)).await
    }

    /// The job for a live session record, recreated from the record if the service has restarted
    /// since the review started.
    fn job_for_session(&self, record: &Value) -> Value {
        let now = crate::job_files::now_ms();
        let job_id = record["jobId"].as_str().unwrap_or("");
        let file = rs::status_file_for(job_id, &self.d.sessions_dir).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        self.d.store.restore(json!({
            "id": job_id, "featureId": "review-in-editor", "status": "approved",
            "data": { "payload": record["payload"], "session": { "file": file, "worktree": record["worktree"] } },
            "result": { "summary": record["summary"], "files": [], "review": record["review"] },
            "createdAt": now, "updatedAt": now,
        }))
    }

    /// Puts back the jobs of reviews still running when the service stopped, so their panels
    /// (reopened after a reload) keep working; forgets the rest. Call once at startup.
    pub async fn restore_live_sessions(&self) {
        for record in rs::list_session_records(&self.d.sessions_dir) {
            let Some(job_id) = record["jobId"].as_str() else { continue };
            let Ok(file) = rs::status_file_for(job_id, &self.d.sessions_dir) else { continue };
            if rs::is_live(&rs::read_status(&file, crate::job_files::now_ms()).await) {
                self.job_for_session(&record);
            } else {
                rs::remove_session_record(record["worktree"].as_str().unwrap_or(""), job_id, &self.d.sessions_dir);
                let _ = std::fs::remove_file(&file);
            }
        }
    }

    /// /start's answer for the PR whose review is already live: that review's job, with a result
    /// saying so, after bringing its terminal to the front. The stored job is left as it was.
    async fn switch_to_live_session(&self, record: &Value) -> Value {
        let mut job = self.job_for_session(record);
        let mut focus = "unknown".to_string();
        if let Some(loc) = terminal_location_from_value(&record["terminal"]) {
            focus = match focus_terminal(&loc, &self.d.osascript).await {
                Ok(true) => "focused".into(),
                Ok(false) => "missing".into(),
                Err(e) => e,
            };
        }
        let switched = build_switched_result(record, &focus);
        job["result"] = json!({ "summary": switched["summary"], "files": [], "review": switched["review"] });
        job
    }

    /// Cancels the live review the user chose to replace and waits for it to stop; fails, before
    /// anything is checked out, if it doesn't.
    async fn replace_live_session(&self, record: &Value, status: &rs::Status) -> Result<(), String> {
        if status.state != "running" {
            return Err("The other review is still starting, so it can't be cancelled yet. Try again in a moment.".into());
        }
        let file = rs::status_file_for(record["jobId"].as_str().unwrap_or(""), &self.d.sessions_dir)?;
        let stop_error = rs::stop_session(&file, STOP_GRACE).await.err();
        if rs::is_live(&rs::wait_until_stopped(&file, REPLACE_WAIT, REPLACE_POLL).await) {
            return Err(format!("{}The other review didn't stop, so this pull request wasn't checked out. Close its terminal window, then try again.", stop_error.map(|e| format!("{e} ")).unwrap_or_default()));
        }
        Ok(())
    }

    async fn run_review(&self, repo: &str, review_dir: &str, payload: &Value, job_id: &str) -> Result<(String, Value), String> {
        let (repo_path, home) = (Path::new(repo), self.d.home.as_path());
        let source_branch = payload["sourceBranch"].as_str().unwrap_or("").to_string();
        let mut dir = review_dir.to_string();
        if !is_registered_worktree(repo_path, Path::new(&dir), &self.d.git).await {
            // A repo added *after* setup never went through provisioning: do it now, on demand,
            // rather than failing.
            provision_review_worktree(repo_path, home, &self.d.git).await?;
        }

        self.git_out(&["fetch", "origin", "--prune"], repo_path, false).await?;

        // The fetch above pruned it if the PR was merged and its branch deleted; say so rather than
        // surface git's "not a commit" error from the checkout.
        let remote_ref = self.git_out(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{source_branch}")], repo_path, true).await?;
        if remote_ref.code != 0 {
            return Err(format!("The branch \"{source_branch}\" no longer exists on the remote, so there is nothing to check out. This usually means the pull request was merged (or declined) and its branch deleted."));
        }

        // The branch is already checked out somewhere else (your own clone or a worktree of yours):
        // review it there as it is, nothing is created and nothing is touched. Only otherwise does
        // the shared review worktree take the branch.
        let found = resolve_workspace(repo_path, Some(&source_branch), None, &self.d.git).await?;
        let existing = match found {
            Workspace::Worktree { dir: found_dir, .. } if crate::node_path::resolve(&found_dir) != crate::node_path::resolve(&dir) => Some(found_dir),
            _ => None,
        };
        let mut dirty_files: Vec<String> = vec![];
        if let Some(existing) = existing {
            dir = existing;
        } else {
            let status = self.git_out(&["status", "--porcelain"], Path::new(&dir), false).await?.stdout;
            dirty_files = parse_dirty_files(&status);
            if !dirty_files.is_empty() {
                self.git_out(&["stash", "push", "-u", "-m", &format!("review-in-editor {source_branch}")], Path::new(&dir), false).await?;
            }
            self.git_out(&["checkout", "-B", &source_branch, "--track", &format!("origin/{source_branch}")], Path::new(&dir), false).await?;
        }
        let pr_id = payload["prId"].as_f64().map(|n| n as u64);
        let unit_pr_key = pr_id.map(|id| pr_key_for(self.d.rules, payload["project"].as_str().unwrap_or(""), payload["repo"].as_str().unwrap_or(""), id));
        record_work_unit(&self.d.work_units, &Unit { repo_path: repo, branch: &source_branch, dir: Some(&dir), issue_key: None, pr_key: unit_pr_key.as_deref() });

        let config = (self.d.config)();
        let editor = resolve_review_editor(config["reviewEditor"].as_str()).and_then(EditorId::parse).ok_or("No editor configured for Review PR, and none could be auto-detected on this machine. Run `npm run setup` again to pick one.")?;
        // Claude Code reviews are remembered per PR (session id + head reviewed): an unchanged head
        // reopens that session instead of reviewing again, a moved one starts a new session told
        // where the last review stopped.
        let head = remote_ref.stdout.trim().to_string();
        let key = pr_key_of(payload);
        let mut reuse = Reuse::Review { since: None };
        if editor == EditorId::ClaudeCode {
            let record = read_review(&key, &self.d.memory_dir);
            let transcript_exists = match &record {
                Some(r) if Path::new(&dir).exists() => {
                    let real = crate::resume::real_cwd_for_transcript(&dir, |p| std::fs::canonicalize(p).map(|c| c.to_string_lossy().into_owned()));
                    Path::new(&crate::resume::session_transcript_path(&self.d.home.to_string_lossy(), &real, r["sessionId"].as_str().unwrap_or(""))).exists()
                }
                _ => false,
            };
            let is_ancestor = match &record {
                Some(r) => self.git_out(&["merge-base", "--is-ancestor", r["reviewedSha"].as_str().unwrap_or(""), &head], repo_path, true).await?.code == 0,
                None => false,
            };
            reuse = decide_reuse(record.as_ref(), &head, &dir, transcript_exists, is_ancestor);
        }
        let (resume_id, since) = match &reuse {
            Reuse::Resume { session_id, .. } => (Some(session_id.clone()), None),
            Reuse::Review { since } => (None, since.clone()),
        };
        let new_id = (editor == EditorId::ClaudeCode && resume_id.is_none()).then(crate::ids::uuid_v4);
        // Only the Claude Code terminal review can be tracked; the IDE chats (Copilot Chat,
        // Cursor's Agent window) can't.
        let status_file = if matches!(editor, EditorId::Vscode | EditorId::Cursor) { None } else { Some(rs::create_status_file(job_id, &self.d.sessions_dir, crate::job_files::now_ms())?) };
        let (title, badge) = build_terminal_title(&payload["prId"], &source_branch, payload["targetBranch"].as_str());
        let prompt = build_review_prompt(&ReviewPromptInput { source_branch: &source_branch, target_branch: payload["targetBranch"].as_str(), pr_id: payload["prId"].clone(), title: payload["title"].as_str(), pr_url: payload["prUrl"].as_str(), since: since.as_deref() });
        let header = build_terminal_header(&HeaderInput { editor: editor.as_str(), dir: &dir, source_branch: &source_branch, target_branch: payload["targetBranch"].as_str(), pr_id: payload["prId"].clone(), title: payload["title"].as_str(), pr_url: payload["prUrl"].as_str() });
        let options = TerminalOptions { title: Some(title), badge: Some(badge), status_file: status_file.as_ref().map(|f| f.to_string_lossy().into_owned()), ..Default::default() };
        let launch = match self.d.editors.open_for_review(&dir, editor, &prompt, &self.d.permission_mode, &header, &options, &ReviewSession { new_id: new_id.clone(), resume_id: resume_id.clone() }).await {
            Ok(l) => l,
            Err(e) => {
                if let Some(f) = &status_file {
                    let _ = std::fs::remove_file(f);
                }
                return Err(e);
            }
        };
        let mut review_note = launch.review_note.clone();
        if let (Some(_), Reuse::Resume { reviewed_sha, reviewed_at, .. }) = (&resume_id, &reuse) {
            let at = reviewed_at.as_i64().and_then(crate::js_date::to_iso_string).unwrap_or_default();
            review_note = format!("No new commits since your last review ({}, {at}), so no new review was started. {review_note}", &reviewed_sha[..reviewed_sha.len().min(7)]);
        } else if let Some(since) = &since {
            review_note = format!("{review_note} It starts from the commits added since your last review ({}).", &since[..since.len().min(7)]);
        }
        if let (Some(id), true) = (&new_id, launch.review_started) {
            let record = json!({ "sessionId": id, "cwd": dir, "reviewedSha": head, "reviewedAt": crate::job_files::now_ms() as i64 });
            let _ = write_review(&key, &record, &self.d.memory_dir, crate::job_files::now_ms() as i64);
        }
        let tracked = status_file.is_some() && launch.review_started;
        if let (false, Some(f)) = (tracked, &status_file) {
            let _ = std::fs::remove_file(f);
        }

        let summary = build_summary(&source_branch, &dir, launch.opened_editor.then(|| editor_label(editor)), &dirty_files, Some(&review_note));
        let review = build_review_result(&ReviewResultInput { payload, dir: &dir, editor: editor.as_str(), review_started: launch.review_started, review_note: Some(&review_note), stashed_files: &dirty_files, tracked });
        if let (true, Some(f)) = (tracked, &status_file) {
            let session = json!({ "file": f.to_string_lossy(), "worktree": dir });
            let _ = self.d.store.set_data(job_id, "session", session);
            let mut record = json!({ "jobId": job_id, "worktree": dir, "payload": payload, "summary": summary, "review": review });
            if let Some(t) = &launch.terminal {
                record["terminal"] = t.to_json();
            }
            let _ = rs::write_session_record(&record, &self.d.sessions_dir);
        }
        Ok((summary, review))
    }
}

fn pr_key_of(payload: &Value) -> String {
    pr_key(payload["project"].as_str().unwrap_or(""), payload["repo"].as_str().unwrap_or(""), &payload["prId"], payload["sourceBranch"].as_str().unwrap_or(""))
}

#[async_trait]
impl<G: Git + 'static> Feature for ReviewInEditor<G> {
    fn id(&self) -> &str {
        "review-in-editor"
    }

    fn label(&self) -> &str {
        "Review PR"
    }

    async fn start(&self, payload_raw: Value, _ctx: FeatureContext) -> FeatureResult<Value> {
        let payload = assert_payload(&payload_raw)?.to_json();
        let replace_job_id = parse_replace_job_id(&payload_raw);
        let (project, repo_name) = (payload["project"].as_str().unwrap_or(""), payload["repo"].as_str().unwrap_or(""));
        let data = || Map::from_iter([("payload".to_string(), payload.clone())]);
        let repo = match (self.d.repo_path)(project, repo_name) {
            Ok(r) => r,
            Err(e) => {
                let job = self.d.store.create("review-in-editor", data());
                return Ok(patch(&self.d.store, job["id"].as_str().unwrap_or(""), json!({ "status": "failed", "error": e })));
            }
        };
        let dir = review_worktree_path(Path::new(&repo), &self.d.home).to_string_lossy().into_owned();
        let lock = self.lock_for(&dir);
        let _guard = lock.lock().await;
        let live = rs::find_live_session(&dir, &self.d.sessions_dir).await;
        let decision = decide_start(&payload, live.as_ref().map(|(r, _)| r), replace_job_id.as_deref());
        if let Some((record, status)) = &live {
            if decision == "reuse" {
                return Ok(self.switch_to_live_session(record).await);
            }
            if decision == "conflict" {
                let conflict = build_conflict(record, status.state);
                let existing = Map::from_iter([("existing".to_string(), conflict["existing"].clone())]);
                return Err(FeatureError::refused(conflict["error"].as_str().unwrap_or(""), conflict["code"].as_str().unwrap_or(""), existing));
            }
        }
        let job = self.d.store.create("review-in-editor", data());
        let id = job["id"].as_str().unwrap_or("").to_string();
        let outcome = async {
            if let (Some((record, status)), "replace") = (&live, decision) {
                self.replace_live_session(record, status).await?;
            }
            self.run_review(&repo, &dir, &payload, &id).await
        }
        .await;
        Ok(match outcome {
            Ok((summary, review)) => patch(&self.d.store, &id, json!({ "status": "approved", "result": { "summary": summary, "files": [], "review": review } })),
            Err(e) => patch(&self.d.store, &id, json!({ "status": "failed", "error": e })),
        })
    }

    /// A terminal-based review is tracked through its status file until it stops; the final status
    /// is then kept on the job and the file (and the worktree's record of it) deleted.
    async fn session_status(&self, job: &Value) -> Option<SessionStatus> {
        let session = job["data"]["session"].as_object()?;
        if let Some(f) = session.get("final").filter(|f| f.is_object()) {
            return Some(session_status_from_json(f));
        }
        let file = PathBuf::from(session.get("file")?.as_str()?);
        let status = rs::read_status(&file, crate::job_files::now_ms()).await;
        if !rs::is_live(&status) {
            let id = job["id"].as_str().unwrap_or("");
            let mut kept = job["data"]["session"].clone();
            kept["final"] = status.to_json();
            let _ = self.d.store.set_data(id, "session", kept);
            let _ = std::fs::remove_file(&file);
            rs::remove_session_record(session.get("worktree").and_then(Value::as_str).unwrap_or(""), id, &self.d.sessions_dir);
        }
        Some(session_status_of(&status))
    }

    async fn stop_session(&self, job: Value) -> FeatureResult<()> {
        let session = &job["data"]["session"];
        let file = session["file"].as_str();
        let (Some(file), false) = (file, session["final"].is_object()) else { return Err(FeatureError::Other("This review isn't running.".into())) };
        rs::stop_session(Path::new(file), STOP_GRACE).await?;
        Ok(())
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::Other(format!("Cannot approve job in status \"{}\" \u{2014} review-in-editor has no approve step", job["status"].as_str().unwrap_or(""))))
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::Other(format!("Cannot reject job in status \"{}\" \u{2014} review-in-editor has no reject step", job["status"].as_str().unwrap_or(""))))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::git_cli::GitOut;
    use std::future::Future;
    use std::os::unix::fs::PermissionsExt;

    type Handler = dyn Fn(&[&str], &Path) -> Result<GitOut, String> + Send + Sync;

    /// A `git` answering from a script and remembering what it was asked.
    struct FakeGit {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        handler: Box<Handler>,
    }

    impl Git for FakeGit {
        fn run(&self, args: &[&str], cwd: &Path, _opts: GitOpts) -> impl Future<Output = Result<GitOut, String>> + Send {
            self.calls.lock().unwrap().push(args.iter().map(|s| s.to_string()).collect());
            let out = (self.handler)(args, cwd);
            async move { out }
        }
    }

    fn ok(stdout: &str) -> Result<GitOut, String> {
        Ok(GitOut { stdout: stdout.into(), stderr: String::new(), code: 0 })
    }

    struct Fx {
        _d: tempfile::TempDir,
        feature: Arc<ReviewInEditor<FakeGit>>,
        store: Arc<JobStore>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        sessions: PathBuf,
        review_dir: PathBuf,
        repo: PathBuf,
        opened: PathBuf,
    }

    const PR: fn() -> Value = || json!({ "project": "ACME", "repo": "sample-app", "prId": 42, "sourceBranch": "feature/x", "targetBranch": "master", "title": "Fix login" });

    /// `dirty`: what `git status --porcelain` says in the review worktree; `remote`: whether the
    /// branch still exists on origin; `elsewhere`: a worktree that already has the branch.
    fn fx(dirty: &str, remote: bool, elsewhere: bool, editor: &str) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let home = root.join("home");
        let repo = root.join("sample-app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let review_dir = review_worktree_path(&repo, &home);
        std::fs::create_dir_all(&review_dir).unwrap();
        let elsewhere = elsewhere.then(|| root.join("elsewhere").to_string_lossy().into_owned());
        if let Some(e) = &elsewhere {
            std::fs::create_dir_all(e).unwrap();
        }
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // `open` stands in for the macOS launcher and records what it was asked to open. Only Cursor
        // is used: the review editor is never auto-detected here, which could open a real terminal.
        let opened = root.join("opened.txt");
        let open = root.join("open");
        std::fs::write(&open, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", opened.display())).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let editors = Editors { open_command: open.to_string_lossy().into_owned(), code_command: "/no/code".into(), vscode_bundled_cli: "/no/bundled-code".into(), terminal: crate::terminal::Terminal { osascript: "/no/osascript".into(), iterm_app: "/no/iterm".into(), require_macos: false, ..crate::terminal::Terminal::default() }, cursor_window_settle: Duration::ZERO, require_macos: false };

        let (rd, dirty) = (review_dir.to_string_lossy().into_owned(), dirty.to_string());
        let calls = Arc::new(Mutex::new(vec![]));
        let git = FakeGit {
            calls: calls.clone(),
            handler: Box::new(move |args, _cwd| match args {
                ["worktree", "list", ..] => ok(&format!("worktree {rd}\nHEAD abc\nbranch refs/heads/old\n\n{}", elsewhere.as_ref().map(|e| format!("worktree {e}\nHEAD def\nbranch refs/heads/feature/x\n\n")).unwrap_or_default())),
                ["rev-parse", "--verify", ..] if remote => ok("0123456789abcdef0123456789abcdef01234567\n"),
                ["rev-parse", "--verify", ..] => Ok(GitOut { stdout: String::new(), stderr: String::new(), code: 1 }),
                ["status", "--porcelain"] => ok(&dirty),
                ["status", "--porcelain", ..] => ok(""),
                _ => ok(""),
            }),
        };
        let store = Arc::new(JobStore::new());
        let config = json!({ "reviewEditor": editor });
        let repo_s = repo.to_string_lossy().into_owned();
        let feature = create_review_in_editor_feature(ReviewDeps {
            store: store.clone(),
            git,
            config: Arc::new(move || config.clone()),
            repo_path: Arc::new(move |p, r| if (p, r) == ("ACME", "sample-app") { Ok(repo_s.clone()) } else { Err(format!("No local clone of {p}/{r}.")) }),
            home,
            sessions_dir: sessions.clone(),
            memory_dir: root.join("memory"),
            work_units: Arc::new(WorkUnits::open(root.join("work-units.json"))),
            rules: GitRemoteRules::BitbucketDc,
            editors,
            permission_mode: "plan".into(),
            osascript: "/no/osascript".into(),
        });
        Fx { _d: d, feature, store, calls, sessions, review_dir, repo, opened }
    }

    async fn start(f: &Fx, payload: Value) -> FeatureResult<Value> {
        f.feature.start(payload, FeatureContext::default()).await
    }

    fn ran(f: &Fx, prefix: &[&str]) -> bool {
        f.calls.lock().unwrap().iter().any(|c| c.len() >= prefix.len() && c.iter().zip(prefix).all(|(a, b)| a == b))
    }

    #[tokio::test]
    async fn a_bad_payload_is_refused_and_a_repo_with_no_clone_fails_the_job() {
        let f = fx("", true, false, "cursor");
        assert!(start(&f, json!({ "project": "ACME" })).await.is_err());
        let job = start(&f, json!({ "project": "ACME", "repo": "nope", "sourceBranch": "x" })).await.unwrap();
        assert_eq!((job["status"].clone(), job["error"].clone()), (json!("failed"), json!("No local clone of ACME/nope.")));
        assert!(f.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_branch_is_checked_out_in_the_review_worktree_and_the_ide_review_started() {
        let f = fx("", true, false, "cursor");
        let job = start(&f, PR()).await.unwrap();
        assert_eq!(job["status"], "approved", "{job}");
        assert!(ran(&f, &["fetch", "origin", "--prune"]));
        assert!(ran(&f, &["checkout", "-B", "feature/x", "--track", "origin/feature/x"]));
        assert!(!ran(&f, &["stash"]));
        let dir = f.review_dir.to_string_lossy().into_owned();
        assert!(job["result"]["summary"].as_str().unwrap().starts_with(&format!("Checked out feature/x in {dir} and opened it in Cursor.")), "{job}");
        assert_eq!(job["result"]["review"]["reviewStarted"], true);
        assert!(std::fs::read_to_string(&f.opened).unwrap().contains("cursor://"));
        // An IDE chat can't be tracked.
        assert_eq!(job["data"]["session"], Value::Null);
        assert!(f.feature.session_status(&job).await.is_none());
    }

    #[tokio::test]
    async fn uncommitted_changes_in_the_review_worktree_are_stashed_before_the_checkout() {
        let f = fx(" M src/a.rs\n?? notes.txt\n", true, false, "cursor");
        let job = start(&f, PR()).await.unwrap();
        assert_eq!(job["status"], "approved", "{job}");
        assert!(ran(&f, &["stash", "push", "-u", "-m", "review-in-editor feature/x"]));
        let stash = f.calls.lock().unwrap().iter().position(|c| c[0] == "stash").unwrap();
        let checkout = f.calls.lock().unwrap().iter().position(|c| c[0] == "checkout").unwrap();
        assert!(stash < checkout);
        assert!(job["result"]["summary"].as_str().unwrap().contains("were stashed first"));
    }

    #[tokio::test]
    async fn a_branch_deleted_on_the_remote_fails_the_job_without_touching_the_worktree() {
        let f = fx("", false, false, "cursor");
        let job = start(&f, PR()).await.unwrap();
        assert_eq!(job["status"], "failed");
        assert!(job["error"].as_str().unwrap().contains("no longer exists on the remote"), "{job}");
        assert!(!ran(&f, &["checkout"]) && !ran(&f, &["stash"]));
    }

    #[tokio::test]
    async fn a_branch_already_checked_out_elsewhere_is_reviewed_there_untouched() {
        let f = fx("", true, true, "cursor");
        let job = start(&f, PR()).await.unwrap();
        assert_eq!(job["status"], "approved", "{job}");
        assert!(job["result"]["summary"].as_str().unwrap().contains("/elsewhere"), "{job}");
        assert!(!ran(&f, &["checkout"]) && !ran(&f, &["stash"]));
    }

    #[tokio::test]
    async fn the_review_has_no_approve_or_reject_step_and_an_untracked_job_has_no_session_to_stop() {
        let f = fx("", true, false, "cursor");
        let job = start(&f, PR()).await.unwrap();
        assert!(f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap_err().to_string().contains("no approve step"));
        assert!(f.feature.reject(job.clone(), FeatureContext::default()).await.unwrap_err().to_string().contains("no reject step"));
        assert_eq!(f.feature.stop_session(job).await.unwrap_err().to_string(), "This review isn't running.");
    }

    #[tokio::test]
    async fn a_final_status_kept_on_the_job_is_reported_without_reading_the_file() {
        let f = fx("", true, false, "cursor");
        let job = f.store.create("review-in-editor", Map::new());
        let id = job["id"].as_str().unwrap();
        f.store.set_data(id, "session", json!({ "file": "/nope", "worktree": "/wt", "final": { "state": "finished", "exitCode": 0 } })).unwrap();
        let s = f.feature.session_status(&f.store.get(id).unwrap()).await.unwrap();
        assert_eq!((s.state.as_str(), s.exit_code), ("finished", Some(0)));
        assert!(f.feature.stop_session(f.store.get(id).unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn a_dead_sessions_record_is_forgotten_on_restore() {
        let f = fx("", true, false, "cursor");
        let job_id = "0a1b2c3d-0000-4000-8000-000000000001";
        let wt = f.review_dir.to_string_lossy().into_owned();
        rs::write_session_record(&json!({ "jobId": job_id, "worktree": wt, "payload": PR(), "summary": "s", "review": {} }), &f.sessions).unwrap();
        assert!(rs::read_session_record(&wt, &f.sessions).is_some());
        f.feature.restore_live_sessions().await;
        assert!(rs::read_session_record(&wt, &f.sessions).is_none());
        assert!(f.store.get(job_id).is_none());
        let _ = &f.repo;
    }
}
