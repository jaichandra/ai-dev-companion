//! The prompt for resolve-conflict (port of `features/resolve-conflict/prompt.ts`).

pub fn build_resolve_conflict_prompt(conflicted_files: &[String]) -> String {
    let list = conflicted_files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n");
    format!(
        r#"You are resolving git merge conflicts produced by merging the destination
branch into a pull request's source branch. The repository is already
checked out here with the merge in progress and conflict markers
(<<<<<<<, =======, >>>>>>>) present in the following files:

{list}

For each file:
1. Open it and understand what changed on both sides of the conflict.
2. Resolve every conflict marker, preserving the intent of BOTH the pull
   request branch's changes and the destination branch's changes. Do not
   simply pick one side when both changes matter — integrate them.
3. Remove all conflict markers. When you are done there must be no
   "<<<<<<<", "=======", or ">>>>>>>" lines remaining anywhere in the repo.
4. If a quick build or test command is obviously available and fast to run,
   run it to sanity-check your resolution. Do not attempt a slow full test
   suite. Note: commands here run sandboxed and can only write inside this
   directory — a build/test step that needs to write elsewhere (e.g. a
   global cache like ~/.npm or ~/.gradle) is expected to fail for that
   reason, and is not itself a sign your resolution is wrong.

Do not run "git commit", "git push", or "git merge --abort" — leave the
merge staged. When finished, report which files you resolved and, for any
file where you made a judgment call between the two sides, briefly explain
the call you made."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_conflicted_file_and_forbids_commit_push_and_abort() {
        let p = build_resolve_conflict_prompt(&["src/a.ts".into(), "docs/b c.md".into()]);
        assert!(p.contains("following files:\n\n- src/a.ts\n- docs/b c.md\n\nFor each file:"), "{p}");
        for needle in ["Do not run \"git commit\", \"git push\", or \"git merge --abort\"", "preserving the intent of BOTH", "can only write inside this\n   directory"] {
            assert!(p.contains(needle), "{needle}");
        }
        assert!(p.starts_with("You are resolving git merge conflicts"));
        assert!(p.ends_with("the call you made."));
    }

    /// Byte-for-byte what the Node implementation produced.
    #[test]
    fn matches_the_node_prompt_exactly() {
        let golden: serde_json::Value = serde_json::from_str(include_str!("../../../tests/fixtures/prompts/resolve-conflict.json")).unwrap();
        assert_eq!(build_resolve_conflict_prompt(&["a.ts".into(), "dir/b.ts".into()]), golden["two_files"].as_str().unwrap());
        assert_eq!(build_resolve_conflict_prompt(&[]), golden["none"].as_str().unwrap());
    }
}
