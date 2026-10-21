// Pure logic for review-in-editor. Plain JS, not TypeScript — same reason
// as create-jira-subtasks/plan.js: lets plan.test.js run this directly
// with zero build step; tsc still picks it up (allowJs) and copies it
// into dist/ for index.ts's runtime use after a build.

const path = require("path");

/**
 * Validates the extension's /start payload: project, repo, and
 * sourceBranch (all strings) — prId is carried through if present but
 * never required, since nothing here actually needs it (it's only ever
 * used for a human-readable summary, and its absence isn't a reason to
 * refuse the request). Same relationship resolve-conflict's assertPayload
 * has to its client. targetBranch, title and prUrl are optional too (an
 * older extension doesn't send them); the review prompt copes without them.
 */
function assertPayload(body) {
  const p = body && typeof body === "object" ? body : null;
  if (!p || typeof p.project !== "string" || typeof p.repo !== "string" || typeof p.sourceBranch !== "string") {
    throw new Error("review-in-editor payload must include project, repo, sourceBranch (strings)");
  }
  return {
    project: p.project,
    repo: p.repo,
    prId: p.prId,
    sourceBranch: p.sourceBranch,
    targetBranch: typeof p.targetBranch === "string" && p.targetBranch ? p.targetBranch : undefined,
    title: typeof p.title === "string" && p.title.trim() ? p.title : undefined,
    prUrl: typeof p.prUrl === "string" && /^https?:\/\//.test(p.prUrl) ? p.prUrl : undefined,
  };
}

/**
 * The prompt the editor's AI assistant starts the review with. Always a
 * single line: it's typed into a terminal for Claude Code and Cursor Agent,
 * where a newline would submit it early. It names no particular skill: the
 * agent knows which skills and commands it has (Claude Code lists them in
 * its own context, so this costs nothing to look up), so it is told to use
 * the best review one if there is one, given the PR URL when we have it,
 * and to follow the built-in instructions otherwise. Without a target
 * branch the diff is against origin/HEAD (the remote's default branch).
 * With `since` (the sha of the last review) it starts from the new commits.
 */
function buildReviewPrompt({ sourceBranch, targetBranch, prId, title, prUrl, since }) {
  const base = targetBranch ? `origin/${targetBranch}` : "origin/HEAD";
  const pr = describePr(prId, title);
  const where = `${prUrl ? `${prUrl}; ` : ""}branch ${sourceBranch}, merging into ${targetBranch || "the default branch"}`;
  const checkedOut =
    `The branch is checked out here; run \`git diff ${base}...HEAD\` to see exactly what it changes. ` +
    `Don't modify any files.` +
    (since
      ? ` This PR was already reviewed at ${since.slice(0, 7)}: start with \`git diff ${since}..HEAD\` (the new commits) ` +
        `and read the rest of the PR only as far as you need to judge them.`
      : "");
  const text =
    `Review pull request ${pr ? pr + " " : ""}(${where}). ` +
    `${checkedOut} ` +
    `If you have a skill or command for reviewing pull requests or code, check your available skills and use the best match` +
    `${prUrl ? ", passing it the PR URL if it needs one" : ""}. ` +
    `Otherwise: look for bugs, edge cases, security problems, missing tests, and anything hard to follow. ` +
    `Report findings grouped by severity, each with a file:line reference.`;
  return stripControlChars(text).replace(/\s+/g, " ").trim();
}

/** `text` with every C0/C1 control character and DEL replaced by a space.
 * PR titles and branch names come from Bitbucket's JSON, which can carry
 * raw ESC/BEL/CR bytes; printed verbatim, those would be terminal escape
 * sequences rather than text. */
function stripControlChars(text) {
  // eslint-disable-next-line no-control-regex
  return String(text).replace(/[\u0000-\u001f\u007f-\u009f]/g, " ");
}

/** `#42 "Title"`, or whichever half is known, or "" (always one line). */
function describePr(prId, title) {
  return [
    prId !== undefined && prId !== null ? stripControlChars(`#${prId}`) : "",
    title ? `"${oneLine(title)}"` : "",
  ]
    .filter(Boolean)
    .join(" ");
}

/** `text` as a single line with no control characters or runs of spaces. */
function oneLine(text) {
  return stripControlChars(text).replace(/\s+/g, " ").trim();
}

/** `text` cut to `max` characters, ending in "…" if it was cut. */
function truncate(text, max) {
  const chars = Array.from(text);
  return chars.length <= max ? text : `${chars.slice(0, max - 1).join("")}…`;
}

const TITLE_MAX = 80;

const BADGE_MAX = 20;

/**
 * The terminal tab/window title for a terminal-based review, which stays
 * visible once the agent's full-screen UI has covered the printed header,
 * and a one-line iTerm badge for when the agent overwrites that title
 * (cursor-agent does). The source branch is what gets shortened to fit
 * TITLE_MAX, since the PR number and target are the short, telling parts.
 */
