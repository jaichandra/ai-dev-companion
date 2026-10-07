// Pure job-status rules shared by every feature that pushes a reviewed
// worktree (resolve-conflict, address-review-comments) — the plain-JS half
// of core/reviewed-push.ts, so it can be required from features/*/plan.js
// and tested with no build step.

/**
 * Whether reject() (and cancel()'s "discard the finished result" path) may
 * proceed against a job currently in `status`. Only "awaiting-approval"
 * (the normal discard) and "failed" (discard after a failed approve, e.g.
 * a rejected push, so the user can abandon rather than retry) are
 * reachable states where there's a worktree sitting around with nothing
 * else acting on it. Every other status is either mid-flight on another
 * action (`approving`, `rejecting`, `running`) or already terminal with
 * no worktree left (`approved`, `rejected`) — reject() must not stamp
 * "rejecting" over any of those.
 */
function canReject(status) {
  return status === "awaiting-approval" || status === "failed";
}

module.exports = { canReject };
