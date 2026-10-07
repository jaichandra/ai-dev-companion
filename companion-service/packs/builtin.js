// The features that ship with the companion. A distribution that wants a different
// set (or its own features) lists other pack files in environment.js `packs`.
const { definePack } = require("../core/packs.js");

module.exports = definePack({
  id: "builtin",
  features: [
    require("../features/resolve-conflict/feature.js"),
    require("../features/create-jira-subtasks/feature.js"),
    require("../features/review-in-editor/feature.js"),
    require("../features/diagnose-build/feature.js"),
    require("../features/analyze-issue/feature.js"),
    require("../features/ticket-workspace/feature.js"),
    require("../features/ticket-to-pr/feature.js"),
    require("../features/summarize-comments/feature.js"),
    require("../features/digest/feature.js"),
    require("../features/address-review-comments/feature.js"),
  ],
});
