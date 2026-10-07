// Pure logic for analyze-issue. Plain JS, not TypeScript — same reason
// as create-jira-subtasks/plan.js: lets plan.test.js run this directly
// with zero build step; tsc still picks it up (allowJs) and copies it
// into dist/ for index.ts's runtime use after a build.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { COMPANION_MCP_SERVER } = require("../../core/mcp-tool-classifier.js");
const similar = require("../../core/similar.js");
const { gitRemoteRules } = require("../../core/prereqs.js");

const ISSUE_KEY_RE = /^([A-Z][A-Z0-9_]*)-(\d+)$/;
const URL_RE = /https?:\/\/[^\s<>"')\]]+/gi;

/**
 * MCP tools a read-only analysis must never be given: the packs' deny lists
 * (core/packs.js `mcpDenyList`), passed as `--disallowedTools` so an analysis job
 * cannot create/update/merge/sync even if an MCP server exposes the tool. Claude
 * loads every server in ~/.claude.json under headless `-p`.
 */
const { PRODUCT_NAME } = require("../../core/product-name.js");
const MUTATING_MCP_TOOLS = require("../../core/packs.js").disallowedMcpTools();

/** Upper-cases and validates an issue key (e.g. "proj-34034" -> PROJ-34034). */
function parseIssueKey(raw) {
  const key = String(raw || "")
    .trim()
    .toUpperCase();
  const match = key.match(ISSUE_KEY_RE);
  if (!match) {
    throw new Error(`"${raw}" doesn't look like a Jira issue key (expected e.g. "PROJ-34034").`);
  }
  return { key, projectKey: match[1] };
}

/** Flatten ADF / wiki / plain text into a searchable string. */
function textFromField(value) {
  if (value == null) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (Array.isArray(value)) return value.map(textFromField).join("\n");
  if (typeof value === "object") {
    if (typeof value.content === "string") return value.content;
    if (Array.isArray(value.content)) return value.content.map(textFromField).join("\n");
    if (typeof value.text === "string") return value.text;
    try {
      return JSON.stringify(value);
    } catch {
      return "";
    }
  }
  return "";
}

/** Strip trailing punctuation that URL_RE often captures from prose. */
function cleanUrl(raw) {
  return String(raw || "").replace(/[.,;:!?)]+$/g, "");
}

/**
 * Pulls https? URLs from description, comments, environment, and remote
 * links, then dedupes (order preserved, first occurrence wins).
 */
function extractUrls(issue) {
  const fields = issue?.fields || {};
  const chunks = [
    textFromField(fields.description),
    textFromField(fields.environment),
  ];
  const comments = fields.comment?.comments || [];
  for (const c of comments) {
    chunks.push(textFromField(c.body));
  }
  const remoteLinks = Array.isArray(issue?.remoteLinks) ? issue.remoteLinks : [];
  for (const link of remoteLinks) {
    const href = link?.object?.url || link?.url;
    if (href) chunks.push(String(href));
  }

  const seen = new Set();
  const urls = [];
  for (const chunk of chunks) {
    if (!chunk) continue;
    const matches = chunk.match(URL_RE) || [];
    for (const m of matches) {
      const url = cleanUrl(m);
      if (!url || seen.has(url)) continue;
      seen.add(url);
      urls.push(url);
    }
  }
  return urls;
}

