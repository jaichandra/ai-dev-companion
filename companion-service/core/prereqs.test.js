const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const net = require("net");
const { execFileSync } = require("child_process");
const prereqs = require("./prereqs.js");

/** Real (but network-free) git repo: `git init` + a fake origin remote. */
function makeFakeClone(dir, originUrl) {
  fs.mkdirSync(dir, { recursive: true });
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["remote", "add", "origin", originUrl], { cwd: dir });
}

test("nodeMeetsMinimum: 22.13 and newer pass, older fail", () => {
  assert.equal(prereqs.nodeMeetsMinimum("22.13.0"), true);
  assert.equal(prereqs.nodeMeetsMinimum("22.12.9"), false);
  assert.equal(prereqs.nodeMeetsMinimum("22.20.1"), true);
  assert.equal(prereqs.nodeMeetsMinimum("24.19.0"), true);
  assert.equal(prereqs.nodeMeetsMinimum("20.18.0"), false);
  assert.equal(prereqs.nodeMeetsMinimum("18.20.4"), false);
  assert.equal(prereqs.nodeMeetsMinimum("garbage"), false);
});

test("checkNodeVersion takes an injected version and names the minimum", () => {
  const bad = prereqs.checkNodeVersion("20.18.0");
  assert.equal(bad.ok, false);
  assert.match(bad.message, /22\.13/);
  assert.equal(prereqs.checkNodeVersion("22.13.0").ok, true);
});

test("checkNodeVersion passes on the Node version running the test", () => {
  const result = prereqs.checkNodeVersion();
  assert.equal(result.ok, true);
});

test("checkGit passes when git is on PATH", () => {
  const result = prereqs.checkGit();
  assert.equal(result.ok, true);
});

test("checkRepoPath fails for a path that does not exist", () => {
  const result = prereqs.checkRepoPath("/definitely/not/a/real/path/xyz-does-not-exist");
  assert.equal(result.ok, false);
  assert.match(result.message, /does not exist/);
});

test("checkRepoPath fails for a directory that exists but is not a git repo", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "prereq-test-"));
  const result = prereqs.checkRepoPath(dir);
  assert.equal(result.ok, false);
  assert.match(result.message, /not a git repository/);
  fs.rmSync(dir, { recursive: true, force: true });
});

/** Prepends a temp dir with a fake `claude` script (shebang'd, chmod +x)
 * onto PATH for the duration of `fn`, then restores it. `script` is the
 * shell script body (no shebang line needed). */
