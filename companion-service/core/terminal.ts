import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { spawn } from "child_process";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const terminalFocus = require("./terminal-focus.js") as {
  parseTerminalLocation(text: string): TerminalLocation | null;
};

/** Where a review's terminal is (core/terminal-focus.js). */
export type TerminalLocation =
  | { app: "iterm"; windowId: string; sessionId: string }
  | { app: "terminal"; windowId: string; tty: string };

/**
 * Launch a new terminal window/tab, cd'd into `cwd`, running `claude`.
 *
 * There's no URL scheme for this (unlike editors — see
 * chrome-extension/open-in-editor.js) since "open a terminal and run a
 * command" isn't something the OS lets a browser link do directly. This
 * goes through the companion service instead, which does have OS access.
 *
 * macOS only (osascript). Prefers iTerm if installed (confirmed present on
 * this machine, and this user's own Claude Code settings already have
 * iTerm-specific status-line hooks), falling back to Terminal.app.
 */
export function openClaudeCodeInTerminal(
  cwd: string,
  args: string[] = [],
  header: HeaderSegment[][] = [],
  options: TerminalOptions = {},
): Promise<TerminalLocation | undefined> {
  if (process.platform !== "darwin") {
    return Promise.reject(new Error('"Open in Claude Code" is only implemented for macOS.'));
  }
  // Claude Code otherwise retitles the tab with its own task summary. With
  // that off the title stays put, so no badge is needed.
  const env = options.title ? { CLAUDE_CODE_DISABLE_TERMINAL_TITLE: "1" } : undefined;
  const badge = options.title ? undefined : options.badge;
  return runInTerminal(cwd, ["claude", ...args], header, { ...options, badge, env });
}

/** POSIX single-quoting: the result is always exactly one shell word, so
 * arbitrary text (e.g. a PR title inside a prompt) can't inject commands. */
