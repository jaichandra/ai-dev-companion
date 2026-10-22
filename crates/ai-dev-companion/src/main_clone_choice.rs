//! The question a job puts to the user when the PR's branch is checked out in
//! the main clone, where Claude may not edit: what to do about it (port of
//! `core/main-clone-choice.ts`). Rendered by the same panel buttons as "Which
//! repository?" (`repo_choice`).

use crate::repo_choice::{RepoChoiceError, RepoChoices, RepoOption, RepoQuestion};
use crate::worktree::{MainCloneChoice, MainCloneInfo, WorktreeError};
use tokio_util::sync::CancellationToken;

pub const WORKTREE_LABEL: &str = "Make a worktree for this branch";
pub const DISPOSABLE_LABEL: &str = "Use a temporary worktree";

/// Asks, and resolves with the choice; `WorkspaceUnusable` for Cancel.
pub async fn ask_main_clone_choice(choices: &RepoChoices, job_id: &str, cancel: Option<&CancellationToken>, info: &MainCloneInfo) -> Result<MainCloneChoice, WorktreeError> {
    let can_switch = info.switch_to.is_some();
    let worktree_option = RepoOption {
        repo_key: WORKTREE_LABEL.into(),
        reason: match &info.switch_to {
            Some(to) => format!("Switches the clone to {to} (git checkout {to}), then keeps a worktree at {} that later jobs reuse.", info.worktree_dir),
            None => format!("Not possible: {}.", info.cannot_switch.as_deref().unwrap_or("null")),
        },
    };
    let disposable_option = RepoOption {
        repo_key: DISPOSABLE_LABEL.into(),
        reason: format!(
            "A throwaway copy of origin's branch, deleted afterwards. The clone is not touched{}",
            if info.ahead > 0 { format!(", and its {} unpushed commit{} not in the copy.", info.ahead, if info.ahead == 1 { " is" } else { "s are" }) } else { ".".into() }
        ),
    };
    let question = RepoQuestion {
        suggested: Some(if can_switch { WORKTREE_LABEL } else { DISPOSABLE_LABEL }.into()),
        options: if can_switch { vec![worktree_option, disposable_option] } else { vec![disposable_option] },
        prompt: Some(format!(
            "The branch {} is checked out in your main clone ({}), and a job that lets Claude edit can't run there. Nothing is changed until you answer. Cancel if you would rather switch the clone to another branch yourself.",
            info.branch, info.dir
        )),
        no_repo_label: Some("Cancel".into()),
        null_is_cancel: Some(true),
    };
    let answer = choices.ask(job_id, cancel, &question, "start the job again.").await.map_err(|e| match e {
        RepoChoiceError::Cancelled => WorktreeError::Other("cancelled".into()),
        other => WorktreeError::Other(other.to_string()),
    })?;
    match answer.as_deref() {
        None => Err(WorktreeError::WorkspaceUnusable("Cancelled: nothing was changed.".into())),
        Some(WORKTREE_LABEL) => Ok(MainCloneChoice::Worktree),
        Some(_) => Ok(MainCloneChoice::Disposable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobStore;
    use serde_json::{json, Map, Value};
    use std::sync::Arc;
    use std::time::Duration;

    fn info(switch_to: Option<&str>, ahead: u64) -> MainCloneInfo {
        MainCloneInfo { dir: "/clone".into(), branch: "feat".into(), ahead, switch_to: switch_to.map(String::from), cannot_switch: switch_to.is_none().then(|| "the clone has uncommitted changes".into()), worktree_dir: "/wt/feat".into() }
    }

    async fn run(info: MainCloneInfo, answer: Value) -> (Result<MainCloneChoice, WorktreeError>, Value) {
        let store = Arc::new(JobStore::new());
        let id = store.create("f", Map::new())["id"].as_str().unwrap().to_string();
        let choices = Arc::new(RepoChoices::new(store.clone()));
        let (c, i) = (choices.clone(), id.clone());
        let waiting = tokio::spawn(async move { ask_main_clone_choice(&c, &i, None, &info).await });
        let mut published = Value::Null;
        for _ in 0..200 {
            published = store.get(&id).unwrap()["data"]["pendingChoice"].clone();
            if !published.is_null() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        choices.confirm(&store.get(&id).unwrap(), Some(&json!({ "repoKey": answer }))).unwrap();
        (waiting.await.unwrap(), published)
    }

    #[tokio::test]
    async fn with_a_branch_to_switch_to_both_options_are_offered_and_the_worktree_is_suggested() {
        let (choice, q) = run(info(Some("master"), 0), json!(WORKTREE_LABEL)).await;
        assert_eq!(choice, Ok(MainCloneChoice::Worktree));
        assert_eq!(q["suggested"], WORKTREE_LABEL);
        assert_eq!(q["options"][0]["reason"], "Switches the clone to master (git checkout master), then keeps a worktree at /wt/feat that later jobs reuse.");
        assert_eq!(q["options"][1]["reason"], "A throwaway copy of origin's branch, deleted afterwards. The clone is not touched.");
        assert_eq!((q["noRepoLabel"].clone(), q["nullIsCancel"].clone()), (json!("Cancel"), json!(true)));
        assert!(q["prompt"].as_str().unwrap().starts_with("The branch feat is checked out in your main clone (/clone), and a job that lets Claude edit can't run there."));
    }

    #[tokio::test]
    async fn without_a_branch_to_switch_to_only_the_temporary_worktree_is_offered_and_unpushed_commits_are_counted() {
        let (choice, q) = run(info(None, 1), json!(DISPOSABLE_LABEL)).await;
        assert_eq!(choice, Ok(MainCloneChoice::Disposable));
        assert_eq!((q["suggested"].clone(), q["options"].as_array().unwrap().len()), (json!(DISPOSABLE_LABEL), 1));
        assert_eq!(q["options"][0]["reason"], "A throwaway copy of origin's branch, deleted afterwards. The clone is not touched, and its 1 unpushed commit is not in the copy.");
        let (_, q) = run(info(None, 3), json!(DISPOSABLE_LABEL)).await;
        assert!(q["options"][0]["reason"].as_str().unwrap().ends_with("and its 3 unpushed commits are not in the copy."));
    }

    #[tokio::test]
    async fn cancel_answers_null_and_fails_with_nothing_changed() {
        let (choice, _) = run(info(Some("master"), 0), Value::Null).await;
        assert_eq!(choice, Err(WorktreeError::WorkspaceUnusable("Cancelled: nothing was changed.".into())));
    }
}
