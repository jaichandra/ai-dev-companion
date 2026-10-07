import { spawn } from "child_process";
import { StringDecoder } from "string_decoder";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const lineSplitter = require("./line-splitter.js") as {
  createLineSplitter(onLine: (line: string) => void): { write(chunk: string): void; flush(): void };
};

export interface ExecResult {
  stdout: string;
  stderr: string;
  code: number;
}

export class ExecError extends Error {
  constructor(
    public readonly command: string,
    public readonly args: string[],
    public readonly result: ExecResult,
  ) {
    super(
      `${command} ${args.join(" ")} exited with ${result.code}: ${result.stderr.trim() || result.stdout.trim()}`,
    );
  }
}

/** Process-group ids (= pids, since each is its group's leader) of every
 * detached child still running — see run's `signal` option. Tracked so a
 * shutdown can take them down with it (killDetachedChildren): a detached
 * child is in its own group, so a signal to this process alone never
 * reaches it, and a headless claude that outlived the service would keep
 * writing a worktree the restarted service then treats as settled. */
const detachedChildren = new Set<number>();

/** Sends `signal` to every tracked detached child's whole process group
 * (claude plus whatever its Bash tool started). ESRCH — the group already
 * exited between its exit event and now — is ignored; so is anything else,
 * since this runs on the way out and must not throw there. */
export function killDetachedChildren(signal: NodeJS.Signals = "SIGTERM"): void {
  for (const pgid of detachedChildren) {
    try {
      process.kill(-pgid, signal);
    } catch {
      // ESRCH (already gone) or similar — nothing left to signal.
    }
  }
}

/** How many detached children haven't exited yet — server.ts's shutdown
 * handler waits (bounded) for this to reach 0 after killDetachedChildren. */
export function detachedChildCount(): number {
  return detachedChildren.size;
}

/**
 * Run a command with an argv array (never a shell string) so branch names,
 * PR ids, or anything else that ultimately traces back to the extension's
 * request body can never be interpreted as shell syntax.
 */
export function run(
  command: string,
  args: string[],
  opts: {
    cwd?: string;
    input?: string;
    allowFailure?: boolean;
    signal?: AbortSignal;
    /** Called with the child's pid right after it spawns — for a detached
     * child (one given a `signal`), that's also its process-group id. The
     * worktree features persist it as `data.claudePid`, so after a crash
     * or `kill -9` (when killDetachedChildren never got to run) the
     * restarted service can tell whether that claude is still alive (see
     * core/process-group.js). */
    onSpawn?: (pid: number) => void;
    /** Called with each complete line of stdout as it arrives (the whole
     * output is still buffered and returned). Used for streamed progress. */
    onStdoutLine?: (line: string) => void;
  } = {},
): Promise<ExecResult> {
  return new Promise((resolve, reject) => {
    // With a signal, the child gets its own process group so aborting can
    // SIGTERM everything it started too (claude's Bash tool runs its own
    // subprocesses), and the promise rejects even with allowFailure, since
    // a killed command's result is meaningless.
    const { signal } = opts;
    const child = spawn(command, args, { cwd: opts.cwd, shell: false, detached: !!signal });
    const pid = child.pid;
    if (signal && pid) {
      detachedChildren.add(pid);
      child.once("exit", () => detachedChildren.delete(pid));
    }
    if (pid) opts.onSpawn?.(pid);
    const onAbort = () => {
      try {
        if (child.pid) process.kill(-child.pid, "SIGTERM");
      } catch {
        child.kill("SIGTERM");
      }
    };
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
    let stdout = "";
    let stderr = "";
    const splitter = opts.onStdoutLine ? lineSplitter.createLineSplitter(opts.onStdoutLine) : undefined;
    const decoder = splitter ? new StringDecoder("utf8") : undefined;
    child.stdout.on("data", (d) => {
      stdout += d.toString();
      if (splitter && decoder) splitter.write(decoder.write(d));
    });
    child.stderr.on("data", (d) => (stderr += d.toString()));
    if (opts.input !== undefined) {
      child.stdin.write(opts.input);
    }
    child.stdin.end();
    child.on("error", reject);
    child.on("close", (code) => {
      signal?.removeEventListener("abort", onAbort);
      if (signal?.aborted) {
        reject(signal.reason instanceof Error ? signal.reason : new Error("aborted"));
        return;
      }
      if (splitter && decoder) {
        splitter.write(decoder.end());
        splitter.flush();
      }
      const result: ExecResult = { stdout, stderr, code: code ?? -1 };
      if (result.code !== 0 && !opts.allowFailure) {
        reject(new ExecError(command, args, result));
        return;
      }
      resolve(result);
    });
  });
}

export function git(
  args: string[],
  cwd: string,
  opts: { allowFailure?: boolean; signal?: AbortSignal } = {},
): Promise<ExecResult> {
  return run("git", args, { cwd, ...opts });
}
