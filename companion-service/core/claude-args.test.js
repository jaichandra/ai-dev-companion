const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execSync } = require("child_process");
const {
  CLAUDE_POLICIES,
  protectedPathRules,
  protectedSandboxPaths,
  isSandboxedPolicy,
  sandboxSettings,
  buildClaudeArgs,
  companionServiceDir,
  checkCommandRules,
  normalizeCheckCommands,
} = require("./claude-args.js");

/** The editRoot + guard scripts every worktree-policy (readGuard) build
 * needs — buildClaudeArgs refuses one without them. */
const WT_GUARDS = {
  editRoot: "/wt",
  editGuardScriptPath: "/abs/core/edit-guard.js",
  readGuardScriptPath: "/abs/core/read-guard.js",
};

test("CLAUDE_POLICIES has the exact readOnly/readOnlyBackground/worktreeWrite/worktreeWriteNarrowBash/noTools shapes", () => {
  assert.deepEqual(CLAUDE_POLICIES, {
    readOnly: {
      tools: [
        "Read",
        "Glob",
        "Grep",
        "WebFetch",
        "WebSearch",
        "Bash(git log:*)",
        "Bash(git blame:*)",
        "Bash(git show:*)",
      ],
      mcpGuard: true,
      denyCwdWrite: true,
    },
    readOnlyBackground: {
      tools: ["Read", "Glob", "Grep", "Bash(git log:*)", "Bash(git blame:*)", "Bash(git show:*)"],
      mcpGuard: true,
      denyCwdWrite: true,
    },
    worktreeWrite: {
      tools: ["Read", "Edit", "Write", "Bash", "Glob", "Grep"],
      mcpGuard: false,
      readGuard: true,
    },
    worktreeWriteNarrowBash: {
      tools: ["Read", "Edit", "Write", "Glob", "Grep", "Bash(git diff:*)", "Bash(git status:*)"],
      mcpGuard: false,
      readGuard: true,
      bashGuard: true,
    },
    noTools: {
      tools: [],
      mcpGuard: false,
    },
  });
});

test("protectedPathRules returns the exact deny rules, not a blanket stateDir deny", () => {
  const rules = protectedPathRules({
    stateDir: "/home/me/.ai-dev-companion",
    companionDir: "/opt/ai-dev-companion/companion-service",
    home: "/home/me",
  });
  assert.deepEqual(rules, [
    "Read(//home/me/.ai-dev-companion/credentials.key)",
    "Read(//home/me/.ai-dev-companion/history.db)",
    "Read(//home/me/.ai-dev-companion/history.db-wal)",
    "Read(//home/me/.ai-dev-companion/history.db-shm)",
    "Read(//opt/ai-dev-companion/companion-service/credentials.enc)",
    "Read(//opt/ai-dev-companion/companion-service/config.json)",
    "Edit(//opt/ai-dev-companion/companion-service/**)",
    "Read(//home/me/.claude.json)",
    "Read(//home/me/.ssh/**)",
    "Read(//home/me/.aws/**)",
    "Read(//home/me/.gnupg/**)",
    "Read(//home/me/.config/gh/**)",
    "Read(//home/me/.kube/**)",
    "Read(//home/me/.bitbucket-ai-companion/**)",
    "Read(//home/me/.netrc)",
    "Read(//home/me/.npmrc)",
    "Read(//home/me/.docker/config.json)",
    "Read(//home/me/.cursor/mcp.json)",
  ]);
  // resolve-conflict worktrees live under stateDir — a blanket deny on all
  // of stateDir would also block those, so only specific files are denied.
  assert.ok(!rules.some((r) => r === "Read(//home/me/.ai-dev-companion/**)"));
});

