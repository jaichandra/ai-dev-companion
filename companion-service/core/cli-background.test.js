// `companion inbox | digest | precheck | hooks` with every side effect
// injected: the MCP call, git, stdin, the clock. No service, no real repo.
const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");
const { main } = require("./cli-run.js");
const { parseArgs } = require("./cli-args.js");
const fmt = require("./cli-format.js");

const SHA_A = "a".repeat(40);
const SHA_B = "b".repeat(40);

function harness({ tools = {}, git = {}, stdin = "", env = {}, config = {}, sleepForever = true } = {}) {
  const out = [];
  const err = [];
  const calls = [];
  const deps = {
    baseUrl: "http://127.0.0.1:8787",
    now: () => 10_000_000,
    home: "/home/me",
    out: (s) => out.push(s),
    err: (s) => err.push(s),
    exists: () => true,
    callTool: async (name, args) => {
      calls.push(["tool", name, args]);
      const v = tools[name];
      if (v === undefined) throw new Error(`unexpected tool ${name}`);
      if (v instanceof Error) throw v;
      return typeof v === "function" ? v(args) : v;
    },
    runClaude: async () => 0,
    runDoctor: async () => 0,
    env,
    cwd: "/work/sample-app/src",
    resolve: path.resolve,
    relative: path.relative,
    readConfig: () => config,
    readStdin: async () => stdin,
    git: async (args, cwd) => {
      calls.push(["git", args, cwd]);
      const key = args.join(" ");
      for (const [pattern, answer] of Object.entries(git)) if (key.includes(pattern)) return typeof answer === "function" ? answer(args) : answer;
      if (args[0] === "rev-parse") return { code: 0, stdout: "/work/sample-app\n", stderr: "" };
      return { code: 0, stdout: "", stderr: "" };
    },
    sleep: (ms) => (sleepForever ? new Promise(() => {}) : Promise.resolve(ms)),
    mkTemp: () => "/tmp/probe",
    rmTemp: (dir) => calls.push(["rm", dir]),
  };
  return { deps, out, err, calls };
}

const MEDIUM = {
  skipped: false,
  level: "medium",
  summary: "Medium risk: 1 of 2 changed files was in earlier regressions.",
  lines: [{ text: "src/login.ts was in 2 regressions: 2 builds 812, 815 (tests: Login with SSO)" }],
};

test("new commands and flags parse", () => {
  assert.equal(parseArgs(["inbox", "--all"]).flags.all, true);
  assert.equal(parseArgs(["precheck", "--stdin"]).flags.stdin, true);
  assert.deepEqual(parseArgs(["hooks", "install", "/r"]).positional, ["install", "/r"]);
  assert.equal(parseArgs(["digest", "--json"]).command, "digest");
});