function withFakeClaudeOnPath(script, fn) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "fake-claude-"));
  const bin = path.join(dir, "claude");
  fs.writeFileSync(bin, `#!/bin/sh\n${script}\n`, { mode: 0o755 });
  const originalPath = process.env.PATH;
  process.env.PATH = `${dir}:${originalPath}`;
  try {
    return fn();
  } finally {
    process.env.PATH = originalPath;
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

test("checkClaudeAuth reports a timed-out claude auth status as a failed check with an actionable message", () => {
  withFakeClaudeOnPath(
    // Ignores TERM (execFileSync's default killSignal) but not KILL, and
    // never produces stdout, so a run that isn't actually killed via
    // SIGKILL would otherwise hang the test for the real 5s default.
    'trap "" TERM\nsleep 5',
    () => {
      const result = prereqs.checkClaudeAuth(200);
      assert.equal(result.ok, false);
      assert.match(result.message, /did not respond within 0\.2s/);
    },
  );
});

test("checkPortFree reports a free port as ok", async () => {
  const result = await prereqs.checkPortFree(58234);
  assert.equal(result.ok, true);
});

test("checkPortFree reports an occupied port as not ok", async () => {
  const server = net.createServer().listen(58235, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  try {
    const result = await prereqs.checkPortFree(58235);
    assert.equal(result.ok, false);
    assert.match(result.message, /already in use/);
  } finally {
    server.close();
  }
});

test("originMatchesProjectRepo matches Bitbucket Server's lowercased-project URL shape", () => {
  assert.equal(
    prereqs.originMatchesProjectRepo("https://bitbucket.example.com/scm/acme/sample-app.git", "ACME", "sample-app"),
    true,
  );
});

test("originMatchesProjectRepo matches the ssh remote shape too", () => {
  assert.equal(
    prereqs.originMatchesProjectRepo("ssh://git@bitbucket.example.com/acme/sample-app.git", "ACME", "sample-app"),
    true,
  );
});

test("originMatchesProjectRepo rejects a different repo under the same project", () => {
  assert.equal(
    prereqs.originMatchesProjectRepo(
      "https://bitbucket.example.com/scm/acme/sample-service.git",
      "ACME",
      "sample-app",
    ),
    false,
  );
});

test("originMatchesProjectRepo rejects a same-named repo under a different project", () => {
  assert.equal(
    prereqs.originMatchesProjectRepo("https://bitbucket.example.com/scm/om/sample-app.git", "ACME", "sample-app"),
    false,
  );
});

test("originMatchesProjectRepo is false for null/missing origin", () => {
  assert.equal(prereqs.originMatchesProjectRepo(null, "ACME", "sample-app"), false);
});

test("inferRepoPath finds a sibling clone whose origin matches the requested project/repo", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "infer-test-"));
  makeFakeClone(path.join(root, "sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
  makeFakeClone(
    path.join(root, "sample-service"),
    "https://bitbucket.example.com/scm/acme/sample-service.git",
  );

  const existingRepos = { "ACME/sample-app": path.join(root, "sample-app") };
  const found = prereqs.inferRepoPath(existingRepos, "ACME", "sample-service");
  assert.equal(found, path.join(root, "sample-service"));

  fs.rmSync(root, { recursive: true, force: true });
});

test("inferRepoPath refuses a sibling directory whose origin points elsewhere", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "infer-test-"));
  makeFakeClone(path.join(root, "sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
  // A directory happens to be named "sample-service" but is actually a
  // clone of something unrelated — must not be silently adopted.
  makeFakeClone(
    path.join(root, "sample-service"),
    "https://bitbucket.example.com/scm/other/unrelated-fork.git",
  );

  const existingRepos = { "ACME/sample-app": path.join(root, "sample-app") };
  const found = prereqs.inferRepoPath(existingRepos, "ACME", "sample-service");
  assert.equal(found, null);

  fs.rmSync(root, { recursive: true, force: true });
});

test("parseBitbucketOrigin handles the HTTPS and both SSH remote shapes", () => {
  const expected = { project: "ACME", repo: "sample-app", key: "ACME/sample-app" };
  assert.deepEqual(prereqs.parseBitbucketOrigin("https://bitbucket.example.com/scm/acme/sample-app.git"), expected);
  assert.deepEqual(
    prereqs.parseBitbucketOrigin("https://jdoe@bitbucket.example.com/scm/acme/sample-app"),
    expected,
  );
  assert.deepEqual(
    prereqs.parseBitbucketOrigin("ssh://git@bitbucket.example.com:7999/acme/sample-app.git"),
    expected,
  );
  assert.deepEqual(prereqs.parseBitbucketOrigin("git@bitbucket.example.com:acme/sample-app.git"), expected);
});

test("parseBitbucketOrigin rejects personal repos, GitHub remotes and non-/scm/ HTTPS URLs", () => {
  assert.equal(prereqs.parseBitbucketOrigin("https://bitbucket.example.com/scm/~jdoe/tools.git"), null);
  assert.equal(prereqs.parseBitbucketOrigin("git@github.com:someone/tools.git"), null);
  assert.equal(prereqs.parseBitbucketOrigin("https://git.example.com/some/mirror.git"), null);
  assert.equal(prereqs.parseBitbucketOrigin(null), null);
});

test("discoverLocalClones finds Bitbucket clones one level deep, skipping worktrees and non-repos", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "discover-test-"));
  makeFakeClone(path.join(root, "sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
  makeFakeClone(path.join(root, "tools"), "git@github.com:someone/tools.git");
  fs.mkdirSync(path.join(root, "plain-folder"));
  // A linked worktree has a .git *file*, not a directory.
  fs.mkdirSync(path.join(root, "sample-app.worktrees", "pr-review"), { recursive: true });
  fs.mkdirSync(path.join(root, "linked"));
  fs.writeFileSync(path.join(root, "linked", ".git"), "gitdir: /elsewhere\n");

  const found = prereqs.discoverLocalClones([root, path.join(root, "does-not-exist")]);
  assert.deepEqual(found, [{ key: "ACME/sample-app", path: path.join(root, "sample-app") }]);

  fs.rmSync(root, { recursive: true, force: true });
});

