const test = require("node:test");
const assert = require("node:assert/strict");
const { READ_VERBS, classifyMcpTool } = require("./mcp-tool-classifier.js");

test("READ_VERBS is the exact fail-closed allowlist of read-ish verbs", () => {
  assert.deepEqual(READ_VERBS, [
    "get",
    "list",
    "search",
    "query",
    "read",
    "grep",
    "glob",
    "find",
    "download",
    "view",
    "fetch",
    "ask",
  ]);
});

const ALLOW_CASES = [
  "mcp__acme-jira-confluence__jira_get_issue",
  "mcp__acme-jira-confluence__confluence_search",
  "mcp__acme-jenkins-dii__get_build_console_output",
  "mcp__wiki__query_wiki",
  "mcp__wiki__grep_repo",
  "mcp__acme-bitbucket__list_pull_requests",
  "mcp__acme-jira-confluence__confluence_download_attachment",
];

const DENY_CASES = [
  "mcp__acme-jira-confluence__jira_delete_issue",
  "mcp__acme-jira-confluence__jira_create_issue",
  "mcp__acme-jira-confluence__confluence_delete_page",
  "mcp__acme-jira-confluence__confluence_move_page",
  "mcp__acme-jira-confluence__jira_assign_issue",
  "mcp__acme-bitbucket__decline_pull_request",
  "mcp__acme-bitbucket__delete_branch",
  "mcp__acme-bitbucket__manage_comment",
  "mcp__acme-jenkins-dii__stop_build",
  "mcp__acme-jenkins-dii__set_node_config",
  "mcp__acme-jenkins-dii__run_groovy_script",
  "mcp__acme-argocd__create_application",
  "mcp__acme-argocd__run_resource_action",
  "mcp__acme-jira-confluence__jira_batch_get_changelogs",
  "mcp__x__unknownthing",
  "nounderscores",
  "",
];

for (const toolName of ALLOW_CASES) {
  test(`classifyMcpTool allows ${JSON.stringify(toolName)}`, () => {
    assert.equal(classifyMcpTool(toolName), "allow");
  });
}

for (const toolName of DENY_CASES) {
  test(`classifyMcpTool denies ${JSON.stringify(toolName)}`, () => {
    assert.equal(classifyMcpTool(toolName), "deny");
  });
}

test("classifyMcpTool fails closed on undefined/null input", () => {
  assert.equal(classifyMcpTool(undefined), "deny");
  assert.equal(classifyMcpTool(null), "deny");
});

test("classifyMcpTool is case-insensitive on the verb token", () => {
  assert.equal(classifyMcpTool("mcp__wiki__QUERY_wiki"), "allow");
  assert.equal(classifyMcpTool("mcp__acme-jira-confluence__JIRA_get_issue"), "allow");
});

test("classifyMcpTool denies the companion's own MCP server", () => {
  assert.equal(classifyMcpTool("mcp__ai-companion__list_jobs"), "deny");
  assert.equal(classifyMcpTool("mcp__ai-companion__get_job"), "deny");
  assert.equal(classifyMcpTool("mcp__ai-companion__analyze_issue"), "deny");
  assert.equal(classifyMcpTool("mcp__ai-companion__forget_item"), "deny");
  assert.equal(classifyMcpTool("mcp__ai-companion__open_location"), "deny");
});

test("classifyMcpTool allows servers with ai-companion in the name but not exact match", () => {
  assert.equal(classifyMcpTool("mcp__ai-companion-other__list_x"), "allow");
});
