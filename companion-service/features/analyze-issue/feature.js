// analyze-issue: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.
const { DEFAULT_JIRA_BASE_URL, hasAgent, platformTokenSource, logAuthNote } = require("../../core/setup-helpers.js");

module.exports = {
  descriptor: {
    id: "analyze-issue",
    summary: "Read-only Claude analysis of a Jira issue and the repo it touches",
    // Setup-wizard-facing name only — see the same note on resolve-conflict
    // above; the extension feature file (chrome-extension/features/<id>.js)'s menuLabel ("Analyze ticket")
    // is unchanged.
    label: "Analyze a Jira issue",
    description:
      "Fetch an issue from the configured Jira projects (default: PROJ Bugs), identify the local repo, and run a read-only Claude analysis (with WebFetch and MCP servers from ~/.claude.json).",
    requiredChecks: ["claudeCli", "claudeAuth"],
    needsRepos: true,
    async promptSetup(rl, helpers, existingConfig) {
      // Not asked when Claude Code or Cursor is available: the model stays
      // whatever it was (blank = the tool's own default), and Settings can
      // change it later.
      const model = hasAgent()
        ? existingConfig.analyzeIssue?.model || ""
        : (
            await helpers.ask(
              rl,
              "Claude model to use for Analyze ticket (Enter keeps Claude Code's default)",
              existingConfig.analyzeIssue?.model || "",
            )
          ).trim();
      const analyzeIssue = { ...(existingConfig.analyzeIssue || {}) };
      if (model) analyzeIssue.model = model;
      else delete analyzeIssue.model;
      const partial = { analyzeIssue };
      // Only ask for the Jira base URL when it isn't already set — create-jira-subtasks
      // (or a prior analyze-issue run) may have written it already.
      if (!existingConfig.jira?.baseUrl) {
        const jiraBaseUrl = DEFAULT_JIRA_BASE_URL;
        logAuthNote(helpers, "Jira", platformTokenSource(helpers, "jira", jiraBaseUrl));
        partial.jira = { ...(existingConfig.jira || {}), baseUrl: jiraBaseUrl };
      }
      return partial;
    },
  },
  factory: () => require("./index").createAnalyzeIssueFeature,
  extension: { script: "features/analyze-issue.js", order: 70 },
  scopeKey: "jira-issue",
  history: { readOnly: true },
  mcpTools: [
  {
    name: "get_issue_analysis",
    featureId: "analyze-issue",
    kind: "read",
    generic: false,
    description:
      "Get the stored analysis for a Jira issue that analyze_issue already ran, without starting a new run.",
    params: {
      issueKey: { type: "issueKey", description: "The Jira issue key, e.g. \"PROJ-1\"." },
    },
  },
  {
    name: "analyze_issue",
    featureId: "analyze-issue",
    kind: "start",
    generic: false,
    description:
      "Start a read-only Claude analysis of a Jira issue: fetch the ticket, identify the local repo it concerns, " +
      "and produce a written analysis. Runs immediately — analyze-issue's Claude policy is read-only, so nothing " +
      "it does can push, comment or otherwise change anything. It accepts issues of the projects and issue types configured in Settings (default: PROJ Bugs).",
    params: {
      issueKey: { type: "issueKey", description: "The Jira issue key, e.g. \"PROJ-1\"." },
      force: {
        type: "boolean",
        optional: true,
        description: "Re-run even if a cached analysis for this issue already exists.",
      },
    },
  },
  ],
};
