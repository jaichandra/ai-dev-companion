// The watchers from the build (npm run build first), with every Bitbucket,
// Jira, git, LLM and job call injected — no network, no Claude, HOME in a
// temp dir.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "watcher-runner-home-"));
const { createWatcherRunner, defaultWatcherIo } = require("../dist/core/watcher-runner.js");
const { AuthSetupError, HttpStatusError } = require("../dist/core/atlassian.js");

const PR = {
  id: 12,
  title: "PROJ-7: fix login",
  state: "OPEN",
  project: "CI",
  repo: "sample-app",
  fromBranch: "bugfix/PROJ-7",
  fromSha: "aaa111",
  toBranch: "master",
  toSha: "bbb222",
  url: "https://bb.example/projects/CI/repos/sample-app/pull-requests/12",
  approvals: 0,
  reviewers: [{ name: "me", status: "UNAPPROVED" }],
};
const KEY = "bitbucket:CI/sample-app#12";
const JOB_ID = "3f2b8c1e-1111-4222-8333-444455556666";
const USAGE = { day: "2026-09-29", used: 1, limit: 5, remaining: 4 };

function harness({ io = {}, llm = {}, grants = [], enabled = ["resolve-conflict", "analyze-issue"], seen = {}, startJob } = {}) {
  const calls = [];
  const inbox = [];
  const seenStore = { conflicts: {}, assignedBugs: {}, reviewRequests: {}, ...seen };
  const deps = {
    config: {
      repos: { "CI/sample-app": "/clones/sample-app" },
      analyzeIssue: { projects: ["PROJ"], issueTypes: ["Bug"] },
      bitbucket: { baseUrl: "https://bb.example" },
      jira: { baseUrl: "https://jira.example" },
    },
    io: {
      listDashboardPullRequests: async (role) => {
        calls.push(["dashboard", role]);
        return [PR];
      },
      getMergeStatus: async (p, r, id) => {
        calls.push(["merge", p, r, id]);
        return { conflicted: true };
      },
      searchIssues: async (jql) => {
        calls.push(["search", jql]);
        return [{ key: "PROJ-7", summary: "Login broken", status: "Open", priority: "High", url: "https://jira.example/browse/PROJ-7" }];
      },
      whoAmI: async () => "me",
      hasCachedAnalysis: () => false,
      repoPath: (p, r) => (`${p}/${r}` === "CI/sample-app" ? "/clones/sample-app" : null),
      gitFetch: async (dir, branch) => void calls.push(["fetch", dir, branch]),
      ...io,
    },
    llm: {
      available: async () => false,
      chat: async () => {
        throw new Error("no chat expected");
      },
      ...llm,
    },
    notifications: { add: (item) => inbox.push(item) },
    enabledFeatureIds: () => enabled,
    activeJobFor: () => false,
    startJob:
      startJob ||
      (async (featureId, payload, watcher) => {
        calls.push(["start", featureId, payload, watcher]);
        return { id: JOB_ID };
      }),
    log: () => {},
  };
  const tracked = [];
  const ctx = {
    now: () => 0,
    quiet: false,
    requestClaudeRun: () => {
      calls.push(["grant"]);
      return grants.length ? grants.shift() : { granted: true, usage: USAGE };
    },
    refundClaudeRun: () => calls.push(["refund"]),
    trackBackgroundJob: (id) => tracked.push(id),
    seen: (w) => ({ ...seenStore[w] }),
    setSeen: (w, s) => (seenStore[w] = s),
  };
  return { runner: createWatcherRunner(deps), ctx, calls, inbox, tracked, seenStore };
}

test("conflicts: a conflicted PR with a clone starts Resolve Conflict (rules only), tracked and in the inbox", async () => {
  const h = harness();
  const report = await h.runner.conflicts(h.ctx);
  assert.deepEqual(h.calls.filter((c) => c[0] === "start"), [
    ["start", "resolve-conflict", { project: "CI", repo: "sample-app", prId: 12, sourceBranch: "bugfix/PROJ-7", destBranch: "master" }, "conflicts"],
  ]);
  assert.deepEqual(h.tracked, [JOB_ID]);
  assert.equal(h.inbox.length, 1);
  assert.equal(h.inbox[0].jobId, JOB_ID);
  assert.equal(h.inbox[0].kind, "conflict");
  assert.deepEqual(h.seenStore.conflicts, { [KEY]: "aaa111:bbb222" });
  assert.deepEqual(report, {
    found: 1,
    events: 1,
    started: 1,
    inboxed: 1,
    deferred: 0,
    mergeErrors: 0,
    triage: { onprem: 0, rules: 1, failed: 0 },
    tier: "rules",
  });
  const again = await h.runner.conflicts(h.ctx);
  assert.equal(again.events, 0, "seen at these commits");
});

