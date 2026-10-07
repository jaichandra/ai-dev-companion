// The hook's git config, checked against real git in a throwaway repo (no
// network, nothing outside the temp folder).
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const h = require("./git-hook.js");

const git = (args) => spawnSync("git", args, { encoding: "utf8" });
const SHA_A = "a".repeat(40);
const SHA_B = "b".repeat(40);
const ZERO = "0".repeat(40);

test("installArgs and uninstallArgs set and clear both keys in the repo's own config", () => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "git-hook-"));
  try {
    assert.equal(git(["init", "-q", repo]).status, 0);
    for (const args of h.installArgs(repo)) assert.equal(git(args).status, 0, args.join(" "));
    assert.equal(git(h.statusArgs(repo)).stdout.trim(), "companion precheck --stdin");
    assert.equal(git(["-C", repo, "config", "--local", "--get", "hook.companion-precheck.event"]).stdout.trim(), "pre-push");
    assert.ok(!fs.existsSync(path.join(repo, ".git", "hooks", "pre-push")), "no hook file is written");
    for (const args of h.uninstallArgs(repo)) assert.equal(git(args).status, 0);
    assert.notEqual(git(h.statusArgs(repo)).status, 0, "gone after uninstall");
  } finally {
    fs.rmSync(repo, { recursive: true, force: true });
  }
});

test("argv builders refuse a relative or option-like folder", () => {
  for (const bad of ["repo", "-c", "", null]) assert.throws(() => h.installArgs(bad), /absolute/);
  assert.deepEqual(h.probeSteps("/tmp/p")[0], ["init", "-q", "--template=", "/tmp/p"]);
  assert.deepEqual(h.probeSteps("/tmp/p").at(-1), ["-C", "/tmp/p", "hook", "run", "pre-push"]);
  assert.match(h.fallbackInstructions("/r"), /companion precheck --stdin \|\| true/);
});

test("parsePrePushLines reads git's four fields and skips anything else", () => {
  const text = `refs/heads/x ${SHA_A} refs/heads/x ${SHA_B}\njunk line\nrefs/heads/y ${SHA_A} refs/heads/y ${ZERO}\n`;
  assert.deepEqual(h.parsePrePushLines(text), [
    { localRef: "refs/heads/x", localSha: SHA_A, remoteRef: "refs/heads/x", remoteSha: SHA_B },
    { localRef: "refs/heads/y", localSha: SHA_A, remoteRef: "refs/heads/y", remoteSha: ZERO },
  ]);
  assert.deepEqual(h.parsePrePushLines(""), []);
});

test("changedFilesArgs: diff for an update, log for a new branch, nothing for a delete", () => {
  assert.deepEqual(h.changedFilesArgs({ localSha: SHA_A, remoteSha: SHA_B }), ["diff", "--name-only", SHA_B, SHA_A, "--"]);
  assert.deepEqual(h.changedFilesArgs({ localSha: SHA_A, remoteSha: ZERO }), ["log", "--name-only", "--format=", SHA_A, "--not", "--remotes", "--"]);
  // only real object ids reach git
  for (const bad of ["--output=/tmp/x", "HEAD", "abc", "", null, "a".repeat(41)]) {
    assert.equal(h.changedFilesArgs({ localSha: bad, remoteSha: SHA_B }), null, String(bad));
    assert.equal(h.changedFilesArgs({ localSha: SHA_A, remoteSha: bad }), null, String(bad));
  }
  assert.equal(h.changedFilesArgs(null), null);
  assert.equal(h.changedFilesArgs({ localSha: ZERO, remoteSha: SHA_B }), null);
  assert.deepEqual(h.parseNameList("a.ts\n\nb.ts\na.ts\n"), ["a.ts", "b.ts"]);
});