function normalizeName(raw) {
  return String(raw || "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "");
}

/** Case-insensitive lookup of a "PROJECT/repo" key in reposMap. */
function findRepoKey(reposMap, project, repo) {
  const want = `${project}/${repo}`.toLowerCase();
  for (const key of Object.keys(reposMap || {})) {
    if (key.toLowerCase() === want) return key;
  }
  return null;
}

const MAX_LINKED_PRS = 10;

function cleanPrTitle(raw) {
  return typeof raw === "string" ? raw.replace(/[\u0000-\u001f\u007f]+/g, " ").trim().slice(0, 200) || null : null;
}

/**
 * The pull requests already linked to the ticket: those Jira's development
 * panel lists (`devStatus` is the raw dev-status `pullrequest` answer, or
 * nothing when that isn't available) and any Bitbucket PR address in the
 * ticket's links, description or comments. Each PR's address is rebuilt from
 * its project, repo and number on the configured Bitbucket host, never taken
 * as given, since it ends up as a link in the panel. Each PR once, at most ten.
 */
function linkedPullRequests(issue, devStatus, bitbucketBaseUrl) {
  let base = null;
  try {
    const u = new URL(bitbucketBaseUrl);
    if (u.protocol === "https:") base = u.origin;
  } catch {
    // No usable host: the PRs still count, just without a link.
  }
  const out = [];
  const add = (url, title, state) => {
    const m = typeof url === "string" ? gitRemoteRules().parsePrUrl(url) : null;
    if (!m) return;
    const { project, repo, id } = m;
    const repoKey = `${project}/${repo}`;
    const existing = out.find((p) => p.repoKey.toLowerCase() === repoKey.toLowerCase() && p.id === Number(id));
    if (existing) {
      if (!existing.title && cleanPrTitle(title)) existing.title = cleanPrTitle(title);
      if (!existing.state && /^[A-Za-z_]{1,20}$/.test(String(state || ""))) existing.state = String(state).toUpperCase();
      return;
    }
    if (out.length >= MAX_LINKED_PRS) return;
    out.push({
      repoKey,
      project,
      repo,
      id: Number(id),
      title: cleanPrTitle(title),
      state: /^[A-Za-z_]{1,20}$/.test(String(state || "")) ? String(state).toUpperCase() : null,
      url: base ? `${base}${gitRemoteRules().prPath(project, repo, id)}` : null,
    });
  };
  const details = devStatus && Array.isArray(devStatus.detail) ? devStatus.detail : [];
  for (const d of details) {
    for (const pr of (d && Array.isArray(d.pullRequests) ? d.pullRequests : [])) {
      add(pr && pr.url, pr && (pr.name || pr.title), pr && pr.status);
    }
  }
  for (const url of extractUrls(issue)) add(url, null, null);
  return out;
}

/**
 * Ranks local-repo candidates from ticket signals. Sources (highest
 * priority first within a source; later sources only add if not already
 * present at equal-or-higher score):
 *   0. Pull requests already linked to the ticket (Jira's development panel, or
 *      PR addresses in its links and text)
 *   1. Bitbucket /projects/X/repos/Y links matched to config.repos keys
 *   2. Components/labels via optional componentRepoMap
 *   3. Normalized name match of components/labels against repo names
 *
 * Returns [{ repoKey, score, reason }, ...] sorted by score desc.
 */
function candidateReposFromTicket(issue, reposMap, componentRepoMap, linkedPrs = []) {
  const repos = reposMap || {};
  const map = componentRepoMap || {};
  const byKey = new Map();

  function add(repoKey, score, reason) {
    if (!repoKey || !repos[repoKey]) return;
    const existing = byKey.get(repoKey);
    if (!existing || score > existing.score) {
      byKey.set(repoKey, { repoKey, score, reason });
    }
  }

  // A PR already linked to the ticket is the strongest sign of where it lives.
  for (const pr of linkedPrs || []) {
    const key = findRepoKey(repos, pr.project, pr.repo);
    if (key) add(key, 120, `Linked PR #${pr.id} in ${pr.repoKey}`);
  }

  const urls = extractUrls(issue);
  for (const url of urls) {
    const match = gitRemoteRules().parseRepoUrl(url);
    if (!match) continue;
    const key = findRepoKey(repos, match.project, match.repo);
    if (key) add(key, 100, `Repository link ${match.project}/${match.repo}`);
  }

  const fields = issue?.fields || {};
  const components = (fields.components || []).map((c) => c?.name).filter(Boolean);
  const labels = Array.isArray(fields.labels) ? fields.labels.filter(Boolean) : [];
  const names = [...components, ...labels];

  for (const name of names) {
    const mapped = map[name] || map[String(name).toLowerCase()];
    if (!mapped) continue;
    const key =
      repos[mapped] != null
        ? mapped
        : Object.keys(repos).find((k) => k.toLowerCase() === String(mapped).toLowerCase());
    if (key) add(key, 80, `Mapped from "${name}"`);
  }

  for (const name of names) {
    const norm = normalizeName(name);
    if (!norm) continue;
    for (const key of Object.keys(repos)) {
      const repoName = key.includes("/") ? key.split("/").pop() : key;
      if (normalizeName(repoName) === norm) {
        add(key, 60, `Name match on "${name}"`);
      }
    }
  }

  return [...byKey.values()].sort((a, b) => b.score - a.score || a.repoKey.localeCompare(b.repoKey));
}

/** True when heuristics produced exactly one top-scoring candidate. */
function isStrongRepoMatch(candidates) {
  if (!candidates || candidates.length === 0) return false;
  if (candidates.length === 1) return true;
  return candidates[0].score > candidates[1].score;
}

/** Repos that similar past tickets were analyzed in, most similar first, limited
 * to keys we still have a clone for. A hint for the pick, not a match. */
function repoHintsFromSimilar(similarItems, repoKeys) {
  const out = [];
  for (const s of similarItems || []) {
    const repo = s && typeof s.repo === "string" ? s.repo : null;
    const key = repo && (repoKeys || []).find((k) => k.toLowerCase() === repo.toLowerCase());
    if (key && !out.includes(key)) out.push(key);
  }
  return out;
}

/** True when the proposed repo is what a component mapping (one the user made
 * or confirmed) gives for one of the ticket's components or labels, so asking
 * again would only be noise. A mapping that points elsewhere settles nothing. */
function isSettledRepoMatch(issue, proposal, componentRepoMap) {
  if (!proposal) return false;
  const fields = (issue && issue.fields) || {};
  const names = [...(fields.components || []).map((c) => c && c.name), ...(Array.isArray(fields.labels) ? fields.labels : [])].filter(Boolean);
  const map = componentRepoMap || {};
  const want = String(proposal).toLowerCase();
  return names.some((name) => {
    const mapped = map[name] || map[String(name).toLowerCase()];
    return typeof mapped === "string" && mapped.toLowerCase() === want;
  });
}

const MAX_CHOICE_OPTIONS = 30;

/**
 * The repos offered when the user confirms the repo: the proposal first, then
 * the other ticket-based candidates, then repos of similar past tickets, then
 * every other repo we know, each once, each with why it is offered.
 */
function repoChoiceOptions({ proposal, proposalReason, candidates, hintKeys, repoKeys, linkedPrs }) {
  const out = [];
  const seen = new Set();
  const add = (repoKey, reason) => {
    if (!repoKey || seen.has(repoKey) || !(repoKeys || []).includes(repoKey)) return;
    seen.add(repoKey);
    out.push({ repoKey, reason });
  };
  // Repos that already have a PR for this ticket come first.
  for (const pr of linkedPrs || []) {
    const key = (repoKeys || []).find((k) => k.toLowerCase() === pr.repoKey.toLowerCase());
    if (key) add(key, "");
  }
  add(proposal, proposalReason || "");
  for (const c of candidates || []) add(c.repoKey, c.reason);
  for (const k of hintKeys || []) add(k, "used for a similar past ticket");
  for (const k of [...(repoKeys || [])].sort((a, b) => a.localeCompare(b))) add(k, "");
  return out.slice(0, MAX_CHOICE_OPTIONS);
}

/** The reason recorded for the repo the user settled on. */
function confirmedReason(choice, proposal, proposalReason) {
  if (choice && choice === proposal) return `You confirmed it${proposalReason ? ` (${proposalReason})` : ""}`;
  return "You chose it";
}

/**
 * The component -> repo entries worth remembering after the user confirmed or
 * chose a repo (never from a guess alone): only for a ticket with exactly one
 * component (with several there is no telling which one the repo belongs to),
 * and never over an entry already there.
 */
function componentMappingsToLearn(issue, repoKey, reason, componentRepoMap) {
  if (!repoKey || !/^You (confirmed|chose)/.test(String(reason || ""))) return {};
  const components = ((issue && issue.fields && issue.fields.components) || []).map((c) => c && c.name).filter(Boolean);
  if (components.length !== 1) return {};
  const name = String(components[0]).trim();
  const map = componentRepoMap || {};
  if (!name || Object.keys(map).some((k) => k.toLowerCase() === name.toLowerCase())) return {};
  return { [name]: repoKey };
}

/** Model for the "Add as comment" summary: a short rewrite, so the small one. */
const COMMENT_SUMMARY_MODEL = "haiku";
/** The summary comment stays under this many words, header included. */
const COMMENT_SUMMARY_MAX_WORDS = 250;
/** What the prompt asks for; below the cap so the header still fits. */
const COMMENT_SUMMARY_TARGET_WORDS = 180;

function buildCommentSummaryPrompt(fullMarkdown) {
  return [
    "Summarize the Jira issue analysis below into a short comment for the ticket.",
    "",
    `- At most ${COMMENT_SUMMARY_TARGET_WORDS} words. Plain Markdown: a few short sentences or bullets, no headings.`,
    "- Cover the likely cause (with confidence), where in the code, and the next steps.",
    "- Keep file paths, symbols and ticket keys exactly as written. Add nothing that is not in the analysis.",
    "- Reply with the comment text only: no preamble, no code fence.",
    "",
    "<analysis>",
    String(fullMarkdown || "").trim(),
    "</analysis>",
  ].join("\n");
}

/** Cut `text` to at most `max` words, keeping line breaks in what is kept. */
function clampWords(text, max) {
  let count = 0;
  const out = [];
  for (const part of String(text).split(/(\s+)/)) {
    if (part && !/^\s+$/.test(part)) {
      if (++count > max) return out.join("").trimEnd() + "…";
    }
    out.push(part);
  }
  return out.join("");
}

/** Header + AI note (as formatAnalysisAsMarkdownComment) over Claude's summary, under COMMENT_SUMMARY_MAX_WORDS in all. */
function assembleSummaryComment({ issueKey, summaryText }) {
  const head = [
    `### AI issue analysis — ${mdOneLine(issueKey || "issue")}`,
    "",
    `_Generated by ${PRODUCT_NAME}. Treat as a starting point, not a verified root cause._`,
    "",
  ].join("\n");
  let body = String(summaryText || "")
    .trim()
    .replace(/^```(?:markdown|md)?\s*\n([\s\S]*?)\n```$/, "$1")
    .trim();
  if (!body) return "";
  const headWords = head.split(/\s+/).filter(Boolean).length;
  body = clampWords(body, COMMENT_SUMMARY_MAX_WORDS - headWords);
  return head + body + "\n";
}

function buildRepoPickPrompt(issue, repoKeys, hintKeys = [], linkedPrs = []) {
  const fields = issue?.fields || {};
  const key = issue?.key || "(unknown)";
  const summary = fields.summary || "";
  const components = (fields.components || []).map((c) => c?.name).filter(Boolean);
  const labels = Array.isArray(fields.labels) ? fields.labels : [];
  const urls = extractUrls(issue);
  return [
    "Pick which local git repository this Jira bug most likely belongs to.",
    "Reply with ONLY a single line: either one exact repo key from the list, or the word none.",
    "Do not explain. Do not use tools.",
    "",
    `Issue: ${key} — ${summary}`,
    `Components: ${components.join(", ") || "(none)"}`,
    `Labels: ${labels.join(", ") || "(none)"}`,
    `URLs: ${urls.slice(0, 20).join(", ") || "(none)"}`,
    "",
    ...(linkedPrs.length > 0
      ? [
          "",
          "Pull requests already linked to this ticket (strong evidence of the repo; titles are written by people):",
          ...linkedPrs.slice(0, 5).map((p) => `- ${p.repoKey} #${p.id}${p.state ? ` (${p.state})` : ""}${p.title ? `: ${p.title}` : ""}`),
        ]
      : []),
    ...(hintKeys.length > 0
      ? ["", `Repos that similar past tickets were analyzed in (a hint, most similar first): ${hintKeys.join(", ")}`]
      : []),
    "",
    "Candidate repo keys:",
    ...repoKeys.map((k) => `- ${k}`),
  ].join("\n");
}

/** Parses Claude's one-line repo pick — an exact key, or null for "none"/garbage. */
function parseRepoPick(text, repoKeys) {
  const line = String(text || "")
    .trim()
    .split(/\r?\n/)
    .map((l) => l.trim())
    .find((l) => l && !l.startsWith("```")) || "";
  const cleaned = line.replace(/^["'`]+|["'`]+$/g, "").trim();
  if (!cleaned || /^none$/i.test(cleaned)) return null;
  const keys = repoKeys || [];
  const exact = keys.find((k) => k === cleaned);
  if (exact) return exact;
  const ci = keys.find((k) => k.toLowerCase() === cleaned.toLowerCase());
  return ci || null;
}

/** How a run is set up, chosen by the one flag that says nobody clicked: a
 * background run (the watchers') uses the stricter policy, no MCP servers and
 * no reference links, since it can fetch nothing. */
function analysisRunOptions(background, readMcpServers) {
  const mcpServers = background ? [] : readMcpServers();
  return {
    policy: background ? "readOnlyBackground" : "readOnly",
    mcpServers,
    extraAllowedTools: mcpServers.map((s) => `mcp__${s}`),
    useUrls: !background,
  };
}

/** The text a similar-ticket search compares: the summary and the start of
 * the description (the search clips and masks it again before embedding). */
function similarQueryText(issue) {
  const fields = (issue && issue.fields) || {};
  return [fields.summary || "", textFromField(fields.description).slice(0, 2000)].filter(Boolean).join("\n");
}

function buildAnalysisPrompt({ issue, urls, repoKey, mcpServers, serverHints = [], background = false, similar: similarItems = [] }) {
  const fields = issue?.fields || {};
  const key = issue?.key || "(unknown)";
  const summary = fields.summary || "";
  const description = textFromField(fields.description).slice(0, 8000);
  const environment = textFromField(fields.environment).slice(0, 2000);
  const components = (fields.components || []).map((c) => c?.name).filter(Boolean);
  const labels = Array.isArray(fields.labels) ? fields.labels : [];
  const priority = fields.priority?.name || "";
  const versions = (fields.versions || []).map((v) => v?.name).filter(Boolean);
  const fixVersions = (fields.fixVersions || []).map((v) => v?.name).filter(Boolean);
  const comments = (fields.comment?.comments || [])
    .slice(-10)
    .map((c) => {
      const author = c.author?.displayName || c.author?.name || "unknown";
      return `[${author}] ${textFromField(c.body).slice(0, 1500)}`;
    })
    .join("\n\n");
  const mcpList = (mcpServers || []).join(", ") || "(none detected)";
  // Earlier tickets from the local history: fenced, clipped, labelled as
  // untrusted (titles by people, analyses earlier AI output). Background runs
  // get it too — it is data, not a tool, and gives no way out.
  const similarBlock = similar.promptBlock(similarItems);
  // Each hint is { server, available, line }: the pack's line for that case.
  const hintLines = serverHints.map((h) => h.line);
  const preferredSchema = serverHints.map((h) => `${JSON.stringify(h.server)}: true`).join(", ");

  return [
    "You are doing a quick, read-only initial analysis of a Jira Bug. Do NOT propose or apply a fix.",
    "Stay read-only: never edit files, never commit, never push, never create/update Jira/Confluence/Bitbucket/Jenkins resources.",
    ...(background
      ? ["No MCP servers or web access are available in this run: work only from the ticket text and the local files."]
      : [...hintLines, "Use Jira, Confluence, Bitbucket, and Jenkins MCP servers (or WebFetch) for the reference URLs as needed."]),
    "You may use Bash only for `git log`, `git blame`, and `git show` — nothing else.",
    "",
    `Repo context: ${repoKey ? `working in local clone for "${repoKey}"` : `NO local repo resolved — reason from ticket text${background ? "" : " and URLs"} only`}.`,
    ...(background ? [] : [`MCP servers loaded from user config: ${mcpList}`]),
    "",
    "Return ONE fenced JSON object (and nothing else after it) matching this schema:",
    "{",
    '  "tldr": "string",',
    '  "affectedArea": [{ "path": "string", "symbol": "string", "why": "string" }],',
    '  "hypotheses": [{ "hypothesis": "string", "confidence": "high|medium|low", "evidence": "string" }],',
    '  "reproScope": "string",',
    '  "referencesReviewed": [{ "url": "string", "takeaway": "string" }],',
    '  "nextSteps": ["string"],',
    '  "openQuestions": ["string"],',
    `  "toolsUsed": { "preferred": { ${preferredSchema} }, "mcpServers": ["string"] }`,
    "}",
    "",
    `Issue: ${key}`,
    `Summary: ${summary}`,
    `Priority: ${priority}`,
    `Components: ${components.join(", ") || "(none)"}`,
    `Labels: ${labels.join(", ") || "(none)"}`,
    `Affects versions: ${versions.join(", ") || "(none)"}`,
    `Fix versions: ${fixVersions.join(", ") || "(none)"}`,
    "",
    "Description:",
    description || "(empty)",
    "",
    "Environment:",
    environment || "(empty)",
    "",
    "Recent comments:",
    comments || "(none)",
    ...(similarBlock ? ["", similarBlock] : []),
    ...(background ? [] : ["", "Reference URLs:", (urls || []).length > 0 ? urls.map((u) => `- ${u}`).join("\n") : "(none)"]),
  ].join("\n");
}

function emptyAnalysis() {
  return {
    tldr: "",
    affectedArea: [],
    hypotheses: [],
    reproScope: "",
    referencesReviewed: [],
    nextSteps: [],
    openQuestions: [],
    toolsUsed: { preferred: {}, mcpServers: [] },
  };
}

function asStringArray(value) {
  if (!Array.isArray(value)) return [];
  return value.map((v) => String(v ?? "").trim()).filter(Boolean);
}

/** { <server>: bool } from a toolsUsed object. Older saved analyses kept each flag at the top level. */
function preferredFlags(toolsUsed) {
  const flags = {};
  for (const [k, v] of Object.entries(toolsUsed)) if (typeof v === "boolean") flags[k] = v;
  if (toolsUsed.preferred && typeof toolsUsed.preferred === "object") {
    for (const [k, v] of Object.entries(toolsUsed.preferred)) flags[k] = !!v;
  }
  return flags;
}

function normalizeAnalysis(parsed) {
  const base = emptyAnalysis();
  if (!parsed || typeof parsed !== "object") return { ...base, raw: undefined };

  const affectedArea = Array.isArray(parsed.affectedArea)
    ? parsed.affectedArea
        .filter((a) => a && typeof a === "object")
        .map((a) => ({
          path: String(a.path || ""),
          symbol: String(a.symbol || ""),
          why: String(a.why || ""),
        }))
    : [];

  const hypotheses = Array.isArray(parsed.hypotheses)
    ? parsed.hypotheses
        .filter((h) => h && typeof h === "object")
        .map((h) => {
          const confidence = String(h.confidence || "medium").toLowerCase();
          return {
            hypothesis: String(h.hypothesis || ""),
            confidence: ["high", "medium", "low"].includes(confidence) ? confidence : "medium",
            evidence: String(h.evidence || ""),
          };
        })
    : [];

  const referencesReviewed = Array.isArray(parsed.referencesReviewed)
    ? parsed.referencesReviewed
        .filter((r) => r && typeof r === "object")
        .map((r) => ({
          url: String(r.url || ""),
          takeaway: String(r.takeaway || ""),
        }))
    : [];

  const toolsUsed =
    parsed.toolsUsed && typeof parsed.toolsUsed === "object"
      ? {
          preferred: preferredFlags(parsed.toolsUsed),
          mcpServers: asStringArray(parsed.toolsUsed.mcpServers),
        }
      : base.toolsUsed;

  return {
    tldr: String(parsed.tldr || ""),
    affectedArea,
    hypotheses,
    reproScope: String(parsed.reproScope || ""),
    referencesReviewed,
    nextSteps: asStringArray(parsed.nextSteps),
    openQuestions: asStringArray(parsed.openQuestions),
    toolsUsed,
  };
}

/** Extract a fenced or raw JSON object from Claude's text; on failure return { raw }. */
function parseAnalysis(text) {
  const raw = String(text || "").trim();
  if (!raw) return { raw: "" };

  const fence = raw.match(/```(?:json)?\s*([\s\S]*?)```/i);
  const candidates = [];
  if (fence) candidates.push(fence[1].trim());
  const firstBrace = raw.indexOf("{");
  const lastBrace = raw.lastIndexOf("}");
  if (firstBrace !== -1 && lastBrace > firstBrace) {
    candidates.push(raw.slice(firstBrace, lastBrace + 1));
  }
  candidates.push(raw);

  for (const candidate of candidates) {
    try {
      const parsed = JSON.parse(candidate);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        return normalizeAnalysis(parsed);
      }
    } catch {
      // try next
    }
  }
  return { raw };
}

/**
 * User-scope MCP server names from ~/.claude.json (or a provided path).
 * Filters out the companion's own MCP server to prevent headless Claude runs
 * from recursively calling the companion's services.
 * Returns [] if the file is missing or invalid — analysis still runs
 * without MCP.
 */
function readMcpServers(claudeJsonPath) {
  const filePath = claudeJsonPath || path.join(os.homedir(), ".claude.json");
  let raw;
  try {
    raw = fs.readFileSync(filePath, "utf8");
  } catch {
    return [];
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  const servers = parsed?.mcpServers;
  if (!servers || typeof servers !== "object") return [];
  return Object.keys(servers).filter(Boolean).filter((s) => s !== COMPANION_MCP_SERVER);
}

function buildSummary({ issueKey, repoKey, analysis }) {
  const repoBit = repoKey ? ` (repo ${repoKey})` : " (no repo matched)";
  if (analysis && typeof analysis.tldr === "string" && analysis.tldr.trim()) {
    const tldr = analysis.tldr.trim().replace(/\s+/g, " ");
    const short = tldr.length > 160 ? `${tldr.slice(0, 157)}…` : tldr;
    return `Analyzed ${issueKey}${repoBit}: ${short}`;
  }
  return `Analyzed ${issueKey}${repoBit}.`;
}

/** Flatten newlines in free-text fields so list/heading lines stay one-line. */
function mdOneLine(text) {
  return String(text || "")
    .replace(/\n+/g, " ")
    .trim();
}

/**
 * Format a structured (or raw) analysis as Markdown for the extension's
 * edit-before-post compose view. Posted comments convert this to Jira
 * Server/DC wiki markup via markdownToJiraWiki (API v2 string body, not ADF).
 */
function formatAnalysisAsMarkdownComment({ issueKey, summary, repoKey, repoMatch, analysis }) {
  const key = issueKey || "issue";
  const lines = [
    `### AI issue analysis — ${mdOneLine(key)}`,
    "",
    `_Generated by ${PRODUCT_NAME}. Treat as a starting point, not a verified root cause._`,
    "",
  ];

  if (summary) lines.push(`**Summary:** ${mdOneLine(summary)}`, "");
  if (repoKey) {
    const matchBit = repoMatch ? ` (${mdOneLine(repoMatch)})` : "";
    lines.push(`**Repo:** \`${mdOneLine(repoKey)}\`${matchBit}`, "");
  } else if (repoMatch) {
    lines.push(`**Repo:** none matched (${mdOneLine(repoMatch)})`, "");
  }

  if (!analysis || typeof analysis !== "object") {
    lines.push("_No analysis content was available._");
    return lines.join("\n");
  }

  // Parse-failure path: only raw Claude text.
  if (typeof analysis.raw === "string" && analysis.raw && !analysis.tldr) {
    lines.push("#### Raw analysis", "```", analysis.raw.trim(), "```");
    return lines.join("\n") + "\n";
  }

  if (analysis.tldr) {
    lines.push("#### Initial assessment", mdOneLine(analysis.tldr), "");
  }

  const affected = Array.isArray(analysis.affectedArea) ? analysis.affectedArea : [];
  if (affected.length > 0) {
    lines.push("#### Likely affected area");
    for (const entry of affected) {
      const where =
        [entry?.path, entry?.symbol].filter(Boolean).map(mdOneLine).join(" · ") || "(unknown)";
      const why = entry?.why ? ` — ${mdOneLine(entry.why)}` : "";
      lines.push(`- \`${where}\`${why}`);
    }
    lines.push("");
  }

  const hypotheses = Array.isArray(analysis.hypotheses) ? analysis.hypotheses : [];
  if (hypotheses.length > 0) {
    lines.push("#### Root-cause hypotheses");
    for (const entry of hypotheses) {
      const conf = entry?.confidence ? `**[${mdOneLine(entry.confidence)}]** ` : "";
      const hyp = mdOneLine(entry?.hypothesis || "");
      const evidence = entry?.evidence ? ` — _${mdOneLine(entry.evidence)}_` : "";
      lines.push(`- ${conf}${hyp}${evidence}`);
    }
    lines.push("");
  }

  if (analysis.reproScope) {
    lines.push("#### Repro and scope", mdOneLine(analysis.reproScope), "");
  }

  const refs = Array.isArray(analysis.referencesReviewed) ? analysis.referencesReviewed : [];
  if (refs.length > 0) {
    lines.push("#### References reviewed");
    for (const entry of refs) {
      const url = typeof entry?.url === "string" ? entry.url.trim() : "";
      const link = url ? `[${url}](${url})` : "(no url)";
      const takeaway = entry?.takeaway ? ` — ${mdOneLine(entry.takeaway)}` : "";
      lines.push(`- ${link}${takeaway}`);
    }
    lines.push("");
  }

  const nextSteps = Array.isArray(analysis.nextSteps) ? analysis.nextSteps : [];
  if (nextSteps.length > 0) {
    lines.push("#### Suggested next steps");
    nextSteps.forEach((step, i) => lines.push(`${i + 1}. ${mdOneLine(step)}`));
    lines.push("");
  }

  const questions = Array.isArray(analysis.openQuestions) ? analysis.openQuestions : [];
  if (questions.length > 0) {
    lines.push("#### Open questions");
    for (const q of questions) lines.push(`- ${mdOneLine(q)}`);
    lines.push("");
  }

  return lines.join("\n").trim() + "\n";
}

/**
 * Convert a small Markdown subset to Jira Server/DC wiki markup for
 * POST /rest/api/2/issue/{key}/comment (plain string body, not ADF).
 * Supports headings, bold, italic, lists, links, inline code, and fences.
 */
function markdownToJiraWiki(md) {
  const src = String(md || "").replace(/\r\n/g, "\n");
  if (!src.trim()) return "";

  const fences = [];
  let text = src.replace(/```([^\n]*)\n?([\s\S]*?)```/g, (_, _lang, code) => {
    const i = fences.length;
    fences.push(`{code}\n${String(code).replace(/\n$/, "")}\n{code}`);
    return `\u0000FENCE${i}\u0000`;
  });

  const inlines = [];
  const hold = (wiki) => {
    const i = inlines.length;
    inlines.push(wiki);
    return `\u0000INL${i}\u0000`;
  };

  // Links before other inline so [text](url) isn't mangled by bold/italic.
  text = text.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label, url) =>
    hold(`[${label}|${url}]`),
  );
  // Autolink-style bare [url] → [url] (wiki already accepts that form).
  text = text.replace(/`([^`\n]+)`/g, (_, code) =>
    hold(`{{${String(code).replace(/\{\{/g, "{ {")}}}`),
  );
  text = text.replace(/\*\*([^*]+)\*\*/g, (_, inner) => hold(`*${inner}*`));
  text = text.replace(/(?<!\*)\*([^*\n]+)\*(?!\*)/g, (_, inner) => hold(`_${inner}_`));
  text = text.replace(/(?<![A-Za-z0-9])_([^_\n]+)_(?![A-Za-z0-9])/g, (_, inner) =>
    hold(`_${inner}_`),
  );

  const out = [];
  for (const line of text.split("\n")) {
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      out.push(`h${heading[1].length}. ${heading[2]}`);
      continue;
    }
    const ol = /^(\d+)\.\s+(.*)$/.exec(line);
    if (ol) {
      out.push(`# ${ol[2]}`);
      continue;
    }
    const ul = /^[-*+]\s+(.*)$/.exec(line);
    if (ul) {
      out.push(`* ${ul[1]}`);
      continue;
    }
    out.push(line);
  }

  let result = out.join("\n");
  result = result.replace(/\u0000INL(\d+)\u0000/g, (_, i) => inlines[Number(i)] || "");
  result = result.replace(/\u0000FENCE(\d+)\u0000/g, (_, i) => fences[Number(i)] || "");
  return result.trim() + (result.trim() ? "\n" : "");
}