test("conflicts: the on-prem triage can veto a run, and sees the PR only as fenced data", async () => {
  const prompts = [];
  const h = harness({
    llm: {
      available: async () => true,
      chat: async (opts) => {
        prompts.push(opts);
        return { text: "", value: { worth: false, reason: "only a version bump" } };
      },
    },
  });
  const report = await h.runner.conflicts(h.ctx);
  assert.equal(h.calls.some((c) => c[0] === "grant" || c[0] === "start"), false);
  assert.match(h.inbox[0].body, /Not pre-warmed: only a version bump/);
  assert.equal(prompts[0].responseSchema.required.join(), "worth,reason");
  assert.match(prompts[0].messages[0].content, /^<untrusted-event>/);
  assert.equal(report.tier, "onprem");
  assert.deepEqual(report.triage, { onprem: 1, rules: 0, failed: 0 });
});

test("conflicts: a broken triage reply falls back to the rules", async () => {
  const h = harness({ llm: { available: async () => true, chat: async () => ({ text: "?", value: { nope: 1 } }) } });
  const report = await h.runner.conflicts(h.ctx);
  assert.deepEqual(report.triage, { onprem: 0, rules: 0, failed: 1 });
  assert.equal(report.started, 1);
});

test("conflicts: no budget files an inbox item; busy leaves the event for the next tick", async () => {
  const spent = harness({ grants: [{ granted: false, reason: "budget", usage: USAGE }] });
  await spent.runner.conflicts(spent.ctx);
  assert.equal(spent.calls.some((c) => c[0] === "start"), false);
  assert.match(spent.inbox[0].body, /budget is used up/);
  assert.deepEqual(spent.seenStore.conflicts, { [KEY]: "aaa111:bbb222" });

  const busy = harness({ grants: [{ granted: false, reason: "busy", usage: USAGE }] });
  const report = await busy.runner.conflicts(busy.ctx);
  assert.equal(report.deferred, 1);
  assert.deepEqual(busy.inbox, []);
  assert.deepEqual(busy.seenStore.conflicts, {}, "not seen, so it's tried again");
});

test("conflicts: no clone or the feature off means an inbox item and no Claude run", async () => {
  const noClone = harness({ io: { repoPath: () => null } });
  await noClone.runner.conflicts(noClone.ctx);
  assert.equal(noClone.calls.some((c) => c[0] === "grant"), false);
  assert.match(noClone.inbox[0].body, /no local clone of CI\/sample-app/);
  const off = harness({ enabled: ["analyze-issue"] });
  await off.runner.conflicts(off.ctx);
  assert.match(off.inbox[0].body, /turned off/);
});

test("no sign-in anywhere: the task fails with needsLogin", async () => {
  const noAuth = harness({ io: { listDashboardPullRequests: async () => { throw new AuthSetupError("Bitbucket: no session and no token."); } } });
  assert.deepEqual(await noAuth.runner.conflicts(noAuth.ctx), { outcome: "failed", needsLogin: true, error: "Bitbucket: no session and no token." });
  const denied = harness({ io: { searchIssues: async () => { throw new HttpStatusError("HTTP 401", 401); } } });
  assert.equal((await denied.runner.assignedBugs(denied.ctx)).needsLogin, true);
  const down = harness({ io: { listDashboardPullRequests: async () => { throw new HttpStatusError("HTTP 503", 503); } } });
  assert.equal((await down.runner.reviewRequests(down.ctx)).needsLogin, false);
});

test("assignedBugs: searches the configured projects, analyzes a new ticket once, skips an analyzed one", async () => {
  const h = harness();
  await h.runner.assignedBugs(h.ctx);
  const search = h.calls.find((c) => c[0] === "search");
  assert.match(search[1], /project in \("PROJ"\) AND issuetype in \("Bug"\)/);
  assert.deepEqual(h.calls.find((c) => c[0] === "start"), ["start", "analyze-issue", { issueKey: "PROJ-7" }, "assignedBugs"]);
  assert.equal(h.inbox[0].url, "https://jira.example/browse/PROJ-7");
  assert.deepEqual(h.seenStore.assignedBugs, { "PROJ-7": "analyzed" });

  const cached = harness({ io: { hasCachedAnalysis: () => true } });
  await cached.runner.assignedBugs(cached.ctx);
  assert.equal(cached.calls.some((c) => c[0] === "grant"), false);
  assert.match(cached.inbox[0].body, /already analyzed/);
});

