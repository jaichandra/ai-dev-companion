const test = require("node:test");
const plan = require("./plan.js");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const {
  extractUrls,
  candidateReposFromTicket,
  isStrongRepoMatch,
  parseRepoPick,
  parseAnalysis,
  readMcpServers,
  MUTATING_MCP_TOOLS,
  parseIssueKey,
  buildSummary,
  writeAnalysisCache,
  readAnalysisCache,
  analysisCachePath,
  isValidAnalysisCache,
} = require("./plan.js");

test("parseIssueKey upper-cases and splits the project key", () => {
  const result = parseIssueKey("proj-34034");
  assert.equal(result.key, "PROJ-34034");
  assert.equal(result.projectKey, "PROJ");
});

test("extractUrls pulls from description, comments, environment, and remote links", () => {
  const issue = {
    fields: {
      description: "See https://example.com/a and also https://example.com/a again.",
      environment: "Log at https://example.com/env",
      comment: {
        comments: [{ body: "Related: https://jira.example/browse/PROJ-1" }],
      },
    },
    remoteLinks: [{ object: { url: "https://bitbucket.example/projects/PROJ/repos/sample-app" } }],
  };
  const urls = extractUrls(issue);
  assert.deepEqual(urls, [
    "https://example.com/a",
    "https://example.com/env",
    "https://jira.example/browse/PROJ-1",
    "https://bitbucket.example/projects/PROJ/repos/sample-app",
  ]);
});

test("candidateReposFromTicket ranks a Bitbucket link highest", () => {
  const repos = {
    "PROJ/sample-app": "/tmp/sample-app",
    "PROJ/other": "/tmp/other",
  };
  const issue = {
    fields: {
      description: "Code in https://bitbucket.example/projects/PROJ/repos/sample-app/browse",
      components: [{ name: "other" }],
    },
  };
  const ranked = candidateReposFromTicket(issue, repos, {});
  assert.equal(ranked[0].repoKey, "PROJ/sample-app");
  assert.equal(ranked[0].score, 100);
  assert.match(ranked[0].reason, /Repository link PROJ\/sample-app/);
});

test("candidateReposFromTicket uses componentRepoMap", () => {
  const repos = { "PROJ/sample-app-rest": "/tmp/sample-app-rest" };
  const issue = {
    fields: {
      components: [{ name: "REST API" }],
      labels: [],
    },
  };
  const ranked = candidateReposFromTicket(issue, repos, { "REST API": "PROJ/sample-app-rest" });
  assert.equal(ranked.length, 1);
  assert.equal(ranked[0].repoKey, "PROJ/sample-app-rest");
  assert.equal(ranked[0].score, 80);
});

test("candidateReposFromTicket matches normalized component/label names", () => {
  const repos = { "CI/sample-app-browser": "/tmp/browser" };
  const issue = {
    fields: {
      labels: ["sample-app-browser"],
      components: [],
    },
  };
  const ranked = candidateReposFromTicket(issue, repos, {});
  assert.equal(ranked.length, 1);
  assert.equal(ranked[0].repoKey, "CI/sample-app-browser");
  assert.equal(ranked[0].score, 60);
});

test("candidateReposFromTicket returns empty when nothing matches", () => {
  const ranked = candidateReposFromTicket(
    { fields: { description: "no links", components: [], labels: [] } },
    { "PROJ/sample-app": "/tmp/sample-app" },
    {},
  );
  assert.deepEqual(ranked, []);
});

test("isStrongRepoMatch is true for a unique top score", () => {
  assert.equal(isStrongRepoMatch([{ repoKey: "a", score: 100 }]), true);
  assert.equal(
    isStrongRepoMatch([
      { repoKey: "a", score: 100 },
      { repoKey: "b", score: 60 },
    ]),
    true,
  );
  assert.equal(
    isStrongRepoMatch([
      { repoKey: "a", score: 80 },
      { repoKey: "b", score: 80 },
    ]),
    false,
  );
  assert.equal(isStrongRepoMatch([]), false);
});

