// diagnose-build: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.

module.exports = {
  descriptor: {
    id: "diagnose-build",
    summary: "On a failed Jenkins build, find out why with Claude Code",
    label: "Diagnose failed builds",
    description:
      "On a failed Jenkins build, open Claude Code to work out why — through a pack's diagnose command when one is installed, otherwise a plain read-only diagnosis.",
    enabledByDefault: true,
    requiredChecks: ["claudeCli"],
    async promptSetup() {
      return {};
    },
  },
  factory: () => require("./index").createDiagnoseBuildFeature,
  extension: { script: "features/diagnose-build.js", order: 40 },
};