test("protectedSandboxPaths returns plain absolute paths, not the //abs disallowedTools form", () => {
  const paths = protectedSandboxPaths({
    stateDir: "/home/me/.ai-dev-companion",
    companionDir: "/opt/ai-dev-companion/companion-service",
    home: "/home/me",
  });
  assert.deepEqual(paths, [
    "/home/me/.ai-dev-companion/credentials.key",
    "/home/me/.ai-dev-companion/history.db",
    "/home/me/.ai-dev-companion/history.db-wal",
    "/home/me/.ai-dev-companion/history.db-shm",
    "/opt/ai-dev-companion/companion-service/credentials.enc",
    "/opt/ai-dev-companion/companion-service/config.json",
    "/home/me/.claude.json",
    "/home/me/.ssh",
    "/home/me/.aws",
    "/home/me/.gnupg",
    "/home/me/.config/gh",
    "/home/me/.kube",
    "/home/me/.bitbucket-ai-companion",
    "/home/me/.netrc",
    "/home/me/.npmrc",
    "/home/me/.docker/config.json",
    "/home/me/.cursor/mcp.json",
  ]);
});

test("isSandboxedPolicy is true for readOnly and worktreeWrite (both grant Bash), false for noTools", () => {
  assert.equal(isSandboxedPolicy(CLAUDE_POLICIES.readOnly), true);
  assert.equal(isSandboxedPolicy(CLAUDE_POLICIES.worktreeWrite), true);
  assert.equal(isSandboxedPolicy(CLAUDE_POLICIES.noTools), false);
});

test("sandboxSettings: denyRead always present, allowWrite/denyWrite omitted when empty", () => {
  assert.deepEqual(sandboxSettings({ denyRead: ["/a"] }), {
    enabled: true,
    allowUnsandboxedCommands: false,
    failIfUnavailable: true,
    filesystem: { denyRead: ["/a"] },
  });
  assert.deepEqual(sandboxSettings({ denyRead: [], allowWrite: [], denyWrite: [] }), {
    enabled: true,
    allowUnsandboxedCommands: false,
    failIfUnavailable: true,
    filesystem: { denyRead: [] },
  });
});

test("sandboxSettings includes allowWrite/denyWrite only when non-empty", () => {
  const settings = sandboxSettings({
    denyRead: ["/a"],
    allowWrite: ["/b"],
    denyWrite: ["/c"],
  });
  assert.deepEqual(settings.filesystem, { denyRead: ["/a"], allowWrite: ["/b"], denyWrite: ["/c"] });
});

/** Base argv every buildClaudeArgs call shares before policy-specific bits
 * (session id / disallowedTools / settings) are appended. `toolsCsv` is a
 * literal expected string (not re-derived from `tools` via the same regex
 * buildClaudeArgs itself uses) so this test can't pass by coincidence if
 * that regex is wrong in exactly the same way in both places. */
function baseArgv({ prompt = "do the thing", tools, toolsCsv, outputFormat = "json" }) {
  return [
    "-p",
    prompt,
    "--permission-mode",
    "bypassPermissions",
    "--allowedTools",
    tools.join(" "),
    "--output-format",
    outputFormat,
    "--tools",
    toolsCsv,
  ];
}