/** @deprecated Use formatAnalysisAsMarkdownComment + markdownToJiraWiki. */
function formatAnalysisAsWikiComment(opts) {
  return markdownToJiraWiki(formatAnalysisAsMarkdownComment(opts));
}

/**
 * companion-service/ root — works whether this file is loaded from
 * features/analyze-issue/ (source / node test) or dist/features/analyze-issue/
 * (after tsc). Same idea as config.ts's CONFIG_PATH resolution.
 */
function companionServiceDir() {
  let dir = path.join(__dirname, "..", "..");
  if (path.basename(dir) === "dist") dir = path.join(dir, "..");
  return dir;
}

/** Durable analyses live next to config.json under analysis-cache/. */
function analysisCacheDir(rootDir) {
  return path.join(rootDir || companionServiceDir(), "analysis-cache");
}

function analysisCachePath(issueKey, rootDir) {
  const { key } = parseIssueKey(issueKey);
  return path.join(analysisCacheDir(rootDir), `${key}.json`);
}

/**
 * True when a cache entry has the fields needed to reopen the report panel
 * without re-running Claude. analysis must be a non-empty object (structured
 * or { raw }).
 */
function isValidAnalysisCache(entry, expectedKey) {
  if (!entry || typeof entry !== "object") return false;
  if (typeof entry.issueKey !== "string" || !entry.issueKey) return false;
  if (expectedKey && entry.issueKey.toUpperCase() !== String(expectedKey).toUpperCase()) {
    return false;
  }
  if (!entry.analysis || typeof entry.analysis !== "object") return false;
  if (typeof entry.completedAt !== "string" || !entry.completedAt) return false;
  return true;
}

