// The state folder (~/.ai-dev-companion, see paths.js): how much space it
// uses and clearing the part that can be rebuilt. Plain JS so it is testable
// without a build.
//
// Only checkouts are purgeable: `worktrees/` (job worktrees), `repos/` (the
// analysis clones) and every `<repo>.worktrees/` (the persistent review
// worktrees). Everything else — history.db, jobs/, sessions/, credentials,
// scheduler and notification state — is never touched.
const fs = require("fs");
const path = require("path");
const { execFile } = require("child_process");

function isPurgeable(name) {
  return name === "worktrees" || name === "repos" || name.endsWith(".worktrees");
}

/** Bytes under `p`. lstat throughout, so the node_modules symlinks inside a
 * worktree are counted as links, never followed into the real clone. */
async function sizeOf(p) {
  let st;
  try {
    st = await fs.promises.lstat(p);
  } catch {
    return 0;
  }
  if (!st.isDirectory()) return st.size;
  let names;
  try {
    names = await fs.promises.readdir(p);
  } catch {
    return st.size;
  }
  const sizes = await Promise.all(names.map((n) => sizeOf(path.join(p, n))));
  return st.size + sizes.reduce((a, b) => a + b, 0);
}

async function dataFolderUsage(dir) {
  let names = [];
  try {
    names = await fs.promises.readdir(dir);
  } catch {
    // No folder yet: nothing is stored.
  }
  const items = await Promise.all(
    names.map(async (name) => ({ name, bytes: await sizeOf(path.join(dir, name)), purgeable: isPurgeable(name) })),
  );
  items.sort((a, b) => b.bytes - a.bytes);
  const totalBytes = items.reduce((sum, i) => sum + i.bytes, 0);
  const purgeableBytes = items.filter((i) => i.purgeable).reduce((sum, i) => sum + i.bytes, 0);
  return { path: dir, totalBytes, purgeableBytes, items };
}

/** The clone a worktree belongs to, read from its `.git` file
 * (`gitdir: <clone>/.git/worktrees/<id>`); null when it can't be told. */
function owningClone(worktreeDir) {
  try {
    const text = fs.readFileSync(path.join(worktreeDir, ".git"), "utf8");
    const m = /^gitdir:\s*(.+?)\s*$/m.exec(text);
    if (!m) return null;
    const marker = `${path.sep}.git${path.sep}worktrees${path.sep}`;
    const at = m[1].indexOf(marker);
    return at > 0 ? m[1].slice(0, at) : null;
  } catch {
    return null;
  }
}

function worktreeDirsUnder(root) {
  const found = [];
  const walk = (dir, depth) => {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (!e.isDirectory()) continue;
      const child = path.join(dir, e.name);
      try {
        if (fs.statSync(path.join(child, ".git")).isFile()) {
          found.push(child);
          continue;
        }
      } catch {
        // no .git here
      }
      if (depth < 2) walk(child, depth + 1);
    }
  };
  walk(root, 0);
  return found;
}

function gitWorktreePrune(clone) {
  return new Promise((resolve) => {
    execFile("git", ["worktree", "prune"], { cwd: clone }, () => resolve());
  });
}

/** Deletes the purgeable folders, then asks each owning clone to forget the
 * worktrees that are gone. Returns what was removed. */
async function purgeDataFolder(dir) {
  const removed = [];
  let names = [];
  try {
    names = fs.readdirSync(dir);
  } catch {
    return { removed, freedBytes: 0 };
  }
  const targets = names.filter(isPurgeable);
  const clones = new Set();
  for (const name of targets) {
    for (const wt of worktreeDirsUnder(path.join(dir, name))) {
      const clone = owningClone(wt);
      if (clone) clones.add(clone);
    }
  }
  let freedBytes = 0;
  for (const name of targets) {
    const target = path.join(dir, name);
    freedBytes += await sizeOf(target);
    await fs.promises.rm(target, { recursive: true, force: true });
    removed.push(name);
  }
  await Promise.all([...clones].filter((c) => fs.existsSync(c)).map(gitWorktreePrune));
  return { removed, freedBytes };
}

module.exports = { isPurgeable, sizeOf, dataFolderUsage, purgeDataFolder, owningClone };
