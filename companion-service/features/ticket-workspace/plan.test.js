const test = require("node:test");
const assert = require("node:assert/strict");
const plan = require("./plan.js");

const BB = "https://bb.example";
const WT = "/Users/me/gitviews/sample-app.worktrees/PROJ-7";
const S1 = "11111111-1111-4111-8111-111111111111";
const S2 = "22222222-2222-4222-8222-222222222222";

const ticketDetail = {
  item: { kind: "ticket", key: "jira:PROJ-7", title: "Login fails" },
  edges: [
    { rel: "worktree", kind: "worktree", key: `worktree:${WT}`, direction: "out" },
    { rel: "links", kind: "pr", key: "bitbucket:ACME/sample-app#12", direction: "out" },
    { rel: "analyzed", kind: "analysis", key: "analysis:jira:PROJ-7", direction: "out" },
    { rel: "links", kind: "job", key: "job:abc", direction: "in" },
  ],
  facts: [],
};
const related = {
  [`worktree:${WT}`]: {
    item: { kind: "worktree", data: { dir: WT, repoKey: "ACME/sample-app", branch: "bugfix/PROJ-7-login", base: "master", fixStartedAt: 5 } },
    edges: [{ kind: "session", key: `session:${S1}`, rel: "links", direction: "out" }],
  },
  "bitbucket:ACME/sample-app#12": { item: { kind: "pr", title: "PROJ-7: fix", url: `${BB}/projects/ACME/repos/sample-app/pull-requests/12` }, edges: [] },
  "analysis:jira:PROJ-7": { item: { kind: "analysis", title: "Analysis of PROJ-7", excerpt: "Null check", updatedAt: 9 }, edges: [] },
  "job:abc": { item: { kind: "job" }, edges: [{ kind: "session", key: `session:${S2}`, rel: "links", direction: "out" }] },
  [`session:${S1}`]: { item: { kind: "session", updatedAt: 20, data: { cwd: WT, permissionMode: "plan" } }, edges: [] },
  [`session:${S2}`]: { item: { kind: "session", updatedAt: 30, data: { cwd: "/Users/me/gitviews/sample-app", permissionMode: "plan" } }, edges: [] },
};

const allow = { worktreeOk: () => true, sessionCwdOk: () => true };

test("parseWorkspacePayload upper-cases the key and refuses anything else", () => {
  assert.deepEqual(plan.parseWorkspacePayload({ issueKey: " proj-7 " }), { issueKey: "PROJ-7" });
  assert.throws(() => plan.parseWorkspacePayload({ issueKey: "PROJ" }));
  assert.throws(() => plan.parseWorkspacePayload(null));
});

test("secondHopKeys and sessionKeysOf walk ticket -> neighbours -> sessions", () => {
  assert.deepEqual(plan.secondHopKeys(ticketDetail), [
    `worktree:${WT}`,
    "bitbucket:ACME/sample-app#12",
    "analysis:jira:PROJ-7",
    "job:abc",
  ]);
  assert.deepEqual(plan.sessionKeysOf([related[`worktree:${WT}`], related["job:abc"]]), [`session:${S1}`, `session:${S2}`]);
});

test("historyFacts collects worktrees, PRs, analyses and valid sessions", () => {
  const f = plan.historyFacts(ticketDetail, related, allow);
  assert.deepEqual(f.worktrees, [{ dir: WT, repoKey: "ACME/sample-app", branch: "bugfix/PROJ-7-login", base: "master", fixStartedAt: 5 }]);
  assert.deepEqual(f.prs, [{ project: "ACME", repo: "sample-app", id: 12, title: "PROJ-7: fix", url: `${BB}/projects/ACME/repos/sample-app/pull-requests/12` }]);
  assert.equal(f.analyses[0].excerpt, "Null check");
  assert.deepEqual(f.sessions.map((s) => s.id).sort(), [S1, S2]);
  const bad = { ...related, [`session:${S1}`]: { item: { data: { cwd: "relative", permissionMode: "plan" } }, edges: [] } };
  assert.deepEqual(plan.historyFacts(ticketDetail, bad, allow).sessions.map((s) => s.id), [S2]);
  assert.deepEqual(plan.historyFacts(null, {}), { worktrees: [], prs: [], analyses: [], sessions: [], notes: [] });
});

test("prUrlAllowed only accepts an https PR page on the configured Bitbucket", () => {
  assert.ok(plan.prUrlAllowed(`${BB}/projects/ACME/repos/sample-app/pull-requests/12`, BB));
  assert.ok(!plan.prUrlAllowed("https://evil.example/projects/ACME/repos/sample-app/pull-requests/12", BB));
  assert.ok(!plan.prUrlAllowed("javascript:alert(1)", BB));
  assert.ok(!plan.prUrlAllowed(`${BB}/plugins/servlet/x`, BB));
});

const liveRepo = (over = {}) => ({
  repoKey: "ACME/sample-app",
  branches: [{ name: "bugfix/PROJ-7-login", local: true, remote: true, ahead: 1, behind: 0 }],
  worktrees: [{ dir: WT, branch: "bugfix/PROJ-7-login", exists: true, dirty: true, changed: 2, ahead: 1, behind: 0, isTicketWorktree: true }],
  prs: [],
  ...over,
});

