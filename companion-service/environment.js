// The framework's placeholder environment profile. A distribution replaces this file with its own
// (see core/environment-base.js for what a profile holds); the hosts below are examples that point
// nowhere, so a fresh install of the bare framework asks you for yours.
const { defineEnvironment } = require("./core/environment-base.js");

module.exports = defineEnvironment({
  /** One entry per external system. `id` is also the config.json key and the credential prefix
   * (`<id>.apiToken`). `kind` says what the system is for; `provider` names the client
   * implementation (core/providers.ts). */
  sites: [
    { id: "jira", kind: "issues", provider: "jira-dc", label: "Jira", baseUrl: "https://jira.example.com" },
    { id: "jenkins", kind: "ci", provider: "jenkins", label: "Jenkins", withUsername: true, baseUrl: "https://jenkins.example.com" },
    { id: "bitbucket", kind: "git", provider: "bitbucket-dc", label: "Bitbucket", baseUrl: "https://bitbucket.example.com" },
  ],

  /** Where `companion update` follows. Installs track `ref`. */
  updateSource: { url: "https://git.example.com/ai-dev-companion.git", ref: "main" },

  /** The OpenAI-compatible LLM proxy (core/llm-proxy-request.js). The key and the prompt text go to
   * `baseUrl`'s host, so it must match one of `hostSuffixes`. */
  llm: {
    baseUrl: "https://llm.example.com",
    keyPageUrl: "https://llm.example.com/keys",
    chatModel: "chat-model",
    embeddingModel: "embedding-model",
    retiredChatModels: [],
    hostSuffixes: [".example.com"],
  },

  /** Defaults for core/targets.js: which Jira projects and issue types the analysis features work on. */
  targets: { projects: ["PROJ"], issueTypes: [] },

  /** How this team uses its issue tracker. */
  issues: {
    subtaskParentTypes: ["Story", "Epic"],
    subtaskTypeName: "Sub-task",
    reviewTransitionName: "In Review",
  },

  /** What the Settings panel calls things; with no `llmProxy` its page is hidden, with no
   * `supportEmail` the Support row is. `appSlug` names the install folder, state folder and launchd job (default `ai-dev-companion`; fixed once
   * installed). `productName` is what the product is called in banners, generated comments and the extension's name
   * (default `AI Dev Companion`). `marketplaceName` is the name of the Claude Code plugin
   * marketplace the distribution's repo publishes (default `ai-dev-companion`). */
  branding: {},

  /** The Claude Code `--permission-mode` for sessions the features start: "auto" (default), "plan" or "default".
   * `permissionMode` sets it for all of them; `permissionModes` overrides it per feature id, e.g.
   * `{ "review-in-editor": "plan" }` to keep Review PR read-only. */
  claude: {},

  /** The pack files (relative to this folder) whose features this distribution offers. */
  packs: ["./packs/builtin.js"],
});