test("inbox and digest print what the service returns", async () => {
  const h = harness({
    tools: {
      list_notifications: { unseen: 1, items: [{ title: "Conflict on CI/sample-app #12: fix", body: "Ready.", url: "https://bb/x", urgent: true, createdAt: 10_000_000 - 120_000 }] },
      get_digest: { headline: "Needs you: 1 conflict.", sections: [{ title: "Your pull requests", items: [{ text: "CI/sample-app #12 fix — conflicts", tone: "bad" }] }] },
    },
  });
  assert.equal(await main(["inbox"], h.deps), 0);
  assert.match(h.out.join(""), /1 unseen:\n! Conflict on CI\/sample-app #12: fix {2}\(2m ago\)\n {4}Ready\.\n {4}https:\/\/bb\/x/);
  assert.equal(await main(["inbox", "--all"], h.deps), 0);
  assert.deepEqual(h.calls[1], ["tool", "list_notifications", { all: true }]);
  assert.equal(await main(["digest"], h.deps), 0);
  assert.match(h.out.join(""), /Needs you: 1 conflict\.\n\nYour pull requests:\n {2}✗ CI\/sample-app #12 fix — conflicts/);
  assert.equal(fmt.formatInbox({ unseen: 0, items: [] }, 0), "Nothing new.");
});

test("precheck as a pre-push hook: reads git's stdin, diffs each update, warns on medium/high only, exits 0", async () => {
  const h = harness({
    stdin: `refs/heads/x ${SHA_A} refs/heads/x ${SHA_B}\n`,
    git: { "diff --name-only": { code: 0, stdout: "src/login.ts\nREADME.md\n", stderr: "" } },
    tools: { get_change_risk: MEDIUM },
  });
  assert.equal(await main(["precheck", "--stdin"], h.deps), 0);
  assert.deepEqual(h.calls.find((c) => c[0] === "git" && c[1][0] === "diff"), ["git", ["diff", "--name-only", SHA_B, SHA_A, "--"], "/work/sample-app"]);
  assert.deepEqual(h.calls.find((c) => c[0] === "tool"), ["tool", "get_change_risk", { files: ["src/login.ts", "README.md"] }]);
  assert.equal(h.err.join(""), `${fmt.formatRisk(MEDIUM)}\n`);
  assert.equal(h.out.join(""), "");

  const low = harness({ stdin: `refs/heads/x ${SHA_A} refs/heads/x ${SHA_B}\n`, git: { "diff --name-only": { code: 0, stdout: "a\n" } }, tools: { get_change_risk: { ...MEDIUM, level: "low" } } });
  assert.equal(await main(["precheck", "--stdin"], low.deps), 0);
  assert.equal(low.err.join(""), "", "a hook stays quiet unless there's something to say");
});

test("precheck is silent when the service is down, skipped, turned off, or over its 3 seconds", async () => {
  const stdin = `refs/heads/x ${SHA_A} refs/heads/x ${SHA_B}\n`;
  const withFiles = { "diff --name-only": { code: 0, stdout: "a\n" } };
  const down = harness({ stdin, git: withFiles, tools: { get_change_risk: new Error("The companion service isn't running (nothing is listening on port 8787).") } });
  assert.equal(await main(["precheck", "--stdin"], down.deps), 0);
  assert.equal(down.err.join(""), "");

  const skip = harness({ env: { COMPANION_SKIP: "1" } });
  assert.equal(await main(["precheck", "--stdin"], skip.deps), 0);
  assert.deepEqual(skip.calls, []);

  const off = harness({ config: { prePush: { mode: "off" } } });
  assert.equal(await main(["precheck"], off.deps), 0);
  assert.deepEqual(off.calls, []);

  const slow = harness({ stdin, git: withFiles, tools: { get_change_risk: () => new Promise(() => {}) }, sleepForever: false });
  assert.equal(await main(["precheck", "--stdin"], slow.deps), 0);
  assert.equal(slow.err.join(""), "");
  const slowByHand = harness({ git: withFiles, tools: { get_change_risk: () => new Promise(() => {}) }, sleepForever: false });
  assert.equal(await main(["precheck"], slowByHand.deps), 0);
  assert.match(slowByHand.err.join(""), /no answer within 3 seconds/);
});

test("precheck by hand: named files are made repo-relative; the answer is printed even when skipped", async () => {
  const h = harness({ tools: { get_change_risk: { skipped: true, reason: "The shared test history isn't set up." } } });
  assert.equal(await main(["precheck", "login.ts", "../README.md"], h.deps), 0);
  assert.deepEqual(h.calls.find((c) => c[0] === "tool")[2], { files: ["src/login.ts", "README.md"] });
  assert.match(h.err.join(""), /skipped — The shared test history isn't set up\./);
  const outside = harness({ git: { "rev-parse": { code: 128, stdout: "", stderr: "not a git repo" } } });
  assert.equal(await main(["precheck"], outside.deps), 0);
  assert.match(outside.err.join(""), /inside a git repository/);
});

test("hooks install probes git first, then sets the two config keys; without config hooks it prints the line", async () => {
  const h = harness({ git: { "hook run": { code: 0, stdout: "companion-probe-ok\n" } } });
  assert.equal(await main(["hooks", "install"], h.deps), 0);
  const configCalls = h.calls.filter((c) => c[0] === "git" && c[1].includes("config") && c[1][1] === "/work/sample-app");
  assert.deepEqual(configCalls.map((c) => c[1].slice(4)), [
    ["hook.companion-precheck.event", "pre-push"],
    ["hook.companion-precheck.command", "companion precheck --stdin"],
  ]);
  assert.ok(h.calls.some((c) => c[0] === "rm" && c[1] === "/tmp/probe"));
  assert.match(h.out.join(""), /Installed the pre-push check in \/work\/sample-app/);

  const old = harness({ git: { "hook run": { code: 129, stdout: "", stderr: "unknown subcommand" } } });
  assert.equal(await main(["hooks", "install"], old.deps), 0);
  assert.match(old.out.join(""), /companion precheck --stdin \|\| true/);
  assert.equal(old.calls.some((c) => c[0] === "git" && c[1].includes("hook.companion-precheck.command") && c[1][1] === "/work/sample-app"), false);
});

test("hooks uninstall tolerates a key that wasn't set; hooks status reports; a bad action is a usage error", async () => {
  const h = harness({ git: { "--unset-all": { code: 5, stdout: "", stderr: "" } } });
  assert.equal(await main(["hooks", "uninstall", "/work/sample-app"], h.deps), 0);
  assert.match(h.out.join(""), /Removed the pre-push check/);
  const status = harness({ git: { "--get": { code: 1, stdout: "" } } });
  await main(["hooks", "status"], status.deps);
  assert.match(status.out.join(""), /isn't installed/);
  const bad = harness();
  assert.equal(await main(["hooks", "frob"], bad.deps), 2);
});

test("history --metrics shows the pre-warmed runs used per watcher", () => {
  const text = fmt.formatMetrics({ days: 30, metrics: [], prewarmed: [{ watcher: "conflicts", runs: 4, used: 3, usedFraction: 0.75 }], matches: [] });
  assert.match(text, /Pre-warmed by the background watchers/);
  assert.match(text, /conflicts\s+4 run\(s\)\s+3 used\s+75%/);
});

test("precheck hardening: the probe is isolated, a missing remote sha and an empty file list are silent, files are capped, the deadline holds", async () => {
  const probe = harness({ git: { "hook run": { code: 0, stdout: "companion-probe-ok\n" } } });
  const opts = [];
  const inner = probe.deps.git;
  probe.deps.git = async (args, cwd, o) => {
    opts.push([args[0] === "-C" ? args[2] : args[0], o]);
    return inner(args, cwd);
  };
  await main(["hooks", "install"], probe.deps);
  const probeOpts = opts.filter(([, o]) => o && o.isolated);
  assert.equal(probeOpts.length, 4, "all four probe steps run isolated");
  assert.equal(opts.filter(([, o]) => !o).length, 3, "the rev-parse and the two config sets on the real repo are not isolated");

  const stdin = `refs/heads/x ${SHA_A} refs/heads/x ${SHA_B}\n`;
  const missing = harness({ stdin, git: { "diff --name-only": { code: 128, stdout: "", stderr: "fatal: bad object" } }, tools: {} });
  assert.equal(await main(["precheck", "--stdin"], missing.deps), 0);
  assert.equal(missing.err.join(""), "");
  assert.equal(missing.calls.some((c) => c[0] === "tool"), false, "no files, no service call");

  const many = harness({
    stdin,
    git: { "diff --name-only": { code: 0, stdout: Array.from({ length: 900 }, (_, i) => `f${i}.js`).join("\n") } },
    tools: { get_change_risk: (a) => ({ skipped: true, reason: String(a.files.length) }) },
  });
  assert.equal(await main(["precheck", "--json", "--stdin"], many.deps), 0);
  assert.equal(many.calls.find((c) => c[0] === "tool")[2].files.length, 500);

  let t = 0;
  const slow = harness({
    stdin: Array.from({ length: 5 }, (_, i) => `refs/heads/x ${SHA_A} refs/heads/y${i} ${SHA_B}`).join("\n") + "\n",
    git: { "diff --name-only": () => ((t += 2000), { code: 0, stdout: "a\n" }) },
    tools: { get_change_risk: { ...MEDIUM, level: "low" } },
  });
  slow.deps.now = () => t;
  assert.equal(await main(["precheck", "--stdin"], slow.deps), 0);
  assert.equal(slow.calls.filter((c) => c[0] === "git" && c[1][0] === "diff").length, 2, "stops diffing once the 3 seconds are used up");
});

test("precheck exits 0 whatever happens: odd replies, a throwing stderr, a bad flag, a missing token", async () => {
  const files = { git: { "diff --name-only": { code: 0, stdout: "a.ts\n" } } };
  for (const odd of ["oops", 42, {}, { skipped: false, level: "high", lines: "nope" }, { skipped: false, level: "high", lines: [null, 5] }]) {
    const h = harness({ ...files, tools: { get_change_risk: odd } });
    assert.equal(await main(["precheck"], h.deps), 0, `reply ${JSON.stringify(odd)}`);
  }
  const broken = harness({ ...files, tools: { get_change_risk: MEDIUM } });
  broken.deps.err = () => {
    throw new Error("EPIPE");
  };
  assert.equal(await main(["precheck"], broken.deps), 0);
  const outBroken = harness({ ...files, tools: { get_change_risk: MEDIUM } });
  outBroken.deps.out = () => {
    throw new Error("EPIPE");
  };
  assert.equal(await main(["precheck", "--json"], outBroken.deps), 0);

  const bad = harness({});
  assert.equal(await main(["precheck", "--bogus"], bad.deps), 0);
  assert.match(bad.err.join(""), /Unknown option/);

  const throwingGit = harness({});
  throwingGit.deps.git = async () => {
    throw new Error("spawn failed");
  };
  assert.equal(await main(["precheck", "--stdin"], throwingGit.deps), 0);

  for (const message of ["No MCP token is saved yet. Run `npm run setup`.", "The companion refused the token. Run `companion doctor`."]) {
    const noToken = harness({ ...files, stdin: `refs/heads/x ${SHA_A} refs/heads/x ${SHA_B}\n`, tools: { get_change_risk: new Error(message) } });
    assert.equal(await main(["precheck", "--stdin"], noToken.deps), 0);
    assert.equal(noToken.err.join(""), "", "a hook says nothing when the command isn't set up");
    const byHand = harness({ ...files, tools: { get_change_risk: new Error(message) } });
    assert.equal(await main(["precheck"], byHand.deps), 0);
    assert.match(byHand.err.join(""), /companion precheck: /, "by hand it says why");
  }
});

test("--json precheck in hook mode prints the raw answer even when low; hooks on a non-repo path and hooks status", async () => {
  const low = { ...MEDIUM, level: "low" };
  const h = harness({ stdin: `refs/heads/x ${SHA_A} refs/heads/x ${SHA_B}\n`, git: { "diff --name-only": { code: 0, stdout: "a\n" } }, tools: { get_change_risk: low } });
  assert.equal(await main(["precheck", "--stdin", "--json"], h.deps), 0);
  assert.deepEqual(JSON.parse(h.out.join("")), low);

  const notRepo = harness({ git: { "rev-parse": { code: 128, stdout: "", stderr: "fatal" } } });
  assert.equal(await main(["hooks", "status", "/nowhere"], notRepo.deps), 1);
  assert.match(notRepo.err.join(""), /isn't inside a git repository/);

  const installed = harness({ git: { "config": { code: 0, stdout: "companion precheck --stdin\n", stderr: "" } } });
  assert.equal(await main(["hooks", "status"], installed.deps), 0);
  assert.match(installed.out.join(""), /is installed in \/work\/sample-app/);
});

test("terminal escapes from the service never reach the terminal", async () => {
  const ESC = "\x1b[2J\x1b]0;pwned\x07";
  const bidi = "a‮b​c\u0085d";
  const h = harness({
    tools: {
      list_notifications: { unseen: 1, items: [{ title: `T${ESC}`, body: `B${ESC}`, url: `https://x/${ESC}${bidi}`, urgent: true, createdAt: 1 }] },
      get_digest: { headline: `H${ESC}`, sections: [{ title: `S${ESC}`, items: [{ text: `I${ESC}`, tone: "ok" }] }] },
      get_change_risk: { skipped: false, level: "high", summary: `sum${ESC}`, lines: [{ text: `line${ESC}${bidi}` }] },
    },
    git: { "diff --name-only": { code: 0, stdout: "a\n" } },
  });
  for (const argv of [["inbox"], ["digest"], ["precheck"]]) assert.equal(await main(argv, h.deps), 0);
  const all = h.out.join("") + h.err.join("");
  assert.doesNotMatch(all, /[\u0000-\u0009\u000b-\u001f\u007f-\u009f​-‏‪-‮]/);
  assert.match(all, /T\[2J/);

  const failing = harness({ tools: { get_digest: new Error(`bad${ESC}`) } });
  assert.equal(await main(["digest"], failing.deps), 1);
  assert.doesNotMatch(failing.err.join(""), /\x1b/);
  const failingPre = harness({ git: { "diff --name-only": { code: 0, stdout: "a\n" } }, tools: { get_change_risk: new Error(`bad${ESC}`) } });
  await main(["precheck"], failingPre.deps);
  assert.doesNotMatch(failingPre.err.join(""), /\x1b|\x07/);
});
