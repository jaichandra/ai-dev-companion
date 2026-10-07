// computeWorkspace from the build (npm run build first), with every
// network and git call injected — no Bitbucket, no credentials, and HOME
// pointed at a temp dir before anything loads.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "ticket-ws-home-"));
const { computeWorkspace, createTicketWorkspaceFeature } = require("../../dist/features/ticket-workspace/index.js");
const { jobStore } = require("../../dist/core/jobs.js");
const { repoWorktreesRoot } = require("../../core/paths.js");

const BB = "https://bb.example";
const SITE = { baseUrl: () => BB };

/** Two "clones" (folders with a .git dir); only their existence is checked here. */
function fakeRepos() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "ticket-ws-"));
  for (const name of ["sample-app", "billing"]) fs.mkdirSync(path.join(root, name, ".git"), { recursive: true });
  return { root, repos: { "ACME/sample-app": path.join(root, "sample-app"), "ACME/billing": path.join(root, "billing"), "ACME/gone": path.join(root, "gone") } };
}

function io(over = {}) {
  const calls = [];
  return {
    calls,
    io: {
      git: () => SITE,
      scanRepoForTicket: async (repoPath, key) => {
        calls.push(["scan", path.basename(repoPath), key]);
        if (path.basename(repoPath) !== "sample-app") return { branches: [], worktrees: [] };
        return {
          branches: [{ name: "bugfix/PROJ-7-login", local: true, remote: true, ahead: 1, behind: 0 }],
          worktrees: [{ dir: path.join(repoWorktreesRoot(repoPath), "PROJ-7"), branch: "bugfix/PROJ-7-login", exists: true, dirty: false, changed: 0, ahead: 1, behind: 0, isTicketWorktree: true }],
        };
      },
      listBranchPullRequests: async (site, auth, project, repo, branch) => {
        calls.push(["prs", project, repo, branch]);
        return [{ id: 12, title: "PROJ-7: fix", state: "OPEN", fromBranch: branch, fromSha: "abc1234", toBranch: "master", url: `${BB}/projects/ACME/repos/sample-app/pull-requests/12`, updatedAt: 1 }];
      },
      getCommitBuildStatus: async (site, auth, project, repo, sha) => {
        calls.push(["builds", sha]);
        return { state: "SUCCESSFUL", counts: { SUCCESSFUL: 1 }, builds: [] };
      },
      ...over,
    },
  };
}