test("parseRepoPick accepts an exact key or none", () => {
  const keys = ["PROJ/sample-app", "PROJ/other"];
  assert.equal(parseRepoPick("PROJ/sample-app", keys), "PROJ/sample-app");
  assert.equal(parseRepoPick("proj/sample-app\n", keys), "PROJ/sample-app");
  assert.equal(parseRepoPick("none", keys), null);
  assert.equal(parseRepoPick("not-a-repo", keys), null);
});

test("parseAnalysis extracts fenced JSON and fills defaults", () => {
  const text = `Here you go:\n\`\`\`json\n{"tldr":"Bug in foo","hypotheses":[{"hypothesis":"null deref","confidence":"HIGH","evidence":"stack"}]}\n\`\`\``;
  const result = parseAnalysis(text);
  assert.equal(result.tldr, "Bug in foo");
  assert.equal(result.hypotheses[0].confidence, "high");
  assert.deepEqual(result.affectedArea, []);
  assert.deepEqual(result.nextSteps, []);
  assert.deepEqual(result.toolsUsed.preferred, {});
});

test("parseAnalysis returns { raw } when JSON is missing", () => {
  const result = parseAnalysis("sorry, I could not produce JSON");
  assert.equal(result.raw, "sorry, I could not produce JSON");
  assert.equal(result.tldr, undefined);
});

test("readMcpServers returns names from a temp claude.json", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-mcp-"));
  const file = path.join(dir, "claude.json");
  fs.writeFileSync(
    file,
    JSON.stringify({ mcpServers: { wiki: {}, "acme-bitbucket": {} } }),
  );
  assert.deepEqual(readMcpServers(file).sort(), ["acme-bitbucket", "wiki"]);
});

test("readMcpServers returns [] for a missing or invalid file", () => {
  assert.deepEqual(readMcpServers("/tmp/definitely-missing-claude-json-xyz"), []);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-mcp-bad-"));
  const file = path.join(dir, "claude.json");
  fs.writeFileSync(file, "{not json");
  assert.deepEqual(readMcpServers(file), []);
});

test("readMcpServers filters out the companion's own MCP server", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-mcp-companion-"));
  const file = path.join(dir, "claude.json");
  fs.writeFileSync(
    file,
    JSON.stringify({ mcpServers: { wiki: {}, "ai-companion": {} } }),
  );
  assert.deepEqual(readMcpServers(file), ["wiki"]);
});


// Defence in depth for read-only analysis jobs (see the mcpGuard hook in
// core/mcp-guard.js, which is the primary control): this list has to name
// every write-shaped tool a currently-connected server exposes, not just
// the handful this feature happens to call directly.

test("buildSummary prefers the analysis tldr", () => {
  const summary = buildSummary({
    issueKey: "PROJ-1",
    repoKey: "PROJ/sample-app",
    analysis: { tldr: "Likely a race in the cache layer." },
  });
  assert.match(summary, /PROJ-1/);
  assert.match(summary, /PROJ\/sample-app/);
  assert.match(summary, /race in the cache/);
});

test("writeAnalysisCache / readAnalysisCache round-trip under a temp root", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-cache-"));
  const entry = {
    issueKey: "proj-99",
    summary: "Something broke",
    repoKey: "PROJ/sample-app",
    repoMatch: "Bitbucket link",
    analysis: { tldr: "Null deref in Foo", toolsUsed: { preferred: {}, mcpServers: [] } },
    completedAt: "2026-09-24T12:00:00.000Z",
  };
  writeAnalysisCache(entry, root);
  const loaded = readAnalysisCache("PROJ-99", root);
  assert.equal(loaded.issueKey, "PROJ-99");
  assert.equal(loaded.summary, "Something broke");
  assert.equal(loaded.repoKey, "PROJ/sample-app");
  assert.equal(loaded.repoMatch, "Bitbucket link");
  assert.equal(loaded.analysis.tldr, "Null deref in Foo");
  assert.equal(loaded.completedAt, entry.completedAt);
  assert.equal(readAnalysisCache("PROJ-1", root), null);
});