test("buildWorkspace merges live and history, and offers Create PR when the ticket worktree has no open PR", () => {
  const ws = plan.buildWorkspace({
    issueKey: "PROJ-7",
    bitbucketBaseUrl: BB,
    repos: [liveRepo()],
    history: plan.historyFacts(ticketDetail, related, allow),
    cwdAllowed: () => true,
  });
  assert.equal(ws.worktrees.length, 1);
  assert.equal(ws.prs.length, 1);
  assert.equal(ws.prs[0].source, "history");
  assert.equal(ws.suggestions.startFix, false);
  assert.deepEqual(ws.suggestions.createPr, { repoKey: "ACME/sample-app", dir: WT, branch: "bugfix/PROJ-7-login" });
  assert.equal(ws.resume.session.id, S1); // the session in the ticket worktree wins over a newer one elsewhere
  assert.equal(ws.resume.worktreeDir, WT);
  assert.equal(plan.workspaceSummary(ws), "PROJ-7: 1 worktree, 1 branch, 1 PR, 1 analysis, 2 Claude sessions.");
});

test("an open PR from the worktree's branch turns Create PR off and carries its build state", () => {
  const pr = {
    id: 12,
    title: "PROJ-7: fix",
    state: "OPEN",
    url: `${BB}/projects/ACME/repos/sample-app/pull-requests/12`,
    fromBranch: "bugfix/PROJ-7-login",
    toBranch: "master",
    build: {
      state: "FAILED",
      counts: { FAILED: 1 },
      builds: [{ state: "FAILED", name: "sample-app » b #6", url: "https://ci/job/6", key: "k", at: 1 }, { state: "SUCCESSFUL", name: 7, url: null }],
    },
  };
  const ws = plan.buildWorkspace({ issueKey: "PROJ-7", bitbucketBaseUrl: BB, repos: [liveRepo({ prs: [pr] })], history: null });
  assert.equal(ws.prs.length, 1);
  assert.deepEqual(ws.prs[0].build, {
    state: "FAILED",
    counts: { FAILED: 1 },
    builds: [{ state: "FAILED", name: "sample-app » b #6", url: "https://ci/job/6" }, { state: "SUCCESSFUL", name: null, url: null }],
  });
  assert.equal(ws.suggestions.createPr, null);
  assert.match(plan.workspaceSummary(ws), /1 PR \(1 open\)/);
});

test("nothing found offers Start fix; a PR on another host is dropped; a vanished history worktree is marked", () => {
  const empty = plan.buildWorkspace({ issueKey: "PROJ-7", bitbucketBaseUrl: BB, repos: [], history: null });
  assert.equal(empty.suggestions.startFix, true);
  assert.equal(empty.suggestions.createPr, null);
  assert.deepEqual(empty.resume, { session: null, worktreeDir: null });

  const evil = plan.buildWorkspace({
    issueKey: "PROJ-7",
    bitbucketBaseUrl: BB,
    repos: [liveRepo({ worktrees: [], branches: [], prs: [{ id: 1, state: "OPEN", url: "https://evil.example/projects/A/repos/b/pull-requests/1" }] })],
    history: null,
  });
  assert.equal(evil.prs.length, 0);

  const gone = plan.buildWorkspace({
    issueKey: "PROJ-7",
    bitbucketBaseUrl: BB,
    repos: [],
    history: plan.historyFacts(ticketDetail, related, allow),
    exists: () => false,
    cwdAllowed: () => true,
  });
  assert.equal(gone.worktrees[0].exists, false);
  assert.equal(gone.suggestions.createPr, null);
  assert.equal(gone.suggestions.startFix, true);
});

test("a Bitbucket branch name that isn't safe degrades to unknown instead of failing the scan", () => {
  const pr = { id: 3, title: "t", state: "OPEN", url: `${BB}/projects/ACME/repos/sample-app/pull-requests/3`, fromBranch: "feat/a#b", toBranch: "master" };
  const ws = plan.buildWorkspace({ issueKey: "PROJ-7", bitbucketBaseUrl: BB, repos: [liveRepo({ prs: [pr] })], history: null });
  assert.equal(ws.prs.length, 1);
  assert.equal(ws.prs[0].fromBranch, null);
  assert.equal(ws.prs[0].toBranch, "master");
});

test("historyFacts drops a tampered history row: a worktree or session folder the policy refuses, with a note; no policy allows nothing", () => {
  const evil = {
    ...related,
    [`worktree:${WT}`]: { item: { kind: "worktree", data: { dir: "/tmp/evil", repoKey: "ACME/sample-app", branch: "b" } }, edges: [{ kind: "session", key: `session:${S1}`, rel: "links", direction: "out" }] },
    [`session:${S1}`]: { item: { kind: "session", updatedAt: 20, data: { cwd: "/tmp/evil", permissionMode: "plan" } }, edges: [] },
  };
  const policy = { worktreeOk: (w) => w.dir === WT, sessionCwdOk: (cwd) => cwd !== "/tmp/evil" };
  const f = plan.historyFacts(ticketDetail, evil, policy);
  assert.deepEqual(f.worktrees, []);
  assert.deepEqual(f.sessions.map((s) => s.id), [S2]);
  assert.equal(f.notes.length, 2);
  const none = plan.historyFacts(ticketDetail, related);
  assert.deepEqual([none.worktrees.length, none.sessions.length], [0, 0]);
});

test("the resume folder fallback needs cwdAllowed; a refused folder is skipped for the next allowed one", () => {
  const two = liveRepo({
    worktrees: [
      { dir: "/somewhere/else", branch: "x", exists: true, isTicketWorktree: true },
      { dir: WT, branch: "y", exists: true },
    ],
  });
  const base = { issueKey: "PROJ-7", bitbucketBaseUrl: BB, repos: [two], history: null };
  assert.equal(plan.buildWorkspace(base).resume.worktreeDir, null);
  assert.equal(plan.buildWorkspace({ ...base, cwdAllowed: (d) => d === WT }).resume.worktreeDir, WT);
});
