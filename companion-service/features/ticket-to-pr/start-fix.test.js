// Start fix from the build (npm run build first) against a real throwaway
// clone, with Jira, Bitbucket and the terminal injected — no network, no
// credentials, no window opened, and HOME pointed at a temp dir first.
const test = require("node:test");
// This file is about a Bitbucket profile; the placeholder profile's git host is GitHub.
require("../../core/with-git-provider.js").useBitbucketProfile();
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "ticket-to-pr-home-"));
const { createTicketToPrFeature } = require("../../dist/features/ticket-to-pr/index.js");
const { jobStore } = require("../../dist/core/jobs.js");

const { repoWorktreesRoot } = require("../../core/paths.js");
const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8" }).trim();

function makeClone() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "ticket-to-pr-")));
  // Worktrees live under $HOME — a fresh one per clone so tests never share <repo>.worktrees/<KEY>.
  process.env.HOME = path.join(root, "home");
  const seed = path.join(root, "seed");
  fs.mkdirSync(seed);
  git(seed, "init", "-q", "-b", "master");
  git(seed, "config", "user.email", "t@example.com");
  git(seed, "config", "user.name", "T");
  fs.writeFileSync(path.join(seed, "README.md"), "hi\n");
  git(seed, "add", "README.md");
  git(seed, "commit", "-q", "-m", "init");
  execFileSync("git", ["clone", "-q", "--bare", seed, path.join(root, "origin.git")]);
  const clone = path.join(root, "sample-app");
  execFileSync("git", ["clone", "-q", path.join(root, "origin.git"), clone]);
  return { root, clone };
}

async function settle(id) {
  for (let i = 0; i < 200; i++) {
    const job = jobStore.get(id);
    if (job.status !== "running") return job;
    await new Promise((r) => setTimeout(r, 25));
  }
  throw new Error("job never settled");
}

function fakeIo(over = {}) {
  const terminals = [];
  return {
    terminals,
    io: {
      readAnalysis: () => ({ summary: "Login fails", repoKey: "ACME/sample-app", repoMatch: "Bitbucket link", analysis: { tldr: "Missing null check" } }),
      issueBasics: async () => ({ key: "PROJ-7", summary: "Login fails on Safari", issueType: "Bug", status: "Open" }),
      issueForRepoGuess: async () => ({ fields: { components: [], labels: [] } }),
      defaultBranch: async () => "master",
      openTerminal: async (cwd, args, header, options) => void terminals.push({ cwd, args, options }),
      confirmWorktree: async () => true,
      resumeSession: async () => ({ ok: false }),
      ...over,
    },
  };
}

