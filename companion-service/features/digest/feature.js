// digest: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.

module.exports = {
  descriptor: {
    id: "digest",
    summary: "Your PRs, reviews and tickets at a glance",
    label: "Morning digest",
    description:
      "Your PRs (conflicts, builds, approvals), PRs waiting on your review, your open tickets and what the background watchers prepared — read-only, on a click (and, if you turn it on, every weekday morning).",
    requiredChecks: [],
    async promptSetup() {
      return {};
    },
  },
  factory: () => require("./index").createDigestFeature,
  extension: { script: "features/digest.js", order: 110 },
  persist: false,
  history: { readOnly: true, eventOnly: true },
  mcpTools: [
  {
    name: "get_digest",
    featureId: "digest",
    kind: "read",
    generic: false,
    description:
      "Today's digest for the developer: their open pull requests (conflicts, failing builds, approvals, needs-work), " +
      "pull requests waiting on their review, their open Jira tickets, and what the background watchers prepared. Read-only.",
    params: {},
  },
  ],
};