test("buildClaudeArgs for readOnly: base argv, no session id, ONE --settings combining the mcpGuard hook and the sandbox", () => {
  const args = buildClaudeArgs({
    prompt: "do the thing",
    policy: CLAUDE_POLICIES.readOnly,
    guardScriptPath: "/abs/core/mcp-guard.js",
    execPath: "/usr/bin/node",
    sandboxDenyRead: ["/home/me/.claude.json"],
    sandboxDenyWrite: ["/repo/session-dir"],
  });

  const expectedBase = baseArgv({
    tools: CLAUDE_POLICIES.readOnly.tools,
    toolsCsv: "Read,Glob,Grep,WebFetch,WebSearch,Bash",
  });
  assert.deepEqual(args.slice(0, expectedBase.length), expectedBase);

  // No session id was given.
  assert.ok(!args.includes("--session-id"));
  // No disallowedTools/protectedRules were given, so no --disallowedTools.
  assert.ok(!args.includes("--disallowedTools"));

  // readOnly is both mcpGuard: true AND sandboxed (it grants Bash via
  // Bash(git log:*) etc) — both must land in the SAME --settings object;
  // there must never be two --settings flags.
  const settingsIdx = args.indexOf("--settings");
  assert.ok(settingsIdx !== -1);
  assert.equal(args.indexOf("--settings", settingsIdx + 1), -1, "only one --settings flag");
  assert.deepEqual(JSON.parse(args[settingsIdx + 1]), {
    hooks: {
      PreToolUse: [
        {
          matcher: "mcp__.*",
          hooks: [{ type: "command", command: "'/usr/bin/node' '/abs/core/mcp-guard.js'" }],
        },
      ],
    },
    sandbox: {
      enabled: true,
      allowUnsandboxedCommands: false,
      failIfUnavailable: true,
      filesystem: {
        denyRead: ["/home/me/.claude.json"],
        denyWrite: ["/repo/session-dir"],
      },
    },
  });
  assert.equal(args.length, settingsIdx + 2);
});

test("buildClaudeArgs adds cwd to sandboxDenyWrite automatically when policy.denyCwdWrite is set (readOnly), merged with any explicit sandboxDenyWrite", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.readOnly,
    cwd: "/tmp/analysis-session",
    sandboxDenyRead: ["/home/me/.claude.json"],
    sandboxDenyWrite: ["/other/explicit/path"],
  });
  const settingsIdx = args.indexOf("--settings");
  const settings = JSON.parse(args[settingsIdx + 1]);
  assert.deepEqual(settings.sandbox.filesystem.denyWrite, ["/other/explicit/path", "/tmp/analysis-session"]);
});

test("buildClaudeArgs does NOT add cwd to sandboxDenyWrite for worktreeWrite (no denyCwdWrite on that policy)", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWrite,
    cwd: "/tmp/some-worktree",
    sandboxDenyRead: ["/home/me/.claude.json"],
    ...WT_GUARDS,
  });
  const settingsIdx = args.indexOf("--settings");
  const settings = JSON.parse(args[settingsIdx + 1]);
  assert.ok(!("denyWrite" in settings.sandbox.filesystem));
});

test("buildClaudeArgs for worktreeWrite: mcpGuard false but sandboxed (grants Bash) — no mcp hook, sandbox present", () => {
  const args = buildClaudeArgs({
    prompt: "resolve the conflict",
    policy: CLAUDE_POLICIES.worktreeWrite,
    guardScriptPath: "/abs/core/mcp-guard.js",
    sandboxDenyRead: ["/home/me/.claude.json"],
    sandboxAllowWrite: ["/repo/.git/worktrees/abc123"],
    ...WT_GUARDS,
  });
  const expectedBase = baseArgv({
    prompt: "resolve the conflict",
    tools: CLAUDE_POLICIES.worktreeWrite.tools,
    toolsCsv: "Read,Edit,Write,Bash,Glob,Grep",
  });
  assert.deepEqual(args.slice(0, expectedBase.length), expectedBase);
  const settingsIdx = args.indexOf("--settings");
  assert.ok(settingsIdx !== -1);
  const settings = JSON.parse(args[settingsIdx + 1]);
  assert.deepEqual(settings.sandbox, {
    enabled: true,
    allowUnsandboxedCommands: false,
    failIfUnavailable: true,
    filesystem: {
      denyRead: ["/home/me/.claude.json"],
      allowWrite: ["/repo/.git/worktrees/abc123"],
    },
  });
  assert.ok(!settings.hooks.PreToolUse.some((h) => h.matcher === "mcp__.*"));
  assert.equal(args.length, settingsIdx + 2);
});

