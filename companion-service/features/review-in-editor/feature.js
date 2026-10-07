// review-in-editor: everything the framework needs to know about this feature in one place — its setup
// descriptor, how to load its implementation, and the MCP tools, scope-key rule, persistence and
// history behaviour that used to be spread over the registry, server.ts, mcp-tools.js, scope-key.js,
// job-files.js and history-record.js. Listed in a pack (packs/*.js); see core/packs.js.
const { REVIEW_EDITORS } = require("../../core/setup-helpers.js");
const prereqs = require("../../core/prereqs.js");

module.exports = {
  descriptor: {
    id: "review-in-editor",
    summary: "Check out a PR and have your editor's AI review it",
    // Setup-wizard-facing name only — see the same note on resolve-conflict
    // above; the extension feature file (chrome-extension/features/<id>.js)'s menuLabel ("Review PR")
    // is unchanged.
    label: "Review PRs in your editor",
    description: "Check out a PR's source branch into a dedicated per-repo worktree and open it in your editor.",
    // A missing editor isn't a hard prerequisite — it just makes the
    // feature fail with a clear message until `npm run setup` is re-run
    // to pick one (see core/editor.ts's runtime fallback) — unlike git,
    // which this can't function at all without.
    requiredChecks: ["git"],
    needsRepos: true,
    async promptSetup(rl, helpers, existingConfig) {
      // No question when the answer is obvious: a saved choice, else Claude
      // Code if installed, else Cursor (prereqs.detectInstalledEditors lists
      // them in that order). Only a machine with neither is asked.
      const saved = REVIEW_EDITORS.some((e) => e.id === existingConfig.reviewEditor) ? existingConfig.reviewEditor : "";
      const picked = saved || prereqs.detectInstalledEditors()[0]?.id;
      if (picked) {
        const label = REVIEW_EDITORS.find((e) => e.id === picked).label.replace(/ \(.*\)$/, "");
        if (helpers.log) helpers.log(`Review PR editor: ${label}${saved ? "" : " (detected)"}.`);
        return { reviewEditor: picked };
      }
      const reviewEditor = await helpers.select(
        rl,
        'Which editor should "Review PR" open PR branches in? (Neither Claude Code nor Cursor was found.)',
        REVIEW_EDITORS,
        "claude-code",
      );
      return { reviewEditor };
    },
  },
  factory: () => require("./index").createReviewInEditorFeature,
  extension: { script: "features/review-in-editor.js", order: 50 },
  persist: false,
};
