const test = require("node:test");
const assert = require("node:assert/strict");
const {
  assertPayload,
  buildReviewPrompt,
  buildTerminalHeader,
  buildTerminalTitle,
  stripControlChars,
  parseDirtyFiles,
  buildSummary,
  buildReviewResult,
  parseReplaceJobId,
  samePr,
  decideStart,
  buildConflict,
  buildSwitchedResult,
  prKey,
  decideReuse,
} = require("./plan.js");

const PR_URL = "https://bitbucket.example.com/projects/ACME/repos/sample-app/pull-requests/42";

/** A styled header as the plain text lines it prints. */
const plainLines = (lines) => lines.map((line) => line.map((s) => s.text).join(""));

test("assertPayload accepts a well-formed payload", () => {
  const result = assertPayload({ project: "ACME", repo: "sample-app", prId: 42, sourceBranch: "feature/x" });
  assert.deepEqual(result, {
    project: "ACME",
    repo: "sample-app",
    prId: 42,
    sourceBranch: "feature/x",
    targetBranch: undefined,
    title: undefined,
    prUrl: undefined,
  });
});

test("assertPayload keeps an http(s) prUrl and drops anything else", () => {
  const base = { project: "ACME", repo: "sample-app", sourceBranch: "x" };
  assert.equal(assertPayload({ ...base, prUrl: PR_URL }).prUrl, PR_URL);
  assert.equal(assertPayload({ ...base, prUrl: "javascript:alert(1)" }).prUrl, undefined);
});

