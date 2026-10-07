// How the background work is wired. The behaviour lives in
// core/background-wiring.ts (tested below with a fake clock, io and feature);
// server.ts can't be loaded in a test (it listens, reads real config), so the
// source-level checks that follow are tripwires on its text for the
// properties the phase's safety rules depend on.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "wiring-home-"));
const { createBackgroundWiring, featureContextFor, ACTIVE_JOB_STATUSES } = require("../dist/core/background-wiring.js");
const { createNotificationStore } = require("./notifications.js");

const read = (...p) => fs.readFileSync(path.join(__dirname, "..", ...p), "utf8");
const server = read("server.ts");
const between = (s, from, to) => {
  const a = s.indexOf(from);
  assert.ok(a >= 0, `missing ${from}`);
  const b = s.indexOf(to, a + from.length);
  assert.ok(b > a, `missing ${to}`);
  return s.slice(a, b);
};

test("ctx.background is set only by the watcher's startJob, never from a request (source tripwire)", () => {
  const wiring = read("core", "background-wiring.ts");
  assert.equal((wiring.match(/background: true/g) || []).length, 1);
  const startJob = between(wiring, "startJob: async (featureId, payload, watcher)", "\n    },\n");
  assert.match(startJob, /background: true[\s\S]*"watcher", watcher/);
  assert.equal((server.match(/background: true/g) || []).length, 0);
  // contextFor (built from the request) and the MCP / extension callers never mention it
  assert.doesNotMatch(between(server, "function contextFor(", "\n}\n"), /background/);
  assert.match(server, /startFeatureJob\(feature, req\.body, contextFor\(req\), "extension"\)/);
  assert.match(server, /contextFor\(req\), "mcp"\)/);
  // startFeatureJob itself drops the flag for any other caller, through the tested function
  assert.match(server, /feature\.start\(payload, featureContextFor\(via, ctx\)\)/);
  assert.match(wiring, /background: via === "watcher" && ctx\.background === true/);
});

