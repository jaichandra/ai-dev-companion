// summarize-comments: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.
const { hasAgent } = require("../../core/setup-helpers.js");

module.exports = {
  descriptor: {
    id: "summarize-comments",
    summary: "Short summary of a Jira ticket's comment thread",
    label: "Summarize comments",
    description:
      "A short summary of a Jira ticket's comment thread (decisions, open questions, next steps), written by Claude Haiku. Read-only; only offered on tickets that have comments.",
    requiredChecks: ["claudeCli", "claudeAuth"],
    async promptSetup(rl, helpers, existingConfig) {
      const current = existingConfig.summarizeComments?.model || "claude-haiku-4-5-20251001";
      const model = hasAgent()
        ? current
        : await helpers.ask(rl, "Claude model to use for Summarize comments (Enter keeps Haiku)", current);
      return { summarizeComments: { model } };
    },
  },
  factory: () => require("./index").createSummarizeCommentsFeature,
  extension: { script: "features/summarize-comments.js", order: 100 },
};
