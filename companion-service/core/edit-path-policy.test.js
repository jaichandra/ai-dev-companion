const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { isEditAllowed, isReadAllowed, isReadToolCallAllowed } = require("./edit-path-policy.js");

/** A fresh root dir under the real system temp dir, itself realpath'd —
 * on macOS `os.tmpdir()` is under `/var/folders/...` which is ITSELF a
 * symlink to `/private/var/folders/...`, so resolving the root the same
 * way isEditAllowed resolves candidates keeps every "inside root" test
 * below comparing like with like. */
function makeRoot() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-test-"));
  return fs.realpathSync(dir);
}

test("isEditAllowed: a file inside root is allowed", () => {
  const root = makeRoot();
  const file = path.join(root, "a.txt");
  fs.writeFileSync(file, "hi");
  assert.equal(isEditAllowed(root, file), true);
});

test("isEditAllowed: a NEW file that doesn't exist yet, inside root, is allowed", () => {
  const root = makeRoot();
  assert.equal(isEditAllowed(root, path.join(root, "brand-new.txt")), true);
});

test("isEditAllowed: the root directory itself is denied", () => {
  const root = makeRoot();
  assert.equal(isEditAllowed(root, root), false);
});

test("isEditAllowed: <root>/.git (a gitlink FILE) is denied", () => {
  const root = makeRoot();
  const gitlink = path.join(root, ".git");
  fs.writeFileSync(gitlink, "gitdir: /somewhere\n");
  assert.equal(isEditAllowed(root, gitlink), false);
});

test("isEditAllowed: anything under a nested .git directory is denied", () => {
  const root = makeRoot();
  fs.mkdirSync(path.join(root, ".git", "hooks"), { recursive: true });
  assert.equal(isEditAllowed(root, path.join(root, ".git", "hooks", "pre-commit")), false);
});

// APFS (macOS's default filesystem) is case-insensitive-but-case-preserving,
// and fs.realpathSync returns the CALLER's casing, not the on-disk casing --
// so a naive exact `".git"` string comparison lets a differently-cased
// gitlink/gitdir straight through on a real worktree. These three mirror
// the two tests just above, one segment differently cased each time.
test("isEditAllowed: <root>/.GIT (differently-cased gitlink) is denied", () => {
  const root = makeRoot();
  const gitlink = path.join(root, ".GIT");
  fs.writeFileSync(gitlink, "gitdir: /somewhere\n");
  assert.equal(isEditAllowed(root, gitlink), false);
});

test("isEditAllowed: <root>/.Git (mixed-case gitlink) is denied", () => {
  const root = makeRoot();
  const gitlink = path.join(root, ".Git");
  fs.writeFileSync(gitlink, "gitdir: /somewhere\n");
  assert.equal(isEditAllowed(root, gitlink), false);
});

test("isEditAllowed: anything under a nested, differently-cased .GIT directory is denied", () => {
  const root = makeRoot();
  fs.mkdirSync(path.join(root, "sub", ".GIT", "hooks"), { recursive: true });
  assert.equal(isEditAllowed(root, path.join(root, "sub", ".GIT", "hooks", "pre-commit")), false);
});

test("isEditAllowed: a ../ escape out of root is denied", () => {
  const root = makeRoot();
  assert.equal(isEditAllowed(root, path.join(root, "..", "escaped.txt")), false);
});

test("isEditAllowed: a symlink inside root pointing OUTSIDE root is denied", () => {
  const root = makeRoot();
  const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-outside-"));
  const outsideFile = path.join(outsideDir, "secret.txt");
  fs.writeFileSync(outsideFile, "secret");
  const link = path.join(root, "link.txt");
  fs.symlinkSync(outsideFile, link);
  assert.equal(isEditAllowed(root, link), false);
});

test("isEditAllowed: an absolute path elsewhere in $HOME (sibling of root, not inside it) is denied", () => {
  const root = makeRoot();
  const sibling = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-sibling-"));
  const siblingFile = path.join(sibling, "outside.txt");
  fs.writeFileSync(siblingFile, "x");
  assert.equal(isEditAllowed(root, siblingFile), false);
});

// Claude Code trims whitespace off `file_path` and expands a leading
// `~`/`~/` to `$HOME` BEFORE a PreToolUse hook ever sees it, so a real
// hook payload's candidate is always already absolute. isEditAllowed
// used to resolve a non-absolute candidate against `root` instead of
// denying it outright -- which meant "~/.zshrc" (not yet tilde-expanded,
// had this function been reached some other way) and " /outside" (a
// leading space defeats a naive absolute-path check) both came back
// ALLOWED, resolved as if they were relative names inside root. Fail
// closed instead: deny anything that isn't already a clean absolute path,
// with no attempt to interpret or resolve it.
test("isEditAllowed: a relative candidate path is denied outright, not resolved against root", () => {
  const root = makeRoot();
  fs.writeFileSync(path.join(root, "rel.txt"), "x");
  assert.equal(isEditAllowed(root, "rel.txt"), false);
  assert.equal(isEditAllowed(root, path.join("..", "escaped-rel.txt")), false);
});

