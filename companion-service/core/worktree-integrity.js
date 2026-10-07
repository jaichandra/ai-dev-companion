// Defense in depth against a resolve-conflict Claude run quietly
// redirecting how git behaves the NEXT time the service itself runs git
// in this worktree (e.g. the `stageAll`/`git add -A` step right after
// Claude finishes) — by rewriting the worktree's own `.git` gitlink file,
// or the metadata git keeps for this worktree in the MAIN repo
// (`<gitDir>/commondir`, `<gitDir>/gitdir`, `<gitDir>/config.worktree`)
// to point git at an attacker-controlled config (`core.hooksPath`,
// `core.fsmonitor`, etc — anything that makes git itself execute
// something on its next invocation). The edit guard (core/edit-guard.js,
// which denies any Edit/Write outside the worktree or touching a `.git`
// segment) and the sandbox's `filesystem.denyWrite`
// (features/resolve-conflict/index.ts) are the primary defense; this is
// the fallback for whatever those miss: snapshot these four paths right
// before Claude runs, and refuse to run ANY further git command in the
// worktree at all if they changed.
//
// Plain JS (not TypeScript) for the same reason as core/paths.js and
// core/claude-args.js: no build step needed to unit test it, and
// features/resolve-conflict/index.ts's TypeScript loads it with
// require + a hand-written type cast.
const fs = require("fs");
const path = require("path");

/** `fs.readFileSync(p, "utf8")`, or `null` if `p` doesn't exist — a
 * missing file (e.g. `config.worktree`, which most worktrees never have)
 * is a normal, comparable snapshot value, not an error. Any other read
 * failure (permissions, `p` being a directory) is NOT swallowed — this
 * check exists specifically to notice something unexpected, so silently
 * treating an unreadable file as "unchanged" would defeat the point. */
function readIfExists(p) {
  try {
    return fs.readFileSync(p, "utf8");
  } catch (err) {
    if (err.code === "ENOENT") return null;
    throw err;
  }
}

/**
 * Snapshots the four files that decide how git behaves in and for a
 * resolve-conflict worktree. `worktreeDir` is the worktree's own checkout
 * (its `.git` is a one-line gitlink FILE, not a directory — that's the
 * `gitlink` field here); `gitDir` is `git rev-parse --absolute-git-dir`
 * run inside it, i.e. the worktree's private metadata dir under the MAIN
 * repo's `.git/worktrees/<id>` (see core/worktree.ts's addWorktree).
 */
function snapshotWorktreeGitMetadata({ worktreeDir, gitDir }) {
  return {
    gitlink: readIfExists(path.join(worktreeDir, ".git")),
    commondir: readIfExists(path.join(gitDir, "commondir")),
    gitdir: readIfExists(path.join(gitDir, "gitdir")),
    configWorktree: readIfExists(path.join(gitDir, "config.worktree")),
  };
}

/** True if anything in `before`/`after` (both from
 * snapshotWorktreeGitMetadata) differs — including one appearing or
 * disappearing (e.g. `config.worktree` going from absent to present is a
 * change, even though most worktrees never have one at all). */
function worktreeGitMetadataChanged(before, after) {
  return (
    before.gitlink !== after.gitlink ||
    before.commondir !== after.commondir ||
    before.gitdir !== after.gitdir ||
    before.configWorktree !== after.configWorktree
  );
}

module.exports = { snapshotWorktreeGitMetadata, worktreeGitMetadataChanged };