test("inferRepoPath also searches extraRoots when no mapped repo is a sibling", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "infer-test-"));
  makeFakeClone(path.join(root, "sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");

  assert.equal(prereqs.inferRepoPath({}, "ACME", "sample-app"), null);
  assert.equal(prereqs.inferRepoPath({}, "ACME", "sample-app", [root]), path.join(root, "sample-app"));

  fs.rmSync(root, { recursive: true, force: true });
});

test("inferRepoPath returns null when no sibling directory exists at all", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "infer-test-"));
  makeFakeClone(path.join(root, "sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");

  const existingRepos = { "ACME/sample-app": path.join(root, "sample-app") };
  const found = prereqs.inferRepoPath(existingRepos, "ACME", "nonexistent-repo");
  assert.equal(found, null);

  fs.rmSync(root, { recursive: true, force: true });
});

test("isSafeRepoSegment accepts project and repo names (a leading dot too: GitHub's .github) and rejects path tricks", () => {
  for (const ok of ["ACME", "sample-app", "sample-service", "libs.commons_2", ".github", ".hidden"]) {
    assert.equal(prereqs.isSafeRepoSegment(ok), true, ok);
  }
  for (const bad of ["", ".", "..", "a..b", "../etc", "a/b", "-rf", ".", ".-x", ".git", ".GIT", "with space", undefined, 42]) {
    assert.equal(prereqs.isSafeRepoSegment(bad), false, String(bad));
  }
});

test("deriveCloneUrl reuses the transport of an existing clone's origin", () => {
  const derive = (origin) =>
    prereqs.deriveCloneUrl({ existingOrigins: [origin], project: "ACME", repo: "sample-service" });
  assert.equal(
    derive("https://bitbucket.example.com/scm/acme/sample-app.git"),
    "https://bitbucket.example.com/scm/acme/sample-service.git",
  );
  assert.equal(
    derive("ssh://git@bitbucket.example.com:7999/cis/sample-app.git"),
    "ssh://git@bitbucket.example.com:7999/acme/sample-service.git",
  );
});

test("deriveCloneUrl skips non-Bitbucket origins and falls back to the page origin", () => {
  assert.equal(
    prereqs.deriveCloneUrl({
      existingOrigins: ["https://github.com/some/mirror.git"],
      pageOrigin: "https://bitbucket.example.com/",
      project: "ACME",
      repo: "sample-app",
    }),
    "https://bitbucket.example.com/scm/acme/sample-app.git",
  );
  assert.equal(prereqs.deriveCloneUrl({ existingOrigins: [], project: "ACME", repo: "sample-app" }), null);
});

test("cloneRoot picks the folder most existing clones live in", () => {
  const repos = { "A/a": "/x/gitviews/a", "A/b": "/x/gitviews/b", "A/c": "/y/other/c" };
  assert.equal(prereqs.cloneRoot(repos, ["/fallback"]), "/x/gitviews");
  assert.equal(prereqs.cloneRoot({}, ["/fallback"]), "/fallback");
  assert.equal(prereqs.cloneRoot({}, []), path.join(os.homedir(), "gitviews"));
});

test("resolveReviewEditor keeps claude-code and cursor, and ignores a saved vscode", () => {
  assert.equal(prereqs.resolveReviewEditor("cursor"), "cursor");
  assert.equal(prereqs.resolveReviewEditor("claude-code"), "claude-code");
  assert.notEqual(prereqs.resolveReviewEditor("vscode"), "vscode");
  assert.ok(prereqs.detectInstalledEditors().every((e) => e.id !== "vscode"));
});

// ---- per-provider remote rules (Bitbucket and GitHub) ----

test("parseGithubOrigin reads HTTPS, scp-style and ssh:// remotes, with or without .git, and refuses Bitbucket's /scm/ paths", () => {
  assert.deepEqual(prereqs.parseGithubOrigin("https://github.com/Octo/hello.git"), { project: "Octo", repo: "hello", key: "Octo/hello" });
  assert.deepEqual(prereqs.parseGithubOrigin("https://github.com/Octo/hello"), { project: "Octo", repo: "hello", key: "Octo/hello" });
  assert.deepEqual(prereqs.parseGithubOrigin("https://token@github.com/octo/hello.git/"), { project: "octo", repo: "hello", key: "octo/hello" });
  assert.equal(prereqs.parseGithubOrigin("git@github.com:octo/.github.git").repo, ".github");
  assert.deepEqual(prereqs.parseGithubOrigin("ssh://git@ghe.example.com:2222/octo/hello.git"), { project: "octo", repo: "hello", key: "octo/hello" });
  assert.equal(prereqs.parseGithubOrigin("https://bitbucket.example.com/scm/acme/sample-app.git"), null);
  assert.equal(prereqs.parseGithubOrigin("https://github.com/only-one-part"), null);
  assert.equal(prereqs.parseGithubOrigin(""), null);
  assert.equal(prereqs.parseGithubOrigin(undefined), null);
});

