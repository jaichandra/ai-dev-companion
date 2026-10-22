//! Provisions (or verifies) every configured repo's dedicated PR-review worktree (port of
//! `provision-review-worktrees.js`; see `review_worktree` for what that means). Run detached right
//! after setup enables "review-in-editor", so the wizard returns to the prompt instead of waiting
//! on a fetch and checkout of every repo, and safe to re-run by hand: provisioning is idempotent.

use crate::git_cli::SystemGit;
use crate::review_worktree::provision_review_worktree;
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Provisions one repo's review worktree: where it is and whether this call created it.
#[async_trait]
pub trait Provisioner: Send + Sync {
    async fn provision(&self, repo_path: &str) -> Result<(String, bool), String>;
}

/// The real one: `git`, with worktrees under `home`'s state folder.
pub struct SystemProvisioner {
    pub home: PathBuf,
}

#[async_trait]
impl Provisioner for SystemProvisioner {
    async fn provision(&self, repo_path: &str) -> Result<(String, bool), String> {
        let p = provision_review_worktree(Path::new(repo_path), &self.home, &SystemGit).await?;
        Ok((p.dir.to_string_lossy().into_owned(), p.created))
    }
}

/// The config at `path` if it is a usable JSON object, else `None` (a missing and a malformed file
/// are treated alike, as the wizard does).
pub fn load_existing_config(path: &Path) -> Option<Value> {
    let parsed: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    parsed.is_object().then_some(parsed)
}

/// One line per repo, `[review-worktrees] OK|FAIL - ...`. Always 0: a repo that fails is a line,
/// not a failure of the run.
pub async fn run_provision(config: Option<&Value>, provisioner: &dyn Provisioner, log: &mut dyn FnMut(&str)) -> i32 {
    let Some(config) = config else {
        log("[review-worktrees] No usable config.json found yet \u{2014} run `companion setup` first.");
        return 0;
    };
    let repos: Vec<(&String, &Value)> = config["repos"].as_object().map(|m| m.iter().collect()).unwrap_or_default();
    if repos.is_empty() {
        log("[review-worktrees] No repos configured \u{2014} nothing to provision.");
        return 0;
    }
    for (key, repo_path) in repos {
        let path = repo_path.as_str().unwrap_or("");
        match provisioner.provision(path).await {
            Ok((dir, created)) => log(&format!("[review-worktrees] OK   - {key}: {} at {dir}", if created { "created" } else { "already present" })),
            Err(e) => log(&format!("[review-worktrees] FAIL - {key} ({path}): {e}")),
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Fake(Mutex<Vec<String>>);

    #[async_trait]
    impl Provisioner for Fake {
        async fn provision(&self, repo_path: &str) -> Result<(String, bool), String> {
            self.0.lock().unwrap().push(repo_path.into());
            match repo_path {
                "/r/new" => Ok(("/wt/new".into(), true)),
                "/r/old" => Ok(("/wt/old".into(), false)),
                _ => Err("not a git repository".into()),
            }
        }
    }

    async fn run(config: Option<Value>) -> (i32, Vec<String>, Vec<String>) {
        let f = Fake(Mutex::new(vec![]));
        let mut lines = vec![];
        let code = run_provision(config.as_ref(), &f, &mut |l| lines.push(l.to_string())).await;
        let calls = f.0.lock().unwrap().clone();
        (code, lines, calls)
    }

    #[tokio::test]
    async fn no_config_and_no_repos_are_a_line_not_an_error() {
        let (code, lines, calls) = run(None).await;
        assert!(code == 0 && lines[0].contains("run `companion setup` first") && calls.is_empty());
        let (code, lines, _) = run(Some(json!({ "repos": {} }))).await;
        assert!(code == 0 && lines == ["[review-worktrees] No repos configured \u{2014} nothing to provision."]);
        let (_, lines, _) = run(Some(json!({}))).await;
        assert!(lines[0].contains("No repos configured"));
    }

    #[tokio::test]
    async fn each_repo_gets_a_line_in_config_order_and_a_failure_does_not_stop_the_rest() {
        let config = json!({ "repos": { "ACME/new": "/r/new", "ACME/bad": "/r/bad", "ACME/old": "/r/old" } });
        let (code, lines, calls) = run(Some(config)).await;
        assert_eq!(code, 0);
        assert_eq!(calls, ["/r/new", "/r/bad", "/r/old"]);
        assert_eq!(
            lines,
            [
                "[review-worktrees] OK   - ACME/new: created at /wt/new",
                "[review-worktrees] FAIL - ACME/bad (/r/bad): not a git repository",
                "[review-worktrees] OK   - ACME/old: already present at /wt/old",
            ]
        );
    }

    #[test]
    fn a_missing_or_malformed_config_loads_as_none_and_an_object_loads() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        assert!(load_existing_config(&p).is_none());
        std::fs::write(&p, "{ nope").unwrap();
        assert!(load_existing_config(&p).is_none());
        std::fs::write(&p, "[1]").unwrap();
        assert!(load_existing_config(&p).is_none());
        std::fs::write(&p, "{\"repos\":{}}").unwrap();
        assert!(load_existing_config(&p).is_some());
    }

    #[tokio::test]
    async fn the_real_provisioner_reports_a_folder_that_is_not_a_repo_as_an_error() {
        let d = tempfile::tempdir().unwrap();
        let p = SystemProvisioner { home: d.path().join("home") };
        assert!(p.provision(&d.path().to_string_lossy()).await.is_err());
    }
}