test("isEditAllowed: a leading ~ (unexpanded home-relative path) is denied", () => {
  const root = makeRoot();
  assert.equal(isEditAllowed(root, "~/.zshrc"), false);
  assert.equal(isEditAllowed(root, "~"), false);
  assert.equal(isEditAllowed(root, "~root/.zshrc"), false);
});

test("isEditAllowed: leading or trailing whitespace on the candidate is denied", () => {
  const root = makeRoot();
  const file = path.join(root, "a.txt");
  fs.writeFileSync(file, "hi");
  assert.equal(isEditAllowed(root, ` ${file}`), false);
  assert.equal(isEditAllowed(root, `${file} `), false);
  assert.equal(isEditAllowed(root, `${file}\n`), false);
});

// Live-probe A's exact case: a normal absolute path already inside root,
// with no whitespace or ~ involved, must still be allowed -- the fixes
// above must not turn into an over-broad deny of the common case.
test("isEditAllowed: a clean absolute in-root path is still allowed (live-probe A)", () => {
  const root = makeRoot();
  const file = path.join(root, "a.txt");
  fs.writeFileSync(file, "hi");
  assert.equal(isEditAllowed(root, file), true);
});

test("isEditAllowed: denies non-string or empty inputs", () => {
  const root = makeRoot();
  assert.equal(isEditAllowed(root, ""), false);
  assert.equal(isEditAllowed(root, null), false);
  assert.equal(isEditAllowed(root, undefined), false);
  assert.equal(isEditAllowed("", path.join(root, "a.txt")), false);
});

// Final-review finding 1: fs.existsSync FOLLOWS symlinks, so the old
// longest-existing-prefix walk saw a dangling link as "doesn't exist yet",
// stepped up to its (real, in-root) parent and re-joined the link's NAME --
// allowing a Write that the OS then followed straight out of the worktree.
test("isEditAllowed: a DANGLING symlink as the final component is denied (points outside)", () => {
  const root = makeRoot();
  const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-outside-"));
  const link = path.join(root, "link");
  fs.symlinkSync(path.join(outsideDir, "newfile.txt"), link);
  assert.equal(isEditAllowed(root, link), false);
});

test("isEditAllowed: a dangling RELATIVE symlink (../outside-new.txt) is denied", () => {
  const root = makeRoot();
  const link = path.join(root, "link");
  fs.symlinkSync(path.join("..", "outside-new.txt"), link);
  assert.equal(isEditAllowed(root, link), false);
});

test("isEditAllowed: a dangling symlink as an INTERMEDIATE directory is denied", () => {
  const root = makeRoot();
  const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-outside-"));
  const dirLink = path.join(root, "dirlink");
  fs.symlinkSync(path.join(outsideDir, "not-yet"), dirLink);
  assert.equal(isEditAllowed(root, path.join(dirLink, "file.txt")), false);
});

test("isEditAllowed: a LIVE symlink as the final component is denied even when it points inside root", () => {
  const root = makeRoot();
  const target = path.join(root, "target.txt");
  fs.writeFileSync(target, "x");
  const link = path.join(root, "link.txt");
  fs.symlinkSync(target, link);
  assert.equal(isEditAllowed(root, link), false);
  // The real file itself stays editable.
  assert.equal(isEditAllowed(root, target), true);
});

test("isEditAllowed: a live intermediate directory symlink that stays inside root is allowed", () => {
  const root = makeRoot();
  fs.mkdirSync(path.join(root, "real"));
  fs.symlinkSync(path.join(root, "real"), path.join(root, "alias"));
  assert.equal(isEditAllowed(root, path.join(root, "alias", "new.txt")), true);
});

test("isEditAllowed: a HARDLINKED file (nlink > 1) is denied -- it may share its inode with a file outside root", () => {
  const root = makeRoot();
  const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-outside-"));
  const outsideFile = path.join(outsideDir, "victim.txt");
  fs.writeFileSync(outsideFile, "x");
  const inside = path.join(root, "hard.txt");
  fs.linkSync(outsideFile, inside);
  assert.equal(isEditAllowed(root, inside), false);
});

test("isEditAllowed: a ../ that walks through a symlinked dir is resolved physically, not lexically", () => {
  const root = makeRoot();
  const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-outside-"));
  fs.mkdirSync(path.join(outsideDir, "deep"));
  // root/in -> outside/deep ; root/in/../x is physically outside/x.
  fs.symlinkSync(path.join(outsideDir, "deep"), path.join(root, "in"));
  assert.equal(isEditAllowed(root, `${root}/in/../x.txt`), false);
});

test("isEditAllowed: normal file and new file (in a new subdir) still allowed after the symlink hardening", () => {
  const root = makeRoot();
  const file = path.join(root, "a.txt");
  fs.writeFileSync(file, "hi");
  assert.equal(isEditAllowed(root, file), true);
  assert.equal(isEditAllowed(root, path.join(root, "new-dir", "deeper", "b.txt")), true);
});