test("buildClaudeArgs for worktreeWrite with editRoot: --settings combines the edit-guard and read-guard PreToolUse hooks and the sandbox", () => {
  const args = buildClaudeArgs({
    prompt: "resolve the conflict",
    policy: CLAUDE_POLICIES.worktreeWrite,
    sandboxDenyRead: [],
    sandboxAllowWrite: ["/repo/.git/worktrees/abc123"],
    editRoot: "/home/me/.ai-dev-companion/worktrees/repo/abc123",
    editGuardScriptPath: "/abs/core/edit-guard.js",
    readGuardScriptPath: "/abs/core/read-guard.js",
    execPath: "/usr/bin/node",
  });
  const settingsIdx = args.indexOf("--settings");
  assert.ok(settingsIdx !== -1);
  assert.equal(args.indexOf("--settings", settingsIdx + 1), -1, "only one --settings flag");
  assert.deepEqual(JSON.parse(args[settingsIdx + 1]), {
    hooks: {
      PreToolUse: [
        {
          matcher: "Edit|Write|MultiEdit|NotebookEdit",
          hooks: [
            {
              type: "command",
              command:
                "'/usr/bin/node' '/abs/core/edit-guard.js' '/home/me/.ai-dev-companion/worktrees/repo/abc123'",
            },
          ],
        },
        {
          matcher: "Read|Glob|Grep",
          hooks: [
            {
              type: "command",
              command:
                "'/usr/bin/node' '/abs/core/read-guard.js' '/home/me/.ai-dev-companion/worktrees/repo/abc123'",
            },
          ],
        },
      ],
    },
    sandbox: {
      enabled: true,
      allowUnsandboxedCommands: false,
      failIfUnavailable: true,
      filesystem: { denyRead: [], allowWrite: ["/repo/.git/worktrees/abc123"] },
    },
  });
});

test("buildClaudeArgs with BOTH the mcpGuard hook and editRoot set: PreToolUse array holds both, mcpGuard first", () => {
  // No real policy combines mcpGuard + editRoot today (readOnly has no
  // Edit/Write grant) — this proves the PreToolUse array mechanism itself
  // handles two hooks correctly, independent of which real policy uses it.
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.readOnly,
    guardScriptPath: "/abs/core/mcp-guard.js",
    editRoot: "/home/me/.ai-dev-companion/worktrees/repo/abc123",
    editGuardScriptPath: "/abs/core/edit-guard.js",
    execPath: "/usr/bin/node",
  });
  const settingsIdx = args.indexOf("--settings");
  const settings = JSON.parse(args[settingsIdx + 1]);
  assert.equal(settings.hooks.PreToolUse.length, 2);
  assert.equal(settings.hooks.PreToolUse[0].matcher, "mcp__.*");
  assert.equal(settings.hooks.PreToolUse[1].matcher, "Edit|Write|MultiEdit|NotebookEdit");
});

test("buildClaudeArgs for noTools: not sandboxed (grants no Bash) — no --settings at all", () => {
  const args = buildClaudeArgs({
    prompt: "pick a repo",
    model: "claude-haiku-4-5-20251001",
    policy: CLAUDE_POLICIES.noTools,
    sandboxDenyRead: ["/home/me/.claude.json"],
  });
  assert.ok(!args.includes("--settings"));
});

test("buildClaudeArgs single-quotes execPath/guardScriptPath so a space, $, and ' survive a real shell", () => {
  const trickyExecPath = "/opt/weird bin/$HOME/node's-copy";
  const trickyGuardPath = "/opt/weird dir/$USER/it's a path/mcp-guard.js";
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.readOnly,
    guardScriptPath: trickyGuardPath,
    execPath: trickyExecPath,
  });
  const settingsIdx = args.indexOf("--settings");
  const settings = JSON.parse(args[settingsIdx + 1]);
  const command = settings.hooks.PreToolUse[0].hooks[0].command;

  // Prove the two quoted words in `command` round-trip through a REAL
  // shell unmangled — no `$HOME`/`$USER` expansion, no word-splitting on
  // the space, and the embedded `'` doesn't break out of its quoting —
  // rather than asserting on the escaped string's exact character soup.
  // `f` echoes its two positional args back, "|||"-delimited; the paths
  // above never contain that delimiter.
  const script = `f() { printf '%s|||%s' "$1" "$2"; }; f ${command}`;
  const out = execSync(script, { shell: "/bin/sh", encoding: "utf8" });
  const [gotExecPath, gotGuardPath] = out.split("|||");
  assert.equal(gotExecPath, trickyExecPath);
  assert.equal(gotGuardPath, trickyGuardPath);
});

