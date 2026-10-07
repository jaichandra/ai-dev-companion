// Pure logic for resuming a headless `claude` run (core/jobs.ts's
// ClaudeSession) in a terminal via `claude --resume`. Split out of
// server.ts's resume route for the same reason as core/paths.js and
// features/*/plan.js: plain JS, zero build step, unit-testable with
// node:test straight from source.
const fs = require("fs");
const path = require("path");

/** Claude Code's own transcript-folder naming: the absolute cwd with
 * every non-alphanumeric character replaced by "-" (verified example in
 * global-constraints.md). */
function claudeProjectSlug(cwd) {
  return cwd.replace(/[^a-zA-Z0-9]/g, "-");
}

/** Where `claude --resume <id>` expects to find this session's
 * transcript. Used to tell a genuinely resumable session (its transcript
 * still on disk) from one that's gone — e.g. a cached analyze-issue
 * result whose ~/.claude data was since cleared. */
function sessionTranscriptPath(home, cwd, id) {
  return path.join(home, ".claude", "projects", claudeProjectSlug(cwd), `${id}.jsonl`);
}

/** `cwd`, resolved to its real path — for the transcript lookup ONLY.
 * Claude Code keys `~/.claude/projects/<slug>/` by the *realpath'd* cwd it
 * was actually run in (symlinks resolved, e.g. macOS's /tmp -> /private/tmp),
 * so a claudeSession.cwd that reached here through a symlink would
 * otherwise slug to a folder Claude Code never created, reporting a live
 * session as expired. The session's STORED cwd (job.data.claudeSession.cwd,
 * and what --resume itself is actually run in) stays exactly as captured —
 * only this lookup needs the resolved form. Falls back to the unresolved
 * `cwd` if realpath itself fails (e.g. the directory is already gone);
 * sessionTranscriptPath then just won't find a file, which the caller
 * already treats as "not resumable" either way. `realpathImpl` is
 * injectable so resume.test.js can assert this without touching the real
 * filesystem. */
function realCwdForTranscript(cwd, realpathImpl = fs.realpathSync) {
  try {
    return realpathImpl(cwd);
  } catch {
    return cwd;
  }
}

/** argv for `claude --resume`, kept in the same permission mode the
 * original headless run used (core/jobs.ts's ClaudeSession.permissionMode). */
function buildResumeArgs({ session }) {
  return ["--resume", session.id, "--permission-mode", session.permissionMode];
}

// Same pattern features/review-in-editor/plan.js uses to validate a
// session id read back off disk.
const UUID = /^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;
const PERMISSION_MODES = ["plan", "auto", "default"];

/**
 * Whether `session` (job.data.claudeSession — for a cached analyze-issue
 * result, read back from a JSON file on disk, see
 * features/analyze-issue/plan.js's readAnalysisCache) is safe to resume.
 * This is the only gate between that value and a shell command
 * server.ts hands to a terminal, so every field is checked before
 * anything about the session is trusted: `id` must be a UUID (not
 * arbitrary text passed to `claude --resume`), `cwd` must be an absolute
 * path, and `permissionMode` must be one of the modes a headless run
 * ever actually stamps onto a job.
 */
function validateSession(session) {
  return (
    !!session &&
    typeof session.id === "string" &&
    UUID.test(session.id) &&
    typeof session.cwd === "string" &&
    path.isAbsolute(session.cwd) &&
    PERMISSION_MODES.includes(session.permissionMode)
  );
}

module.exports = {
  claudeProjectSlug,
  sessionTranscriptPath,
  realCwdForTranscript,
  buildResumeArgs,
  validateSession,
};