/**
 * A cache file is read back as plain JSON, not the ClaudeSession type
 * core/jobs.ts declares, so a hand-edited or pre-this-feature file could
 * have anything under `claudeSession` — dropped (returns undefined)
 * rather than trusted unless it has the exact shape a real session has.
 */
function sanitizeClaudeSession(raw) {
  if (!raw || typeof raw !== "object") return undefined;
  if (typeof raw.id !== "string" || !raw.id) return undefined;
  if (typeof raw.cwd !== "string" || !raw.cwd) return undefined;
  if (!["plan", "auto", "default"].includes(raw.permissionMode)) return undefined;
  return { id: raw.id, cwd: raw.cwd, permissionMode: raw.permissionMode };
}

/** Load a cached analysis for issueKey, or null if missing/invalid. */
function readAnalysisCache(issueKey, rootDir) {
  const { key } = parseIssueKey(issueKey);
  const filePath = analysisCachePath(key, rootDir);
  let raw;
  try {
    raw = fs.readFileSync(filePath, "utf8");
  } catch {
    return null;
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!isValidAnalysisCache(parsed, key)) return null;
  return {
    issueKey: key,
    summary: typeof parsed.summary === "string" ? parsed.summary : "",
    repoKey: parsed.repoKey == null ? null : String(parsed.repoKey),
    repoMatch: parsed.repoMatch == null ? null : String(parsed.repoMatch),
    analysis: parsed.analysis,
    completedAt: parsed.completedAt,
    claudeSession: sanitizeClaudeSession(parsed.claudeSession),
  };
}