test("writeAnalysisCache / readAnalysisCache round-trip a claudeSession", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-cache-session-"));
  const claudeSession = { id: "session-abc", cwd: "/tmp/sample-app", permissionMode: "plan" };
  const entry = {
    issueKey: "proj-100",
    summary: "Something broke",
    repoKey: "PROJ/sample-app",
    repoMatch: "Bitbucket link",
    analysis: { tldr: "Null deref in Foo" },
    completedAt: "2026-09-24T12:00:00.000Z",
    claudeSession,
  };
  writeAnalysisCache(entry, root);
  const loaded = readAnalysisCache("PROJ-100", root);
  assert.deepEqual(loaded.claudeSession, claudeSession);
});

test("readAnalysisCache still reads an old entry with no claudeSession", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-cache-old-"));
  const filePath = analysisCachePath("PROJ-101", root);
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(
    filePath,
    JSON.stringify({
      issueKey: "PROJ-101",
      summary: "Legacy entry",
      analysis: { tldr: "x" },
      completedAt: "2026-01-01T00:00:00.000Z",
    }),
  );
  const loaded = readAnalysisCache("PROJ-101", root);
  assert.ok(loaded);
  assert.equal(loaded.summary, "Legacy entry");
  assert.equal(loaded.claudeSession, undefined);
});

test("readAnalysisCache drops a malformed claudeSession instead of trusting it", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-cache-malformed-"));

  const missingId = analysisCachePath("PROJ-102", root);
  fs.mkdirSync(path.dirname(missingId), { recursive: true });
  fs.writeFileSync(
    missingId,
    JSON.stringify({
      issueKey: "PROJ-102",
      analysis: { tldr: "x" },
      completedAt: "2026-01-01T00:00:00.000Z",
      claudeSession: { cwd: "/tmp/x", permissionMode: "plan" }, // missing id
    }),
  );
  assert.equal(readAnalysisCache("PROJ-102", root).claudeSession, undefined);

  const badPermissionMode = analysisCachePath("PROJ-103", root);
  fs.writeFileSync(
    badPermissionMode,
    JSON.stringify({
      issueKey: "PROJ-103",
      analysis: { tldr: "x" },
      completedAt: "2026-01-01T00:00:00.000Z",
      claudeSession: { id: "s1", cwd: "/tmp/x", permissionMode: "bypassPermissions" },
    }),
  );
  assert.equal(readAnalysisCache("PROJ-103", root).claudeSession, undefined);

  const nonStringCwd = analysisCachePath("PROJ-104", root);
  fs.writeFileSync(
    nonStringCwd,
    JSON.stringify({
      issueKey: "PROJ-104",
      analysis: { tldr: "x" },
      completedAt: "2026-01-01T00:00:00.000Z",
      claudeSession: { id: "s1", cwd: 42, permissionMode: "default" },
    }),
  );
  assert.equal(readAnalysisCache("PROJ-104", root).claudeSession, undefined);
});

test("readAnalysisCache returns null for missing or invalid files", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-cache-bad-"));
  assert.equal(readAnalysisCache("PROJ-1", root), null);
  const badPath = analysisCachePath("PROJ-2", root);
  fs.mkdirSync(path.dirname(badPath), { recursive: true });
  fs.writeFileSync(badPath, "{not json");
  assert.equal(readAnalysisCache("PROJ-2", root), null);
  fs.writeFileSync(
    analysisCachePath("PROJ-3", root),
    JSON.stringify({ issueKey: "PROJ-3", analysis: { tldr: "x" } }),
  );
  assert.equal(readAnalysisCache("PROJ-3", root), null);
});

