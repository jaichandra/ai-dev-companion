// create-jira-subtasks: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.
const { DEFAULT_JIRA_BASE_URL, platformTokenSource, logAuthNote } = require("../../core/setup-helpers.js");

module.exports = {
  descriptor: {
    id: "create-jira-subtasks",
    summary: "Add several subtasks to a Jira Story or Epic at once",
    // Setup-wizard-facing name only — see the same note on resolve-conflict
    // above; the extension feature file (chrome-extension/features/<id>.js)'s menuLabel ("Create Subtasks")
    // is unchanged.
    label: "Create JIRA Subtasks (multiple at a time)",
    description: "Add subtasks to a Jira Story/Epic, with search-as-you-type assignees.",
    requiredChecks: [],
    async promptSetup(rl, helpers, existingConfig) {
      const jiraBaseUrl = existingConfig.jira?.baseUrl || DEFAULT_JIRA_BASE_URL;
      const source = platformTokenSource(helpers, "jira", jiraBaseUrl);
      logAuthNote(helpers, "Jira", source);
      const jiraApiToken = source
        ? existingConfig.jira?.apiToken || ""
        : await helpers.ask(
            rl,
            "Jira API token (leave blank to rely on browser SSO)",
            existingConfig.jira?.apiToken || "",
          );
      return { jira: { baseUrl: jiraBaseUrl, apiToken: jiraApiToken } };
    },
  },
  factory: () => require("./index").createJiraSubtasksFeature,
  extension: { script: "features/create-jira-subtasks.js", order: 30 },
};