test("buildClaudeArgs for noTools: empty allowedTools/tools strings", () => {
  const args = buildClaudeArgs({
    prompt: "pick a repo",
    model: "claude-haiku-4-5-20251001",
    policy: CLAUDE_POLICIES.noTools,
  });
  assert.deepEqual(args, [
    "-p",
    "pick a repo",
    "--model",
    "claude-haiku-4-5-20251001",
    "--permission-mode",
    "bypassPermissions",
    "--allowedTools",
    "",
    "--output-format",
    "json",
    "--tools",
    "",
  ]);
});

test("buildClaudeArgs adds --session-id when sessionId is given", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.noTools,
    sessionId: "11111111-1111-1111-1111-111111111111",
  });
  const idx = args.indexOf("--session-id");
  assert.ok(idx !== -1);
  assert.equal(args[idx + 1], "11111111-1111-1111-1111-111111111111");
});

test("buildClaudeArgs joins disallowedTools and protectedRules into one --disallowedTools", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWrite,
    disallowedTools: ["mcp__acme-jenkins-dii__stop_build"],
    protectedRules: ["Read(//home/me/.ai-dev-companion/credentials.key)"],
    ...WT_GUARDS,
  });
  const idx = args.indexOf("--disallowedTools");
  assert.ok(idx !== -1);
  assert.equal(
    args[idx + 1],
    "mcp__acme-jenkins-dii__stop_build Read(//home/me/.ai-dev-companion/credentials.key)",
  );
});

test("buildClaudeArgs omits --disallowedTools when disallowedTools and protectedRules are both empty", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWrite,
    disallowedTools: [],
    protectedRules: [],
    ...WT_GUARDS,
  });
  assert.ok(!args.includes("--disallowedTools"));
});

test("buildClaudeArgs appends extraAllowedTools to --allowedTools but not to --tools", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.readOnly,
    extraAllowedTools: ["mcp__wiki"],
  });
  const allowedIdx = args.indexOf("--allowedTools");
  assert.equal(args[allowedIdx + 1], `${CLAUDE_POLICIES.readOnly.tools.join(" ")} mcp__wiki`);
  const toolsIdx = args.indexOf("--tools");
  assert.ok(!args[toolsIdx + 1].includes("mcp__wiki"));
});

test("companionServiceDir resolves the companion-service root from core/", () => {
  assert.equal(companionServiceDir(), path.join(__dirname, ".."));
});

test("buildClaudeArgs respects a custom outputFormat", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.noTools,
    outputFormat: "text",
  });
  const idx = args.indexOf("--output-format");
  assert.equal(args[idx + 1], "text");
});

test("buildClaudeArgs adds --verbose with stream-json, and only then", () => {
  const BASE = { prompt: "p", model: "m", policy: CLAUDE_POLICIES.noTools };
  const stream = buildClaudeArgs({ ...BASE, outputFormat: "stream-json" });
  assert.equal(stream[stream.indexOf("--output-format") + 1], "stream-json");
  assert.ok(stream.includes("--verbose"));
  assert.ok(!buildClaudeArgs({ ...BASE }).includes("--verbose"));
});

// ---- worktreeWriteNarrowBash + checkCommandRules ----