test("formatAnalysisAsMarkdownComment builds readable Markdown with AI note", () => {
  const { formatAnalysisAsMarkdownComment } = require("./plan.js");
  const body = formatAnalysisAsMarkdownComment({
    issueKey: "PROJ-1",
    summary: "Null deref",
    repoKey: "PROJ/sample-app",
    repoMatch: "Bitbucket link",
    analysis: {
      tldr: "Likely NPE in Foo.bar",
      affectedArea: [{ path: "src/Foo.java", symbol: "bar", why: "stack points here" }],
      hypotheses: [{ hypothesis: "missing null check", confidence: "high", evidence: "NPE at L42" }],
      reproScope: "Only on tenant X",
      referencesReviewed: [{ url: "https://example.com/doc", takeaway: "mentions Foo" }],
      nextSteps: ["Add null guard", "Add unit test"],
      openQuestions: ["Is tenant X on a fork?"],
    },
  });
  assert.match(body, /AI Dev Companion/);
  assert.match(body, /### AI issue analysis — PROJ-1/);
  assert.match(body, /\*\*Summary:\*\* Null deref/);
  assert.match(body, /`PROJ\/sample-app`/);
  assert.match(body, /#### Initial assessment/);
  assert.match(body, /Likely NPE in Foo\.bar/);
  assert.match(body, /missing null check/);
  assert.match(body, /1\. Add null guard/);
  assert.match(body, /\[https:\/\/example\.com\/doc\]\(https:\/\/example\.com\/doc\)/);
});

test("formatAnalysisAsMarkdownComment falls back to raw fenced text", () => {
  const { formatAnalysisAsMarkdownComment } = require("./plan.js");
  const body = formatAnalysisAsMarkdownComment({
    issueKey: "PROJ-2",
    analysis: { raw: "Could not produce JSON\nline 2" },
  });
  assert.match(body, /```/);
  assert.match(body, /Could not produce JSON/);
});

test("markdownToJiraWiki converts headings, emphasis, lists, links, and code", () => {
  const { markdownToJiraWiki } = require("./plan.js");
  const wiki = markdownToJiraWiki(
    [
      "### Title",
      "",
      "_italic note_",
      "",
      "**Summary:** value",
      "",
      "#### Section",
      "- `path/file` — why",
      "- **[high]** hyp — _ev_",
      "1. First step",
      "2. Second step",
      "- [https://ex.com](https://ex.com) — takeaway",
      "",
      "```",
      "raw line",
      "```",
      "",
    ].join("\n"),
  );
  assert.match(wiki, /h3\. Title/);
  assert.match(wiki, /h4\. Section/);
  assert.match(wiki, /_italic note_/);
  assert.match(wiki, /\*Summary:\* value/);
  assert.match(wiki, /\* \{\{path\/file\}\}/);
  assert.match(wiki, /\* \*\[high\]\* hyp — _ev_/);
  assert.match(wiki, /# First step/);
  assert.match(wiki, /# Second step/);
  assert.match(wiki, /\[https:\/\/ex\.com\|https:\/\/ex\.com\]/);
  assert.match(wiki, /\{code\}\nraw line\n\{code\}/);
});

test("formatAnalysisAsWikiComment still yields wiki via MD round-trip", () => {
  const { formatAnalysisAsWikiComment } = require("./plan.js");
  const body = formatAnalysisAsWikiComment({
    issueKey: "PROJ-1",
    summary: "Null deref",
    repoKey: "PROJ/sample-app",
    analysis: {
      tldr: "Likely NPE",
      nextSteps: ["Add null guard"],
    },
  });
  assert.match(body, /AI Dev Companion/);
  assert.match(body, /h3\. AI issue analysis — PROJ-1/);
  assert.match(body, /Likely NPE/);
  assert.match(body, /# Add null guard/);
});

test("isValidAnalysisCache requires issueKey, analysis, and completedAt", () => {
  assert.equal(isValidAnalysisCache(null), false);
  assert.equal(
    isValidAnalysisCache({
      issueKey: "PROJ-1",
      analysis: { tldr: "ok" },
      completedAt: "2026-01-01T00:00:00.000Z",
    }),
    true,
  );
  assert.equal(
    isValidAnalysisCache(
      {
        issueKey: "PROJ-2",
        analysis: { tldr: "ok" },
        completedAt: "2026-01-01T00:00:00.000Z",
      },
      "PROJ-1",
    ),
    false,
  );
});

test("a background run is set up read-only-strict: no MCP servers, no links, the stricter policy; an interactive run is unchanged", () => {
  const read = () => ["wiki", "jira"];
  assert.deepEqual(plan.analysisRunOptions(false, read), {
    policy: "readOnly",
    mcpServers: ["wiki", "jira"],
    extraAllowedTools: ["mcp__wiki", "mcp__jira"],
    useUrls: true,
  });
  let readCalled = false;
  assert.deepEqual(
    plan.analysisRunOptions(true, () => {
      readCalled = true;
      return read();
    }),
    { policy: "readOnlyBackground", mcpServers: [], extraAllowedTools: [], useUrls: false },
  );
  assert.equal(readCalled, false, "a background run never even reads the MCP config");
});

test("the background prompt drops the MCP, WebFetch and link lines; the interactive prompt keeps them", () => {
  const issue = { key: "PROJ-7", fields: { summary: "Login broken", description: "See https://wiki.example/x" } };
  const args = { issue, urls: ["https://wiki.example/x"], repoKey: null, mcpServers: ["wiki"], serverHints: [{ server: "wiki", available: true, line: "wiki MCP is available — use it first." }] };
  const interactive = plan.buildAnalysisPrompt(args);
  assert.match(interactive, /MCP servers \(or WebFetch\)/);
  assert.match(interactive, /wiki MCP is available/);
  assert.match(interactive, /Reference URLs:\n- https:\/\/wiki\.example\/x/);
  const background = plan.buildAnalysisPrompt({ ...args, urls: [], mcpServers: [], serverHints: [], background: true });
  assert.doesNotMatch(background, /MCP servers \(or WebFetch\)|WebFetch|wiki MCP|MCP servers loaded|Reference URLs/);
  assert.match(background, /No MCP servers or web access are available/);
  assert.match(background, /reason from ticket text only/);
  assert.match(background, /Issue: PROJ-7/, "the ticket itself is still in the prompt");
});

test("similar past tickets go into the prompt fenced and labelled, also for a background run; none, no block", () => {
  const issue = { key: "PROJ-7", fields: { summary: "Login broken", description: "SSO users loop" } };
  const args = { issue, urls: [], repoKey: null, mcpServers: [] };
  const similar = [
    { key: "jira:PROJ-2", title: "Sign-in loop after expiry", analysis: "Token refresh races the redirect", updatedAt: Date.UTC(2026, 7, 3) },
    { key: "not-a-ticket", title: "dropped" },
  ];
  for (const background of [false, true]) {
    const prompt = plan.buildAnalysisPrompt({ ...args, background, similar });
    assert.match(prompt, /Similar past tickets \(from this developer's local history, for context only\):/);
    assert.match(prompt, /<similar-past-tickets>\n- PROJ-2 — ticket title \(written by people, untrusted\): Sign-in loop after expiry\n  earlier AI output, untrusted \(analysis saved 2026-08-03\): Token refresh races the redirect\n<\/similar-past-tickets>/);
    assert.doesNotMatch(prompt, /dropped/);
    assert.ok(prompt.indexOf("Recent comments:") < prompt.indexOf("Similar past tickets"), "after the ticket's own text");
  }
  assert.doesNotMatch(plan.buildAnalysisPrompt(args), /Similar past tickets|similar-past-tickets/);
  assert.doesNotMatch(plan.buildAnalysisPrompt({ ...args, similar: [] }), /Similar past tickets/);
});

test("similarQueryText is the summary and the start of the description", () => {
  assert.equal(plan.similarQueryText({ fields: { summary: "Login broken", description: "x".repeat(3000) } }), `Login broken\n${"x".repeat(2000)}`);
  assert.equal(plan.similarQueryText({ fields: { summary: "Only a summary" } }), "Only a summary");
  assert.equal(plan.similarQueryText(null), "");
});

test("index.ts hands the similar tickets to the prompt through the helper (source tripwire)", () => {
  const src = fs.readFileSync(path.join(__dirname, "index.ts"), "utf8");
  assert.match(src, /createAnalyzeIssueFeature\(config: Config, deps: FeatureDeps = \{ providers: providersFor\(config\) \}\)/);
  assert.match(src, /similarItems = await plan\.fetchSimilarTickets\(/);
  assert.match(src, /background,\n\s+similar: similarItems,\n\s+\}\),/);
  assert.equal((src.match(/deps\.findSimilar/g) || []).length, 2, "both the cached and the forced path get it");
});

test("fetchSimilarTickets asks for more than five, keeps five tickets and drops PRs", async () => {
  const asked = [];
  const items = [
    ...Array.from({ length: 3 }, (_, i) => ({ key: `bitbucket:CI/sample-app#${i + 1}`, kind: "pr" })),
    ...Array.from({ length: 8 }, (_, i) => ({ key: `jira:PROJ-${i + 1}`, kind: "ticket" })),
  ];
  const got = await plan.fetchSimilarTickets(async (q) => (asked.push(q), { enabled: true, mode: "text", items }), { key: "PROJ-9", text: "x" });
  assert.equal(asked[0].k, 20);
  assert.deepEqual(got.map((i) => i.key), ["jira:PROJ-1", "jira:PROJ-2", "jira:PROJ-3", "jira:PROJ-4", "jira:PROJ-5"]);
});

test("fetchSimilarTickets never fails a run: a rejection, junk, or a hang gives an empty list", async () => {
  const warned = [];
  const warn = (m) => warned.push(m);
  assert.deepEqual(await plan.fetchSimilarTickets(async () => { throw new Error("db locked"); }, { key: "PROJ-1", text: "x" }, { warn }), []);
  assert.deepEqual(warned, ["db locked"]);
  assert.deepEqual(await plan.fetchSimilarTickets(() => { throw new Error("sync"); }, { key: "PROJ-1", text: "x" }, { warn }), []);
  assert.deepEqual(await plan.fetchSimilarTickets(async () => null, { key: "PROJ-1", text: "x" }), []);
  assert.deepEqual(await plan.fetchSimilarTickets(undefined, { key: "PROJ-1", text: "x" }), []);
  const started = Date.now();
  assert.deepEqual(await plan.fetchSimilarTickets(() => new Promise(() => {}), { key: "PROJ-1", text: "x" }, { timeoutMs: 30, warn }), []);
  assert.ok(Date.now() - started < 2000);
  assert.equal(warned.at(-1), "timed out");
});

test("repoHintsFromSimilar lists still-cloned repos of similar tickets, once each, in order", () => {
  const keys = ["ACME/sample-app", "ACME/api"];
  const hints = plan.repoHintsFromSimilar(
    [{ repo: "sample-app/API" }, { repo: "ACME/gone" }, { repo: null }, {}, { repo: "ACME/api" }, { repo: "ACME/sample-app" }],
    keys,
  );
  assert.deepEqual(hints, ["ACME/api", "ACME/sample-app"]);
  assert.deepEqual(plan.repoHintsFromSimilar(undefined, keys), []);
});

test("buildRepoPickPrompt mentions the hints only when there are some", () => {
  const issue = { key: "ICI-1", fields: { summary: "s" } };
  assert.doesNotMatch(plan.buildRepoPickPrompt(issue, ["A/b"]), /similar past tickets/);
  assert.match(plan.buildRepoPickPrompt(issue, ["A/b"], ["A/b"]), /similar past tickets were analyzed in.*A\/b/);
});

test("componentMappingsToLearn learns only what the user confirmed or chose, for one component, nothing already mapped", () => {
  const one = { fields: { components: [{ name: "Login UI" }] } };
  assert.deepEqual(plan.componentMappingsToLearn(one, "W/web", "You confirmed it (Claude pick)", {}), { "Login UI": "W/web" });
  assert.deepEqual(plan.componentMappingsToLearn(one, "W/web", "You chose it", undefined), { "Login UI": "W/web" });
  assert.deepEqual(plan.componentMappingsToLearn(one, "W/web", "Claude pick", {}), {});
  assert.deepEqual(plan.componentMappingsToLearn(one, "W/web", "Bitbucket link W/web", {}), {});
  assert.deepEqual(plan.componentMappingsToLearn(one, "W/web", 'Mapped from "Login UI"', {}), {});
  assert.deepEqual(plan.componentMappingsToLearn(one, "W/web", "You chose it", { "login ui": "W/other" }), {});
  const two = { fields: { components: [{ name: "A" }, { name: "B" }] } };
  assert.deepEqual(plan.componentMappingsToLearn(two, "W/web", "You chose it", {}), {});
  assert.deepEqual(plan.componentMappingsToLearn({ fields: {} }, "W/web", "You chose it", {}), {});
  assert.deepEqual(plan.componentMappingsToLearn(one, null, "You chose it", {}), {});
});

test("isSettledRepoMatch is true only when a component mapping gives the proposed repo", () => {
  const issue = { fields: { components: [{ name: "Login UI" }], labels: ["sso"] } };
  assert.equal(plan.isSettledRepoMatch(issue, "W/web", { "Login UI": "W/web" }), true);
  assert.equal(plan.isSettledRepoMatch(issue, "w/WEB", { "Login UI": "W/web" }), true);
  assert.equal(plan.isSettledRepoMatch(issue, "W/web", { sso: "W/web" }), true);
  assert.equal(plan.isSettledRepoMatch(issue, "W/api", { "Login UI": "W/web" }), false);
  assert.equal(plan.isSettledRepoMatch(issue, "W/web", {}), false);
  assert.equal(plan.isSettledRepoMatch(issue, "W/web", undefined), false);
  assert.equal(plan.isSettledRepoMatch(issue, null, { "Login UI": "W/web" }), false);
  assert.equal(plan.isSettledRepoMatch({ fields: {} }, "W/web", { "Login UI": "W/web" }), false);
});

test("repoChoiceOptions lists the proposal first, then candidates, hints and the rest, each once", () => {
  const repoKeys = ["W/zed", "W/api", "W/web", "W/docs"];
  const out = plan.repoChoiceOptions({
    proposal: "W/web",
    proposalReason: "Claude pick",
    candidates: [{ repoKey: "W/api", reason: "Name match" }, { repoKey: "W/web", reason: "Name match" }, { repoKey: "W/gone", reason: "x" }],
    hintKeys: ["W/docs", "W/api"],
    repoKeys,
  });
  assert.deepEqual(out, [
    { repoKey: "W/web", reason: "Claude pick" },
    { repoKey: "W/api", reason: "Name match" },
    { repoKey: "W/docs", reason: "used for a similar past ticket" },
    { repoKey: "W/zed", reason: "" },
  ]);
  assert.deepEqual(plan.repoChoiceOptions({ proposal: null, candidates: [], hintKeys: [], repoKeys: ["B/b", "A/a"] }).map((o) => o.repoKey), ["A/a", "B/b"]);
  assert.equal(plan.repoChoiceOptions({ proposal: null, repoKeys: Array.from({ length: 50 }, (_, i) => `P/r${i}`) }).length, 30);
});

test("confirmedReason says whether the user confirmed the proposal or picked another", () => {
  assert.equal(plan.confirmedReason("W/web", "W/web", "Claude pick"), "You confirmed it (Claude pick)");
  assert.equal(plan.confirmedReason("W/web", "W/web", ""), "You confirmed it");
  assert.equal(plan.confirmedReason("W/api", "W/web", "Claude pick"), "You chose it");
  assert.equal(plan.confirmedReason("W/api", null, ""), "You chose it");
});

const BASE = "https://bitbucket.example.com";
const devStatus = (...prs) => ({ detail: [{ pullRequests: prs }] });

test("linkedPullRequests reads Jira's development panel and PR addresses in the ticket, once each", () => {
  const issue = {
    fields: { description: `See ${BASE}/projects/ACME/repos/sample-app/pull-requests/12/overview and ${BASE}/projects/ACME/repos/api/pull-requests/7` },
    remoteLinks: [{ object: { url: `${BASE}/projects/ACME/repos/sample-app/pull-requests/12` } }],
  };
  const out = plan.linkedPullRequests(
    issue,
    devStatus({ url: `${BASE}/projects/ACME/repos/sample-app/pull-requests/12`, name: "PROJ-1: fix\nlogin", status: "merged" }, { url: "https://evil.example/x" }, null),
    BASE,
  );
  assert.deepEqual(out, [
    { repoKey: "ACME/sample-app", project: "ACME", repo: "sample-app", id: 12, title: "PROJ-1: fix login", state: "MERGED", url: `${BASE}/projects/ACME/repos/sample-app/pull-requests/12` },
    { repoKey: "ACME/api", project: "ACME", repo: "api", id: 7, title: null, state: null, url: `${BASE}/projects/ACME/repos/api/pull-requests/7` },
  ]);
});

test("linkedPullRequests rebuilds the link on the configured host, caps the list, and copes with nothing", () => {
  const foreign = plan.linkedPullRequests({}, devStatus({ url: "https://other.example/projects/ACME/repos/sample-app/pull-requests/3" }), BASE);
  assert.equal(foreign[0].url, `${BASE}/projects/ACME/repos/sample-app/pull-requests/3`);
  assert.equal(plan.linkedPullRequests({}, devStatus({ url: "https://o/projects/ACME/repos/sample-app/pull-requests/3" }), "not a url")[0].url, null);
  const many = devStatus(...Array.from({ length: 30 }, (_, i) => ({ url: `${BASE}/projects/ACME/repos/sample-app/pull-requests/${i + 1}` })));
  assert.equal(plan.linkedPullRequests({}, many, BASE).length, 10);
  for (const bad of [undefined, null, {}, { detail: "x" }, { detail: [null, { pullRequests: "x" }] }]) {
    assert.deepEqual(plan.linkedPullRequests({}, bad, BASE), [], JSON.stringify(bad));
  }
});

test("a repo with a linked PR outranks every other signal, and is the top offered option", () => {
  const repos = { "ACME/sample-app": "/c/sample-app", "ACME/api": "/c/api" };
  const issue = { fields: { components: [{ name: "api" }] } };
  const prs = plan.linkedPullRequests(issue, devStatus({ url: `${BASE}/projects/acme/repos/Sample-App/pull-requests/12`, name: "fix", status: "OPEN" }), BASE);
  const c = plan.candidateReposFromTicket(issue, repos, { api: "ACME/api" }, prs);
  assert.deepEqual(c[0], { repoKey: "ACME/sample-app", score: 120, reason: "Linked PR #12 in acme/Sample-App" });
  assert.equal(plan.isStrongRepoMatch(c), true);
  const options = plan.repoChoiceOptions({ proposal: "ACME/api", proposalReason: "Mapped", candidates: c, hintKeys: [], repoKeys: Object.keys(repos), linkedPrs: prs });
  assert.deepEqual(options.map((o) => o.repoKey), ["ACME/sample-app", "ACME/api"]);
  assert.deepEqual(options[0], { repoKey: "ACME/sample-app", reason: "" });
});

test("the repo pick prompt lists linked PRs only when there are some", () => {
  const issue = { key: "ICI-1", fields: { summary: "s" } };
  assert.doesNotMatch(plan.buildRepoPickPrompt(issue, ["A/b"]), /already linked/);
  const p = plan.buildRepoPickPrompt(issue, ["A/b"], [], [{ repoKey: "A/b", id: 4, state: "OPEN", title: "fix it" }]);
  assert.match(p, /already linked to this ticket[^\n]*\n- A\/b #4 \(OPEN\): fix it/);
});

test("comment summary: prompt carries the analysis; assembled comment keeps the header and stays under the word cap", () => {
  const { buildCommentSummaryPrompt, assembleSummaryComment, COMMENT_SUMMARY_MAX_WORDS } = require("./plan.js");
  assert.match(buildCommentSummaryPrompt("FULL TEXT"), /<analysis>\nFULL TEXT\n<\/analysis>/);

  const short = assembleSummaryComment({ issueKey: "PROJ-1", summaryText: "```markdown\n- NPE in Foo.bar\n```" });
  assert.match(short, /^### AI issue analysis — PROJ-1\n/);
  assert.match(short, /AI Dev Companion/);
  assert.match(short, /- NPE in Foo\.bar\n$/);
  assert.doesNotMatch(short, /```/);

  const long = assembleSummaryComment({ issueKey: "PROJ-1", summaryText: Array(600).fill("word").join(" ") });
  assert.ok(long.split(/\s+/).filter(Boolean).length <= COMMENT_SUMMARY_MAX_WORDS);
  assert.match(long, /…\n$/);

  assert.equal(assembleSummaryComment({ issueKey: "PROJ-1", summaryText: "  " }), "");
});