test("buildReviewPrompt tells the agent to use whichever review skill it has, with the PR URL", () => {
  const prompt = buildReviewPrompt({ sourceBranch: "x", targetBranch: "master", prId: 42, prUrl: PR_URL });
  assert.match(prompt, /^Review pull request #42 \(https:\S+\/pull-requests\/42; branch x, merging into master\)\./);
  assert.match(prompt, /git diff origin\/master\.\.\.HEAD/);
  assert.match(prompt, /Don't modify any files/);
  assert.match(prompt, /If you have a skill or command for reviewing/);
  assert.match(prompt, /available skills/);
  assert.match(prompt, /Otherwise: look for bugs/);
  assert.match(prompt, /grouped by severity/);
  assert.doesNotMatch(prompt, /review-pr/);
});

test("buildReviewPrompt works without a PR URL", () => {
  const prompt = buildReviewPrompt({ sourceBranch: "x", targetBranch: "master", prId: 42 });
  assert.match(prompt, /^Review pull request #42 \(branch x, merging into master\)\./);
  assert.match(prompt, /grouped by severity/);
});

test("assertPayload carries targetBranch and title through when present", () => {
  const result = assertPayload({
    project: "ACME",
    repo: "sample-app",
    sourceBranch: "feature/x",
    targetBranch: "master",
    title: "Fix bars",
  });
  assert.equal(result.targetBranch, "master");
  assert.equal(result.title, "Fix bars");
});

test("assertPayload drops a non-string or blank targetBranch/title", () => {
  const result = assertPayload({ project: "ACME", repo: "sample-app", sourceBranch: "x", targetBranch: 5, title: "  " });
  assert.equal(result.targetBranch, undefined);
  assert.equal(result.title, undefined);
});

test("buildReviewPrompt diffs against the PR's target branch", () => {
  const prompt = buildReviewPrompt({ sourceBranch: "feature/x", targetBranch: "develop", prId: 42, title: "Fix bars" });
  assert.match(prompt, /git diff origin\/develop\.\.\.HEAD/);
  assert.match(prompt, /#42 "Fix bars"/);
  assert.match(prompt, /Don't modify any files/);
});

test("buildReviewPrompt falls back to origin/HEAD without a target branch", () => {
  const prompt = buildReviewPrompt({ sourceBranch: "feature/x", prId: 42 });
  assert.match(prompt, /git diff origin\/HEAD\.\.\.HEAD/);
  assert.match(prompt, /the default branch/);
});

test("buildReviewPrompt is always a single line, even for a multi-line title", () => {
  const prompt = buildReviewPrompt({ sourceBranch: "x", targetBranch: "master", prId: 1, title: "a\nb\r\n\tc" });
  assert.doesNotMatch(prompt, /[\r\n\t]/);
  assert.match(prompt, /"a b c"/);
});

test("buildSummary includes the review note when given", () => {
  const summary = buildSummary({
    branch: "feature/x",
    dir: "/repo.worktrees/pr-review",
    editorLabel: "Cursor",
    stashedFiles: [],
    reviewNote: "Cursor Agent is reviewing it.",
  });
  assert.match(summary, /opened it in Cursor\. Cursor Agent is reviewing it\./);
  assert.doesNotMatch(summary, /skill/);
});

test("buildTerminalHeader shows the PR, branch, worktree and PR link", () => {
  const lines = buildTerminalHeader({
    editor: "claude-code",
    dir: "/Users/me/sample-app.worktrees/pr-review",
    sourceBranch: "feature/x",
    targetBranch: "master",
    prId: 42,
    title: "Fix bars",
    prUrl: PR_URL,
  });
  const text = plainLines(lines);
  assert.ok(text.includes('PR review  #42 "Fix bars"'));
  assert.ok(text.some((l) => /^ {2}Branch +feature\/x -> master$/.test(l)));
  assert.ok(text.some((l) => /^ {2}Worktree +\/Users\/me\/sample-app\.worktrees\/pr-review$/.test(l)));
  assert.ok(text.some((l) => l === `  ${"PR".padEnd(16)}${PR_URL}`));
  assert.ok(text.some((l) => /^Starting Claude Code/.test(l)));
});

test("buildTerminalHeader styles each part so the terminal can color it", () => {
  const lines = buildTerminalHeader({
    editor: "claude-code",
    dir: "/wt",
    sourceBranch: "feature/x",
    targetBranch: "master",
    title: "Fix bars",
    prUrl: PR_URL,
  });
  const styleOf = (text) => lines.flat().find((s) => s.text === text)?.style;
  assert.equal(styleOf("PR review"), "title");
  assert.equal(styleOf('  "Fix bars"'), "heading");
  assert.equal(styleOf("feature/x"), "branch");
  assert.equal(styleOf("master"), "target");
  assert.equal(styleOf("/wt"), "path");
  assert.equal(styleOf(PR_URL), "link");
  assert.ok(lines.flat().every((s) => typeof s.text === "string"));
});

test("buildTerminalHeader is empty for Cursor, whose review runs in the IDE", () => {
  assert.deepEqual(buildTerminalHeader({ editor: "cursor", dir: "/wt", sourceBranch: "x" }), []);
});

test("buildTerminalHeader for Claude Code has no IDE link", () => {
  const lines = buildTerminalHeader({ editor: "claude-code", dir: "/wt", sourceBranch: "x", targetBranch: "master" });
  const text = plainLines(lines);
  assert.ok(text.every((l) => !/Open in/.test(l)));
  assert.ok(text.some((l) => /^Starting Claude Code/.test(l)));
});

test("buildTerminalHeader keeps every segment single-line and copes with missing PR details", () => {
  const lines = buildTerminalHeader({ editor: "claude-code", dir: "/wt", sourceBranch: "x", title: "a\nb" });
  const text = plainLines(lines);
  assert.ok(text.includes('PR review  "a b"'));
  assert.ok(text.some((l) => /^ {2}Branch +x -> \(default branch\)$/.test(l)));
  assert.ok(lines.flat().every((s) => !/[\r\n]/.test(s.text)));
  assert.ok(text.every((l) => !/^ {2}PR /.test(l)));
});

test("buildTerminalHeader is empty for VS Code, whose review isn't in a terminal", () => {
  assert.deepEqual(buildTerminalHeader({ editor: "vscode", dir: "/wt", sourceBranch: "x" }), []);
});

test("buildSummary doesn't claim an editor was opened when none was", () => {
  const summary = buildSummary({
    branch: "feature/x",
    dir: "/wt",
    stashedFiles: [],
    reviewNote: "Cursor Agent is reviewing it.",
  });
  assert.match(summary, /^Checked out feature\/x in \/wt\. Cursor Agent is reviewing it\./);
  assert.doesNotMatch(summary, /opened it in/);
});

test("assertPayload rejects a missing project", () => {
  assert.throws(() => assertPayload({ repo: "sample-app", sourceBranch: "x" }), /project, repo, sourceBranch/);
});

test("assertPayload rejects a missing repo", () => {
  assert.throws(() => assertPayload({ project: "ACME", sourceBranch: "x" }), /project, repo, sourceBranch/);
});

test("assertPayload rejects a missing sourceBranch", () => {
  assert.throws(() => assertPayload({ project: "ACME", repo: "sample-app" }), /project, repo, sourceBranch/);
});

test("assertPayload rejects a non-object body", () => {
  assert.throws(() => assertPayload(null), /project, repo, sourceBranch/);
});

test("parseDirtyFiles extracts file paths from porcelain output", () => {
  const porcelain = " M src/api/client.ts\n?? notes.md\n";
  assert.deepEqual(parseDirtyFiles(porcelain), ["src/api/client.ts", "notes.md"]);
});

test("parseDirtyFiles returns an empty array for a clean worktree", () => {
  assert.deepEqual(parseDirtyFiles(""), []);
});

test("buildSummary names the stash when files were stashed", () => {
  const summary = buildSummary({
    branch: "feature/x",
    dir: "/repo.worktrees/pr-review",
    editorLabel: "Cursor",
    stashedFiles: ["a.ts", "b.ts"],
  });
  assert.match(summary, /Checked out feature\/x/);
  assert.match(summary, /2 files/);
  assert.match(summary, /git stash pop/);
});

test("buildSummary uses singular 'file' for exactly one stashed file", () => {
  const summary = buildSummary({
    branch: "feature/x",
    dir: "/repo.worktrees/pr-review",
    editorLabel: "Cursor",
    stashedFiles: ["a.ts"],
  });
  assert.match(summary, /1 file:/);
});

test("buildSummary omits the stash note when nothing was stashed", () => {
  const summary = buildSummary({
    branch: "feature/x",
    dir: "/repo.worktrees/pr-review",
    editorLabel: "Cursor",
    stashedFiles: [],
  });
  assert.doesNotMatch(summary, /stash/);
});

// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

test("stripControlChars replaces C0, C1 and DEL characters with spaces", () => {
  assert.equal(stripControlChars("a\x1b[31mb\x07c\rd\x7fe\x9bf"), "a [31mb c d e f");
  assert.equal(stripControlChars("plain → text"), "plain → text");
});

test("buildTerminalHeader strips terminal escapes from PR-supplied text", () => {
  const lines = buildTerminalHeader({
    editor: "claude-code",
    dir: "/wt",
    sourceBranch: "feat\x1b]0;pwned\x07",
    targetBranch: "mas\x9bter",
    prId: 42,
    title: "Fix \x1b[2Jbars\x07",
    prUrl: `${PR_URL}\x1b[0m`,
  });
  assert.ok(lines.flat().every((s) => !CONTROL.test(s.text)));
  assert.ok(plainLines(lines).includes('PR review  #42 "Fix [2Jbars"'));
});

test("buildReviewPrompt strips control characters from the title", () => {
  const prompt = buildReviewPrompt({ sourceBranch: "x", prId: 1, title: "a\x1b]0;t\x07b" });
  assert.doesNotMatch(prompt, CONTROL);
  assert.match(prompt, /"a \]0;t b"/);
});

test("buildTerminalTitle summarises the PR number and branches", () => {
  const { title, badge } = buildTerminalTitle({
    prId: 5429,
    sourceBranch: "proj-31764-activity-color-is-not-displayed-in-tooltip",
    targetBranch: "master",
  });
  assert.equal(title, "PR #5429 · proj-31764-activity-color-is-not-displayed-in-tooltip → master");
  assert.equal(badge, "PR #5429");
});

test("buildTerminalTitle shortens a long source branch to fit, keeping the target", () => {
  const { title } = buildTerminalTitle({ prId: 1, sourceBranch: "b".repeat(200), targetBranch: "release/2026.09" });
  assert.equal(Array.from(title).length, 80);
  assert.match(title, /^PR #1 · b+… → release\/2026\.09$/);
});

test("buildTerminalTitle copes with a missing PR number and target, and strips control characters", () => {
  const { title, badge } = buildTerminalTitle({ sourceBranch: "x\x1b]0;y\x07" });
  assert.equal(title, "PR review · x ]0;y → (default branch)");
  assert.doesNotMatch(title, CONTROL);
  assert.equal(badge, "x ]0;y");
});

test("buildTerminalTitle's badge without a PR id is the source branch, cut to one short line", () => {
  const { badge } = buildTerminalTitle({ sourceBranch: "abc-31764-activity-color-is-not-displayed-in-tooltip" });
  assert.equal(badge, "abc-31764-activity-…");
  assert.doesNotMatch(badge, CONTROL);
});

test("buildReviewResult lays out a started Cursor review for the extension's panel", () => {
  const review = buildReviewResult({
    payload: { prId: 42, title: "Fix bars", prUrl: PR_URL, sourceBranch: "feature/x", targetBranch: "master" },
    dir: "/my repos/pr-review",
    editor: "cursor",
    reviewStarted: true,
    reviewNote: "Cursor Agent is reviewing it.",
    stashedFiles: ["a.ts"],
    tracked: true,
  });
  assert.deepEqual(review, {
    prId: 42,
    title: "Fix bars",
    prUrl: PR_URL,
    sourceBranch: "feature/x",
    targetBranch: "master",
    worktree: "/my repos/pr-review",
    editor: "cursor",
    editorLabel: "Cursor",
    openUrl: "cursor://file/my%20repos/pr-review",
    agentLabel: "Cursor Agent (Ask mode, read-only)",
    reviewStarted: true,
    reviewNote: "Cursor Agent is reviewing it.",
    stashedFiles: ["a.ts"],
    tracked: true,
  });
});

test("buildReviewResult has no IDE link for Claude Code, and no agent when the review didn't start", () => {
  const payload = { sourceBranch: "x" };
  const claude = buildReviewResult({ payload, dir: "/wt", editor: "claude-code", reviewStarted: true });
  assert.equal(claude.openUrl, undefined);
  assert.equal(claude.agentLabel, "Claude Code");
  assert.deepEqual(claude.stashedFiles, []);

  const failed = buildReviewResult({ payload, dir: "/wt", editor: "vscode", reviewStarted: false });
  assert.equal(failed.openUrl, "vscode://file/wt");
  assert.equal(failed.agentLabel, undefined);
  assert.equal(failed.tracked, false);
});

const LIVE_JOB = "11111111-2222-4333-8444-555555555555";
const pr = (extra = {}) => ({ project: "ACME", repo: "sample-app", prId: 5429, sourceBranch: "proj-31764-tooltip", ...extra });
const liveRecord = (extra = {}) => ({
  jobId: LIVE_JOB,
  worktree: "/wt/sample-app.worktrees/pr-review",
  payload: pr(),
  summary: "Checked out.",
  review: { prId: 5429, tracked: true, worktree: "/wt/sample-app.worktrees/pr-review" },
  ...extra,
});

test("parseReplaceJobId accepts only a job id", () => {
  assert.equal(parseReplaceJobId({ replaceJobId: LIVE_JOB }), LIVE_JOB);
  assert.equal(parseReplaceJobId({ replaceJobId: "../../x" }), undefined);
  assert.equal(parseReplaceJobId({ replaceJobId: 5 }), undefined);
  assert.equal(parseReplaceJobId({}), undefined);
  assert.equal(parseReplaceJobId(null), undefined);
});

test("samePr compares PR ids within a repo, or source branches without them", () => {
  assert.equal(samePr(pr(), pr({ sourceBranch: "renamed", title: "x" })), true);
  assert.equal(samePr(pr(), pr({ prId: "5429" })), true);
  assert.equal(samePr(pr(), pr({ prId: 5430 })), false);
  assert.equal(samePr(pr(), pr({ repo: "sample-app-rest" })), false);
  assert.equal(samePr(pr({ prId: undefined }), pr({ prId: 7 })), true);
  assert.equal(samePr(pr({ prId: undefined }), pr({ prId: 7, sourceBranch: "other" })), false);
});

test("decideStart starts when nothing is live in the worktree", () => {
  assert.equal(decideStart({ payload: pr(), live: null }), "start");
  assert.equal(decideStart({ payload: pr(), live: null, replaceJobId: LIVE_JOB }), "start");
});

test("decideStart reuses the live review of the same PR", () => {
  assert.equal(decideStart({ payload: pr(), live: liveRecord() }), "reuse");
});

test("decideStart refuses another PR unless it names the live review to replace", () => {
  const other = pr({ prId: 6000, sourceBranch: "other-branch" });
  assert.equal(decideStart({ payload: other, live: liveRecord() }), "conflict");
  assert.equal(decideStart({ payload: other, live: liveRecord(), replaceJobId: LIVE_JOB }), "replace");
  const stale = "99999999-2222-4333-8444-555555555555";
  assert.equal(decideStart({ payload: other, live: liveRecord(), replaceJobId: stale }), "conflict");
});

test("buildConflict names the live review and carries what the panel needs", () => {
  const conflict = buildConflict(liveRecord(), "running");
  assert.equal(conflict.code, "review-in-progress");
  assert.match(conflict.error, /^A review of PR #5429 \(proj-31764-tooltip\) is in progress in \/wt\/sample-app/);
  assert.deepEqual(conflict.existing, {
    jobId: LIVE_JOB,
    state: "running",
    worktree: "/wt/sample-app.worktrees/pr-review",
    payload: pr(),
  });
});

test("buildConflict keeps control characters out of the message", () => {
  const conflict = buildConflict(liveRecord({ payload: pr({ sourceBranch: "a\u001b]0;x\u0007b" }) }), "running");
  // eslint-disable-next-line no-control-regex
  assert.doesNotMatch(conflict.error, /[\u0000-\u001f]/);
});

test("buildSwitchedResult marks the live review's result as switched", () => {
  const focused = buildSwitchedResult(liveRecord(), "focused");
  assert.match(focused.summary, /^Switched to the review of PR #5429 \(proj-31764-tooltip\) already running in /);
  assert.deepEqual(focused.review, {
    ...liveRecord().review,
    switched: { focused: true, note: "Its terminal window is in front." },
  });
  assert.equal(buildSwitchedResult(liveRecord(), "missing").review.switched.focused, false);
  assert.match(buildSwitchedResult(liveRecord(), "missing").summary, /couldn't be found/);
  assert.match(buildSwitchedResult(liveRecord(), "unknown").summary, /isn't known/);
  assert.match(buildSwitchedResult(liveRecord(), "osascript failed: x").summary, /front: osascript failed: x$/);
});

const ID = "0f8b2d0e-1c3a-4b5d-9e7f-123456789abc";
const record = { sessionId: ID, cwd: "/wt/pr-review", reviewedSha: "a".repeat(40), reviewedAt: 1 };
const base = { record, head: "a".repeat(40), dir: "/wt/pr-review", transcriptExists: true, isAncestor: true };

test("prKey is stable per PR, falling back to the branch without an id", () => {
  assert.equal(prKey({ project: "P", repo: "r", prId: 7, sourceBranch: "b" }), "P/r#7");
  assert.equal(prKey({ project: "P", repo: "r", sourceBranch: "b" }), "P/r@b");
});

test("decideReuse resumes the recorded session when the head is unchanged", () => {
  assert.deepEqual(decideReuse(base), { action: "resume", sessionId: ID, reviewedSha: record.reviewedSha, reviewedAt: 1 });
});

test("decideReuse starts a review with no record", () => {
  assert.deepEqual(decideReuse({ ...base, record: null }), { action: "review" });
});

test("decideReuse reviews only the new commits when the head moved forward", () => {
  assert.deepEqual(decideReuse({ ...base, head: "b".repeat(40) }), { action: "review", since: record.reviewedSha });
});

test("decideReuse does a full review after a force-push", () => {
  assert.deepEqual(decideReuse({ ...base, head: "b".repeat(40), isAncestor: false }), { action: "review" });
});

test("decideReuse won't resume a gone transcript, another folder or a bad id", () => {
  assert.equal(decideReuse({ ...base, transcriptExists: false }).action, "review");
  assert.equal(decideReuse({ ...base, dir: "/elsewhere" }).action, "review");
  assert.equal(decideReuse({ ...base, record: { ...record, sessionId: "not-a-uuid" } }).action, "review");
});

test("buildReviewPrompt points at the new commits when `since` is given", () => {
  const sha = "abcdef1234567890".padEnd(40, "0");
  const p = buildReviewPrompt({ sourceBranch: "f", targetBranch: "main", prId: 1, since: sha });
  assert.match(p, /already reviewed at abcdef1/);
  assert.match(p, /git diff abcdef1234567890/);
  assert.ok(!p.includes("\n"));
  assert.ok(!buildReviewPrompt({ sourceBranch: "f" }).includes("already reviewed"));
});
