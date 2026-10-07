// ticket-to-pr: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.

module.exports = {
  descriptor: {
    id: "ticket-to-pr",
    summary: "Start a fix branch from a ticket, then open the PR",
    label: "Ticket to PR",
    description:
      "Start fix: a worktree and branch for a Jira ticket, with Claude Code in plan mode on its analysis. Create PR: push, open the PR with default reviewers, link it on the ticket and move it to review.",
    requiredChecks: ["git", "claudeCli", "claudeAuth"],
    needsRepos: true,
    async promptSetup() {
      return {};
    },
  },
  factory: () => require("./index").createTicketToPrFeature,
  extension: { script: "features/ticket-to-pr.js", order: 90 },
  scopeKey: "jira-issue",
  history: { milestone: true },
};
