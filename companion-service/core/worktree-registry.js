// Whether a directory is a linked worktree that git itself has registered for
// a repo, read from the repo's own `.git/worktrees/<name>/gitdir` files (each
// holds the path of that worktree's `.git` file). Synchronous and git-free so
// the job loader can use it on untrusted job files at startup. The repo's own
// directory is not a linked worktree, so it never counts.
const fs = require("fs");
const path = require("path");

function real(p) {
  try {
    return fs.realpathSync(p);
  } catch {
    return path.resolve(p);
  }
}

function isLinkedWorktreeOf(repoPath, dir) {
  if (typeof repoPath !== "string" || typeof dir !== "string" || !path.isAbsolute(dir)) return false;
  const base = path.join(repoPath, ".git", "worktrees");
  let names;
  try {
    names = fs.readdirSync(base);
  } catch {
    return false;
  }
  const want = path.join(real(dir), ".git");
  for (const name of names) {
    try {
      const gitdir = fs.readFileSync(path.join(base, name, "gitdir"), "utf8").trim();
      if (gitdir && path.join(real(path.dirname(gitdir)), ".git") === want) return true;
    } catch {
      // A half-removed registration: not a match.
    }
  }
  return false;
}

module.exports = { isLinkedWorktreeOf };
