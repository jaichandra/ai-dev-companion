// Opens the file named by a selected stack-trace line in Cursor or VS Code.
// The text is untrusted (it comes from a web page): it is only ever parsed
// into candidates, and a candidate opens only if `git ls-files` lists it in
// a configured repo — so nothing outside those repos can be opened, and no
// shell is involved (the URL goes to `open` as one argv word).
import * as fs from "fs";
import * as path from "path";
import type { Config } from "../config";
import { detectInstalledEditors } from "./editor";
import { git, run } from "./exec";

/* eslint-disable @typescript-eslint/no-var-requires */
const locationParse = require("./location-parse.js") as {
  MAX_TEXT: number;
  parseLocations(text: string): { path: string; line: number; column: number | null }[];
};
type Resolved =
  | { ok: true; repoKey: string; root: string; rel: string; abs: string; line: number; column: number | null }
  | { ok: false; reason: "not-found" | "ambiguous"; matches?: { repoKey: string; rel: string }[] };
const locationResolve = require("./location-resolve.js") as {
  resolveLocation(
    candidate: { path: string; line: number; column: number | null },
    repos: { key: string; root: string; files: string[] }[],
    hints?: { repoKey?: string },
  ): Resolved;
  repoKeyFromPageUrl(pageUrl: unknown, repoKeys: string[]): string | undefined;
};
const editorUrl = require("./editor-url.js") as {
  pickEditor(opts: { configured: string | undefined; installed: string[] }): "cursor" | "vscode" | null;
  editorUrl(editor: string, absPath: string, line: number, column: number | null): string;
};
/* eslint-enable @typescript-eslint/no-var-requires */

export type OpenLocationErrorCode = "no-location" | "not-found" | "ambiguous" | "no-editor" | "unsupported" | "git-failed";

export class OpenLocationError extends Error {
  constructor(
    readonly code: OpenLocationErrorCode,
    message: string,
    readonly details?: unknown,
  ) {
    super(message);
    this.name = "OpenLocationError";
  }
}

export interface OpenedLocation {
  repo: string;
  path: string;
  line: number;
  column: number | null;
  editor: string;
}

const FILE_LIST_TTL_MS = 30_000;
const MAX_FILES = 500_000;
const MAX_CANDIDATES_TRIED = 5;
const fileLists = new Map<string, { at: number; files: string[] }>();

/** `git ls-files` for a repo, cached briefly (a click burst re-uses it). */
async function trackedFiles(root: string): Promise<string[]> {
  const hit = fileLists.get(root);
  if (hit && Date.now() - hit.at < FILE_LIST_TTL_MS) return hit.files;
  const out = await git(["ls-files", "-z"], root, { allowFailure: true });
  if (out.code !== 0) {
    // Not cached: the user should see the real failure, and a retry should try again.
    throw new OpenLocationError(
      "git-failed",
      `Couldn't list the tracked files of ${root} (git ls-files failed${out.stderr ? `: ${String(out.stderr).trim().slice(0, 200)}` : ""}).`,
    );
  }
  const files = out.stdout.split("\0").filter(Boolean).slice(0, MAX_FILES);
  fileLists.set(root, { at: Date.now(), files });
  return files;
}

/** True when the file, after following symlinks, is still inside the repo root (both really exist). */
function staysInsideRepo(root: string, abs: string): boolean {
  try {
    const realRoot = fs.realpathSync(root);
    const realAbs = fs.realpathSync(abs);
    return realAbs.startsWith(realRoot.endsWith(path.sep) ? realRoot : realRoot + path.sep);
  } catch {
    return false;
  }
}

async function configuredRepos(config: Config): Promise<{ key: string; root: string; files: string[] }[]> {
  const repos: { key: string; root: string; files: string[] }[] = [];
  for (const [key, dir] of Object.entries(config.repos || {})) {
    const root = path.resolve(dir);
    if (!fs.existsSync(path.join(root, ".git"))) continue;
    repos.push({ key, root, files: await trackedFiles(root) });
  }
  return repos;
}

export interface OpenLocationDeps {
  /** Hands the URL to the editor. Default: macOS `open <url>`. */
  open?: (url: string) => Promise<void>;
  /** Editor ids that are installed. Default: detectInstalledEditors(). */
  installedEditors?: () => string[];
}

export async function openLocation(
  config: Config,
  input: { text?: unknown; pageUrl?: unknown },
  deps: OpenLocationDeps = {},
): Promise<OpenedLocation> {
  if (process.platform !== "darwin" && !deps.open) {
    throw new OpenLocationError("unsupported", "Open in editor only works on macOS for now.");
  }
  const text = typeof input.text === "string" ? input.text.slice(0, locationParse.MAX_TEXT) : "";
  const candidates = locationParse.parseLocations(text);
  if (candidates.length === 0) {
    throw new OpenLocationError("no-location", "No file:line location was found in the selected text.");
  }

  const installed = (deps.installedEditors ?? (() => detectInstalledEditors().map((e) => e.id)))();
  const editor = editorUrl.pickEditor({ configured: config.reviewEditor, installed });
  if (!editor) {
    throw new OpenLocationError(
      "no-editor",
      "Cursor is needed to open a file at a line, and it was not found in /Applications.",
    );
  }

  const repos = await configuredRepos(config);
  if (repos.length === 0) {
    throw new OpenLocationError("not-found", "No repositories are set up yet — map one in ⚙ Settings first.");
  }
  const hints = { repoKey: locationResolve.repoKeyFromPageUrl(input.pageUrl, repos.map((r) => r.key)) };

  let ambiguous: Extract<Resolved, { ok: false }> | undefined;
  for (const candidate of candidates.slice(0, MAX_CANDIDATES_TRIED)) {
    const resolved = locationResolve.resolveLocation(candidate, repos, hints);
    if (resolved.ok) {
      if (!staysInsideRepo(resolved.root, resolved.abs)) continue; // a symlink pointing out of the repo
      const url = editorUrl.editorUrl(editor, resolved.abs, resolved.line, resolved.column);
      await (deps.open ?? ((u: string) => run("open", [u]).then(() => undefined)))(url);
      return { repo: resolved.repoKey, path: resolved.rel, line: resolved.line, column: resolved.column, editor };
    }
    if (resolved.reason === "ambiguous" && !ambiguous) ambiguous = resolved;
  }
  if (ambiguous) {
    const list = (ambiguous.matches ?? []).map((m) => `${m.repoKey}: ${m.rel}`).join("; ");
    throw new OpenLocationError("ambiguous", `That file name matches several files (${list}).`, {
      matches: ambiguous.matches,
    });
  }
  throw new OpenLocationError(
    "not-found",
    "None of the files in the selected text is tracked in your set-up repositories.",
  );
}