test("Start fix makes <repo>.worktrees/<KEY> on a branch named from the ticket and opens Claude (auto mode) with the analysis", async () => {
  const { root, clone } = makeClone();
  try {
    const f = fakeIo();
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, f.io);
    const started = await feature.start({ issueKey: "proj-7" }, { auth: {} });
    const job = await settle(started.id);
    assert.equal(job.status, "awaiting-approval", job.error);
    const dir = path.join(repoWorktreesRoot(clone), "PROJ-7");
    assert.equal(job.data.ticketWorktree.dir, dir);
    assert.equal(job.data.branch, "proj-7-login-fails-on-safari");
    assert.equal(job.data.base, "master");
    assert.equal(job.data.worktree, undefined); // never the deletable per-job worktree field
    assert.equal(git(dir, "rev-parse", "--abbrev-ref", "HEAD"), "proj-7-login-fails-on-safari");
    assert.equal(f.terminals.length, 1);
    const [flag, id, modeFlag, mode, prompt, ...rest] = f.terminals[0].args;
    assert.deepEqual([flag, modeFlag, mode, rest.length], ["--session-id", "--permission-mode", "auto", 0]);
    assert.equal(id, job.data.claudeSession.id);
    assert.equal(job.data.claudeSession.cwd, dir);
    assert.match(prompt, /Missing null check/);
    assert.equal(f.terminals[0].cwd, dir);
    assert.ok(Number.isFinite(job.data.fixStartedAt));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("adopt reuses the worktree without opening Claude; no repository means a clear failure; Bitbucket down falls back to git", async () => {
  const { root, clone } = makeClone();
  try {
    const first = fakeIo({ defaultBranch: async () => { throw new Error("Bitbucket is down"); } });
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, first.io);
    const a = await settle((await feature.start({ issueKey: "PROJ-7" }, { auth: {} })).id);
    assert.equal(a.status, "awaiting-approval", a.error);
    assert.equal(a.data.base, "master"); // from origin/HEAD

    const again = fakeIo();
    const adopting = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, again.io);
    const b = await settle((await adopting.start({ issueKey: "PROJ-7", adopt: true, repoKey: "ACME/sample-app" }, { auth: {} })).id);
    assert.equal(b.status, "awaiting-approval", b.error);
    assert.equal(b.data.ticketWorktree.dir, a.data.ticketWorktree.dir);
    assert.equal(again.terminals.length, 0);
    assert.equal(b.data.claudeSession, undefined);

    const none = fakeIo({ readAnalysis: () => null });
    const noRepo = createTicketToPrFeature({ repos: {}, claudeModel: "m" }, {}, none.io);
    const c = await settle((await noRepo.start({ issueKey: "PROJ-8" }, { auth: {} })).id);
    assert.equal(c.status, "failed");
    assert.match(c.error, /No repositories are set up/);

    await assert.rejects(noRepo.start({ issueKey: "PROJ-8", repoKey: "ACME/other" }, { auth: {} }), /repositories/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Stop tracking leaves the worktree alone", async () => {
  const { root, clone } = makeClone();
  try {
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, fakeIo().io);
    const job = await settle((await feature.start({ issueKey: "PROJ-9" }, { auth: {} })).id);
    await feature.reject(job, { auth: {} });
    assert.equal(jobStore.get(job.id).status, "rejected");
    assert.ok(fs.existsSync(job.data.ticketWorktree.dir));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Create PR and the actions refuse a worktree that isn't where Start fix puts it, and a draft needs a commit", async () => {
  const { root, clone } = makeClone();
  try {
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, fakeIo().io);
    const job = await settle((await feature.start({ issueKey: "PROJ-10" }, { auth: {} })).id);
    assert.equal(job.status, "awaiting-approval", job.error);
    // No commits on the branch yet: refused before any Claude run.
    await assert.rejects(feature.actions["draft-pr"](job, { auth: {} }), /no commits ahead of origin\/master/);

    jobStore.patchData(job.id, { ticketWorktree: { dir: root } }); // as if a job file were edited
    const tampered = jobStore.get(job.id);
    await assert.rejects(feature.actions["open-terminal"](tampered, { auth: {} }), /isn't a checkout of this repository/);
    await assert.rejects(feature.approve(tampered, { auth: {}, body: { title: "t", description: "" } }), /isn't a checkout of this repository/);
    assert.equal(jobStore.get(job.id).status, "awaiting-approval");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Create PR and Draft with Claude fetch origin/<base> first: a change already landed there counts as no commits ahead", async () => {
  const { root, clone } = makeClone();
  try {
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, fakeIo().io);
    const job = await settle((await feature.start({ issueKey: "PROJ-11" }, { auth: {} })).id);
    assert.equal(job.status, "awaiting-approval", job.error);
    const dir = job.data.ticketWorktree.dir;
    git(dir, "config", "user.email", "t@example.com");
    git(dir, "config", "user.name", "T");
    fs.writeFileSync(path.join(dir, "fix.txt"), "fix\n");
    git(dir, "add", "fix.txt");
    git(dir, "commit", "-q", "-m", "PROJ-11: fix");
    // The same commit lands on origin's master by another route (by URL, so this
    // clone's origin/master is left stale). Without a fetch it would look 1 ahead
    // and the draft would start a Claude run / the push would go ahead.
    git(dir, "push", "-q", path.join(root, "origin.git"), "HEAD:master");
    assert.equal(git(dir, "rev-list", "--count", "refs/remotes/origin/master..HEAD"), "1");
    await assert.rejects(feature.actions["draft-pr"](job, { auth: {} }), /no commits ahead of origin\/master/);
    await assert.rejects(feature.approve(job, { auth: {}, body: { title: "t", description: "" } }), /no commits ahead of origin\/master/);
    assert.equal(jobStore.get(job.id).status, "awaiting-approval");
    // A malformed form is refused by the handler itself.
    await assert.rejects(feature.approve(job, { auth: {}, body: null }), /title/i);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("two concurrent Create PR clicks: exactly one proceeds, the other is refused; a failed check puts the job back", async () => {
  const { root, clone } = makeClone();
  try {
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, fakeIo().io);
    const job = await settle((await feature.start({ issueKey: "PROJ-12" }, { auth: {} })).id);
    const body = { title: "t", description: "" };
    const results = await Promise.allSettled([
      feature.approve(jobStore.get(job.id), { auth: {}, body }),
      feature.approve(jobStore.get(job.id), { auth: {}, body }),
    ]);
    assert.deepEqual(results.map((r) => r.status), ["rejected", "rejected"]);
    const messages = results.map((r) => r.reason.message);
    assert.equal(messages.filter((m) => /no commits ahead/.test(m)).length, 1, messages.join(" | "));
    assert.equal(messages.filter((m) => /"approving"/.test(m)).length, 1, messages.join(" | "));
    assert.equal(jobStore.get(job.id).status, "awaiting-approval");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a hostile default branch from Bitbucket falls back to git's; an existing worktree on an unsafe branch fails the job", async () => {
  const { root, clone } = makeClone();
  try {
    const hostile = fakeIo({ defaultBranch: async () => "--upload-pack=touch /tmp/x" });
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, hostile.io);
    const a = await settle((await feature.start({ issueKey: "PROJ-13" }, { auth: {} })).id);
    assert.equal(a.status, "awaiting-approval", a.error);
    assert.equal(a.data.base, "master");

    // Someone switches the reused worktree onto a branch with an unsafe name.
    git(a.data.ticketWorktree.dir, "checkout", "-q", "-b", "x;y");
    const again = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, fakeIo().io);
    const b = await settle((await again.start({ issueKey: "PROJ-13", adopt: true, repoKey: "ACME/sample-app" }, { auth: {} })).id);
    assert.equal(b.status, "failed");
    assert.match(b.error, /unsafe branch/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a worktree on a different branch than the job's, or a detached HEAD, is refused", async () => {
  const { root, clone } = makeClone();
  try {
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, fakeIo().io);
    const job = await settle((await feature.start({ issueKey: "PROJ-14" }, { auth: {} })).id);
    const dir = job.data.ticketWorktree.dir;
    git(dir, "checkout", "-q", "-b", "other-branch");
    await assert.rejects(feature.actions["draft-pr"](job, { auth: {} }), /is on "other-branch", not/);
    git(dir, "checkout", "-q", "--detach");
    await assert.rejects(feature.approve(job, { auth: {}, body: { title: "t", description: "" } }), /is on "HEAD", not/);
    assert.equal(jobStore.get(job.id).status, "awaiting-approval");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a pull request address that isn't a page on the configured Bitbucket is never stored or reused", async () => {
  const { root, clone } = makeClone();
  try {
    const cfg = { repos: { "ACME/sample-app": clone }, claudeModel: "m", bitbucket: { baseUrl: "https://bb.example.com" } };
    const feature = createTicketToPrFeature(cfg, {}, fakeIo().io);
    const good = "https://bb.example.com/projects/ACME/repos/sample-app/pull-requests/5";
    const mod = require("../../dist/features/ticket-to-pr/index.js");
    assert.equal(mod.safePrRef({ id: 5, url: good }, cfg).url, good);
    assert.equal(mod.safePrRef({ id: 5, url: "javascript:alert(1)" }, cfg).url, null);
    assert.equal(mod.safePrRef({ id: 5, url: "https://evil.example.org/projects/ACME/repos/sample-app/pull-requests/5" }, cfg).url, null);
    assert.equal(mod.safePrRef(null, cfg), null);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Create PR: anything that throws after the claim (a malformed Bitbucket setting) puts the job back to awaiting-approval with the error, not stuck in approving", async () => {
  const { root, clone } = makeClone();
  try {
    // baseUrl isn't a string, so bitbucketSite(config) throws once the job is claimed.
    const cfg = { repos: { "ACME/sample-app": clone }, claudeModel: "m", bitbucket: { baseUrl: 5 } };
    const feature = createTicketToPrFeature(cfg, {}, fakeIo().io);
    const job = await settle((await feature.start({ issueKey: "PROJ-15" }, { auth: {} })).id);
    assert.equal(job.status, "awaiting-approval", job.error);
    const dir = job.data.ticketWorktree.dir;
    git(dir, "config", "user.email", "t@example.com");
    git(dir, "config", "user.name", "T");
    fs.writeFileSync(path.join(dir, "fix.txt"), "fix\n");
    git(dir, "add", "fix.txt");
    git(dir, "commit", "-q", "-m", "PROJ-15: fix");
    await assert.rejects(feature.approve(jobStore.get(job.id), { auth: {}, body: { title: "t", description: "" } }), /trim|baseUrl/);
    const after = jobStore.get(job.id);
    assert.equal(after.status, "awaiting-approval");
    assert.equal(after.progress, undefined);
    assert.ok(after.error);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Draft with Claude is single flight per job; a served job's PR address is re-checked", async () => {
  const { root, clone } = makeClone();
  try {
    const cfg = { repos: { "ACME/sample-app": clone }, claudeModel: "m", bitbucket: { baseUrl: "https://bb.example.com" } };
    const feature = createTicketToPrFeature(cfg, {}, fakeIo().io);
    const job = await settle((await feature.start({ issueKey: "PROJ-16" }, { auth: {} })).id);
    // No commits: both calls fail the same way, but overlapping ones are refused earlier.
    const results = await Promise.allSettled([feature.actions["draft-pr"](job, { auth: {} }), feature.actions["draft-pr"](job, { auth: {} })]);
    const msgs = results.map((r) => r.reason.message);
    assert.equal(msgs.filter((m) => /already being written/.test(m)).length, 1, msgs.join(" | "));
    assert.equal(msgs.filter((m) => /no commits ahead/.test(m)).length, 1, msgs.join(" | "));
    // Sequential again once the first has finished.
    await assert.rejects(feature.actions["draft-pr"](job, { auth: {} }), /no commits ahead/);

    const tampered = { ...job, data: { ...job.data, pr: { id: 5, url: "javascript:alert(1)" } } };
    assert.equal(feature.present(tampered).data.pr.url, null);
    const good = "https://bb.example.com/projects/ACME/repos/sample-app/pull-requests/5";
    assert.equal(feature.present({ ...job, data: { ...job.data, pr: { id: 5, url: good } } }).data.pr.url, good);
    assert.equal(tampered.data.pr.url, "javascript:alert(1)"); // not mutated
    assert.equal(feature.present(job), job);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

async function pending(id) {
  for (let i = 0; i < 200; i++) {
    const job = jobStore.get(id);
    if (job.data?.pendingChoice || job.status !== "running") return job;
    await new Promise((r) => setTimeout(r, 25));
  }
  throw new Error("job never asked");
}

test("no analysis: Start fix asks which repository (the workspace's repos offered), then goes on with the pick", async () => {
  const { root, clone } = makeClone();
  try {
    const { io, terminals } = fakeIo({ readAnalysis: () => null });
    const config = { repos: { "ACME/other": path.join(root, "nope"), "ACME/sample-app": clone }, claudeModel: "m" };
    const feature = createTicketToPrFeature(config, {}, io);
    const started = await feature.start({ issueKey: "CIS-83623", hintRepoKeys: ["ACME/sample-app", "ACME/other", "ACME/unknown"] }, { auth: {} });
    const asking = await pending(started.id);
    assert.equal(asking.status, "running");
    assert.equal(asking.data.pendingChoice.suggested, null); // two related repos: no single favourite
    assert.deepEqual(asking.data.pendingChoice.options.map((o) => o.repoKey), ["ACME/sample-app", "ACME/other"]);
    assert.equal(asking.data.pendingChoice.noRepoLabel, "Cancel");

    await assert.rejects(feature.actions["confirm-repo"](asking, { body: { repoKey: "ACME/elsewhere" } }), /offered/);
    await feature.actions["confirm-repo"](asking, { body: { repoKey: "ACME/sample-app" } });
    const done = await settle(started.id);
    assert.equal(done.status, "awaiting-approval", done.error);
    assert.equal(done.data.repoKey, "ACME/sample-app");
    assert.equal(done.data.branch, "cis-83623-login-fails-on-safari");
    assert.equal(terminals.length, 1);
    assert.equal(done.data.pendingChoice ?? null, null);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("no analysis: a single repo from the workspace scan needs no question; choosing Cancel fails the job", async () => {
  const { root, clone } = makeClone();
  try {
    const config = { repos: { "ACME/sample-app": clone }, claudeModel: "m" };
    const sure = fakeIo({ readAnalysis: () => null });
    const a = await settle((await createTicketToPrFeature(config, {}, sure.io).start({ issueKey: "CIS-1", hintRepoKeys: ["ACME/sample-app"] }, { auth: {} })).id);
    assert.equal(a.status, "awaiting-approval", a.error);
    assert.equal(a.data.repoKey, "ACME/sample-app");

    const unsure = fakeIo({ readAnalysis: () => null });
    const feature = createTicketToPrFeature(config, {}, unsure.io);
    const started = await feature.start({ issueKey: "CIS-2" }, { auth: {} });
    const asking = await pending(started.id);
    assert.equal(asking.data.pendingChoice.suggested, null);
    await feature.actions["confirm-repo"](asking, { body: { repoKey: null } });
    const b = await settle(started.id);
    assert.equal(b.status, "failed");
    assert.match(b.error, /cancelled/);
    assert.equal(unsure.terminals.length, 0);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a checkout of the ticket's branch you already have is reused: nothing is created and nothing is asked", async () => {
  const { root, clone } = makeClone();
  try {
    git(clone, "config", "user.email", "t@example.com");
    git(clone, "config", "user.name", "T");
    const mine = path.join(root, "my-own-tree");
    git(clone, "worktree", "add", "-q", "-b", "feature/PROJ-20-login", mine, "origin/master");
    fs.writeFileSync(path.join(mine, "wip.txt"), "unfinished"); // dirty is fine for your own terminal
    const asked = [];
    const f = fakeIo({ confirmWorktree: async (id, q) => (asked.push(q), true) });
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, f.io);
    const job = await settle((await feature.start({ issueKey: "PROJ-20" }, { auth: {} })).id);
    assert.equal(job.status, "awaiting-approval", job.error);
    assert.equal(job.data.ticketWorktree.dir, mine);
    assert.equal(job.data.branch, "feature/PROJ-20-login");
    assert.deepEqual(asked, []);
    assert.equal(fs.existsSync(repoWorktreesRoot(clone)), false);
    assert.equal(f.terminals[0].cwd, mine);
    assert.match(job.result.summary, /Reused .* on feature\/PROJ-20-login/);
    assert.match(job.result.summary, /uncommitted change/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a ticket branch with no checkout asks first; declining creates nothing, accepting makes a worktree of that branch", async () => {
  const { root, clone } = makeClone();
  try {
    git(clone, "config", "user.email", "t@example.com");
    git(clone, "config", "user.name", "T");
    git(clone, "push", "-q", "origin", "HEAD:refs/heads/feature/PROJ-21-logout");
    git(clone, "fetch", "-q", "origin");
    const asked = [];
    const no = fakeIo({ confirmWorktree: async (id, q) => (asked.push(q), false) });
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, no.io);
    const declined = await settle((await feature.start({ issueKey: "PROJ-21" }, { auth: {} })).id);
    assert.equal(declined.status, "failed");
    assert.match(declined.error, /no worktree was created/);
    assert.match(asked[0].prompt, /feature\/PROJ-21-logout but no checkout/);
    assert.equal(fs.existsSync(repoWorktreesRoot(clone)), false);
    assert.equal(no.terminals.length, 0);

    const yes = fakeIo({ confirmWorktree: async () => true });
    const again = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, yes.io);
    const made = await settle((await again.start({ issueKey: "PROJ-21" }, { auth: {} })).id);
    assert.equal(made.status, "awaiting-approval", made.error);
    assert.equal(made.data.branch, "feature/PROJ-21-logout");
    assert.equal(made.data.ticketWorktree.dir, path.join(repoWorktreesRoot(clone), "feature-PROJ-21-logout"));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a ticket with no branch at all asks before making one, unless the setting says to just do it", async () => {
  const { root, clone } = makeClone();
  try {
    const asked = [];
    const no = fakeIo({ confirmWorktree: async (id, q) => (asked.push(q), false) });
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, no.io);
    const declined = await settle((await feature.start({ issueKey: "PROJ-22" }, { auth: {} })).id);
    assert.equal(declined.status, "failed");
    assert.match(asked[0].prompt, /no branch or checkout yet/);
    assert.equal(asked[0].detail, path.join(repoWorktreesRoot(clone), "PROJ-22"));
    assert.equal(fs.existsSync(repoWorktreesRoot(clone)), false);

    const never = fakeIo({ confirmWorktree: async () => assert.fail("must not ask") });
    const cfg = { repos: { "ACME/sample-app": clone }, claudeModel: "m", ticketToPr: { createWorktreeWithoutAsking: true } };
    const auto = createTicketToPrFeature(cfg, {}, never.io);
    const made = await settle((await auto.start({ issueKey: "PROJ-22" }, { auth: {} })).id);
    assert.equal(made.status, "awaiting-approval", made.error);
    assert.equal(made.data.ticketWorktree.dir, path.join(repoWorktreesRoot(clone), "PROJ-22"));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a second Continue reopens the recorded Claude session instead of starting another; if it can't, a new one starts", async () => {
  const { root, clone } = makeClone();
  try {
    const cfg = { repos: { "ACME/sample-app": clone }, claudeModel: "m", ticketToPr: { createWorktreeWithoutAsking: true } };
    const first = fakeIo();
    const job1 = await settle((await createTicketToPrFeature(cfg, {}, first.io).start({ issueKey: "PROJ-23" }, { auth: {} })).id);
    assert.equal(job1.status, "awaiting-approval", job1.error);
    assert.equal(first.terminals.length, 1);

    const resumes = [];
    const second = fakeIo({ resumeSession: async (c, session, header, dir) => (resumes.push({ session, dir }), { ok: true }) });
    const job2 = await settle((await createTicketToPrFeature(cfg, {}, second.io).start({ issueKey: "PROJ-23" }, { auth: {} })).id);
    assert.equal(job2.status, "awaiting-approval", job2.error);
    assert.equal(second.terminals.length, 0);
    assert.equal(resumes[0].session.id, job1.data.claudeSession.id);
    assert.equal(job2.data.claudeSession.id, job1.data.claudeSession.id);
    assert.match(job2.result.summary, /Reopened your Claude Code session/);

    const third = fakeIo({ resumeSession: async () => ({ ok: false }) });
    const job3 = await settle((await createTicketToPrFeature(cfg, {}, third.io).start({ issueKey: "PROJ-23" }, { auth: {} })).id);
    assert.equal(third.terminals.length, 1);
    assert.notEqual(job3.data.claudeSession.id, job1.data.claudeSession.id);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Track PR (adopt) with no worktree for the ticket fails instead of creating one", async () => {
  const { root, clone } = makeClone();
  try {
    const never = fakeIo({ confirmWorktree: async () => assert.fail("must not ask") });
    const feature = createTicketToPrFeature({ repos: { "ACME/sample-app": clone }, claudeModel: "m" }, {}, never.io);
    const job = await settle((await feature.start({ issueKey: "PROJ-24", adopt: true }, { auth: {} })).id);
    assert.equal(job.status, "failed");
    assert.match(job.error, /no worktree for PROJ-24 to track/);
    assert.equal(fs.existsSync(repoWorktreesRoot(clone)), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