test("nothing in the watcher path approves or pushes: only startJob is handed over", () => {
  const runner = read("core", "watcher-runner.ts");
  assert.doesNotMatch(runner, /approve|git\(\["push|"push"/i);
  const wiring = read("core", "background-wiring.ts");
  assert.doesNotMatch(wiring, /\.(approve|reject)\b|\b(approve|reject)\(|"\/(approve|reject)/);
  assert.doesNotMatch(between(server, "createBackgroundWiring({", "\n});"), /approve|reject/);
  // Resolve Conflict pushes in exactly one place: approve()
  const rc = read("features", "resolve-conflict", "index.ts");
  const pushes = [...rc.matchAll(/git\(\["push"/g)];
  assert.equal(pushes.length, 1);
  const approveStart = rc.indexOf("async approve(job: Job)");
  assert.ok(approveStart > 0 && pushes[0].index > approveStart);
  assert.ok(pushes[0].index < rc.indexOf("async reject(", approveStart));
  // start() ends at awaiting-approval and does not consult ctx.background
  assert.doesNotMatch(rc, /ctx\.background|background/);
});

test("the scheduler and the LLM proxy get the one live config object that Settings saves mutate", () => {
  assert.match(server, /const llm = llmProxyFor\(config\);/);
  assert.match(between(server, "createBackgroundWiring({", "features:"), /^\s*config,$/m);
  const wiring = read("core", "background-wiring.ts");
  assert.match(between(wiring, "new Scheduler({", "tasks:"), /config: deps\.config,/);
  assert.match(between(wiring, "createWatcherRunner({", "io:"), /config: deps\.config,/);
  const apply = between(server, "function applySavedConfig(", "\n}\n");
  assert.match(apply, /const running = config as unknown/);
  assert.match(apply, /running\[key\] = value/);
  assert.doesNotMatch(apply, /config = /);
  // config is assigned once, at startup, and never rebound afterwards
  assert.equal((server.match(/(^|[^.\w])config = /gm) || []).length, 1);
});

test("maintenance runs from the scheduler only: the old timers are gone, start and stop are wired once", () => {
  assert.doesNotMatch(server, /setInterval\(\(\) => void dailyMaintenance/);
  assert.doesNotMatch(server, /setTimeout\(\(\) => void dailyMaintenance/);
  assert.doesNotMatch(server, /MAINTENANCE_INTERVAL_MS/);
  assert.equal((server.match(/[^n] dailyMaintenance\(\)/g) || []).length, 1, "only the scheduler task calls it");
  assert.equal((server.match(/scheduler\.start\(\)/g) || []).length, 1);
  assert.equal((server.match(/scheduler\.stop\(\)/g) || []).length, 1);
});

test("the /notifications routes are registered after the Host guard and the shared-secret middleware", () => {
  const host = server.indexOf("app.use(hostGuard.createHostGuard(");
  const secret = server.indexOf('req.header("x-companion-secret") !== config.sharedSecret');
  assert.ok(host > 0 && secret > host);
  for (const route of ['app.get("/notifications"', 'app.post("/notifications/seen"', 'app.post("/notifications/announce"', 'app.post("/notifications/:id/open"']) {
    assert.equal(server.split(route).length, 2, route);
    assert.ok(server.indexOf(route) > secret, `${route} must come after the secret check`);
  }
});

// ---- behaviour, through core/background-wiring.ts ----

const MIN = 60 * 1000;
const HOUR = 60 * MIN;
const T0 = new Date(2026, 8, 29, 8, 0).getTime();
const JOB_ID = "3f2b8c1e-1111-4222-8333-444455556666";
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
  reviewers: [],
};

function fakeClock() {
  let now = T0;
  const timers = [];
  return {
    clock: {
      now: () => now,
      setTimer: (fn, ms) => {
        const t = { fn, at: now + ms, cleared: false };
        timers.push(t);
        return t;
      },
      clearTimer: (t) => {
        t.cleared = true;
      },
    },
    advance: (ms) => (now += ms),
    timers,
  };
}

function setupWiring({ shuttingDown = false } = {}) {
  const c = fakeClock();
  const jobs = new Map();
  const seenCtx = [];
  const calls = { approve: 0, reject: 0, maintenance: 0, startFeatureJob: [] };
  const resolveConflict = {
    id: "resolve-conflict",
    label: "Resolve Conflict",
    start: async (payload, ctx) => {
      seenCtx.push(ctx);
      const job = { id: JOB_ID, featureId: "resolve-conflict", status: "awaiting-approval", createdAt: c.clock.now(), updatedAt: c.clock.now() };
      jobs.set(job.id, job);
      return job;
    },
    approve: async () => void calls.approve++,
    reject: async () => void calls.reject++,
  };
  const jobStore = { list: () => [...jobs.values()], get: (id) => jobs.get(id) };
  // What server.ts's startFeatureJob does with the context and the tagging.
  const startFeatureJob = async (feature, payload, ctx, via, watcher) => {
    calls.startFeatureJob.push([feature.id, via, watcher, ctx.background]);
    const job = await feature.start(payload, featureContextFor(via, ctx));
    Object.assign(job, { scopeKey: "bitbucket:CI/sample-app#12", startedVia: via, ...(watcher ? { watcher } : {}) });
    return job;
  };
  const inbox = createNotificationStore({ now: c.clock.now });
  const wiring = createBackgroundWiring({
    config: { repos: { "CI/sample-app": "/clones/sample-app" }, watchers: { conflicts: { enabled: true, intervalMinutes: 15 } } },
    features: [resolveConflict],
    jobStore,
    startFeatureJob,
    inbox,
    llm: { available: async () => false, chat: async () => ({ value: null }) },
    io: {
      listDashboardPullRequests: async () => [PR],
      getMergeStatus: async () => ({ conflicted: true }),
      searchIssues: async () => [],
      whoAmI: async () => "me",
      hasCachedAnalysis: () => false,
      repoPath: () => "/clones/sample-app",
      gitFetch: async () => {},
    },
    clock: c.clock,
    statePath: null,
    maintenance: async () => {
      calls.maintenance++;
      return {};
    },
    isShuttingDown: () => shuttingDown,
    log: () => {},
  });
  return { ...c, wiring, jobs, jobStore, seenCtx, calls, inbox };
}

test("featureContextFor: only the watcher's own call keeps ctx.background", () => {
  const ctx = { auth: {}, body: { a: 1 } };
  assert.equal(featureContextFor("extension", { ...ctx, background: true }).background, false);
  assert.equal(featureContextFor("mcp", { ...ctx, background: true }).background, false);
  assert.equal(featureContextFor("watcher", { ...ctx, background: true }).background, true);
  assert.equal(featureContextFor("watcher", ctx).background, false, "no flag, no background");
  assert.equal(featureContextFor("watcher", { ...ctx, background: "yes" }).background, false, "only a real true counts");
  assert.deepEqual(featureContextFor("extension", ctx).body, { a: 1 });
});

test("a watcher-started Resolve Conflict is background, tagged, and stops at awaiting-approval", async () => {
  const t = setupWiring();
  assert.deepEqual(await t.wiring.scheduler.tick(), ["watcher.conflicts"]);
  assert.deepEqual(t.calls.startFeatureJob, [["resolve-conflict", "watcher", "conflicts", true]]);
  assert.equal(t.seenCtx.length, 1);
  assert.equal(t.seenCtx[0].background, true);
  const job = t.jobs.get(JOB_ID);
  assert.equal(job.status, "awaiting-approval");
  assert.equal(job.startedVia, "watcher");
  assert.equal(job.watcher, "conflicts");
  assert.equal(t.calls.approve + t.calls.reject, 0, "nothing approved or rejected on the user's behalf");
  const items = t.inbox.list({});
  assert.equal(items.length, 1);
  assert.equal(items[0].jobId, JOB_ID);
});

test("activeJobFor treats awaiting-approval and pending-start as open", () => {
  const t = setupWiring();
  const key = "bitbucket:CI/sample-app#12";
  assert.deepEqual(ACTIVE_JOB_STATUSES, ["running", "awaiting-approval", "approving", "rejecting", "pending-start"]);
  for (const status of ACTIVE_JOB_STATUSES) {
    t.jobs.set("j", { id: "j", featureId: "resolve-conflict", status, scopeKey: key });
    assert.equal(t.wiring.activeJobFor(key, "resolve-conflict"), true, status);
  }
  for (const status of ["approved", "failed", "rejected", "expired"]) {
    t.jobs.set("j", { id: "j", featureId: "resolve-conflict", status, scopeKey: key });
    assert.equal(t.wiring.activeJobFor(key, "resolve-conflict"), false, status);
  }
  t.jobs.set("j", { id: "j", featureId: "analyze-issue", status: "running", scopeKey: key });
  assert.equal(t.wiring.activeJobFor(key, "resolve-conflict"), false, "another feature");
});

test("the scheduler runs maintenance once at +1 minute, then daily; stop() ends the loop", async () => {
  const t = setupWiring();
  await t.wiring.scheduler.tick();
  assert.equal(t.calls.maintenance, 0);
  t.advance(MIN);
  await t.wiring.scheduler.tick();
  assert.equal(t.calls.maintenance, 1);
  t.advance(23 * HOUR);
  await t.wiring.scheduler.tick();
  assert.equal(t.calls.maintenance, 1, "not before a day has passed");
  t.advance(HOUR);
  await t.wiring.scheduler.tick();
  assert.equal(t.calls.maintenance, 2);

  t.wiring.scheduler.start();
  const armed = t.timers.filter((x) => !x.cleared);
  assert.equal(armed.length, 1);
  t.wiring.scheduler.stop();
  assert.equal(armed[0].cleared, true);
});

test("after the service starts shutting down, a watcher starts nothing and says so in the inbox", async () => {
  const t = setupWiring({ shuttingDown: true });
  await t.wiring.scheduler.tick();
  assert.deepEqual(t.calls.startFeatureJob, []);
  assert.equal(t.jobs.size, 0);
  assert.match(t.inbox.list({})[0].body, /couldn't start \(The service is shutting down/);
});

test("the routes are the four inbox handlers", () => {
  const t = setupWiring();
  assert.deepEqual(Object.keys(t.wiring.routes).sort(), ["announce", "list", "open", "seen"]);
});

test("GET /settings never waits on the proxy's full detection timeout (source tripwire)", () => {
  const view = between(server, "async function settingsView(", "\n}\n");
  assert.doesNotMatch(view, /await llm\.detect\(/);
  assert.match(view, /await cappedLlmDetect\(\)/);
  assert.match(between(server, "async function cappedLlmDetect(", "\n}\n"), /Promise\.race/);
});

test("the watcher's startJob adapter refuses new jobs once the service is shutting down (source tripwire)", () => {
  assert.match(server, /isShuttingDown: \(\) => shuttingDown/);
  assert.match(read("core", "background-wiring.ts"), /if \(deps\.isShuttingDown\?\.\(\)\) throw new Error/);
});

// ---- Phase 8: history.embed and the similar-item search ----

function embedWiring({ history }) {
  const c = fakeClock();
  const wiring = createBackgroundWiring({
    config: {},
    features: [],
    jobStore: { list: () => [], get: () => undefined },
    startFeatureJob: async () => {
      throw new Error("no jobs here");
    },
    inbox: createNotificationStore({ now: c.clock.now }),
    history,
    llm: {
      available: async () => false,
      chat: async () => ({ value: null }),
      detect: async () => ({ state: "no-key", label: "no key", at: 0 }),
      embed: async () => {
        throw new Error("must not be called without a key");
      },
      settings: () => ({ embeddingModel: "Qwen3-Embedding-8B" }),
    },
    io: {},
    clock: c.clock,
    statePath: null,
    maintenance: async () => ({}),
    log: () => {},
  });
  return { ...c, wiring };
}

test("history.embed runs two minutes after start with a history, and records 'skipped: no proxy' without a key", async () => {
  const events = [];
  const t = embedWiring({ history: { recordEvent: (e) => events.push(e) } });
  t.advance(2 * MIN);
  assert.deepEqual(await t.wiring.scheduler.tick(), ["maintenance", "history.embed"]);
  const row = events.find((e) => e.featureId === "scheduler:history.embed");
  assert.ok(row, "an events row for the embed run");
  assert.equal(row.outcome, "completed");
  assert.deepEqual(row.metrics, { task: "history.embed", skipped: "no proxy", proxy: "no-key", embedded: 0 });
});

test("server.ts builds the similar search only with a history and hands it to the features (source tripwire)", () => {
  assert.ok(server.indexOf("const llm = llmProxyFor(config);") < server.indexOf("const FEATURES: Feature[]"), "the proxy client exists before the features");
  assert.match(server, /const similarSearch = history \? similarSearchModule\.createSimilarSearch\(\{ history, llm, getConfig: \(\) => config \}\) : undefined;/);
  assert.match(server, /buildFeatures\(enabledIds, FEATURE_FACTORIES, config, \{ providers, history, findSimilar: similarSearch\?\.find \}\)/);
  const wiring = read("core", "background-wiring.ts");
  assert.match(wiring, /\.\.\.\(deps\.history \? \{ "history\.embed": embedTask\.createEmbedTask\(/);
});

test("buildFeatures hands every enabled factory the same deps, so analyze-issue receives findSimilar", () => {
  const { buildFeatures } = require("../dist/core/background-wiring.js");
  const seen = {};
  const factories = {
    "analyze-issue": (_c, deps) => ((seen["analyze-issue"] = deps), { id: "analyze-issue" }),
    other: (_c, deps) => ((seen.other = deps), { id: "other" }),
    off: () => assert.fail("a factory that isn't enabled must not run"),
  };
  const findSimilar = async () => ({ enabled: true, mode: "text", items: [] });
  const features = buildFeatures(["analyze-issue", "other", "unknown"], factories, {}, { history: undefined, findSimilar });
  assert.deepEqual(features.map((f) => f.id), ["analyze-issue", "other"]);
  assert.equal(seen["analyze-issue"].findSimilar, findSimilar);
  assert.equal(seen.other.findSimilar, findSimilar);
  const bare = buildFeatures(["analyze-issue"], factories, {}, { history: undefined, findSimilar: undefined });
  assert.equal(bare.length, 1);
  assert.equal(seen["analyze-issue"].findSimilar, undefined, "no history: no search");
});

test("without a history the scheduler runs only maintenance: no embed task is registered", async () => {
  const t = embedWiring({ history: undefined });
  t.advance(2 * MIN);
  assert.deepEqual(await t.wiring.scheduler.tick(), ["maintenance"]);
  t.advance(60 * MIN);
  assert.ok(!(await t.wiring.scheduler.tick()).includes("history.embed"));
});

test("buildFeatures builds in the order of the enabled ids, skips ids without a factory and never runs a factory that isn't enabled", () => {
  const { buildFeatures } = require("../dist/core/background-wiring.js");
  const ran = [];
  const factories = {
    a: () => (ran.push("a"), { id: "a" }),
    b: () => (ran.push("b"), { id: "b" }),
    off: () => (ran.push("off"), { id: "off" }),
  };
  assert.deepEqual(buildFeatures(["b", "nope", "a"], factories, {}, {}).map((f) => f.id), ["b", "a"]);
  assert.deepEqual(ran, ["b", "a"]);
  assert.deepEqual(buildFeatures([], factories, {}, {}), []);
});

test("the real analyze-issue factory, built through buildFeatures with findSimilar, yields the analyze-issue feature", () => {
  const { buildFeatures } = require("../dist/core/background-wiring.js");
  const { createAnalyzeIssueFeature } = require("../dist/features/analyze-issue/index.js");
  const findSimilar = async () => ({ enabled: true, mode: "text", items: [] });
  const [feature, ...rest] = buildFeatures(["analyze-issue"], { "analyze-issue": createAnalyzeIssueFeature }, {}, { history: undefined, findSimilar });
  assert.equal(rest.length, 0);
  assert.equal(feature.id, "analyze-issue");
  assert.equal(typeof feature.start, "function");
  // The same feature comes out of the pack's own factory, which is what server.ts builds with.
  const fromPack = buildFeatures(["analyze-issue"], require("../dist/core/packs.js").featureFactories(), {}, { history: undefined, findSimilar });
  assert.equal(fromPack.length, 1);
  assert.equal(fromPack[0].id, "analyze-issue");
  assert.match(server, /packs\.featureFactories\(\)/);
});