test("worktreeWriteNarrowBash differs from worktreeWrite only in its Bash grant", () => {
  const nonBash = (tools) => tools.filter((t) => t !== "Bash" && !t.startsWith("Bash("));
  assert.deepEqual(
    nonBash(CLAUDE_POLICIES.worktreeWriteNarrowBash.tools),
    nonBash(CLAUDE_POLICIES.worktreeWrite.tools),
  );
  assert.equal(CLAUDE_POLICIES.worktreeWriteNarrowBash.mcpGuard, CLAUDE_POLICIES.worktreeWrite.mcpGuard);
  assert.equal(CLAUDE_POLICIES.worktreeWriteNarrowBash.denyCwdWrite, undefined);
  assert.equal(CLAUDE_POLICIES.worktreeWriteNarrowBash.bashGuard, true);
  assert.equal(CLAUDE_POLICIES.worktreeWrite.bashGuard, undefined);
  assert.equal(isSandboxedPolicy(CLAUDE_POLICIES.worktreeWriteNarrowBash), true);
});

test("checkCommandRules turns each check command into a Bash(<cmd>:*) rule, trimmed", () => {
  assert.deepEqual(checkCommandRules(["npm test", "  npx tsc --noEmit  "]), [
    "Bash(npm test:*)",
    "Bash(npx tsc --noEmit:*)",
  ]);
  assert.deepEqual(checkCommandRules([]), []);
  assert.deepEqual(checkCommandRules(undefined), []);
});

test("checkCommandRules rejects entries that could widen the rule, naming the bad entry", () => {
  for (const bad of [
    "",
    "   ",
    "npm test) Bash(rm",
    "npm test\nrm -rf /",
    "npm\rtest",
    "npm *",
    "a(b",
    // Anything the bash guard would refuse at run time is refused here
    // too, so a configured check command can never be one Claude can't run.
    "npm test && npm run lint",
    "npm test; rm x",
    "npm test | tee log",
    "npm test > log",
    "npm test < in",
    "echo $HOME",
    "echo `id`",
    "npm test \\",
    42,
    null,
  ]) {
    assert.throws(
      () => checkCommandRules(["npm test", bad]),
      (err) => /addressReviewComments\.checkCommands\[1\]/.test(err.message) && /config\.json/.test(err.message),
      `expected ${JSON.stringify(bad)} to be rejected`,
    );
  }
  assert.throws(() => checkCommandRules("npm test"), /must be an array/);
});

test("normalizeCheckCommands returns the same trimmed, validated list checkCommandRules builds rules from", () => {
  assert.deepEqual(normalizeCheckCommands(["  npm test ", "npx tsc --noEmit"]), ["npm test", "npx tsc --noEmit"]);
  assert.deepEqual(normalizeCheckCommands(undefined), []);
  assert.throws(() => normalizeCheckCommands(["npm test && x"]), /checkCommands\[0\]/);
});