function buildTerminalTitle({ prId, sourceBranch, targetBranch }) {
  const hasId = prId !== undefined && prId !== null;
  const pr = hasId ? oneLine(`PR #${prId}`) : "PR review";
  const target = targetBranch ? oneLine(targetBranch) : "(default branch)";
  const fixed = `${pr} ·  → ${target}`;
  const source = truncate(oneLine(sourceBranch), Math.max(12, TITLE_MAX - fixed.length));
  return {
    title: truncate(`${pr} · ${source} → ${target}`, TITLE_MAX),
    badge: truncate(hasId ? pr : oneLine(sourceBranch), BADGE_MAX),
  };
}

// How each terminal-based review announces itself, and which IDE (if any)
// its header links to.
const TERMINAL_AGENTS = {
  "claude-code": { agent: "Claude Code" },
};

const EDITORS = {
  cursor: { label: "Cursor", agent: "Cursor Agent (Ask mode, read-only)", scheme: "cursor" },
  "claude-code": { label: "Claude Code", agent: TERMINAL_AGENTS["claude-code"].agent },
  vscode: { label: "VS Code", agent: "Copilot Chat (Ask mode, read-only)", scheme: "vscode" },
};

const RULE = "─".repeat(64);

/**
 * The lines printed at the top of a terminal-based review before the agent
 * starts, so the worktree and branch stay on screen above its output. An IDE
 * agent that has an `ide` also gets a link that opens the worktree there.
 *
 * Each line is a list of { text, style } segments. `style` names what the
 * text is (core/terminal.ts's HEADER_STYLES maps it to a color), so the
 * colors live in a fixed printf format and PR-supplied text like the title
 * is only ever printed as data. Every segment is single-line, for the same
 * reason as the prompt, and free of control characters (stripControlChars).
 */
function buildTerminalHeader({ editor, dir, sourceBranch, targetBranch, prId, title, prUrl }) {
  const agent = TERMINAL_AGENTS[editor];
  if (!agent) return [];
  const pr = describePr(prId, title);
  const row = (label, ...value) => [{ text: `  ${label.padEnd(16)}`, style: "label" }, ...value];
  const lines = [
    [{ text: RULE, style: "rule" }],
    [{ text: "PR review", style: "title" }, ...(pr ? [{ text: `  ${pr}`, style: "heading" }] : [])],
    [{ text: RULE, style: "rule" }],
    row(
      "Branch",
      { text: sourceBranch, style: "branch" },
      { text: " -> ", style: "muted" },
      { text: targetBranch || "(default branch)", style: "target" },
    ),
    row("Worktree", { text: dir, style: "path" }),
  ];
  if (prUrl) lines.push(row("PR", { text: prUrl, style: "link" }));
  if (agent.ide) {
    lines.push(row(`Open in ${agent.ide}`, { text: `${agent.scheme}://file${encodeURI(dir)}`, style: "link" }));
  }
  lines.push(
    [{ text: RULE, style: "rule" }],
    [{ text: `Starting ${agent.agent}...`, style: "status" }],
    [],
  );
  return lines.map((line) => line.map((seg) => ({ ...seg, text: stripControlChars(seg.text) })));
}

/** Parses `git status --porcelain` output into the list of affected file
 * paths (dropping the two-character status column + separating space) —
 * used to name what got auto-stashed before checking out the PR branch. */
function parseDirtyFiles(porcelainStdout) {
  return porcelainStdout
    .split("\n")
    .map((line) => line.slice(3).trim())
    .filter(Boolean);
}

/** The one-shot job's result.summary — the only thing the extension shows
 * the user, since this feature has no review panel of its own (the whole
 * point is that you review it in your own editor instead). */
function buildSummary({ branch, dir, editorLabel, stashedFiles, reviewNote }) {
  const stashNote =
    stashedFiles && stashedFiles.length > 0
      ? ` Your uncommitted changes (${stashedFiles.length} file${stashedFiles.length === 1 ? "" : "s"}: ` +
        `${stashedFiles.join(", ")}) were stashed first — run \`git stash pop\` in ${dir} to get them back.`
      : "";
  const review = reviewNote ? ` ${reviewNote}` : "";
  const opened = editorLabel ? ` and opened it in ${editorLabel}` : "";
  return `Checked out ${branch} in ${dir}${opened}.${review}${stashNote}`;
}

/** The one-shot job's result.review: the same outcome as buildSummary, as
 * fields for the extension's review panel to lay out. `openUrl` opens the
 * worktree in the configured editor's IDE (none for Claude Code, which has
 * no IDE); `agentLabel` is only set when a review actually started.
 * `tracked` says whether GET /status reports the review's live `session`
 * (terminal-based reviews only). */
function buildReviewResult({ payload, dir, editor, reviewStarted, reviewNote, stashedFiles, tracked }) {
  const known = EDITORS[editor] || { label: editor };
  return {
    prId: payload.prId,
    title: payload.title,
    prUrl: payload.prUrl,
    sourceBranch: payload.sourceBranch,
    targetBranch: payload.targetBranch,
    worktree: dir,
    editor,
    editorLabel: known.label,
    openUrl: known.scheme ? `${known.scheme}://file${encodeURI(dir)}` : undefined,
    agentLabel: reviewStarted ? known.agent : undefined,
    reviewStarted: !!reviewStarted,
    reviewNote,
    stashedFiles: stashedFiles || [],
    tracked: !!tracked,
  };
}

