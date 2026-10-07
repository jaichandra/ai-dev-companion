const test = require("node:test");
const assert = require("node:assert/strict");
const { createExternalTokens, parseEnvFile } = require("./external-tokens.js");

const JIRA_ENV = 'JIRA_URL=https://jira.example.com\nJIRA_PERSONAL_TOKEN="jira-pat"\n# comment\nOTHER=x\n';
const CLAUDE_JSON = JSON.stringify({
  mcpServers: { "acme-bitbucket": { env: { BITBUCKET_TOKEN: "bb-tok", BITBUCKET_BASE_URL: "https://bitbucket.example.com/" } } },
});

const CLAUDE_JSON_JENKINS = JSON.stringify({
  mcpServers: {
    "acme-jenkins-dii": { command: "uvx", args: ["mcp-jenkins", "--jenkins-url", "https://jenkins.example.com/", "--jenkins-username", "jdoe", "--jenkins-password", "jk-pass"] },
  },
});

function fsWith(files) {
  return {
    readFileSync(p) {
      if (!(p in files)) throw Object.assign(new Error("ENOENT"), { code: "ENOENT" });
      return files[p];
    },
  };
}
const make = (files, clock = () => 0) => createExternalTokens({ home: "/h", fsImpl: fsWith(files), clock });
const FILES = { "/h/.ai/.jira.env": JIRA_ENV, "/h/.claude.json": CLAUDE_JSON };

test("parseEnvFile handles quotes, comments and blank lines", () => {
  assert.deepEqual(parseEnvFile('A=1\n\n# c\nB="two"\nC=\'3\'\n'), { A: "1", B: "two", C: "3" });
});


test("a token with no URL beside it is never used", () => {
  const t = make({ "/h/.ai/.jira.env": "JIRA_PERSONAL_TOKEN=abc\n" });
  assert.equal(t.tokenFor("jira.apiToken", "https://jira.example.com"), undefined);
});

test("missing or malformed files and unknown names give undefined, never throw", () => {
  assert.equal(make({}).tokenFor("jira.apiToken", "https://jira.example.com"), undefined);
  assert.equal(make({ "/h/.claude.json": "{not json" }).tokenFor("bitbucket.apiToken", "https://bitbucket.example.com"), undefined);
  assert.equal(make(FILES).tokenFor("jenkins.apiToken", "https://jira.example.com"), undefined);
});




test("Jenkins: a missing flag value, or a missing flag, gives nothing", () => {
  const args = (a) => JSON.stringify({ mcpServers: { "acme-jenkins-dii": { args: a } } });
  const u = "https://jenkins.example.com";
  assert.equal(make({ "/h/.claude.json": args(["--jenkins-url", u, "--jenkins-username", "x"]) }).tokenFor("jenkins.apiToken", u), undefined);
  assert.equal(make({ "/h/.claude.json": args(["--jenkins-url", u, "--jenkins-password"]) }).tokenFor("jenkins.apiToken", u), undefined);
  assert.equal(make({ "/h/.claude.json": args("nope") }).tokenFor("jenkins.apiToken", u), undefined);
});


test("sources come from the caller: a custom one replaces the packs', and one that throws hides nothing", () => {
  const { createExternalTokens } = require("./external-tokens.js");
  const boom = () => {
    throw new Error("unreadable");
  };
  const mine = ({ originOf }) => ({ "jira.apiToken": { token: "t1", origin: originOf("https://jira.example.com/x"), source: "my file" } });
  const tokens = createExternalTokens({ sources: () => [boom, mine] });
  assert.deepEqual(tokens.tokenFor("jira.apiToken", "https://jira.example.com"), { token: "t1", source: "my file" });
  assert.equal(tokens.tokenFor("jira.apiToken", "https://elsewhere.example.com"), undefined, "wrong host gets nothing");
  assert.equal(tokens.tokenFor("bitbucket.apiToken", "https://jira.example.com"), undefined);
  assert.deepEqual(createExternalTokens({ sources: () => [] }).tokenFor("jira.apiToken", "https://jira.example.com"), undefined);
});