/** Persist a successful analysis next to config.json. */
function writeAnalysisCache(entry, rootDir) {
  const { key } = parseIssueKey(entry.issueKey);
  if (!isValidAnalysisCache({ ...entry, issueKey: key }, key)) {
    throw new Error("writeAnalysisCache: entry is missing required fields");
  }
  const dir = analysisCacheDir(rootDir);
  fs.mkdirSync(dir, { recursive: true });
  const payload = {
    issueKey: key,
    summary: typeof entry.summary === "string" ? entry.summary : "",
    repoKey: entry.repoKey == null ? null : entry.repoKey,
    repoMatch: entry.repoMatch == null ? null : entry.repoMatch,
    analysis: entry.analysis,
    completedAt: entry.completedAt,
    claudeSession: entry.claudeSession ? sanitizeClaudeSession(entry.claudeSession) : undefined,
  };
  fs.writeFileSync(analysisCachePath(key, rootDir), JSON.stringify(payload, null, 2) + "\n", "utf8");
}

const SIMILAR_TIMEOUT_MS = 4000;
const SIMILAR_ASK = 20;
const SIMILAR_KEEP = 5;

/**
 * The similar past tickets for a run's prompt: asks for more than will be
 * shown (PR results are dropped by the prompt block, so asking for exactly
 * five could leave fewer), keeps the tickets, at most five, and gives up
 * after `timeoutMs` (an unref'd timer, always cleared). Never throws: any
 * failure or timeout is an empty list and one `warn` line.
 */
