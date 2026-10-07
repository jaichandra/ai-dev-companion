import { run } from "./exec";

export interface CursorAgentRunOptions {
  /** Directory the agent runs in — in practice a disposable worktree. */
  cwd: string;
  prompt: string;
  /** Passed as `--model` only when set; unset means Cursor's own default. */
  model?: string;
  signal?: AbortSignal;
  /** Called with the CLI's pid as soon as it spawns (see core/exec.ts). */
  onSpawn?: (pid: number) => void;
}

export interface CursorAgentRunResult {
  /** The agent's final text response. */
  text: string;
  raw: string;
  /** The chat id, when the CLI reported one. */
  sessionId?: string;
}

/**
 * Runs `cursor-agent` headlessly in `cwd`. `--force` lets it edit and run
 * commands without asking (there is nobody to ask), so the run is confined
 * the only ways the CLI offers: `--sandbox enabled`, and the caller's use of
 * a disposable worktree that nothing pushes until a human approves its diff.
 * Unlike runClaude there are no per-tool allow-lists. `--trust` skips the
 * interactive "trust this workspace" prompt, which a new worktree always
 * triggers.
 */
export async function runCursorAgent(opts: CursorAgentRunOptions): Promise<CursorAgentRunResult> {
  const args = ["-p", "--trust", "--force", "--sandbox", "enabled", "--output-format", "json"];
  if (opts.model) args.push("--model", opts.model);
  args.push(opts.prompt);
  const result = await run("cursor-agent", args, { cwd: opts.cwd, signal: opts.signal, onSpawn: opts.onSpawn });
  return parseCursorAgentOutput(result.stdout);
}

/** `--output-format json` prints one result object; its field names have
 * varied between versions, so anything unexpected degrades to the raw text. */
export function parseCursorAgentOutput(stdout: string): CursorAgentRunResult {
  try {
    const parsed = JSON.parse(stdout);
    const text =
      typeof parsed.result === "string" ? parsed.result : typeof parsed.text === "string" ? parsed.text : stdout;
    const id = parsed.session_id ?? parsed.chatId ?? parsed.chat_id;
    return { text, raw: stdout, sessionId: typeof id === "string" ? id : undefined };
  } catch {
    return { text: stdout, raw: stdout };
  }
}