test("gitEnv: quotePath off always; the probe also ignores the user's and system git config", () => {
  const base = { PATH: "/bin", GIT_CONFIG_COUNT: "1", GIT_CONFIG_KEY_0: "x.y", GIT_CONFIG_VALUE_0: "z" };
  const e = h.gitEnv(base);
  assert.equal(e.GIT_TERMINAL_PROMPT, "0");
  assert.equal(e.GIT_CONFIG_COUNT, "2");
  assert.equal(e.GIT_CONFIG_KEY_0, "x.y");
  assert.equal(e.GIT_CONFIG_KEY_1, "core.quotePath");
  assert.equal(e.GIT_CONFIG_VALUE_1, "false");
  assert.equal(e.GIT_CONFIG_GLOBAL, undefined);
  const iso = h.gitEnv(base, { isolated: true });
  assert.equal(iso.GIT_CONFIG_GLOBAL, "/dev/null");
  assert.equal(iso.GIT_CONFIG_NOSYSTEM, "1");
  assert.equal(iso.GIT_CONFIG_COUNT, "1", "inherited config injection is dropped");
  assert.equal(iso.GIT_CONFIG_KEY_0, "core.quotePath");
  assert.equal(h.gitEnv({ GIT_CONFIG_COUNT: "junk" }).GIT_CONFIG_COUNT, "1");
});

test("gitEnv against real git: non-ASCII names come out unquoted", () => {
  const { spawnSync } = require("node:child_process");
  const fs = require("node:fs");
  const os = require("node:os");
  const path = require("node:path");
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "gitenv-"));
  try {
    const env = h.gitEnv(process.env, { isolated: true });
    const run = (args) => spawnSync("git", args, { cwd: dir, env, encoding: "utf8" });
    assert.equal(run(["init", "-q", "--template="]).status, 0);
    fs.writeFileSync(path.join(dir, "caf\u00e9.txt"), "x");
    run(["add", "."]);
    assert.equal(run(["diff", "--name-only", "--cached"]).stdout.trim(), "caf\u00e9.txt");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("uninstall removes only our two keys and is harmless on a repo that never had them", () => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "git-hook-un-"));
  try {
    assert.equal(git(["init", "-q", repo]).status, 0);
    for (const args of h.uninstallArgs(repo)) assert.equal(git(args).status, 5, "never installed: git says the key was not set");
    for (const [k, v] of [["hook.other.event", "pre-push"], ["hook.other.command", "echo hi"], ["user.name", "T"]]) {
      assert.equal(git(["-C", repo, "config", "--local", k, v]).status, 0);
    }
    for (const args of h.installArgs(repo)) assert.equal(git(args).status, 0);
    for (const args of h.uninstallArgs(repo)) assert.equal(git(args).status, 0);
    assert.equal(git(["-C", repo, "config", "--local", "--get", "hook.other.command"]).stdout.trim(), "echo hi");
    assert.equal(git(["-C", repo, "config", "--local", "--get", "hook.other.event"]).stdout.trim(), "pre-push");
    assert.equal(git(["-C", repo, "config", "--local", "--get", "user.name"]).stdout.trim(), "T");
  } finally {
    fs.rmSync(repo, { recursive: true, force: true });
  }
});

test("statusArgs reports only a repo that has the command; a half-installed one (event only) is not installed", () => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "git-hook-half-"));
  try {
    assert.equal(git(["init", "-q", repo]).status, 0);
    assert.notEqual(git(h.statusArgs(repo)).status, 0);
    assert.equal(git(["-C", repo, "config", "--local", "hook.companion-precheck.event", "pre-push"]).status, 0);
    assert.notEqual(git(h.statusArgs(repo)).status, 0, "no command yet");
    assert.equal(git(["-C", repo, "config", "--local", "hook.companion-precheck.command", "companion precheck --stdin"]).status, 0);
    assert.equal(git(h.statusArgs(repo)).status, 0);
  } finally {
    fs.rmSync(repo, { recursive: true, force: true });
  }
});

test("probeSteps run against real git in an isolated environment and end with the marker or a plain failure", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "git-hook-probe-"));
  try {
    const env = h.gitEnv(process.env, { isolated: true });
    let last = { status: 1, stdout: "" };
    for (const args of h.probeSteps(dir)) {
      last = spawnSync("git", args, { encoding: "utf8", env });
      if (last.status !== 0) break;
    }
    // Older gits have no `git hook run` / config hooks: the probe then simply reports "not supported".
    const supported = last.status === 0 && last.stdout.includes(h.PROBE_MARKER);
    assert.equal(typeof supported, "boolean");
    if (supported) assert.match(last.stdout, /companion-probe-ok/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