test("reviewRequests: fetches the branch into the clone, never asks for a Claude run", async () => {
  const h = harness();
  const report = await h.runner.reviewRequests(h.ctx);
  assert.deepEqual(h.calls.filter((c) => c[0] === "fetch"), [["fetch", "/clones/sample-app", "bugfix/PROJ-7"]]);
  assert.equal(h.calls.some((c) => c[0] === "grant" || c[0] === "start"), false);
  assert.equal(h.inbox[0].kind, "review-request");
  assert.match(h.inbox[0].body, /branch is fetched/);
  assert.deepEqual(report, { found: 1, events: 1, fetched: 1 });

  const unsafe = harness({ io: { listDashboardPullRequests: async () => [{ ...PR, fromBranch: "-x" }] } });
  await unsafe.runner.reviewRequests(unsafe.ctx);
  assert.equal(unsafe.calls.some((c) => c[0] === "fetch"), false);
  assert.match(unsafe.inbox[0].body, /can't be fetched safely/);
});

test("conflicts: only the author dashboard (your own PRs) is read, never the reviewer one", async () => {
  const h = harness();
  await h.runner.conflicts(h.ctx);
  assert.deepEqual(h.calls.filter((c) => c[0] === "dashboard"), [["dashboard", "AUTHOR"]]);
  const r = harness();
  await r.runner.reviewRequests(r.ctx);
  assert.deepEqual(r.calls.filter((c) => c[0] === "dashboard"), [["dashboard", "REVIEWER"]]);
});

test("triage is never asked when the rules already say no, so it can only veto", async () => {
  let asked = 0;
  const h = harness({
    io: { repoPath: () => null },
    llm: { available: async () => true, chat: async () => { asked++; return { text: "", value: { worth: true, reason: "yes" } }; } },
  });
  const report = await h.runner.conflicts(h.ctx);
  assert.equal(asked, 0);
  assert.equal(report.started, 0);
  assert.equal(h.calls.some((c) => c[0] === "grant" || c[0] === "start"), false);
});

test("re-running the same commits yields the same inbox key, which carries the commit stamp", async () => {
  const h = harness();
  await h.runner.conflicts(h.ctx);
  h.ctx.setSeen("conflicts", {});
  await h.runner.conflicts({ ...h.ctx, seen: () => ({}) });
  assert.equal(new Set(h.inbox.map((i) => i.key)).size, 1, "same commits, same key");
});

test("the real git fetch refuses an unsafe branch name before spawning anything", async () => {
  const io = defaultWatcherIo({ repos: {} });
  await assert.rejects(io.gitFetch("/nonexistent", "-x"), /not a safe branch name/);
  await assert.rejects(io.gitFetch("/nonexistent", "a b"), /not a safe branch name/);
});

const prN = (n, over = {}) => ({ ...PR, id: n, url: `https://bb.example/projects/CI/repos/sample-app/pull-requests/${n}`, fromBranch: `b${n}`, ...over });
const keyN = (n) => `bitbucket:CI/sample-app#${n}`;

test("seen is saved after each settled event, so a crash mid-loop cannot start the same job twice", async () => {
  const h = harness({ io: { listDashboardPullRequests: async () => [prN(1), prN(2), prN(3)] } });
  const snapshots = [];
  h.ctx.setSeen = (w, s) => {
    h.seenStore[w] = s;
    snapshots.push(Object.keys(s).length);
  };
  await h.runner.conflicts(h.ctx);
  assert.deepEqual(snapshots.slice(0, 3), [1, 2, 3], "one save per settled event, before the final prune");
  const busy = harness({ io: { listDashboardPullRequests: async () => [prN(1)] }, grants: [{ granted: false, reason: "busy", usage: USAGE }] });
  const saves = [];
  busy.ctx.setSeen = (w, s) => saves.push(s);
  await busy.runner.conflicts(busy.ctx);
  assert.deepEqual(saves.filter((s) => Object.keys(s).length), [], "a deferred event is not marked seen");
});

test("inbox links must be https on the configured host; anything else is dropped", async () => {
  const h = harness({
    io: {
      listDashboardPullRequests: async () => [prN(1, { url: "http://bb.example/x" }), prN(2, { url: "https://evil.example/x" }), prN(3, { url: "javascript:alert(1)" }), prN(4)],
    },
  });
  await h.runner.conflicts(h.ctx);
  const byPr = Object.fromEntries(h.inbox.map((i) => [i.scopeKey, i.url]));
  assert.equal(byPr[keyN(1)], null);
  assert.equal(byPr[keyN(2)], null);
  assert.equal(byPr[keyN(3)], null);
  assert.equal(byPr[keyN(4)], "https://bb.example/projects/CI/repos/sample-app/pull-requests/4");
});

test("the reported tier is the one that actually decided", async () => {
  const failing = harness({ llm: { available: async () => true, chat: async () => { throw new Error("proxy down"); } } });
  assert.equal((await failing.runner.conflicts(failing.ctx)).tier, "rules");
  let asked = 0;
  const quiet = harness({ llm: { available: async () => { asked++; return true; } } });
  quiet.ctx.quiet = true;
  assert.equal((await quiet.runner.conflicts(quiet.ctx)).tier, "rules");
  assert.equal(asked, 0, "quiet hours never ask the model");
});

test("a PR from a fork is not started or fetched: this clone has no such branch", async () => {
  const fork = prN(1, { fromRepo: { projectKey: "~ME", slug: "sample-app" } });
  const h = harness({ io: { listDashboardPullRequests: async () => [fork] } });
  const report = await h.runner.conflicts(h.ctx);
  assert.equal(report.started, 0);
  assert.equal(h.calls.some((c) => c[0] === "grant" || c[0] === "start"), false);
  assert.match(h.inbox[0].body, /fork/);
  const r = harness({ io: { listDashboardPullRequests: async () => [fork] } });
  await r.runner.reviewRequests(r.ctx);
  assert.equal(r.calls.some((c) => c[0] === "fetch"), false);
  assert.match(r.inbox[0].body, /fork/);
  const same = prN(1, { fromRepo: { projectKey: "ci", slug: "Sample-App" } });
  const s = harness({ io: { listDashboardPullRequests: async () => [same] } });
  assert.equal((await s.runner.conflicts(s.ctx)).started, 1, "same repo, any case");
});

test("caps: at most 20 PRs looked at and 10 events handled; seen keeps every PR the dashboard returned", async () => {
  const many = Array.from({ length: 25 }, (_, i) => prN(i + 1));
  const h = harness({ io: { listDashboardPullRequests: async () => many }, seen: { conflicts: { [keyN(25)]: "old:old", [keyN(99)]: "gone:gone" } } });
  const report = await h.runner.conflicts(h.ctx);
  assert.equal(report.found, 20);
  assert.equal(report.events, 10);
  assert.ok(h.seenStore.conflicts[keyN(25)], "PR 25 is still open, just past the 20 looked at");
  assert.equal(h.seenStore.conflicts[keyN(99)], undefined, "a PR that is gone is forgotten");
});

test("merge status errors are counted and skipped; a failed start is filed and settled", async () => {
  const h = harness({
    io: {
      listDashboardPullRequests: async () => [prN(1), prN(2)],
      getMergeStatus: async (p, r, id) => {
        if (id === 1) throw new Error("HTTP 500");
        return { conflicted: true };
      },
    },
  });
  const report = await h.runner.conflicts(h.ctx);
  assert.equal(report.mergeErrors, 1);
  assert.equal(report.events, 1);

  const failing = harness({
    startJob: async () => {
      throw new Error("Resolve Conflict needs a Claude session");
    },
  });
  const r = await failing.runner.conflicts(failing.ctx);
  assert.equal(r.started, 0);
  assert.equal(r.inboxed, 1);
  assert.match(failing.inbox[0].body, /couldn't start \(Resolve Conflict needs a Claude session\)/);
  assert.ok(failing.seenStore.conflicts[KEY], "settled: not retried at these commits");
});

test("the real git fetch runs exactly: git -C <clone> fetch --no-tags origin +refs/heads/<b>:refs/remotes/origin/<b>, without prompting", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "fake-git-"));
  const log = path.join(dir, "argv.json");
  fs.writeFileSync(path.join(dir, "git"), `#!/bin/sh\nprintf '%s\\n' "$GIT_TERMINAL_PROMPT" "$@" > "${log}"\n`, { mode: 0o755 });
  const oldPath = process.env.PATH;
  process.env.PATH = `${dir}:${oldPath}`;
  try {
    await defaultWatcherIo({ repos: {} }).gitFetch("/clones/sample-app", "feature/a-b");
    assert.deepEqual(fs.readFileSync(log, "utf8").trim().split("\n"), [
      "0",
      "-C",
      "/clones/sample-app",
      "fetch",
      "--no-tags",
      "origin",
      "+refs/heads/feature/a-b:refs/remotes/origin/feature/a-b",
    ]);
  } finally {
    process.env.PATH = oldPath;
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a start that throws gives its run back to the budget; a start that works does not", async () => {
  const failing = harness({ startJob: async () => { throw new Error("no session"); } });
  await failing.runner.conflicts(failing.ctx);
  assert.deepEqual(failing.calls.filter((c) => c[0] === "grant" || c[0] === "refund").map((c) => c[0]), ["grant", "refund"]);
  const ok = harness();
  await ok.runner.conflicts(ok.ctx);
  assert.equal(ok.calls.some((c) => c[0] === "refund"), false);
});