export function shellQuote(arg: string): string {
  return `'${arg.replace(/'/g, `'\\''`)}'`;
}

export type HeaderStyle =
  | "title"
  | "heading"
  | "rule"
  | "label"
  | "muted"
  | "branch"
  | "target"
  | "path"
  | "link"
  | "status";

export interface HeaderSegment {
  text: string;
  style?: HeaderStyle;
}

export interface TerminalOptions {
  /** Tab/window title. */
  title?: string;
  /** iTerm's badge: large text drawn behind the session's own, so it stays
   * visible under a full-screen program that overwrites the title. Keep it
   * one short line: iTerm wraps a long one over the session's output. */
  badge?: string;
  /** Extra environment variables for the command only. */
  env?: Record<string, string>;
  /** A core/review-session.js status file for the script to record the
   * shell, the command's process and its exit code in. */
  statusFile?: string;
}

/** Run by `sh -c` with the status file as $0 and the command as "$@": the
 * process records itself, then runs the command as its child and exits
 * with its status. Not exec: cancelling signals only this process's
 * descendants, so it exits normally and the rest of the sourced script
 * still runs (a foreground job killed by SIGINT would abort it). */
const RECORD_AND_RUN = `printf 'agent %s %s\\n' "$$" "$(LC_ALL=C ps -o lstart= -p $$)" >> "$0" && "$@"`;

// eslint-disable-next-line no-control-regex
const CONTROL_CHARS = /[\u0000-\u001f\u007f-\u009f]/g;

/** SGR codes per style. The basic 16 colors only, so the header follows
 * the user's terminal theme instead of fighting it. */
const HEADER_STYLES: Record<HeaderStyle, string> = {
  title: "1;36",
  heading: "1",
  rule: "36",
  label: "90",
  muted: "90",
  branch: "1;32",
  target: "1;33",
  path: "1",
  link: "4;36",
  status: "1;35",
};

/** The shell command runInTerminal runs: `header` is printed on a cleared
 * screen, so it replaces the typed command at the top of the window,
 * then `argv` runs in `cwd`. The color codes are only ever in printf's
 * format string; segment text is passed as quoted arguments, so it's
 * printed verbatim and can't inject escape sequences or format directives.
 * The title and badge escapes follow the same rule; the badge is base64 as
 * iTerm requires, with backslashes doubled so iTerm's badge format doesn't
 * interpolate `\(...)` in it. */
export function buildTerminalCommand(
  cwd: string,
  argv: string[],
  header: HeaderSegment[][] = [],
  options: TerminalOptions = {},
): string {
  const parts = [`cd ${shellQuote(cwd)}`];
  if (options.title) {
    parts.push(`printf '\\033]0;%s\\007' ${shellQuote(options.title.replace(CONTROL_CHARS, " "))}`);
  }
  if (options.badge) {
    const badge = Buffer.from(options.badge.replace(CONTROL_CHARS, " ").replace(/\\/g, "\\\\")).toString("base64");
    parts.push(`printf '\\033]1337;SetBadgeFormat=%s\\007' ${shellQuote(badge)}`);
  }
  if (header.length > 0) {
    let format = "";
    const args: string[] = [];
    for (const line of header) {
      for (const seg of line) {
        format += seg.style ? `\\033[${HEADER_STYLES[seg.style]}m%s\\033[0m` : "%s";
        args.push(seg.text);
      }
      format += "\\n";
    }
    parts.push("clear", `printf '${format}' ${args.map(shellQuote).join(" ")}`.trimEnd());
  }
  const env = Object.entries(options.env || {}).map(([name, value]) => {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) throw new Error(`Invalid environment variable name: ${name}`);
    return `${name}=${shellQuote(value)}`;
  });
  parts.push([...env, ...argv.map(shellQuote)].join(" "));
  return parts.join(" && ");
}

/** POSIX sh script (sourced, so `$0` isn't its path) that deletes itself
 * and its private dir before running buildTerminalCommand's command. */
export function buildTerminalScript(
  scriptPath: string,
  cwd: string,
  argv: string[],
  header: HeaderSegment[][] = [],
  options: TerminalOptions = {},
): string {
  const cleanup = `rm -f ${shellQuote(scriptPath)}; rmdir ${shellQuote(path.dirname(scriptPath))}`;
  if (!options.statusFile) return `${cleanup}\n${buildTerminalCommand(cwd, argv, header, options)}\n`;
  const status = shellQuote(options.statusFile);
  // $$ in a sourced script is the user's shell. After a cancel, the killed
  // full-screen agent may have left the terminal in raw mode and on its
  // alternate screen, so both are reset before the prompt comes back.
  const tracked = ["sh", "-c", RECORD_AND_RUN, options.statusFile, ...argv];
  return [
    cleanup,
    `printf 'shell %s %s\\n' "$$" "$(LC_ALL=C ps -o lstart= -p $$)" >> ${status}`,
    buildTerminalCommand(cwd, tracked, header, options),
    `printf 'exit %s\\n' "$?" >> ${status}`,
    `if grep -qx cancel ${status}; then stty sane 2>/dev/null; ` +
      `printf '\\033[?1049l\\033[?25h\\033[1;35m%s\\033[0m\\n' 'Review cancelled.'; fi`,
    "",
  ].join("\n");
}

/** Writes buildTerminalScript to a fresh 0700 temp dir; returns the script path. */
export function writeTerminalScript(
  cwd: string,
  argv: string[],
  header: HeaderSegment[][] = [],
  options: TerminalOptions = {},
): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pr-review-"));
  const scriptPath = path.join(dir, "review.sh");
  fs.writeFileSync(scriptPath, buildTerminalScript(scriptPath, cwd, argv, header, options), { mode: 0o600 });
  return scriptPath;
}

/** AppleScript string literal — escapes backslashes and double quotes so
 * `text` can never break out of the quotes it's placed in. */
function appleScriptString(text: string): string {
  return `"${text.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/**
 * The osascript that opens a new window and types `command` into it. The
 * title is also set here, on the tab itself (iTerm's session name,
 * Terminal's custom title), because cursor-agent overwrites any title set
 * by escape sequence. It's set after the command is typed and in a `try`,
 * so a terminal that refuses it still runs the review. It prints where the
 * command runs, for core/terminal-focus.js (empty if that can't be read).
 */
export function buildTerminalAppleScript(useITerm: boolean, command: string, title?: string): string {
  const cmd = appleScriptString(command);
  const name = title ? appleScriptString(title.replace(CONTROL_CHARS, " ")) : null;
  return useITerm
    ? `
      tell application "iTerm"
        activate
        set newWindow to (create window with default profile)
        tell current session of newWindow
          write text ${cmd}
          ${name ? `try\n            set name to ${name}\n          end try` : ""}
        end tell
        set loc to ""
        try
          set loc to "iterm " & (id of newWindow) & " " & (id of current session of newWindow)
        end try
        return loc
      end tell
    `
    : `
      tell application "Terminal"
        activate
        set newTab to do script ${cmd}
        ${name ? `try\n          set custom title of newTab to ${name}\n          set title displays custom title of newTab to true\n        end try` : ""}
        set loc to ""
        try
          set loc to "terminal " & (id of front window) & " " & (tty of newTab)
        end try
        return loc
      end tell
    `;
}

/** Same as openClaudeCodeInTerminal, but for any argv. Resolves where it
 * opened, if the terminal said. */
export function runInTerminal(
  cwd: string,
  argv: string[],
  header: HeaderSegment[][] = [],
  options: TerminalOptions = {},
): Promise<TerminalLocation | undefined> {
  if (process.platform !== "darwin") {
    return Promise.reject(new Error("Opening a terminal is only implemented for macOS."));
  }

  const useITerm = fs.existsSync("/Applications/iTerm.app");
  // Typed input is truncated at the tty's ~1024-byte line limit (MAX_CANON),
  // and a review prompt plus header is longer than that, so only a short
  // line is typed. Sourced, not executed, so the agent runs in the user's
  // interactive shell (their env/PATH) and it stays in `cwd` afterwards.
  const scriptPath = writeTerminalScript(cwd, argv, header, useITerm ? options : { ...options, badge: undefined });
  const script = buildTerminalAppleScript(useITerm, `. ${shellQuote(scriptPath)}`, options.title);

  return new Promise((resolve, reject) => {
    const child = spawn("osascript", ["-e", script]);
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    const removeScript = () => fs.rmSync(path.dirname(scriptPath), { recursive: true, force: true });
    child.on("error", (err) => {
      removeScript();
      reject(err);
    });
    child.on("close", (code) => {
      if (code !== 0) {
        removeScript();
        reject(new Error(`osascript exited with ${code}: ${stderr.trim()}`));
        return;
      }
      resolve(terminalFocus.parseTerminalLocation(stdout) || undefined);
    });
  });
}
