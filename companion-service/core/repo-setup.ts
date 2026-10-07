import * as fs from "fs";
import * as path from "path";
import { spawn, execFile } from "child_process";
import { Config, saveRepo } from "../config";
import { Job, jobStore } from "./jobs";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const prereqs = require("./prereqs.js") as {
  checkRepoPath(repoPath: string): { ok: boolean; message: string };
  getOriginUrl(repoPath: string): string | null;
  originMatchesProjectRepo(originUrl: string | null, project: string, repo: string): boolean;
  isSafeRepoSegment(value: unknown): boolean;
  deriveCloneUrl(args: {
    existingOrigins: string[];
    pageOrigin?: string;
    project: string;
    repo: string;
  }): string | null;
  cloneRoot(existingRepos: Record<string, string>): string;
};

/** A user-facing problem with the request (bad folder, cancelled picker,
 * unsupported platform) — server.ts maps it to a 400 rather than a 500. */
export class RepoSetupError extends Error {
  constructor(
    message: string,
    public readonly code?: string,
  ) {
    super(message);
  }
}

/** Lower-cased "PROJECT/repo" -> id of its running clone job. */
const clonesInProgress = new Map<string, string>();

export function assertRepoRequest(body: unknown): { project: string; repo: string; key: string } {
  const b = body as { project?: unknown; repo?: unknown } | null;
  if (!b || !prereqs.isSafeRepoSegment(b.project) || !prereqs.isSafeRepoSegment(b.repo)) {
    throw new RepoSetupError("Expected a body with a valid project and repo.");
  }
  const project = b.project as string;
  const repo = b.repo as string;
  return { project, repo, key: `${project}/${repo}` };
}

function escapeAppleScript(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

/** Native macOS folder picker, brought to the front (the service runs in the
 * background, so without `activate` the dialog can open behind Chrome).
 * Resolves null if the user cancels. */
function pickFolder(prompt: string, defaultDir: string): Promise<string | null> {
  const location = fs.existsSync(defaultDir)
    ? ` default location (POSIX file "${escapeAppleScript(defaultDir)}")`
    : "";
  const script =
    "tell current application\n" +
    "  activate\n" +
    `  POSIX path of (choose folder with prompt "${escapeAppleScript(prompt)}"${location})\n` +
    "end tell";
  return new Promise((resolve) => {
    execFile("osascript", ["-e", script], { encoding: "utf8" }, (err, stdout) => {
      resolve(err ? null : stdout.trim().replace(/\/$/, ""));
    });
  });
}

/** Lets the user point at an existing clone, verifies it really is
 * project/repo, and saves the mapping. Returns the saved path. */
export async function chooseRepoFolder(config: Config, body: unknown): Promise<string> {
  const { project, repo, key } = assertRepoRequest(body);
  if (process.platform !== "darwin") {
    throw new RepoSetupError(
      `Picking a folder from the browser needs macOS. Add "${key}": "<path to its clone>" to repos in ` +
        "config.json instead, or let the assistant clone it.",
      "unsupported",
    );
  }
  const picked = await pickFolder(`Select your local clone of ${key}`, prereqs.cloneRoot(config.repos));
  if (!picked) throw new RepoSetupError("No folder was selected.", "cancelled");

  const check = prereqs.checkRepoPath(picked);
  if (!check.ok) throw new RepoSetupError(check.message);
  const origin = prereqs.getOriginUrl(picked);
  if (!prereqs.originMatchesProjectRepo(origin, project, repo)) {
    throw new RepoSetupError(`${picked} isn't a clone of ${key} — its origin is ${origin}.`);
  }
  saveRepo(config, key, picked);
  console.log(`[repos] ${key} -> ${picked} (chosen in the browser)`);
  return picked;
}

/**
 * Starts cloning project/repo next to the user's other clones and returns
 * the job to poll. The clone URL is derived server-side — never taken from
 * the request — and git is told never to prompt, since there's no terminal
 * to answer on; a credentials problem fails with instructions instead of
 * hanging.
 */
export function startClone(config: Config, body: unknown, pageOrigin: string | undefined): Job {
  const { project, repo, key } = assertRepoRequest(body);
  // A half-written clone already has a matching origin, so a second request
  // mid-clone must join the running job, not "reuse" the folder below.
  const inProgress = clonesInProgress.get(key.toLowerCase());
  if (inProgress) return jobStore.get(inProgress) ?? jobStore.create("clone-repo", { key });
  const existingOrigins = Object.values(config.repos)
    .map((p) => prereqs.getOriginUrl(p))
    .filter((o): o is string => !!o);
  const url = prereqs.deriveCloneUrl({ existingOrigins, pageOrigin, project, repo });
  if (!url) {
    throw new RepoSetupError(
      `Couldn't work out a clone URL for ${key} — reload the Bitbucket page and try again.`,
    );
  }
  const target = path.join(prereqs.cloneRoot(config.repos), repo);
  if (fs.existsSync(target)) {
    if (prereqs.originMatchesProjectRepo(prereqs.getOriginUrl(target), project, repo)) {
      saveRepo(config, key, target);
      const job = jobStore.create("clone-repo", { key, target });
      return jobStore.update(job.id, {
        status: "approved",
        result: { summary: `Using ${target}.`, files: [] },
      });
    }
    throw new RepoSetupError(
      `${target} already exists and isn't a clone of ${key}. Choose its folder instead.`,
    );
  }

  const job = jobStore.create("clone-repo", { key, target, url });
  jobStore.update(job.id, { progress: { stepId: "clone", label: `Cloning ${key} into ${target}…` } });
  clonesInProgress.set(key.toLowerCase(), job.id);
  fs.mkdirSync(path.dirname(target), { recursive: true });

  const child = spawn("git", ["clone", "--progress", url, target], {
    shell: false,
    env: {
      ...process.env,
      GIT_TERMINAL_PROMPT: "0",
      GIT_SSH_COMMAND: process.env.GIT_SSH_COMMAND || "ssh -o BatchMode=yes",
    },
  });
  let stderrTail = "";
  child.stderr.on("data", (chunk: Buffer) => {
    const text = chunk.toString();
    stderrTail = (stderrTail + text).slice(-4000);
    const latest = text
      .split(/[\r\n]/)
      .filter((l) => /\d+%/.test(l))
      .pop();
    if (latest) {
      jobStore.update(job.id, { progress: { stepId: "clone", label: `Cloning ${key}: ${latest.trim()}` } });
    }
  });
  child.on("error", (err) => {
    clonesInProgress.delete(key.toLowerCase());
    jobStore.update(job.id, { status: "failed", error: `Couldn't run git: ${err.message}` });
  });
  child.on("close", (code) => {
    clonesInProgress.delete(key.toLowerCase());
    if (code === 0) {
      saveRepo(config, key, target);
      console.log(`[repos] ${key} -> ${target} (cloned from ${url})`);
      jobStore.update(job.id, {
        status: "approved",
        result: { summary: `Cloned ${key} into ${target}.`, files: [] },
      });
      return;
    }
    fs.rmSync(target, { recursive: true, force: true });
    const detail = stderrTail
      .split(/[\r\n]/)
      .filter((l) => l.trim() && !/\d+%/.test(l))
      .slice(-5)
      .join("\n");
    jobStore.update(job.id, {
      status: "failed",
      error:
        `Cloning ${key} failed:\n${detail}\n\nIf git needs a password or passphrase, clone it once in a ` +
        `terminal instead — it will be found automatically afterwards:\n  git clone ${url} ${target}`,
    });
  });
  return job;
}
