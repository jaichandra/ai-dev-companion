// Small helpers the feature descriptors' setup prompts share. Plain JS with no
// dependency on the registry or on packs, so a feature's own descriptor file
// can require it without a cycle (registry -> packs -> feature -> here).
const prereqs = require("./prereqs.js");
const environment = require("../environment.js");

/** Every editor review-in-editor can open — config.reviewEditor must be one
 * of these ids. VS Code is hidden for now (its code paths remain in
 * core/editor.ts and review-in-editor, but nothing offers or detects it);
 * a saved "vscode" is ignored and the editor is detected again. Shared by setup's prompt and the extension's Settings panel
 * (core/settings.js) so the two can't offer different choices. */
const REVIEW_EDITORS = [
  { id: "cursor", label: "Cursor" },
  { id: "claude-code", label: "Claude Code (opens a new terminal)" },
];

// The servers setup assumes instead of asking (environment.js); Settings can change them.
const DEFAULT_JIRA_BASE_URL = environment.defaultBaseUrl("jira");
const DEFAULT_JENKINS_BASE_URL = environment.defaultBaseUrl("jenkins");
const DEFAULT_BITBUCKET_BASE_URL = environment.defaultBaseUrl("bitbucket");

/** Whether Claude Code or Cursor is installed, so model questions can be skipped. */
function hasAgent() {
  return prereqs.detectInstalledEditors().length > 0;
}

/** The file the borrowed token (a pack's externalTokens) for `site` ("jira" | "jenkins" | "bitbucket") would
 * come from at `baseUrl`, or "" when there is none (or setup gave no detector). Never the token. */
function platformTokenSource(helpers, site, baseUrl) {
  const hit = helpers.externalToken ? helpers.externalToken(site, baseUrl) : undefined;
  return hit ? hit.source : "";
}

/** Says how `site` is authenticated, so the prompts match what the companion really does:
 * a token saved here, else a borrowed one, else the browser login. */
function logAuthNote(helpers, label, source) {
  const tool = require("./packs.js").externalTokenLabel();
  if (source) {
    helpers.log(`${label}: using the credentials from ${tool} (${source}). No token needed.`);
  } else {
    helpers.log(`${label}: no ${tool} credentials found, so your browser's ${label} login is used.`);
  }
}

module.exports = {
  REVIEW_EDITORS,
  DEFAULT_JIRA_BASE_URL,
  DEFAULT_JENKINS_BASE_URL,
  DEFAULT_BITBUCKET_BASE_URL,
  hasAgent,
  platformTokenSource,
  logAuthNote,
};
