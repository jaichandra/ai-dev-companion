import * as fs from "fs";
import { run } from "./exec";
import { HeaderSegment, openClaudeCodeInTerminal, TerminalLocation, TerminalOptions } from "./terminal";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const prereqs = require("./prereqs.js") as {
  detectInstalledEditors(): Array<{ id: EditorId; label: string }>;
  resolveReviewEditor(configured: string | undefined): EditorId | undefined;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const { permissionModeFor } = require("./permission-mode.js") as { permissionModeFor(featureId: string): "plan" | "auto" | "default" };
const { buildCursorPromptUrl } = require("./cursor-deeplink.js") as {
  buildCursorPromptUrl(dir: string, prompt: string): string;
};

export type EditorId = "vscode" | "cursor" | "claude-code";

export interface EditorOption {
  id: EditorId;
  label: string;
}

const APP_NAMES: Record<"vscode" | "cursor", string> = {
  vscode: "Visual Studio Code",
  cursor: "Cursor",
};

/** Which editors are actually installed on this machine, in preference
 * order — delegates to core/prereqs.js (the plain-JS home for "is X
 * installed" checks) so setup.js's promptSetup and this runtime fallback
 * share one implementation instead of two. */
export function detectInstalledEditors(): EditorOption[] {
  return prereqs.detectInstalledEditors();
}

/** The editor to use: the configured one if supported, else the first
 * installed (core/prereqs.js). */
export function resolveReviewEditor(configured: string | undefined): EditorId | undefined {
  return prereqs.resolveReviewEditor(configured);
}

/**
 * Opens `dir` in the given editor. "claude-code" reuses the existing
 * openClaudeCodeInTerminal (a new terminal window running `claude`, since
 * there's no URL scheme for that); "vscode"/"cursor" shell out to `open
 * -a` rather than the cursor://file… URL scheme
 * chrome-extension/open-in-editor.js uses client-side — no scheme
 * registration needed, and it covers VS Code, which that scheme-based
 * approach never did. macOS only, same precedent (and same style of
 * error) as openClaudeCodeInTerminal.
 */
export async function openInEditor(dir: string, editor: EditorId): Promise<void> {
  if (editor === "claude-code") {
    await openClaudeCodeInTerminal(dir);
    return;
  }
  if (process.platform !== "darwin") {
    throw new Error(`Opening ${APP_NAMES[editor]} is only implemented for macOS.`);
  }
  await run("open", ["-a", APP_NAMES[editor], dir]);
}

const VSCODE_BUNDLED_CLI = "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code";

export interface ReviewLaunch {
  /** Whether the editor's IDE window was opened. */
  openedEditor: boolean;
  reviewStarted: boolean;
  reviewNote: string;
  /** The terminal a terminal-based review opened in, if it said. */
  terminal?: TerminalLocation;
}

/**
 * Starts an AI review of `dir` on `prompt` (Claude Code uses the profile's permission mode, auto by default).
 * `header` is
 * printed at the top of the terminal for the terminal-based reviews, and
 * `options` titles its terminal tab and names its status file.
 *
 * - Claude Code runs in a new terminal window.
 * - Cursor opens the folder, then a prompt deeplink starts the review in
 *   its Agent window (see openCursorReview).
 * - VS Code opens the folder, then starts the review in its Copilot Chat.
 *
 * Throws only if nothing at all could be opened; a review that fails to
 * start is reported in `reviewNote`.
 */
export async function openForReview(
  dir: string,
  editor: EditorId,
  prompt: string,
  header: HeaderSegment[][] = [],
  options: TerminalOptions = {},
): Promise<ReviewLaunch> {
  if (editor === "claude-code") {
    const terminal = await openClaudeCodeInTerminal(dir, ["--permission-mode", permissionModeFor("review-in-editor"), prompt], header, options);
    return {
      openedEditor: false,
      reviewStarted: true,
      reviewNote: "Claude Code is reviewing it in a new terminal window.",
      terminal,
    };
  }

  if (editor === "cursor") {
    try {
      return await openCursorReview(dir, prompt);
    } catch (err) {
      return { openedEditor: true, reviewStarted: false, reviewNote: reviewFailureNote(err, "cursor") };
    }
  }

  await openInEditor(dir, editor);
  try {
    const cli = fs.existsSync(VSCODE_BUNDLED_CLI) ? VSCODE_BUNDLED_CLI : "code";
    // `code chat` targets the window for its working directory.
    await run(cli, ["chat", "--mode", "ask", prompt], { cwd: dir });
    return {
      openedEditor: true,
      reviewStarted: true,
      reviewNote: "Copilot Chat is reviewing it in Ask mode (read-only).",
    };
  } catch (err) {
    return { openedEditor: true, reviewStarted: false, reviewNote: reviewFailureNote(err, editor) };
  }
}

/** How long Cursor gets to open the worktree's window before the deeplink
 * is sent, so the link's `workspace` match finds that window instead of
 * landing in whichever one was frontmost. */
const CURSOR_WINDOW_SETTLE_MS = 4000;

/**
 * Opens `dir` in Cursor, then sends its prompt deeplink so the review is a
 * new Ask-mode chat in the IDE's Agent window. Cursor always shows a
 * "Create chat with prompt" confirmation, so the review starts when the user
 * clicks Create Chat; the chat can't be tracked from here (no status file).
 * Cursor rejects prompts over ~10k encoded characters, so longer ones throw
 * instead of being silently refused inside the IDE.
 */
async function openCursorReview(dir: string, prompt: string): Promise<ReviewLaunch> {
  if (process.platform !== "darwin") {
    throw new Error("Opening Cursor is only implemented for macOS.");
  }
  const url = buildCursorPromptUrl(dir, prompt);
  await openInEditor(dir, "cursor");
  await new Promise((resolve) => setTimeout(resolve, CURSOR_WINDOW_SETTLE_MS));
  await run("open", [url]);
  return {
    openedEditor: true,
    reviewStarted: true,
    reviewNote:
      "Cursor opened a new Ask-mode (read-only) chat with the review prompt in its Agent window. " +
      "Click Create Chat in Cursor to start it.",
  };
}

function reviewFailureNote(err: unknown, editor: EditorId): string {
  const missing = (err as NodeJS.ErrnoException).code === "ENOENT";
  const reason = missing
    ? editor === "cursor"
      ? "Cursor isn't installed"
      : "the `code` command isn't installed"
    : (err as Error).message;
  return `Couldn't start the automatic review: ${reason}.`;
}
