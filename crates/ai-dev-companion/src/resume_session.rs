//! Resume a recorded headless Claude session (a job's `claudeSession`) in a
//! terminal (port of `core/resume-session.ts`): the one implementation behind
//! `POST /jobs/:jobId/open-in-claude-code {resume: true}` and the ticket
//! workspace's "Resume Claude session". The session is only ever server-held job
//! data, re-validated here before anything is built from it.

use crate::node_path;
use crate::paths::state_dir;
use crate::resume::{build_resume_args, real_cwd, session_transcript_path, validate_session};
use crate::session_cwd::{session_cwd_allowed, SessionCwdRules};
use crate::terminal::{HeaderSegment, Terminal, TerminalOptions};
use crate::terminal_focus::TerminalLocation;
use crate::worktree::worktree_root;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeOutcome {
    Resumed,
    /// Not a session this service would have recorded, including one whose folder isn't a mapped
    /// repo, a ticket worktree or a sessions folder (a hand-edited job file or history row must
    /// not open a terminal in an attacker's folder).
    Invalid,
    /// Its folder or transcript no longer exists (Claude Code cleans old ones up).
    Gone,
}

/// `job_worktree_dir`: the job's own worktree, also allowed when the session ran in it.
pub async fn resume_session_in_terminal(terminal: &Terminal, config: &Value, session: &Value, header: &[Vec<HeaderSegment>], job_worktree_dir: Option<&str>, home: &Path) -> Result<ResumeOutcome, String> {
    resume_session_located(terminal, config, session, header, job_worktree_dir, home).await.map(|(outcome, _)| outcome)
}