async function fetchSimilarTickets(findSimilar, query, { timeoutMs = SIMILAR_TIMEOUT_MS, warn = () => {} } = {}) {
  if (typeof findSimilar !== "function") return [];
  let timer;
  try {
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("timed out")), timeoutMs);
      if (timer.unref) timer.unref();
    });
    const result = await Promise.race([Promise.resolve().then(() => findSimilar({ ...query, k: SIMILAR_ASK })), timeout]);
    const items = result && Array.isArray(result.items) ? result.items : [];
    return items.filter((i) => i && typeof i.key === "string" && i.key.startsWith("jira:")).slice(0, SIMILAR_KEEP);
  } catch (err) {
    warn(err && err.message ? err.message : String(err));
    return [];
  } finally {
    clearTimeout(timer);
  }
}

module.exports = {
  fetchSimilarTickets,
  MUTATING_MCP_TOOLS,
  parseIssueKey,
  extractUrls,
  candidateReposFromTicket,
  isStrongRepoMatch,
  buildRepoPickPrompt,
  COMMENT_SUMMARY_MODEL,
  COMMENT_SUMMARY_MAX_WORDS,
  buildCommentSummaryPrompt,
  assembleSummaryComment,
  repoHintsFromSimilar,
  linkedPullRequests,
  isSettledRepoMatch,
  repoChoiceOptions,
  confirmedReason,
  componentMappingsToLearn,
  parseRepoPick,
  buildAnalysisPrompt,
  similarQueryText,
  analysisRunOptions,
  parseAnalysis,
  readMcpServers,
  buildSummary,
  formatAnalysisAsMarkdownComment,
  markdownToJiraWiki,
  formatAnalysisAsWikiComment,
  textFromField,
  companionServiceDir,
  analysisCacheDir,
  analysisCachePath,
  isValidAnalysisCache,
  readAnalysisCache,
  writeAnalysisCache,
};
