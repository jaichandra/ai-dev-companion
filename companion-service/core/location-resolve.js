// Decides which tracked file a parsed "path:line" location names. Pure: the
// caller supplies each configured repo's `git ls-files` output, so a match is
// always a file git tracks inside a configured repo root — a path in pasted
// text can never point outside them.
//
// A location's path may be absolute on the build machine
// (/home/ci/workspace/sample-app/src/a.ts), a webpack-relative path (src/a.ts) or
// a Java package path (com/x/Y.java). The longest trailing run of path
// segments that matches a tracked file wins.
const path = require("path");

const MAX_AMBIGUOUS = 10;

/** @typedef {{ key: string, root: string, files: string[] }} Repo */

function fileSet(repo) {
  if (!repo._fileSet) repo._fileSet = new Set(repo.files);
  return repo._fileSet;
}

function byBase(repo) {
  if (repo._byBase) return repo._byBase;
  const map = new Map();
  for (const f of repo.files) {
    const base = f.slice(f.lastIndexOf("/") + 1);
    const list = map.get(base);
    if (list) list.push(f);
    else map.set(base, [f]);
  }
  repo._byBase = map;
  return map;
}

function safeRel(rel) {
  return typeof rel === "string" && rel && !rel.startsWith("/") && !rel.split("/").includes("..");
}

function found(repo, rel, candidate) {
  return {
    ok: true,
    repoKey: repo.key,
    root: repo.root,
    rel,
    abs: path.join(repo.root, rel),
    line: candidate.line,
    column: candidate.column,
  };
}

/**
 * @param {{ path: string, line: number, column: number | null }} candidate
 * @param {Repo[]} repos
 * @param {{ repoKey?: string }} [hints] the repo the page belongs to, when known
 * @returns {{ ok: true, repoKey: string, root: string, rel: string, abs: string, line: number, column: number | null }
 *   | { ok: false, reason: "not-found" | "ambiguous", matches?: { repoKey: string, rel: string }[] }}
 */
function resolveLocation(candidate, repos, hints = {}) {
  const normalized = path.posix.normalize(candidate.path);
  const segs = normalized.split("/").filter(Boolean);
  if (segs.length === 0) return { ok: false, reason: "not-found" };

  // 1. An absolute path that is really inside a configured repo (the
  //    developer's own machine, or a stack trace copied from a local run).
  if (normalized.startsWith("/")) {
    for (const repo of repos) {
      const root = repo.root.endsWith("/") ? repo.root : `${repo.root}/`;
      if (!normalized.startsWith(root)) continue;
      const rel = normalized.slice(root.length);
      if (safeRel(rel) && fileSet(repo).has(rel)) return found(repo, rel, candidate);
    }
  }

  // 2. Longest trailing run of segments that matches a tracked file.
  const base = segs[segs.length - 1];
  for (let k = segs.length; k >= 1; k--) {
    const suffix = segs.slice(-k).join("/");
    const matches = [];
    for (const repo of repos) {
      for (const rel of byBase(repo).get(base) || []) {
        if (safeRel(rel) && (rel === suffix || rel.endsWith(`/${suffix}`))) matches.push({ repo, rel });
      }
    }
    if (matches.length === 0) continue;
    if (matches.length === 1) return found(matches[0].repo, matches[0].rel, candidate);
    const hinted = hints.repoKey ? matches.filter((m) => m.repo.key === hints.repoKey) : [];
    if (hinted.length === 1) return found(hinted[0].repo, hinted[0].rel, candidate);
    return {
      ok: false,
      reason: "ambiguous",
      matches: (hinted.length > 1 ? hinted : matches).slice(0, MAX_AMBIGUOUS).map((m) => ({ repoKey: m.repo.key, rel: m.rel })),
    };
  }
  return { ok: false, reason: "not-found" };
}

/**
 * The configured repo key ("PROJECT/repo") a Bitbucket page belongs to, matched
 * case-insensitively from /projects/<P>/repos/<r>/… in the page URL, or undefined.
 * Used only to break ties between files with the same name.
 */
function repoKeyFromPageUrl(pageUrl, repoKeys) {
  if (typeof pageUrl !== "string") return undefined;
  const m = require("./prereqs.js").gitRemoteRules().parseRepoUrl(pageUrl);
  if (!m) return undefined;
  let want;
  try {
    want = `${decodeURIComponent(m.project)}/${decodeURIComponent(m.repo)}`.toLowerCase();
  } catch {
    return undefined;
  }
  return repoKeys.find((k) => k.toLowerCase() === want);
}

module.exports = { resolveLocation, repoKeyFromPageUrl };
