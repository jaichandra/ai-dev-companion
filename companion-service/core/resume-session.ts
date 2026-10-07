// Resume a recorded headless Claude session (core/jobs.ts's ClaudeSession)
// in a terminal — the one implementation behind POST
// /jobs/:jobId/open-in-claude-code {resume: true} and the ticket
// workspace's "Resume Claude session". The session is only ever
// server-held job data, re-validated here before anything is built from it.
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import type { Config } from "../config";
import type { ClaudeSession } from "./jobs";
import { HeaderSegment, openClaudeCodeInTerminal } from "./terminal";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const resume = require("./resume.js") as {
  sessionTranscriptPath(home: string, cwd: string, id: string): string;
  realCwdForTranscript(cwd: string): string;
  buildResumeArgs(args: { session: ClaudeSession }): string[];
  validateSession(session: unknown): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const sessionCwd = require("./session-cwd.js") as {
  sessionCwdAllowed(cwd: string, opts: { repoPaths: string[]; sessionsRoot: string; checkoutsRoot: string }): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("./paths.js") as { stateDir(): string };

export type ResumeOutcome = { ok: true } | { ok: false; reason: "invalid" | "gone" };

/** "invalid": not a session this service would have recorded — including
 * one whose folder isn't a mapped repo, a ticket worktree or a sessions
 * folder (a hand-edited job file or history row must not open a terminal in
 * an attacker's folder). `jobWorktreeDir`: the job's own worktree, also
 * allowed when the session ran in it. "gone": its folder or transcript no
 * longer exists (Claude Code cleans old ones up). */
export async function resumeSessionInTerminal(
  config: Config,
  session: unknown,
  header: HeaderSegment[][] = [],
  jobWorktreeDir?: string,
): Promise<ResumeOutcome> {
  if (!resume.validateSession(session)) return { ok: false, reason: "invalid" };
  const s = session as ClaudeSession;
  const allowed =
    (typeof jobWorktreeDir === "string" && path.resolve(jobWorktreeDir) === path.resolve(s.cwd)) ||
    sessionCwd.sessionCwdAllowed(s.cwd, {
      repoPaths: Object.values(config.repos || {}),
      sessionsRoot: path.join(paths.stateDir(), "sessions"),
      checkoutsRoot: path.join(paths.stateDir(), "repos"),
    });
  if (!allowed) return { ok: false, reason: "invalid" };
  // Only the transcript lookup needs the realpath'd cwd (Claude Code keys
  // ~/.claude/projects/<slug>/ by it); the session runs in the cwd as captured.
  const transcript = resume.sessionTranscriptPath(os.homedir(), resume.realCwdForTranscript(s.cwd), s.id);
  if (!fs.existsSync(s.cwd) || !fs.existsSync(transcript)) return { ok: false, reason: "gone" };
  await openClaudeCodeInTerminal(s.cwd, resume.buildResumeArgs({ session: s }), header);
  return { ok: true };
}
