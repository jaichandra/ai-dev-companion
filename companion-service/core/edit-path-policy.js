// Pure decision logic for core/edit-guard.js's and core/read-guard.js's
// PreToolUse hooks. Fixes a
// regression from the first attempt at confining worktreeWrite's
// Edit/Write tools with plain `--disallowedTools` permission-rule strings
// (core/claude-args.js's now-removed worktreeGitEditDenyRules): those
// rules are glob-like string patterns with no filesystem resolution, so a
// broad-enough deny (`Edit(//<home>/**)`) also matched paths INSIDE the
// worktree itself once the worktree lived under $HOME, as it does in
// production (`~/.ai-dev-companion/worktrees/...`) — live-verified to
// deny Claude's own Edit calls on files it needed to edit. This module
// does real filesystem resolution instead: given one allowed root, decide
// whether a candidate path is really inside it (following symlinks) and
// doesn't touch a `.git` path anywhere.
//
// Plain JS (no build step) — core/edit-guard.js requires this directly
// and runs standalone with plain `node`, same reason as
// core/mcp-tool-classifier.js (core/mcp-guard.js's sibling pure module).
const fs = require("fs");
const path = require("path");

/**
 * Resolves `absPath` one component at a time, with `fs.lstatSync`, the way
 * the OS itself will when the tool call opens it — returns
 * `{ real, finalIsSymlink, finalStat }`, or `null` when the path can't be
 * resolved safely.
 *
 * The previous version walked up from the full path with `fs.existsSync`
 * until something existed and rejoined the rest — but `existsSync` FOLLOWS
 * symlinks, so a dangling link (`<root>/link -> ../outside/newfile.txt`)
 * looked like "a new file that doesn't exist yet": the walk stepped up to
 * the link's in-root parent, re-joined the link's NAME and allowed a Write
 * the OS then followed straight out of the root. It also let
 * `fs.realpathSync` normalize `..` lexically (`<root>/dirlink/../x` became
 * `<root>/x`), while the OS resolves `dirlink` first and `..` from
 * wherever that lands.
 *
 * So here, starting at `/`:
 *   - `.` is skipped and `..` takes the dirname of the (already real)
 *     current directory — physical, not lexical, resolution;
 *   - a symlink component is resolved with `fs.realpathSync`; if that
 *     throws (dangling, a loop, a permission error) the whole path is
 *     unresolvable and the caller denies;
 *   - the first component that doesn't exist (ENOENT) ends the walk: it
 *     and everything after it are plain names under a real directory, so
 *     they're rejoined as-is (after refusing any `..` among them, which
 *     would otherwise be normalized lexically again by `path.join`);
 *   - any other lstat error (ENOTDIR, EACCES, …) is unresolvable too.
 * `finalIsSymlink` / `finalStat` describe the LAST component (its lstat,
 * before any symlink is followed), for callers with rules about the
 * target itself (isEditAllowed); both stay unset for a not-yet-existing
 * target.
 */
function resolvePathSafely(absPath) {
  const parts = absPath.split(path.sep).filter((p) => p !== "");
  let cur = path.parse(absPath).root || path.sep;
  let finalIsSymlink = false;
  let finalStat = null;
  for (let i = 0; i < parts.length; i++) {
    const part = parts[i];
    const isLast = i === parts.length - 1;
    if (part === ".") continue;
    if (part === "..") {
      cur = path.dirname(cur);
      continue;
    }
    const next = path.join(cur, part);
    let st;
    try {
      st = fs.lstatSync(next);
    } catch (err) {
      if (err && err.code === "ENOENT") {
        const rest = parts.slice(i);
        if (rest.includes("..")) return null;
        return { real: path.join(cur, ...rest.filter((p) => p !== ".")), finalIsSymlink: false, finalStat: null };
      }
      return null;
    }
    if (st.isSymbolicLink()) {
      try {
        cur = fs.realpathSync(next);
      } catch {
        return null;
      }
      if (isLast) finalIsSymlink = true;
    } else {
      cur = next;
    }
    if (isLast) finalStat = st;
  }
  return { real: cur, finalIsSymlink, finalStat };
}

/** True if `absPath` is STRICTLY nested inside `root` (not equal to it —
 * `root` itself is a directory, never a valid Edit/Write target). Uses
 * `path.relative`, not a naive string-prefix check, so `/foo-bar` is
 * correctly NOT considered inside `/foo`. */
function isStrictlyInside(root, absPath) {
  const rel = path.relative(root, absPath);
  return rel !== "" && rel !== "." && !rel.startsWith(`..${path.sep}`) && rel !== ".." && !path.isAbsolute(rel);
}

/** True if any path segment of `absPath` is `.git`, case-INsensitively —
 * covers the worktree's own gitlink FILE (`<root>/.git`) and anything
 * nested under a `.git` directory anywhere inside the root (hooks,
 * config, index, …). Case-insensitive because APFS (macOS's default
 * filesystem) is case-insensitive-but-case-preserving: `fs.realpathSync`
 * returns whatever casing the CALLER used, not the casing the directory
 * entry was created with, so `<root>/.GIT` or `<root>/.Git` resolves to
 * itself unchanged and a naive exact-`".git"` comparison lets a
 * differently-cased gitlink/gitdir straight through. */
function touchesGitPath(absPath) {
  return absPath.split(path.sep).some((segment) => segment.toLowerCase() === ".git");
}