test("buildClaudeArgs for worktreeWriteNarrowBash: argv pre-approves only git diff/status + checks, and ONE --settings holds the edit guard, the bash guard (enforcement) and the sandbox", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWriteNarrowBash,
    cwd: "/wt",
    extraAllowedTools: checkCommandRules(["npm test"]),
    sandboxDenyRead: ["/home/me/.claude.json"],
    sandboxAllowWrite: ["/repo/.git/worktrees/job"],
    sandboxDenyWrite: ["/wt/.git", "/repo/.git/worktrees/job/commondir"],
    editRoot: "/wt",
    editGuardScriptPath: "/abs/core/edit-guard.js",
    readGuardScriptPath: "/abs/core/read-guard.js",
    bashGuardScriptPath: "/abs/core/bash-guard.js",
    execPath: "/usr/bin/node",
  });
  assert.equal(args.indexOf("--settings", args.indexOf("--settings") + 1), -1, "only one --settings flag");
  const allowed = args[args.indexOf("--allowedTools") + 1];
  assert.equal(allowed, "Read Edit Write Glob Grep Bash(git diff:*) Bash(git status:*) Bash(npm test:*)");
  const bashRules = allowed.split(/ (?=[A-Z])/).filter((t) => t === "Bash" || t.startsWith("Bash("));
  assert.deepEqual(bashRules, ["Bash(git diff:*)", "Bash(git status:*)", "Bash(npm test:*)"]);
  assert.equal(args[args.indexOf("--tools") + 1], "Read,Edit,Write,Glob,Grep,Bash");

  const settings = JSON.parse(args[args.indexOf("--settings") + 1]);
  assert.deepEqual(settings.sandbox, {
    enabled: true,
    allowUnsandboxedCommands: false,
    failIfUnavailable: true,
    filesystem: {
      denyRead: ["/home/me/.claude.json"],
      allowWrite: ["/repo/.git/worktrees/job"],
      denyWrite: ["/wt/.git", "/repo/.git/worktrees/job/commondir"],
    },
  });
  // --allowedTools above only PRE-APPROVES under bypassPermissions; the
  // bash guard hook is what refuses everything else (see core/bash-guard.js).
  assert.deepEqual(settings.hooks.PreToolUse, [
    {
      matcher: "Edit|Write|MultiEdit|NotebookEdit",
      hooks: [{ type: "command", command: "'/usr/bin/node' '/abs/core/edit-guard.js' '/wt'" }],
    },
    {
      matcher: "Read|Glob|Grep",
      hooks: [{ type: "command", command: "'/usr/bin/node' '/abs/core/read-guard.js' '/wt'" }],
    },
    {
      matcher: "Bash",
      hooks: [
        {
          type: "command",
          command: `'/usr/bin/node' '/abs/core/bash-guard.js' '["git diff","git status","npm test"]'`,
        },
      ],
    },
  ]);
});

test("buildClaudeArgs refuses a bashGuard policy with no guard script path (fail closed, not unguarded)", () => {
  assert.throws(
    () =>
      buildClaudeArgs({
        prompt: "p",
        model: "m",
        policy: CLAUDE_POLICIES.worktreeWriteNarrowBash,
        sandboxDenyRead: [],
        ...WT_GUARDS,
      }),
    /bash guard/,
  );
});

test("buildClaudeArgs adds no bash guard hook for worktreeWrite", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWrite,
    bashGuardScriptPath: "/abs/core/bash-guard.js",
    sandboxDenyRead: [],
    ...WT_GUARDS,
  });
  const settings = JSON.parse(args[args.indexOf("--settings") + 1]);
  assert.ok(!settings.hooks.PreToolUse.some((h) => h.matcher === "Bash"));
});

test("the bash guard command survives a real shell with a check command containing a quote", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWriteNarrowBash,
    extraAllowedTools: checkCommandRules(["npm run it's"]),
    bashGuardScriptPath: "/abs/core/bash-guard.js",
    execPath: "/usr/bin/node",
    sandboxDenyRead: [],
    ...WT_GUARDS,
  });
  const command = JSON.parse(args[args.indexOf("--settings") + 1]).hooks.PreToolUse.find((h) => h.matcher === "Bash")
    .hooks[0].command;
  const argv = JSON.parse(
    execSync(`node -e 'console.log(JSON.stringify(process.argv.slice(1)))' ${command.split(" ").slice(2).join(" ")}`, {
      encoding: "utf8",
    }),
  );
  assert.deepEqual(JSON.parse(argv[0]), ["git diff", "git status", "npm run it's"]);
});

// ---- read guard (final-review finding 3) ----

test("both worktree policies ask for the read guard; readOnly and noTools don't", () => {
  assert.equal(CLAUDE_POLICIES.worktreeWrite.readGuard, true);
  assert.equal(CLAUDE_POLICIES.worktreeWriteNarrowBash.readGuard, true);
  assert.equal(CLAUDE_POLICIES.readOnly.readGuard, undefined);
  assert.equal(CLAUDE_POLICIES.noTools.readGuard, undefined);
});

