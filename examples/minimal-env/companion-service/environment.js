// An example distribution's environment profile: GitHub as the git host, Jira Server/Data Center for tickets and
// Jenkins for builds. Copy this file into your own distribution (as companion-service/environment.js, laid over the
// framework by scripts/assemble.js) and change the hosts; see ../README.md.
const { defineEnvironment } = require("./core/environment-base.js");

module.exports = defineEnvironment({
  sites: [
    {
      id: "github",
      kind: "git",
      provider: "github",
      label: "GitHub",
      // github.com, or your GitHub Enterprise address (Settings can override it too).
      baseUrl: "https://github.com",
      // GitHub's browser login doesn't authenticate its API, so the companion uses a personal access token only; the
      // extension is not given github.com's cookies and runs only on pull request pages.
      tokenOnly: true,
      pageMatches: ["{origin}/*/*/pull/*"],
    },
    { id: "jira", kind: "issues", provider: "jira-dc", label: "Jira", baseUrl: "https://jira.example.com" },
    { id: "jenkins", kind: "ci", provider: "jenkins", label: "Jenkins", withUsername: true, baseUrl: "https://jenkins.example.com" },
  ],

  updateSource: { url: "https://git.example.com/my-distribution.git", ref: "main" },

  llm: {
    baseUrl: "https://llm.example.com",
    keyPageUrl: "https://llm.example.com/keys",
    chatModel: "chat-model",
    embeddingModel: "embedding-model",
    retiredChatModels: [],
    hostSuffixes: [".example.com"],
  },

  targets: { projects: ["PROJ"], issueTypes: [] },

  issues: { subtaskParentTypes: ["Story", "Epic"], subtaskTypeName: "Sub-task", reviewTransitionName: "In Review" },

  branding: {},

  packs: ["./packs/builtin.js"],
});