test("originMatchesProjectRepo works with or without .git, case-insensitively, and treats names literally", () => {
  const m = prereqs.originMatchesProjectRepo;
  assert.equal(m("https://github.com/Octo/hello", "octo", "hello"), true);
  assert.equal(m("https://github.com/Octo/hello.git", "octo", "hello"), true);
  assert.equal(m("git@github.com:octo/hello.git", "OCTO", "hello"), true);
  assert.equal(m("https://bb.example.com/scm/acme/sample-app.git", "ACME", "sample-app"), true);
  assert.equal(m("https://github.com/octo/hello-world", "octo", "hello"), false, "a longer name is another repo");
  assert.equal(m("https://github.com/octo/hello", "octo", "hel.o"), false, "the dot is a dot, not a wildcard");
  assert.equal(m("https://github.com/o.cto/hello", "oXcto", "hello"), false);
  assert.equal(m("", "o", "r"), false);
});

test("each provider's rules: Bitbucket upper-cases the project in keys, GitHub lower-cases both", () => {
  const { GIT_REMOTE_RULES: rules } = prereqs;
  assert.equal(rules["bitbucket-dc"].prKey("acme", "Sample-App", 7), "bitbucket:ACME/sample-app#7");
  assert.equal(rules.github.prKey("Octo", "Hello", 7), "github:octo/hello#7");
  assert.equal(rules["bitbucket-dc"].keyPrefix, "bitbucket");
  assert.equal(rules.github.keyPrefix, "github");
});

test("each provider's rules: the path of a PR page and reading one back", () => {
  const { GIT_REMOTE_RULES: rules } = prereqs;
  assert.equal(rules["bitbucket-dc"].prPath("ACME", "sample-app", 7), "/projects/ACME/repos/sample-app/pull-requests/7");
  assert.equal(rules.github.prPath("Octo", "hello", 7), "/Octo/hello/pull/7");
  assert.deepEqual(rules["bitbucket-dc"].parsePrUrl("https://bb.example.com/projects/ACME/repos/sample-app/pull-requests/7/overview"), { project: "ACME", repo: "sample-app", id: 7 });
  assert.deepEqual(rules.github.parsePrUrl("https://github.com/Octo/hello/pull/7/files"), { project: "Octo", repo: "hello", id: 7 });
  assert.deepEqual(rules.github.parsePrUrl("/Octo/hello/pull/7"), { project: "Octo", repo: "hello", id: 7 });
  assert.equal(rules.github.parsePrUrl("https://github.com/Octo/hello/issues/7"), null);
  assert.equal(rules["bitbucket-dc"].parsePrUrl("https://github.com/Octo/hello/pull/7"), null);
  assert.deepEqual(rules.github.parseRepoUrl("https://github.com/Octo/hello/tree/main"), { project: "Octo", repo: "hello" });
  assert.deepEqual(rules["bitbucket-dc"].parseRepoUrl("https://bb.example.com/projects/ACME/repos/sample-app/browse"), { project: "ACME", repo: "sample-app" });
  assert.equal(rules.github.parseRepoUrl("https://github.com/"), null);
});

test("each provider's rules: where a fresh clone comes from", () => {
  const { GIT_REMOTE_RULES: rules } = prereqs;
  assert.equal(rules["bitbucket-dc"].pageCloneUrl("https://bb.example.com/", "ACME", "sample-app"), "https://bb.example.com/scm/acme/sample-app.git");
  assert.equal(rules.github.pageCloneUrl("https://github.com/", "Octo", "hello"), "https://github.com/Octo/hello.git");
  assert.equal(rules.github.pageCloneUrl("https://ghe.example.com", "o", ".github"), "https://ghe.example.com/o/.github.git");
});

test("the rules in force are the profile's git provider's (the placeholder profile's is Bitbucket)", () => {
  assert.equal(prereqs.gitRemoteRules(), prereqs.GIT_REMOTE_RULES["bitbucket-dc"]);
});
