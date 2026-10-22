//! Pure logic for resolve-conflict (port of `features/resolve-conflict/plan.js`).

use std::collections::HashSet;

pub use crate::reviewed_push_policy::can_reject;

/// Splits a NUL-terminated list, the output of `git diff --name-only -z`, into
/// paths. NUL (not newline) matters: git's two-column `--porcelain` text format
/// breaks on a rename (`R  old -> new`: the arrow isn't a delimiter, it's
/// literal text sharing the line with two real paths) and C-quotes any path
/// containing a space or a non-ASCII byte. `-z` output has none of that.
pub fn split_nul_separated(text: &str) -> Vec<String> {
    text.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// The intersection of two NUL-terminated path lists, deduped, in the order
/// paths appear in `b_z`. A file that differs from BOTH merge parents
/// (`git diff --name-only -z preMergeSha tree` intersected with
/// `git diff --name-only -z destSha tree`) actually combines content from both
/// sides of the merge (a resolved conflict, an auto-merged file, or a stray new
/// file). A file that differs from only ONE parent came through untouched from
/// the other side and was never really "merged": nothing to review there.
pub fn intersect_nul_separated(a_z: &str, b_z: &str) -> Vec<String> {
    let a: HashSet<String> = split_nul_separated(a_z).into_iter().collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for file in split_nul_separated(b_z) {
        if a.contains(&file) && seen.insert(file.clone()) {
            out.push(file);
        }
    }
    out
}

/// Whether approve() should skip `git commit` and push HEAD as-is: the index
/// already matches HEAD exactly (`git diff --cached --quiet` exits 0), HEAD has
/// moved past `preMergeSha` (`git rev-list --count preMergeSha..HEAD`), and
/// that move is a real ancestry relationship (`git merge-base --is-ancestor`).
/// Together they mean the merge (or the user's own commit, made in a resumed
/// terminal session) is already committed on top of `preMergeSha`, and `git
/// commit` would just fail with "nothing to commit".
pub fn should_skip_commit(staged_clean: bool, ahead_count: i64, is_ancestor: bool) -> bool {
    staged_clean && ahead_count > 0 && is_ancestor
}

/// Whether there's nothing at all to push: the index matches HEAD AND HEAD never
/// moved past `preMergeSha`: the merge itself was "Already up to date" and
/// nothing else changed either. Checked before [`should_skip_commit`]: a
/// different, more final outcome (approve without ever running `git push`).
pub fn is_nothing_to_push(staged_clean: bool, ahead_count: i64) -> bool {
    staged_clean && ahead_count == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAgainst {
    /// The destination branch's commit.
    Dest,
    /// The PR branch as it was before the merge.
    PreMerge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedFile {
    pub path: String,
    pub diff_against: DiffAgainst,
    pub note: Option<&'static str>,
}

/// Decides which base to diff a CONFLICTED file against, and what to tell the
/// user, when it was resolved wholesale to one side rather than actually
/// combining both sides' changes:
///  - `is_empty_vs_pre_merge` (no difference from preMergeSha: resolved
///    wholesale to the PR branch's own side): a diff against preMergeSha would
///    be empty and would hide that the destination branch's changes were
///    dropped for this file entirely, so diff against destSha, with a note.
///  - `equals_dest` (no difference from destSha: resolved wholesale to the
///    destination's side): the normal diff against preMergeSha already shows
///    the change; just add a note.
///  - neither: both sides actually combined: diff against preMergeSha, no note.
///
/// `is_empty_vs_pre_merge` is checked first, so the degenerate case where both
/// are true is reported as "kept the PR branch's version": an arbitrary but
/// deterministic tie-break.
pub fn classify_conflicted_file(path: &str, is_empty_vs_pre_merge: bool, equals_dest: bool) -> ConflictedFile {
    let (diff_against, note) = if is_empty_vs_pre_merge {
        (DiffAgainst::Dest, Some("Kept the PR branch's version \u{2014} the destination branch's changes to this file were dropped."))
    } else if equals_dest {
        (DiffAgainst::PreMerge, Some("Took the destination branch's version \u{2014} the PR branch's changes to this file were replaced."))
    } else {
        (DiffAgainst::PreMerge, None)
    };
    ConflictedFile { path: path.to_string(), diff_against, note }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_nul_separated_splits_on_nul_and_drops_the_trailing_empty_entry() {
        assert_eq!(split_nul_separated("a.txt\0b.txt\0"), ["a.txt", "b.txt"]);
        assert!(split_nul_separated("").is_empty());
        assert_eq!(split_nul_separated("a\0\0b"), ["a", "b"]);
    }

    #[test]
    fn intersect_keeps_only_paths_in_both_lists_deduped_in_the_second_lists_order() {
        assert_eq!(intersect_nul_separated("shared.ts\0only-a.ts\0", "shared.ts\0only-b.ts\0"), ["shared.ts"]);
        assert!(intersect_nul_separated("only-a.ts\0", "only-b.ts\0").is_empty());
        assert_eq!(intersect_nul_separated("shared.ts\0", "shared.ts\0shared.ts\0"), ["shared.ts"]);
        assert_eq!(
            intersect_nul_separated("docs/release notes.md\0src/caf\u{e9}.ts\0", "docs/release notes.md\0src/caf\u{e9}.ts\0"),
            ["docs/release notes.md", "src/caf\u{e9}.ts"]
        );
        for (a, b) in [("", "a.ts\0"), ("a.ts\0", ""), ("", "")] {
            assert!(intersect_nul_separated(a, b).is_empty());
        }
        assert_eq!(intersect_nul_separated("a\0b\0c\0", "c\0a\0"), ["c", "a"]);
    }

    #[test]
    fn should_skip_commit_needs_clean_staging_head_ahead_and_a_real_ancestor() {
        assert!(should_skip_commit(true, 1, true));
        assert!(!should_skip_commit(false, 1, true));
        assert!(!should_skip_commit(true, 0, true));
        assert!(!should_skip_commit(true, 1, false));
        assert!(!should_skip_commit(true, -1, true));
    }

    #[test]
    fn is_nothing_to_push_needs_clean_staging_and_head_never_moved() {
        assert!(is_nothing_to_push(true, 0));
        assert!(!is_nothing_to_push(true, 1));
        assert!(!is_nothing_to_push(false, 0));
    }

    #[test]
    fn classify_conflicted_file_picks_the_diff_base_and_the_note() {
        let pr_side = classify_conflicted_file("a.ts", true, false);
        assert_eq!(pr_side.diff_against, DiffAgainst::Dest);
        assert_eq!(pr_side.note, Some("Kept the PR branch's version \u{2014} the destination branch's changes to this file were dropped."));
        let dest_side = classify_conflicted_file("a.ts", false, true);
        assert_eq!(dest_side.diff_against, DiffAgainst::PreMerge);
        assert_eq!(dest_side.note, Some("Took the destination branch's version \u{2014} the PR branch's changes to this file were replaced."));
        let combined = classify_conflicted_file("a.ts", false, false);
        assert_eq!((combined.diff_against, combined.note), (DiffAgainst::PreMerge, None));
        assert_eq!(classify_conflicted_file("a.ts", true, true).diff_against, DiffAgainst::Dest, "the degenerate case prefers the PR-branch side");
        assert_eq!(classify_conflicted_file("src/a.ts", false, false).path, "src/a.ts");
    }

    #[test]
    fn can_reject_is_the_shared_policy() {
        assert!(can_reject("awaiting-approval") && can_reject("failed"));
        for s in ["approving", "rejecting", "approved", "rejected", "running"] {
            assert!(!can_reject(s), "{s}");
        }
    }
}