// The read guard (core/read-guard.js) shares the symlink-safe resolution
// but not the edit-only .git / nlink denials.
test("isReadAllowed: in-root file, root itself, .git and a hardlinked file are all readable", () => {
  const root = makeRoot();
  const file = path.join(root, "a.txt");
  fs.writeFileSync(file, "hi");
  fs.mkdirSync(path.join(root, ".git"));
  fs.writeFileSync(path.join(root, ".git", "HEAD"), "ref: x\n");
  const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-outside-"));
  fs.writeFileSync(path.join(outsideDir, "h.txt"), "x");
  fs.linkSync(path.join(outsideDir, "h.txt"), path.join(root, "h.txt"));
  assert.equal(isReadAllowed(root, file), true);
  assert.equal(isReadAllowed(root, root), true);
  assert.equal(isReadAllowed(root, path.join(root, ".git", "HEAD")), true);
  assert.equal(isReadAllowed(root, path.join(root, "h.txt")), true);
});

test("isReadAllowed: relative paths resolve against root; escapes, ~ and whitespace are denied", () => {
  const root = makeRoot();
  fs.mkdirSync(path.join(root, "src"));
  assert.equal(isReadAllowed(root, "src"), true);
  assert.equal(isReadAllowed(root, "."), true);
  assert.equal(isReadAllowed(root, ".."), false);
  assert.equal(isReadAllowed(root, "../sibling/x"), false);
  assert.equal(isReadAllowed(root, "~/.ssh/id_rsa"), false);
  assert.equal(isReadAllowed(root, " src"), false);
  assert.equal(isReadAllowed(root, ""), false);
  assert.equal(isReadAllowed(root, null), false);
});

test("isReadAllowed: outside root, a symlink out of root, and a dangling symlink are denied; an in-root live link is allowed", () => {
  const root = makeRoot();
  const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "edit-path-policy-outside-"));
  const secret = path.join(outsideDir, "secret.txt");
  fs.writeFileSync(secret, "s");
  assert.equal(isReadAllowed(root, secret), false);
  fs.symlinkSync(secret, path.join(root, "out-link"));
  assert.equal(isReadAllowed(root, path.join(root, "out-link")), false);
  fs.symlinkSync(path.join(outsideDir, "missing"), path.join(root, "dangling"));
  assert.equal(isReadAllowed(root, path.join(root, "dangling")), false);
  fs.writeFileSync(path.join(root, "t.txt"), "t");
  fs.symlinkSync(path.join(root, "t.txt"), path.join(root, "in-link"));
  assert.equal(isReadAllowed(root, path.join(root, "in-link")), true);
});

test("isReadToolCallAllowed: Read checks file_path; missing file_path denies", () => {
  const root = makeRoot();
  fs.writeFileSync(path.join(root, "a.txt"), "x");
  assert.equal(isReadToolCallAllowed(root, "Read", { file_path: path.join(root, "a.txt") }), true);
  assert.equal(isReadToolCallAllowed(root, "Read", { file_path: "/etc/hosts" }), false);
  assert.equal(isReadToolCallAllowed(root, "Read", {}), false);
  assert.equal(isReadToolCallAllowed(root, "Read", null), false);
});

test("isReadToolCallAllowed: Glob/Grep with no path allowed (defaults to cwd = root); path outside denied", () => {
  const root = makeRoot();
  fs.mkdirSync(path.join(root, "src"));
  for (const tool of ["Glob", "Grep"]) {
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "**/*.js" }), true, tool);
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "x", path: "" }), true, tool);
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "x", path: root }), true, tool);
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "x", path: "src" }), true, tool);
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "x", path: path.join(root, "src") }), true, tool);
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "x", path: os.homedir() }), false, tool);
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "x", path: ".." }), false, tool);
    assert.equal(isReadToolCallAllowed(root, tool, { pattern: "x", path: 42 }), false, tool);
  }
});

test("isReadToolCallAllowed: a Glob pattern that is absolute, ~-prefixed or has a .. segment is denied", () => {
  const root = makeRoot();
  assert.equal(isReadToolCallAllowed(root, "Glob", { pattern: "/etc/*" }), false);
  assert.equal(isReadToolCallAllowed(root, "Glob", { pattern: "~/.ssh/*" }), false);
  assert.equal(isReadToolCallAllowed(root, "Glob", { pattern: "../*" }), false);
  assert.equal(isReadToolCallAllowed(root, "Glob", { pattern: "src/../../x/*" }), false);
  assert.equal(isReadToolCallAllowed(root, "Glob", { pattern: "src\\..\\..\\x" }), false);
  assert.equal(isReadToolCallAllowed(root, "Glob", {}), false);
  // `..` only as part of a name is fine.
  assert.equal(isReadToolCallAllowed(root, "Glob", { pattern: "**/a..b.txt" }), true);
});

test("isReadToolCallAllowed: an unknown tool name denies (fail closed)", () => {
  const root = makeRoot();
  assert.equal(isReadToolCallAllowed(root, "WebFetch", { url: "x" }), false);
  assert.equal(isReadToolCallAllowed(root, "", {}), false);
});