test("scans only real clones, asks Bitbucket only for origin branches, and adds build state to open PRs", async () => {
  const { root, repos } = fakeRepos();
  try {
    const f = io();
    const ws = await computeWorkspace({ repos }, {}, {}, "PROJ-7", undefined, f.io);
    assert.deepEqual(f.calls.filter((c) => c[0] === "scan").map((c) => c[1]).sort(), ["billing", "sample-app"]);
    assert.deepEqual(f.calls.find((c) => c[0] === "prs"), ["prs", "ACME", "sample-app", "bugfix/PROJ-7-login"]);
    assert.deepEqual(f.calls.find((c) => c[0] === "builds"), ["builds", "abc1234"]);
    assert.equal(ws.prs.length, 1);
    assert.equal(ws.prs[0].build.state, "SUCCESSFUL");
    assert.equal(ws.suggestions.createPr, null); // the worktree's branch already has an open PR
    assert.equal(ws.resume.worktreeDir, path.join(repoWorktreesRoot(path.join(root, "sample-app")), "PROJ-7"));
    assert.deepEqual(ws.notes, []);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a git or Bitbucket failure becomes a note, never a failed scan", async () => {
  const { root, repos } = fakeRepos();
  try {
    const f = io({
      listBranchPullRequests: async () => {
        throw new Error("Bitbucket rejected the saved API token");
      },
    });
    const base = f.io.scanRepoForTicket;
    f.io.scanRepoForTicket = async (repoPath, key) => {
      if (path.basename(repoPath) === "billing") throw new Error("not a git repository");
      return base(repoPath, key);
    };
    const ws = await computeWorkspace({ repos }, {}, {}, "PROJ-7", undefined, f.io);
    assert.equal(ws.prs.length, 0);
    assert.equal(ws.worktrees.length, 1);
    assert.ok(ws.notes.some((n) => /ACME\/billing: not a git repository/.test(n)));
    assert.ok(ws.notes.some((n) => /Bitbucket \(ACME\/sample-app bugfix\/PROJ-7-login\): Bitbucket rejected/.test(n)));
    assert.deepEqual(ws.suggestions.createPr, { repoKey: "ACME/sample-app", dir: path.join(repoWorktreesRoot(path.join(root, "sample-app")), "PROJ-7"), branch: "bugfix/PROJ-7-login" });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("the history is read first when there is one", async () => {
  const seen = [];
  const history = {
    getItem(key) {
      seen.push(key);
      if (key === "jira:PROJ-7") {
        return { item: { key }, edges: [{ kind: "analysis", key: "analysis:jira:PROJ-7" }], facts: [] };
      }
      if (key === "analysis:jira:PROJ-7") return { item: { title: "Analysis of PROJ-7", excerpt: "Null check", updatedAt: 3 }, edges: [], facts: [] };
      return null;
    },
  };
  const ws = await computeWorkspace({ repos: {} }, { history }, {}, "PROJ-7", undefined, io().io);
  assert.deepEqual(seen, ["jira:PROJ-7", "analysis:jira:PROJ-7"]);
  assert.equal(ws.analyses[0].excerpt, "Null check");
  assert.equal(ws.suggestions.startFix, true);
});

test("computeWorkspace refuses anything but an issue key before touching history, git or Bitbucket", async () => {
  const f = io();
  const history = { getItem: () => assert.fail("history must not be read") };
  await assert.rejects(computeWorkspace({ repos: {} }, { history }, {}, "PROJ-7; rm -rf /", undefined, f.io), /issueKey/);
  assert.deepEqual(f.calls, []);
});

const S_EVIL = "33333333-3333-4333-8333-333333333333";
const S_OK = "44444444-4444-4444-8444-444444444444";

/** A history store whose rows were hand-edited: an evil worktree and session, and a good session. */
function tamperedHistory(goodCwd) {
  const rows = {
    "jira:PROJ-7": { item: { key: "jira:PROJ-7" }, edges: [{ kind: "worktree", key: "worktree:/tmp/evil" }, { kind: "job", key: "job:x" }], facts: [] },
    "worktree:/tmp/evil": { item: { data: { dir: "/tmp/evil", repoKey: "ACME/sample-app", branch: "b" } }, edges: [{ kind: "session", key: `session:${S_EVIL}` }] },
    "job:x": { item: {}, edges: [{ kind: "session", key: `session:${S_OK}` }] },
    [`session:${S_EVIL}`]: { item: { updatedAt: 9, data: { cwd: "/tmp/evil", permissionMode: "plan" } }, edges: [] },
    [`session:${S_OK}`]: { item: { updatedAt: 1, data: { cwd: goodCwd, permissionMode: "plan" } }, edges: [] },
  };
  return { getItem: (k) => rows[k] || null };
}

test("a tampered history row (cwd /tmp/evil, dir /tmp/evil) is dropped from the scan, with notes, and never reaches resume", async () => {
  const { root, repos } = fakeRepos();
  try {
    const good = path.join(root, "sample-app");
    const ws = await computeWorkspace({ repos }, { history: tamperedHistory(good) }, {}, "PROJ-7", undefined, io().io);
    assert.ok(!ws.worktrees.some((w) => w.dir === "/tmp/evil"));
    assert.deepEqual(ws.sessions.map((s) => s.id), [S_OK]);
    assert.ok(ws.notes.some((n) => /\/tmp\/evil/.test(n)));
    assert.notEqual(ws.resume.worktreeDir, "/tmp/evil");
    assert.notEqual(ws.resume.session && ws.resume.session.cwd, "/tmp/evil");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("the workspace's own actions refuse a folder or session that isn't in the scan, and a tampered one never gets that far", async () => {
  const { root, repos } = fakeRepos();
  try {
    const feature = createTicketWorkspaceFeature({ repos }, { history: tamperedHistory(path.join(root, "sample-app")) }, io().io);
    const started = await feature.start({ issueKey: "PROJ-7" }, { auth: {} });
    for (let i = 0; i < 100 && jobStore.get(started.id).status === "running"; i++) await new Promise((r) => setTimeout(r, 20));
    const job = jobStore.get(started.id);
    assert.equal(job.status, "awaiting-approval", job.error);
    await assert.rejects(feature.actions["open-worktree"](job, { auth: {}, body: { dir: "/tmp/evil" } }), /isn't part of this workspace/);
    await assert.rejects(feature.actions["open-worktree"](job, { auth: {}, body: {} }), /isn't part of this workspace/);
    await assert.rejects(feature.actions["resume-session"](job, { auth: {}, body: { sessionId: S_EVIL } }), /isn't part of this workspace/);
    await assert.rejects(feature.actions["resume-session"](job, { auth: {}, body: { sessionId: "nope" } }), /isn't part of this workspace/);
    // A closed workspace refuses both.
    const closed = { ...job, status: "rejected" };
    await assert.rejects(feature.actions["open-worktree"](closed, { auth: {}, body: { dir: "x" } }), /closed/);
    await assert.rejects(feature.actions["resume-session"](closed, { auth: {}, body: { sessionId: S_OK } }), /closed/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a Bitbucket setting that can't be read is a note, not a failed scan", async () => {
  const { root, repos } = fakeRepos();
  try {
    const f = io({
      git: () => {
        throw new Error("bitbucket.baseUrl is not a valid URL");
      },
    });
    const ws = await computeWorkspace({ repos }, {}, {}, "PROJ-7", undefined, f.io);
    assert.ok(ws.notes.some((n) => /Bitbucket: bitbucket\.baseUrl is not a valid URL/.test(n)));
    assert.equal(ws.worktrees.length, 1);
    assert.equal(ws.prs.length, 0);
    assert.equal(f.calls.filter((c) => c[0] === "prs").length, 0);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a history PR with a bad address is replaced by the rebuilt one on the configured host", async () => {
  const history = {
    getItem(key) {
      if (key === "jira:PROJ-7") return { item: {}, edges: [{ kind: "pr", key: "bitbucket:ACME/sample-app#12" }], facts: [] };
      if (key === "bitbucket:ACME/sample-app#12") return { item: { title: "t", url: "javascript:alert(1)" }, edges: [] };
      return null;
    },
  };
  const ws = await computeWorkspace({ repos: {} }, { history }, {}, "PROJ-7", undefined, io().io);
  assert.equal(ws.prs.length, 1);
  assert.equal(ws.prs[0].url, `${BB}/projects/ACME/repos/sample-app/pull-requests/12`);
});

test("a PR Jira links to the ticket is added even though its branch is gone, and one the branch lookup found isn't doubled", async () => {
  const { root, repos } = fakeRepos();
  try {
    const f = io({
      linkedPullRequests: async () => [
        { repoKey: "ACME/sample-app", id: 12, title: "dup", state: "OPEN", url: `${BB}/projects/ACME/repos/sample-app/pull-requests/12` },
        { repoKey: "ACME/sample-app", id: 9, title: "PROJ-7 merged fix", state: "MERGED", url: `${BB}/projects/ACME/repos/sample-app/pull-requests/9` },
        { repoKey: "ACME/other", id: 3, title: "elsewhere", state: "DECLINED", url: `${BB}/projects/ACME/repos/other/pull-requests/3` },
      ],
    });
    const ws = await computeWorkspace({ repos }, {}, {}, "PROJ-7", undefined, f.io);
    assert.deepEqual(ws.prs.map((p) => [p.repoKey, p.id, p.state]).sort(), [
      ["ACME/other", 3, "DECLINED"],
      ["ACME/sample-app", 12, "OPEN"],
      ["ACME/sample-app", 9, "MERGED"],
    ].sort());
    assert.equal(ws.prs.find((p) => p.id === 12).title, "PROJ-7: fix");
    assert.deepEqual(ws.notes, []);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Jira's linked PRs failing is a note, and the branch PRs still show", async () => {
  const { root, repos } = fakeRepos();
  try {
    const f = io({
      linkedPullRequests: async () => {
        throw new Error("No Jira browser session");
      },
    });
    const ws = await computeWorkspace({ repos }, {}, {}, "PROJ-7", undefined, f.io);
    assert.equal(ws.prs.length, 1);
    assert.ok(ws.notes.some((n) => /Jira linked pull requests: No Jira browser session/.test(n)));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