test("buildClaudeArgs for worktreeWrite: ONE --settings holds the edit guard, the Read|Glob|Grep read guard (same root) and the sandbox", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWrite,
    sandboxDenyRead: [],
    editRoot: "/wt",
    editGuardScriptPath: "/abs/core/edit-guard.js",
    readGuardScriptPath: "/abs/core/read-guard.js",
    execPath: "/usr/bin/node",
  });
  assert.equal(args.filter((a) => a === "--settings").length, 1);
  const settings = JSON.parse(args[args.indexOf("--settings") + 1]);
  assert.deepEqual(settings.hooks.PreToolUse, [
    {
      matcher: "Edit|Write|MultiEdit|NotebookEdit",
      hooks: [{ type: "command", command: "'/usr/bin/node' '/abs/core/edit-guard.js' '/wt'" }],
    },
    {
      matcher: "Read|Glob|Grep",
      hooks: [{ type: "command", command: "'/usr/bin/node' '/abs/core/read-guard.js' '/wt'" }],
    },
  ]);
  assert.equal(settings.sandbox.enabled, true);
});

test("buildClaudeArgs for worktreeWriteNarrowBash: all three hooks (edit, read, bash) in ONE --settings", () => {
  const args = buildClaudeArgs({
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWriteNarrowBash,
    sandboxDenyRead: [],
    editRoot: "/wt",
    editGuardScriptPath: "/abs/core/edit-guard.js",
    readGuardScriptPath: "/abs/core/read-guard.js",
    bashGuardScriptPath: "/abs/core/bash-guard.js",
    execPath: "/usr/bin/node",
  });
  assert.equal(args.filter((a) => a === "--settings").length, 1);
  const settings = JSON.parse(args[args.indexOf("--settings") + 1]);
  assert.deepEqual(
    settings.hooks.PreToolUse.map((h) => h.matcher),
    ["Edit|Write|MultiEdit|NotebookEdit", "Read|Glob|Grep", "Bash"],
  );
});

test("buildClaudeArgs refuses a readGuard policy with no editRoot or no read guard script (fail closed)", () => {
  const base = {
    prompt: "p",
    model: "m",
    policy: CLAUDE_POLICIES.worktreeWrite,
    sandboxDenyRead: [],
    editGuardScriptPath: "/abs/core/edit-guard.js",
    readGuardScriptPath: "/abs/core/read-guard.js",
  };
  assert.throws(() => buildClaudeArgs({ ...base, editRoot: undefined }), /read guard/);
  assert.throws(() => buildClaudeArgs({ ...base, editRoot: "/wt", readGuardScriptPath: undefined }), /read guard/);
});

test("protected secret locations are denied to every run: Read rules and sandbox denyRead", () => {
  const opts = { stateDir: "/h/.ai-dev-companion", companionDir: "/c", home: "/h" };
  const rules = protectedPathRules(opts);
  const sandbox = protectedSandboxPaths(opts);
  for (const dir of [".ssh", ".aws", ".gnupg", ".config/gh", ".kube", ".bitbucket-ai-companion"]) {
    assert.ok(rules.includes(`Read(//h/${dir}/**)`), dir);
    assert.ok(sandbox.includes(`/h/${dir}`), dir);
  }
  for (const file of [".netrc", ".npmrc", ".docker/config.json", ".cursor/mcp.json"]) {
    assert.ok(rules.includes(`Read(//h/${file})`), file);
    assert.ok(sandbox.includes(`/h/${file}`), file);
  }
});

test("buildClaudeArgs passes --model only when a model is given", () => {
  const withModel = buildClaudeArgs({ prompt: "p", model: "claude-haiku-4-5-20251001", policy: CLAUDE_POLICIES.noTools });
  const i = withModel.indexOf("--model");
  assert.ok(i !== -1);
  assert.equal(withModel[i + 1], "claude-haiku-4-5-20251001");

  for (const model of [undefined, ""]) {
    const without = buildClaudeArgs({ prompt: "p", model, policy: CLAUDE_POLICIES.noTools });
    assert.ok(!without.includes("--model"));
  }
});