/// `resume_session_in_terminal`, plus where the terminal opened (when it said), so the caller can
/// record it for "bring this terminal to the front".
pub async fn resume_session_located(terminal: &Terminal, config: &Value, session: &Value, header: &[Vec<HeaderSegment>], job_worktree_dir: Option<&str>, home: &Path) -> Result<(ResumeOutcome, Option<TerminalLocation>), String> {
    if !validate_session(Some(session)) {
        return Ok((ResumeOutcome::Invalid, None));
    }
    let (id, cwd, mode) = (session["id"].as_str().unwrap_or(""), session["cwd"].as_str().unwrap_or(""), session["permissionMode"].as_str().unwrap_or(""));
    let repo_paths: Vec<String> = config["repos"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let state = state_dir(home);
    let (sessions, checkouts, worktrees) = (state.join("sessions").to_string_lossy().into_owned(), state.join("repos").to_string_lossy().into_owned(), worktree_root(home).to_string_lossy().into_owned());
    let own_worktree = job_worktree_dir.is_some_and(|d| node_path::resolve(d) == node_path::resolve(cwd));
    let allowed = own_worktree
        || session_cwd_allowed(Some(cwd), &SessionCwdRules { repo_paths: &repo_paths, sessions_root: Some(&sessions), checkouts_root: Some(&checkouts), worktrees_root: Some(&worktrees) }, home);
    if !allowed {
        return Ok((ResumeOutcome::Invalid, None));
    }
    // Only the transcript lookup needs the realpath'd cwd (Claude Code keys
    // ~/.claude/projects/<slug>/ by it); the session runs in the cwd as captured.
    let transcript = session_transcript_path(&home.to_string_lossy(), &real_cwd(cwd), id);
    if !Path::new(cwd).exists() || !Path::new(&transcript).exists() {
        return Ok((ResumeOutcome::Gone, None));
    }
    let location = terminal.open_claude_code(cwd, &build_resume_args(id, mode), header, &TerminalOptions::default()).await?;
    Ok((ResumeOutcome::Resumed, location))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "123e4567-e89b-42d3-a456-426614174000";

    struct Fx {
        _d: tempfile::TempDir,
        home: std::path::PathBuf,
        repo: String,
        terminal: Terminal,
        log: std::path::PathBuf,
    }

    fn fx() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let (home, repo) = (root.join("home"), root.join("repo"));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let log = root.join("typed.txt");
        let osa = root.join("osascript");
        fs::write(&osa, format!("#!/bin/sh\nprintf '%s' \"$2\" > '{}'\necho ''\n", log.display())).unwrap();
        fs::set_permissions(&osa, fs::Permissions::from_mode(0o755)).unwrap();
        let tmp = root.join("tmp");
        fs::create_dir(&tmp).unwrap();
        let terminal = Terminal { osascript: osa.to_string_lossy().into_owned(), iterm_app: "/no/iterm".into(), tmp_dir: tmp, require_macos: false };
        let repo = repo.to_string_lossy().into_owned();
        Fx { _d: d, home, repo, terminal, log }
    }

    fn transcript(f: &Fx, cwd: &str) {
        let p = session_transcript_path(&f.home.to_string_lossy(), &real_cwd(cwd), ID);
        fs::create_dir_all(Path::new(&p).parent().unwrap()).unwrap();
        fs::write(p, "{}\n").unwrap();
    }

    fn session(cwd: &str) -> Value {
        json!({ "id": ID, "cwd": cwd, "permissionMode": "plan" })
    }

    #[tokio::test]
    async fn a_session_in_a_mapped_repo_with_its_transcript_resumes_in_a_terminal() {
        let f = fx();
        transcript(&f, &f.repo);
        let config = json!({ "repos": { "app": f.repo } });
        let r = resume_session_in_terminal(&f.terminal, &config, &session(&f.repo), &[], None, &f.home).await.unwrap();
        assert_eq!(r, ResumeOutcome::Resumed);
        assert!(fs::read_to_string(&f.log).unwrap().contains("do script"));
        let script = fs::read_dir(&f.terminal.tmp_dir).unwrap().next().unwrap().unwrap().path().join("review.sh");
        assert!(fs::read_to_string(script).unwrap().contains(&format!("'claude' '--resume' '{ID}' '--permission-mode' 'plan'")));
    }

    #[tokio::test]
    async fn a_session_that_fails_validation_or_runs_elsewhere_is_invalid_and_opens_nothing() {
        let f = fx();
        let config = json!({ "repos": { "app": f.repo } });
        let other = tempfile::tempdir().unwrap();
        let other = other.path().to_string_lossy().into_owned();
        transcript(&f, &other);
        for bad in [json!({ "id": "not-a-uuid", "cwd": f.repo, "permissionMode": "plan" }), json!({ "id": ID, "cwd": "relative", "permissionMode": "plan" }), json!({ "id": ID, "cwd": f.repo, "permissionMode": "bypassPermissions" }), session(&other), json!(null)] {
            assert_eq!(resume_session_in_terminal(&f.terminal, &config, &bad, &[], None, &f.home).await.unwrap(), ResumeOutcome::Invalid, "{bad}");
        }
        assert!(!f.log.exists(), "no terminal was opened");
        // The job's own worktree is allowed even though no mapped repo covers it.
        assert_eq!(resume_session_in_terminal(&f.terminal, &config, &session(&other), &[], Some(&other), &f.home).await.unwrap(), ResumeOutcome::Resumed);
    }

    #[tokio::test]
    async fn a_missing_folder_or_transcript_is_gone() {
        let f = fx();
        let config = json!({ "repos": { "app": f.repo } });
        assert_eq!(resume_session_in_terminal(&f.terminal, &config, &session(&f.repo), &[], None, &f.home).await.unwrap(), ResumeOutcome::Gone, "no transcript");
        let gone = format!("{}/vanished", f.repo);
        assert_eq!(resume_session_in_terminal(&f.terminal, &config, &session(&gone), &[], Some(&gone), &f.home).await.unwrap(), ResumeOutcome::Gone, "no folder");
        assert!(!f.log.exists());
    }

    #[tokio::test]
    async fn a_terminal_failure_is_an_error() {
        let f = fx();
        transcript(&f, &f.repo);
        let config = json!({ "repos": { "app": f.repo } });
        let broken = Terminal { osascript: "/no/osascript".into(), ..f.terminal.clone() };
        assert!(resume_session_in_terminal(&broken, &config, &session(&f.repo), &[], None, &f.home).await.is_err());
    }
}
