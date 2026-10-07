// address-review-comments: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.
const { DEFAULT_BITBUCKET_BASE_URL, platformTokenSource, logAuthNote } = require("../../core/setup-helpers.js");

module.exports = {
  descriptor: {
    id: "address-review-comments",
    summary: "Have the AI fix a PR's open review comments; you approve the diff",
    label: "Address review comments",
    description:
      "Have Claude fix a Bitbucket PR's open review comments in an isolated worktree; you review the diff, then push and reply.",
    requiredChecks: ["git", "claudeCli", "claudeAuth"],
    needsRepos: true,
    async promptSetup(rl, helpers, existingConfig) {
      const baseUrl = existingConfig.bitbucket?.baseUrl || DEFAULT_BITBUCKET_BASE_URL;
      logAuthNote(helpers, "Bitbucket", platformTokenSource(helpers, "bitbucket", baseUrl));
      return { bitbucket: { ...existingConfig.bitbucket, baseUrl } };
    },
  },
  factory: () => require("./index").createAddressReviewCommentsFeature,
  extension: { script: "features/address-review-comments.js", order: 20 },
  scopeKey: "pr",
  mcpTools: [
  {
    name: "start_address_review_comments",
    featureId: "address-review-comments",
    kind: "pending-start",
    generic: false,
    description:
      "Queues an Address Review Comments job for a Bitbucket PR. Nothing runs until the user clicks Start on the " +
      "PR page in Chrome; tell them to open it.",
    params: {
      project: { type: "repoSegment", description: "The Bitbucket project key, e.g. \"CI\"." },
      repo: { type: "repoSegment", description: "The Bitbucket repo slug, e.g. \"sample-app\"." },
      prId: { type: "positiveInt", description: "The pull request number." },
    },
  },
  ],
};