/**
 * Decides whether an Edit/Write/MultiEdit/NotebookEdit call targeting
 * `rawPath` may proceed, given `root` — an absolute path the caller
 * should already have resolved to a real path (see core/edit-guard.js).
 * Allows ONLY when the resolved candidate is strictly inside `root` and
 * doesn't touch any `.git` path segment. Everything else (outside root,
 * a `../` escape, a symlink resolving outside root, `root` itself, `.git`
 * anywhere inside root, a missing/non-string path) is denied.
 *
 * On top of that, the target itself must not be a way to write somewhere
 * else (see resolvePathSafely for the dangling-link bypass this closes):
 *   - any symlink in the path that can't be resolved denies;
 *   - the final component being a symlink AT ALL denies, dangling or live
 *     — Claude has no reason to write through a link, and a live one that
 *     points in-root today can be re-pointed by a later Bash call before
 *     the write lands;
 *   - an existing non-directory target with `nlink > 1` denies: a
 *     hardlink shares its inode with another name that may live outside
 *     the root (a committed file can't be one, but Bash can make one).
 *
 * `rawPath` must already be an absolute, untrimmed-whitespace-free,
 * non-`~`-prefixed path — exactly what Claude Code hands a PreToolUse
 * hook, since Claude Code itself trims whitespace off `file_path` and
 * expands a leading `~`/`~/` to `$HOME` BEFORE the hook ever sees it.
 * Resolving a non-absolute candidate against `root` (the previous
 * behavior) trusted that expansion to have already happened and, if it
 * hadn't — a raw `"~/.zshrc"` or a stray `" /outside"` reaching this
 * function some other way — silently treated it as a relative name
 * inside `root` and allowed it. Fail closed instead: anything that
 * doesn't already look like a real absolute path is denied outright,
 * with no attempt to interpret or resolve it against `root`.
 */
function isEditAllowed(root, rawPath) {
  if (typeof root !== "string" || root.length === 0) return false;
  if (typeof rawPath !== "string" || rawPath.length === 0) return false;
  if (rawPath !== rawPath.trim()) return false;
  if (rawPath.startsWith("~")) return false;
  if (!path.isAbsolute(rawPath)) return false;

  const resolved = resolvePathSafely(rawPath);
  if (!resolved) return false;
  if (resolved.finalIsSymlink) return false;
  if (resolved.finalStat && !resolved.finalStat.isDirectory() && resolved.finalStat.nlink > 1) return false;

  if (!isStrictlyInside(root, resolved.real)) return false;
  if (touchesGitPath(resolved.real)) return false;
  return true;
}

/**
 * Decides whether a Read/Glob/Grep call may look at `rawPath`, given the
 * same real-path `root` as isEditAllowed (see core/read-guard.js). Same
 * symlink-safe resolution — a link out of the root, or one that can't be
 * resolved, denies — but deliberately WITHOUT the edit-only rules: reading
 * inside `.git` (Claude inspecting refs or config) and reading a
 * hardlinked file are both harmless, and `root` itself is a valid Glob/
 * Grep search path. A live final symlink is fine as long as it resolves
 * inside the root.
 *
 * Unlike isEditAllowed, a relative candidate is resolved against `root`
 * rather than denied: Glob/Grep's `path` may legitimately be relative, and
 * the run's cwd IS `root`, so that's exactly where the tool would look.
 * A leading `~` or surrounding whitespace still denies outright — the
 * same "don't guess what the tool will do with it" reasoning as
 * isEditAllowed.
 */
function isReadAllowed(root, rawPath) {
  if (typeof root !== "string" || root.length === 0) return false;
  if (typeof rawPath !== "string" || rawPath.length === 0) return false;
  if (rawPath !== rawPath.trim()) return false;
  if (rawPath.startsWith("~")) return false;

  // Not path.resolve: that normalizes `..` lexically before any symlink is
  // looked at — resolvePathSafely does it physically.
  const candidate = path.isAbsolute(rawPath) ? rawPath : `${root}${path.sep}${rawPath}`;
  const resolved = resolvePathSafely(candidate);
  if (!resolved) return false;
  return resolved.real === root || isStrictlyInside(root, resolved.real);
}

/** True if a Glob `pattern` could reach outside the search directory on
 * its own: absolute, `~`-prefixed, or with a `..` segment (either
 * separator). `..` inside a name (`a..b`) is fine. */
function globPatternEscapes(pattern) {
  if (pattern.startsWith("~") || path.isAbsolute(pattern)) return true;
  return pattern.split(/[\\/]/).some((segment) => segment === "..");
}

/**
 * The read guard's whole decision for one PreToolUse payload: `toolName`
 * and its `toolInput` exactly as Claude Code sends them. Read's target is
 * `file_path`; Glob's and Grep's is their optional `path` (absent or empty
 * means the run's cwd, which is `root`, so that's allowed). Glob's
 * `pattern` is also checked, since `/etc/*` or `../../*` would widen the
 * search on its own; Grep's `pattern` is a regex over file CONTENTS, not a
 * path, and its `glob` filter only narrows which files under `path` are
 * searched, so neither needs a check. Any other tool name, a missing Read
 * `file_path`, or a non-string field denies.
 */
function isReadToolCallAllowed(root, toolName, toolInput) {
  const input = toolInput && typeof toolInput === "object" ? toolInput : null;
  if (!input) return false;
  const optionalPathAllowed = () => {
    if (input.path === undefined || input.path === null || input.path === "") return true;
    return typeof input.path === "string" && isReadAllowed(root, input.path);
  };
  switch (toolName) {
    case "Read":
      return typeof input.file_path === "string" && isReadAllowed(root, input.file_path);
    case "Glob":
      if (typeof input.pattern !== "string" || globPatternEscapes(input.pattern)) return false;
      return optionalPathAllowed();
    case "Grep":
      return optionalPathAllowed();
    default:
      return false;
  }
}

module.exports = {
  isEditAllowed,
  isReadAllowed,
  isReadToolCallAllowed,
  resolvePathSafely,
  isStrictlyInside,
  touchesGitPath,
};
