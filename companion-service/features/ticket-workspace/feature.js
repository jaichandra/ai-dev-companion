// ticket-workspace: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.

module.exports = {
  descriptor: {
    id: "ticket-workspace",
    summary: "See a ticket's branches, PRs, builds and past analyses",
    label: "Ticket workspace",
    description:
      "On a Jira ticket, show its local branches and worktrees, their Bitbucket PRs and builds, and earlier analyses and Claude sessions — read-only, on a click.",
    requiredChecks: ["git"],
    needsRepos: true,
    async promptSetup() {
      return {};
    },
  },
  factory: () => require("./index").createTicketWorkspaceFeature,
  extension: { script: "features/ticket-workspace.js", order: 80 },
  scopeKey: "jira-issue",
  persist: false,
  history: { readOnly: true, eventOnly: true },
  mcpTools: [
  {
    name: "ticket_workspace",
    featureId: "ticket-workspace",
    kind: "read",
    generic: false,
    description:
      "Everything the companion can find for a Jira ticket on this machine: local branches and worktrees whose name " +
      "has the key (uncommitted changes, commits ahead), their Bitbucket pull requests and build state, and earlier " +
      "analyses and Claude Code sessions from the local history. Read-only. Use it to pick up work on a ticket.",
    params: {
      issueKey: { type: "issueKey", description: "The Jira issue key, e.g. \"PROJ-1\"." },
    },
  },
  ],
};
