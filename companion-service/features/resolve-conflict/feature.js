// resolve-conflict: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.

module.exports = {
  descriptor: {
    id: "resolve-conflict",
    summary: "Fix a PR's merge conflicts with Claude, in a throwaway worktree",
    // Setup-wizard-facing name only — the extension feature file (chrome-extension/features/<id>.js)'s own
    // menuLabel ("Resolve Conflict") is deliberately left as-is; the two
    // are allowed to read differently since they're different surfaces
    // (a terminal prompt vs. the ✨ menu), not different features.
    label: "Resolve Merge Conflicts",
    description: "Detect a merge-conflicted Bitbucket PR and resolve it with Claude in an isolated git worktree.",
    requiredChecks: ["git", "claudeCli", "claudeAuth"],
    needsRepos: true,
  },
  factory: () => require("./index").createResolveConflictFeature,
  extension: { script: "features/resolve-conflict.js", order: 10 },
  scopeKey: "pr",
  mcpTools: [
  {
    name: "start_resolve_conflict",
    featureId: "resolve-conflict",
    kind: "pending-start",
    generic: false,
    description:
      "Queues a Resolve Conflict job for a Bitbucket PR. Nothing runs until the user clicks Start on the PR page " +
      "in Chrome; tell them to open it.",
    params: {
      project: { type: "repoSegment", description: "The Bitbucket project key, e.g. \"CI\"." },
      repo: { type: "repoSegment", description: "The Bitbucket repo slug, e.g. \"sample-app\"." },
      prId: { type: "positiveInt", description: "The pull request number." },
    },
  },
  ],
};
