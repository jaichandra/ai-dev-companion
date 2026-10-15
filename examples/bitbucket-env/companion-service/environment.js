// An example distribution's environment profile: Bitbucket Server/Data Center as the git host, Jira Server/Data Center for tickets and
// Jenkins for builds. Copy this file into your own distribution (as companion-service/environment.js, laid over the
// framework by scripts/assemble.js) and change the hosts; see ../README.md.
const { defineEnvironment } = require("./core/environment-base.js");

module.exports = defineEnvironment({
  sites: [
    { id: "bitbucket", kind: "git", provider: "bitbucket-dc", label: "Bitbucket", baseUrl: "https://bitbucket.example.com" },
    { id: "jira", kind: "issues", provider: "jira-dc", label: "Jira", baseUrl: "https://jira.corp.example.net" },
    { id: "jenkins", kind: "ci", provider: "jenkins", label: "Jenkins", withUsername: true, baseUrl: "https://jenkins.corp.example.net" },
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
