#!/usr/bin/env node
// Provisions (or verifies) every configured repo's dedicated PR-review
// worktree — see core/review-worktree.js for what that means and why it's
// a different lifecycle from the per-job worktrees resolve-conflict uses.
//
// Run automatically, detached, right after `npm run setup` enables the
// "review-in-editor" feature (see setup.js's main()) — the wizard prints
// this file's log path and returns to the prompt immediately rather than
// blocking on however long fetching + checking out every repo takes.
// Also safe to re-run by hand any time via
// `npm run review-worktrees:provision`: provisioning is idempotent, so a
// repo that already has a valid worktree is left untouched.
const path = require("path");
const setup = require("./setup.js");
const reviewWorktree = require("./core/review-worktree.js");

const CONFIG_PATH = path.join(__dirname, "config.json");

async function main() {
  // Reuses setup.js's own loader (rather than a second JSON.parse) so
  // "no config yet" / "malformed config" are treated identically here and
  // in the wizard itself.
  const config = setup.loadExistingConfig(CONFIG_PATH);
  if (!config) {
    console.log("[review-worktrees] No usable config.json found yet — run `npm run setup` first.");
    return;
  }

  const entries = Object.entries(config.repos || {});
  if (entries.length === 0) {
    console.log("[review-worktrees] No repos configured — nothing to provision.");
    return;
  }

  for (const [key, repoPath] of entries) {
    try {
      const { dir, created } = await reviewWorktree.provisionReviewWorktree(repoPath);
      console.log(`[review-worktrees] OK   - ${key}: ${created ? "created" : "already present"} at ${dir}`);
    } catch (err) {
      console.log(`[review-worktrees] FAIL - ${key} (${repoPath}): ${err.message}`);
    }
  }
}

main().catch((err) => {
  console.error("[review-worktrees] Unexpected failure:", err.message);
  process.exitCode = 1;
});
