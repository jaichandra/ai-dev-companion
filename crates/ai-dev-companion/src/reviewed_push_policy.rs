//! Pure job-status rules shared by every feature that pushes a reviewed
//! worktree (resolve-conflict, address-review-comments); port of
//! `core/reviewed-push-policy.js`.

/// Whether reject() (and cancel()'s "discard the finished result" path) may
/// proceed against a job currently in `status`. Only "awaiting-approval" (the
/// normal discard) and "failed" (discard after a failed approve, e.g. a
/// rejected push, so the user can abandon rather than retry) are reachable
/// states where there's a worktree sitting around with nothing else acting on
/// it. Every other status is either mid-flight on another action (`approving`,
/// `rejecting`, `running`) or already terminal with no worktree left
/// (`approved`, `rejected`): reject() must not stamp "rejecting" over any.
pub fn can_reject(status: &str) -> bool {
    matches!(status, "awaiting-approval" | "failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_awaiting_approval_and_failed_can_be_rejected() {
        assert!(can_reject("awaiting-approval"));
        assert!(can_reject("failed"));
        for s in ["approving", "rejecting", "running", "approved", "rejected", "", "Failed"] {
            assert!(!can_reject(s), "{s}");
        }
    }
}