const UUID = /^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;

/** The /start body's `replaceJobId`: the live review the user confirmed
 * cancelling to review this PR instead. Only a well-formed job id. */
function parseReplaceJobId(body) {
  const id = body && typeof body === "object" ? body.replaceJobId : undefined;
  return typeof id === "string" && UUID.test(id) ? id : undefined;
}

/** Whether payloads `a` and `b` are the same pull request: same repo and
 * PR id, or the same source branch when either has no id. */
function samePr(a, b) {
  if (a.project !== b.project || a.repo !== b.repo) return false;
  const hasId = (p) => p.prId !== undefined && p.prId !== null && p.prId !== "";
  if (hasId(a) && hasId(b)) return String(a.prId) === String(b.prId);
  return a.sourceBranch === b.sourceBranch;
}

/**
 * What /start does, given the session record of the review still live in
 * the worktree (`live`, or null): "start" as usual; "reuse" the live one
 * when it's this PR; "replace" it when the request names that very review
 * as the one to cancel first; otherwise "conflict", since checking out
 * another branch would change the files under the running agent.
 */
function decideStart({ payload, live, replaceJobId }) {
  if (!live) return "start";
  if (replaceJobId && replaceJobId === live.jobId) return "replace";
  return samePr(live.payload, payload) ? "reuse" : "conflict";
}

/** "PR #42 (branch)" for the live review in `payload`, on one line. */
function describeLiveReview(payload) {
  const branch = oneLine(payload.sourceBranch || "");
  const hasId = payload.prId !== undefined && payload.prId !== null && payload.prId !== "";
  const pr = hasId ? oneLine(`PR #${payload.prId}`) : "a pull request";
  return branch ? `${pr} (${branch})` : pr;
}

/** The body of the 409 /start answers a "conflict" with: enough for the
 * panel to name the live review and offer to go to it or replace it. */
function buildConflict(live, state) {
  return {
    code: "review-in-progress",
    error:
      `A review of ${describeLiveReview(live.payload)} is in progress in ${live.worktree}. ` +
      "Checking out this pull request there would change the files under it.",
    existing: {
      jobId: live.jobId,
      state,
      worktree: live.worktree,
      payload: live.payload,
    },
  };
}

/** The result /start returns when it "reuse"s the live review: that
 * review's own, marked `switched` with how bringing its terminal to the
 * front went (`focus`: "focused", "missing", "unknown" or an error). */
function buildSwitchedResult(live, focus) {
  const notes = {
    focused: "Its terminal window is in front.",
    missing: "Its terminal window couldn't be found; it may have just been closed.",
    unknown: "Which terminal window it's in isn't known, so it wasn't brought to the front.",
  };
  const note = notes[focus] || `Couldn't bring its terminal window to the front: ${focus}`;
  return {
    summary: `Switched to the review of ${describeLiveReview(live.payload)} already running in ${live.worktree}. ${note}`,
    review: { ...live.review, switched: { focused: focus === "focused", note } },
  };
}

/** Key of a PR's remembered review: repo and PR id, or the branch when
 * there is no id. */
function prKey({ project, repo, prId, sourceBranch }) {
  const hasId = prId !== undefined && prId !== null && prId !== "";
  return hasId ? `${project}/${repo}#${prId}` : `${project}/${repo}@${sourceBranch}`;
}

/**
 * What to do about this PR's last review (`record`: {sessionId, cwd,
 * reviewedSha, reviewedAt}, or null) given the branch's `head` now:
 * "resume" that session when the head hasn't moved and its transcript is
 * still there, in the folder this review would use (`dir`); otherwise a new
 * "review", told the sha to start from when that sha is still an ancestor
 * of `head` (not after a force-push). Resuming an old session to review new
 * commits is deliberately not an option: its whole transcript would be
 * re-read, usually uncached, to review less than a fresh session would.
 */
function decideReuse({ record, head, dir, transcriptExists, isAncestor }) {
  if (!record || !/^[0-9a-f]{7,64}$/i.test(String(record.reviewedSha))) return { action: "review" };
  if (
    record.reviewedSha === head &&
    transcriptExists &&
    UUID.test(String(record.sessionId)) &&
    path.resolve(record.cwd) === path.resolve(dir)
  ) {
    return { action: "resume", sessionId: record.sessionId, reviewedSha: record.reviewedSha, reviewedAt: record.reviewedAt };
  }
  return record.reviewedSha !== head && isAncestor ? { action: "review", since: record.reviewedSha } : { action: "review" };
}

module.exports = {
  prKey,
  decideReuse,
  assertPayload,
  buildReviewPrompt,
  stripControlChars,
  buildTerminalHeader,
  buildTerminalTitle,
  parseDirtyFiles,
  buildSummary,
  buildReviewResult,
  parseReplaceJobId,
  samePr,
  decideStart,
  describeLiveReview,
  buildConflict,
  buildSwitchedResult,
};
