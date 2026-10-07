#!/usr/bin/env node
// `npm run doctor` — checks every piece the extension depends on, in the
// order they depend on each other, and says exactly what to do about the
// first thing that's wrong. Read-only: never changes config or restarts
// anything.
const fs = require("fs");
const os = require("os");
const path = require("path");
const prereqs = require("./core/prereqs.js");
const registry = require("./core/feature-registry.js");
const serviceControl = require("./core/service-control.js");
const claudeArgs = require("./core/claude-args.js");
const paths = require("./core/paths.js");
const environment = require("./environment.js");
const { PRODUCT_NAME } = require("./core/product-name.js");
const credentialStoreModule = require("./core/credential-store.js");
const mcpRegistration = require("./core/mcp-registration.js");
const packs = require("./core/packs.js");
const pluginInfo = require("./core/plugin-info.js");
const { loadExistingConfig, EXTENSION_CONFIG_PATH } = require("./setup.js");

const CREDENTIALS_PATH = path.join(__dirname, "credentials.enc");
const CREDENTIALS_KEY_PATH = path.join(paths.stateDir(), "credentials.key");
const CLAUDE_JSON_PATH = path.join(os.homedir(), ".claude.json");
const CURSOR_DIR_PATH = path.join(os.homedir(), ".cursor");
const CURSOR_MCP_JSON_PATH = path.join(CURSOR_DIR_PATH, "mcp.json");

const CONFIG_PATH = path.join(__dirname, "config.json");
const IS_MAC = process.platform === "darwin";
const IS_LINUX = process.platform === "linux";
const IS_WINDOWS = process.platform === "win32";

// `doctor --bitbucket-contract [PR url]` — parsed up front so both main()
// and the top-level .finally() below can tell this apart from an ordinary
// doctor run (see the ruling: this is read-only, never touches config, and
// its own report() calls already say everything worth saying — the usual
// "reload the extension" epilogue doesn't apply to it).
const cliArgs = process.argv.slice(2);
const bitbucketContractFlagIndex = cliArgs.indexOf("--bitbucket-contract");

const fixes = [];
function report(level, message, fix) {
  console.log(`${level.padEnd(4)} - ${message}`);
  if (fix && level !== "OK") fixes.push(fix);
}

/** Whether `bin` is an executable file somewhere on PATH — scans
 * `process.env.PATH` directly with `fs.accessSync(..., X_OK)` rather than
 * actually running the binary (running an arbitrary `--version` command
 * as a way to check presence is unnecessary process-spawning for what's
 * really just a filesystem question, and some binaries don't even
 * support `--version`). Only used for bwrap/socat on Linux. */
function isOnPath(bin) {
  const dirs = (process.env.PATH || "").split(path.delimiter);
  return dirs.some((dir) => {
    if (!dir) return false;
    try {
      fs.accessSync(path.join(dir, bin), fs.constants.X_OK);
      return true;
    } catch {
      return false;
    }
  });
}

/** Whether `p` (a file that must exist for this to matter) is owner-only
 * (mode 0600) — credential-store.js always writes credentials.enc and
 * credentials.key with that mode; anything looser means some other tool
 * (an editor, a backup restore) loosened it after the fact. Returns null
 * for a path that doesn't exist — "not created yet" isn't a permissions
 * problem, see checkCredentials' own OK-when-missing handling. */
function isOwnerOnly(p) {
  if (!fs.existsSync(p)) return null;
  return (fs.statSync(p).mode & 0o777) === 0o600;
}

/**
 * Task 3's encrypted credential store: can it be decrypted, are its two
 * files (credentials.enc, credentials.key) owner-only, and has every
 * apiToken actually finished moving out of config.json. A missing
 * credentials.enc (no tokens ever saved) is reported OK, not FAIL — see
 * the ruling this follows: "if the key dir/key are fine but the file
 * doesn't exist, that's OK (no tokens yet)". An undecryptable store (bad/
 * missing key, tampering) surfaces credential-store.js's own message,
 * which already tells the user to re-enter their tokens in Settings —
 * nothing here needs to add its own "key is missing" wording on top of
 * that.
 */
function checkCredentials(config) {
  const store = credentialStoreModule.createCredentialStore({
    filePath: CREDENTIALS_PATH,
    keyPath: CREDENTIALS_KEY_PATH,
  });
  let names;
  try {
    names = store.list();
  } catch (err) {
    report("FAIL", `Encrypted credentials: ${err.message}`, err.message);
    names = null;
  }
  if (names !== null) {
    report(
      "OK",
      names.length > 0
        ? `Encrypted credentials: ${names.length} token(s) saved, decrypts fine (${CREDENTIALS_PATH}).`
        : "Encrypted credentials: none saved yet.",
    );
  }

  for (const [label, filePath] of [
    ["credentials.enc", CREDENTIALS_PATH],
    ["credentials.key", CREDENTIALS_KEY_PATH],
  ]) {
    const ownerOnly = isOwnerOnly(filePath);
    if (ownerOnly === null) continue; // Doesn't exist yet — nothing to check.
    report(
      ownerOnly ? "OK" : "WARN",
      `${label} permissions: ${ownerOnly ? "owner-only (0600)." : "looser than 0600 — should be owner-only."}`,
      `chmod 600 ${filePath}`,
    );
  }

  // Tokens borrowed from another tool (a pack's externalTokens) when none is saved here (core/external-tokens.js).
  // Needs the built config.ts defaults for the site URLs; skipped when the service isn't built.
  try {
    const dist = require(require("node:path").join(__dirname, "dist", "config.js"));
    const external = require("./core/external-tokens.js").createExternalTokens();
    const urls = {
      jira: config.jira?.baseUrl || dist.DEFAULT_JIRA_BASE_URL,
      jenkins: config.jenkins?.baseUrl || dist.DEFAULT_JENKINS_BASE_URL,
      bitbucket: config.bitbucket?.baseUrl || dist.DEFAULT_BITBUCKET_BASE_URL,
    };
    const saved = new Set(names || []);
    const used = Object.entries(external.sourcesFor(urls)).filter(([site]) => !saved.has(`${site}.apiToken`));
    if (used.length) report("OK", `Tokens from ${packs.externalTokenLabel()} (none saved here): ${used.map(([site, src]) => `${site} <- ${src}`).join("; ")}.`);
  } catch {
    /* not built yet: the build check reports that */
  }

  const leftInConfig = ["jira", "jenkins", "bitbucket"].filter(
    (site) => typeof config[site]?.apiToken === "string" && config[site].apiToken.trim() !== "",
  );
  if (leftInConfig.length === 0) {
    report("OK", "No API tokens left in config.json.");
  } else {
    report(
      "FAIL",
      `config.json still has apiToken set for: ${leftInConfig.join(", ")} — this should have moved to ` +
        "the encrypted credential store automatically.",
      "Restart the companion service (it migrates any remaining token at startup), or re-run `npm run setup`.",
    );
  }
}

/**
 * Background work (Phase 7): which watchers are on, the Claude budget,
 * quiet hours and the digest; the LLM proxy (optional — one tiny
 * call when a key is saved); and, for the first 5 repos, whether the
 * pre-push check is installed and `companion` is on PATH for it. All
 * informational except a hook whose `companion` command can't be found.
 */
async function checkBackground(config) {
  try {
    await checkBackgroundLines(config);
  } catch (err) {
    // Informational: a surprise here (an odd config value) must not stop the rest of the report.
    report("WARN", `Background work: couldn't be checked (${String((err && err.message) || err).slice(0, 160)}).`);
  }
}

async function checkBackgroundLines(config) {
  const schedule = require("./core/schedule.js");
  const hm = (minutes) => `${String(Math.floor(minutes / 60)).padStart(2, "0")}:${String(minutes % 60).padStart(2, "0")}`;
  const watchers = Object.entries(schedule.watcherSettings(config)).filter(([, w]) => w.enabled);
  report(
    "OK",
    watchers.length
      ? `Background watchers: ${watchers.map(([name, w]) => `${name} every ${w.intervalMinutes} min`).join(", ")}; at most ${schedule.budgetLimit(config)} Claude run(s) a day.`
      : "Background watchers: all off (optional — turn them on in ⚙ Settings → Background work).",
  );
  const quiet = schedule.quietHoursFrom(config);
  if (quiet) report("OK", `Quiet hours: ${hm(quiet.start)}–${hm(quiet.end)} (no watcher runs, notifications held for the digest).`);
  const digest = schedule.digestSettings(config);
  report("OK", digest.enabled ? `Morning digest: weekdays at ${digest.time}.` : "Morning digest: not scheduled (the ✨ Morning digest row still works).");

  let apiKey;
  try {
    apiKey = credentialStoreModule.createCredentialStore({ filePath: CREDENTIALS_PATH, keyPath: CREDENTIALS_KEY_PATH }).get("llmProxy.apiKey");
  } catch {
    apiKey = undefined;
  }
  const { createLlmProxyClient } = require("./core/llm-proxy-client.js");
  const proxy = createLlmProxyClient({ getConfig: () => config, getApiKey: () => apiKey, osUser: os.userInfo().username });
  const detected = await proxy.detect();
  const proxyName = (environment.branding.llmProxy && environment.branding.llmProxy.name) || "LLM proxy";
  const proxyHints = {
    "no-key": " Optional: create a key on the proxy site and save it in ⚙ Settings → LLM proxy; until then watchers use their rule-based filters.",
    unreachable: " Are you on the VPN? Watchers fall back to their rule-based filters.",
    tls: " This Node doesn't trust the proxy's certificate. Run node install.js --yes (it starts the service with NODE_USE_SYSTEM_CA=1); a shell-run doctor needs NODE_USE_SYSTEM_CA=1 too.",
    application: ` The proxy matches a key to the application it was created for by the client name (User-Agent): set ⚙ Settings → ${proxyName} → Client name to that application's name, or ask the proxy team for an application for this tool.`,
    user: ` The sign-on user name wasn't accepted: use your plain SSO name (⚙ Settings → ${proxyName} → Sign-on user name).`,
    "key-rejected": " Create a new key on the proxy site and save it in ⚙ Settings → LLM proxy.",
  };
  report(
    detected.state === "ready" || detected.state === "no-key" ? "OK" : "WARN",
    `${proxyName}: ${detected.label} (${proxy.settings().chatModel}).${proxyHints[detected.state] || ""}`,
    "Check the key in ⚙ Settings → LLM proxy.",
  );
  try {
    checkSimilar(config, detected.state, proxy.settings().embeddingModel);
  } catch (err) {
    report("WARN", `Similar past tickets: couldn't be checked (${safeText(err && err.message)}).`);
  }

  const { spawnSync } = require("child_process");
  const withHook = [];
  for (const [key, repoPath] of Object.entries(config.repos || {}).slice(0, 5)) {
    const r = spawnSync("git", ["-C", repoPath, "config", "--local", "--get", "hook.companion-precheck.command"], { encoding: "utf8", timeout: 5000 });
    if (r.status === 0) withHook.push(key);
  }
  if (withHook.length === 0) {
    report("OK", "Pre-push check: not installed in any repo (optional — `companion hooks install` in a repo).");
  } else if (isOnPath("companion")) {
    report("OK", `Pre-push check installed in: ${withHook.join(", ")}.`);
  } else {
    report(
      "WARN",
      `Pre-push check installed in ${withHook.join(", ")}, but \`companion\` isn't on your PATH, so git can't run it.`,
      "Re-run `node install.js --yes` (it links `companion` into ~/.local/bin) and make sure ~/.local/bin is on your PATH.",
    );
  }
}

/**
 * Whether the companion's own MCP server (see core/mcp-registration.js) is
 * actually registered with Claude Code and, if installed, Cursor, at the
 * port and mcp.token this run of the service is using — registration
 * drifts silently after a token rotation (see server.ts's reRegisterMcp)
 * or a manual edit of either client's config, so this is worth checking
 * every run rather than only right after setup. A missing mcp.token means
 * setup has never created one yet, which is reported on its own — there's
 * nothing to compare a client's entry against without it. Claude Code's
 * line is skipped (with an informational OK) when `claude` isn't on
 * PATH — the standing rule to gate optional integrations on detection —
 * since there would be nothing to register there regardless. Never prints
 * the token itself, in any message.
 */
function checkMcpRegistration(config) {
  const store = credentialStoreModule.createCredentialStore({
    filePath: CREDENTIALS_PATH,
    keyPath: CREDENTIALS_KEY_PATH,
  });
  let token;
  try {
    token = store.get("mcp.token");
  } catch {
    token = undefined; // Undecryptable store — same as "no token saved" for this check.
  }
  if (!token) {
    report(
      "WARN",
      "MCP token not created yet.",
      "re-run the installer (`node install.js --yes`) or `npm run setup`",
    );
    return;
  }

  const claudeAvailable = prereqs.checkClaudeCli().ok;
  if (!claudeAvailable) {
    report("OK", "Claude Code not installed — MCP registration skipped.");
  }

  const readTextOrNull = (filePath) => {
    try {
      return fs.readFileSync(filePath, "utf8");
    } catch {
      return null; // Missing (ENOENT) or unreadable — registrationStatus treats null as "no entry".
    }
  };
  const cursorInstalled = fs.existsSync(CURSOR_DIR_PATH);
  const statuses = mcpRegistration.registrationStatus({
    claudeJsonText: readTextOrNull(CLAUDE_JSON_PATH),
    cursorJsonText: cursorInstalled ? readTextOrNull(CURSOR_MCP_JSON_PATH) : null,
    cursorInstalled,
    port: config.port,
    token,
  });

  const LEVELS = { ok: "OK", missing: "WARN", "stale-token": "FAIL", "wrong-url": "FAIL", unreadable: "WARN" };
  const DETAILS = {
    ok: "registered and up to date",
    missing: "not registered",
    "stale-token": "token doesn't match (it changes when you rotate it)",
    "wrong-url": "registered at a different URL/port",
    unreadable: "its config file isn't valid JSON",
  };
  for (const { client, state } of statuses) {
    if (client === "Claude Code" && !claudeAvailable) continue;
    report(LEVELS[state], `MCP registration (${client}): ${DETAILS[state]}.`, "npm run setup");
  }
  if (claudeAvailable) {
    try {
      checkPluginPath(statuses.find((s) => s.client === "Claude Code").state);
    } catch (err) {
      report("WARN", `Claude Code plugin path: couldn't be checked (${safeText(err && err.message)}).`);
    }
  }
}

function safeText(value) {
  return require("./core/cli-format.js").oneLine(String(value == null ? "" : value)).slice(0, 200);
}

/**
 * Which way Claude Code reaches the companion's tools (Phase 8): the
 * `claude mcp add` entry checked above, the ai-companion plugin, or both
 * (then its tools appear twice). Reads Claude Code's own plugin files;
 * silent when neither mentions the plugin. The folder is $CLAUDE_CONFIG_DIR
 * when set, else ~/.claude.
 */
function checkPluginPath(claudeState) {
  const read = (...p) => {
    try {
      return fs.readFileSync(path.join(pluginInfo.claudeConfigDir(process.env, os.homedir()), ...p), "utf8");
    } catch {
      return null;
    }
  };
  const plugin = pluginInfo.pluginStatus({ installedPluginsText: read("plugins", "installed_plugins.json"), settingsText: read("settings.json") });
  const line = pluginInfo.mcpPathSummary({ claudeState, plugin });
  if (line) report(line.level, line.message, line.fix);
}

/**
 * Similar past tickets (Phase 8): on or off, and how many history items
 * have an embedding. Opens history.db read-only and only if it exists —
 * doctor never creates or migrates it (the service does at start).
 */
function checkSimilar(config, proxyState, embeddingModel) {
  const similar = require("./core/similar.js");
  if (!similar.similarSettings(config).enabled) {
    report("OK", "Similar past tickets: off (⚙ Settings → Local history).");
    return;
  }
  const how = proxyState === "ready" ? `by meaning (${safeText(embeddingModel)}, on-prem) and shared words` : "by shared words only, until the LLM proxy is ready";
  const file = path.join(paths.stateDir(), "history.db");
  if (!fs.existsSync(file)) {
    report("OK", `Similar past tickets: on, ${how}; no local history on this machine yet (the installed copy keeps it).`);
    return;
  }
  let counted = null;
  let problem = null; // "schema" (older layout) or an unreadable-file message
  try {
    const { DatabaseSync } = require("node:sqlite");
    const db = new DatabaseSync(file, { readOnly: true });
    try {
      // Real vectors only: a one-number placeholder marks an item that could
      // not be embedded (same rule as history.hasVectors).
      const vectors = db.prepare("SELECT COUNT(*) AS n FROM item_vectors WHERE dim > 1 AND model = ?").get(String(embeddingModel)).n;
      const items = db.prepare("SELECT COUNT(*) AS n FROM items WHERE kind IN ('ticket', 'analysis', 'pr')").get().n;
      counted = { vectors, items };
    } finally {
      db.close();
    }
  } catch (err) {
    problem = /no such table/i.test(String(err && err.message)) ? "schema" : safeText(err && err.message);
  }
  if (counted) {
    report("OK", `Similar past tickets: on, ${how}; ${counted.vectors} of ${counted.items} ticket, analysis and PR item(s) embedded.`);
  } else if (problem === "schema") {
    report("OK", `Similar past tickets: on, ${how}; the history is in an older layout (the service adds the vectors at its next start).`);
  } else {
    report("WARN", `Similar past tickets: on, ${how}; the history file couldn't be read (${problem}). It may be corrupt or locked.`);
  }
}

function readExtensionConfig() {
  try {
    const self = {};
    new Function("self", fs.readFileSync(EXTENSION_CONFIG_PATH, "utf8"))(self);
    return self.COMPANION_CONFIG || null;
  } catch {
    return null;
  }
}

/**
 * `doctor --bitbucket-contract [PR url]` (Task 5). Read-only: only ever
 * makes GET requests to Bitbucket, and the only thing it writes anywhere
 * is stateDir()/bitbucket-version.json (the version-tracking file the
 * ruling asks for) — config.json and credentials.enc are never touched.
 *
 * Without a PR url, only `application-properties` (and so the Bitbucket
 * version) is checked. With one, the pull request, its activities, their
 * comments and any comment anchors are checked too — each diffed against
 * core/bitbucket-normalize.js's EXPECTED_KEYS (the single source of truth;
 * this never hand-maintains its own copy of that list).
 */
async function runBitbucketContractCheck(config, prUrlArg) {
  console.log("== Bitbucket contract check ==\n");

  // Reuses the exact same store construction as checkCredentials above,
  // rather than going through core/credentials.ts's cached wrapper — this
  // is a one-shot CLI run, so there's nothing for a read cache to help
  // with, and it keeps doctor's only way of touching credentials.enc in
  // one place.
  const store = credentialStoreModule.createCredentialStore({
    filePath: CREDENTIALS_PATH,
    keyPath: CREDENTIALS_KEY_PATH,
  });
  let token;
  try {
    token = store.get("bitbucket.apiToken");
  } catch {
    token = undefined; // Undecryptable store — treated the same as "no token saved".
  }
  if (!token) {
    console.log("skipped — add a Bitbucket token in Settings to run the live contract check");
    return;
  }

  const distBitbucketPath = path.join(__dirname, "dist", "core", "bitbucket.js");
  if (!fs.existsSync(distBitbucketPath)) {
    report(
      "FAIL",
      "The service hasn't been built (dist/core/bitbucket.js is missing).",
      `cd ${__dirname} && npm run build`,
    );
    return;
  }
  const bitbucket = require(distBitbucketPath);
  const normalize = require("./core/bitbucket-normalize.js");
  const contract = require("./core/bitbucket-contract.js");

  const site = bitbucket.bitbucketSite(config);
  // No relayed browser session is available to a CLI tool — this always
  // goes straight to the saved token, exactly as the ruling asks for.
  const auth = {};

  let appPropsRaw;
  try {
    appPropsRaw = await bitbucket.fetchApplicationPropertiesRaw(site, auth);
  } catch (err) {
    report("FAIL", `Could not reach Bitbucket at ${site.baseUrl}: ${err.message}`);
    return;
  }
  const appProps = normalize.normalizeAppProperties(appPropsRaw);
  const liveVersion = appProps && appProps.version;
  report(liveVersion ? "OK" : "WARN", `Bitbucket version: ${liveVersion || "unknown"}.`);

  const missingAppProps = contract.missingKeys(appPropsRaw, normalize.EXPECTED_KEYS.applicationProperties);
  report(
    missingAppProps.length === 0 ? "OK" : "WARN",
    `application-properties fields: ${
      missingAppProps.length === 0 ? "all present." : `missing ${missingAppProps.join(", ")}.`
    }`,
  );

  const versionFile = path.join(paths.stateDir(), "bitbucket-version.json");
  let recorded = null;
  try {
    recorded = JSON.parse(fs.readFileSync(versionFile, "utf8"));
  } catch {
    recorded = null; // Missing, unreadable or corrupt — treated as "never recorded".
  }
  const comparison = contract.compareVersion(recorded, liveVersion);
  if (comparison.message) console.log(comparison.message);
  if (liveVersion) {
    try {
      fs.mkdirSync(path.dirname(versionFile), { recursive: true });
      fs.writeFileSync(
        versionFile,
        `${JSON.stringify({ version: liveVersion, seenAt: new Date().toISOString() }, null, 2)}\n`,
      );
    } catch (err) {
      report("WARN", `Could not record the Bitbucket version to ${versionFile}: ${err.message}`);
    }
  }

  if (!prUrlArg) {
    console.log(
      "\n(No PR URL given — only application-properties was checked. Pass one to also check the " +
        "pullRequest, activity, comment and commentAnchor fields.)",
    );
    return;
  }

  const parsed = contract.parsePrUrl(prUrlArg);
  if (!parsed) {
    report(
      "FAIL",
      `"${prUrlArg}" doesn't look like a Bitbucket PR URL (expected .../projects/P/repos/R/pull-requests/N).`,
    );
    return;
  }

  let prRaw;
  try {
    prRaw = await bitbucket.fetchPullRequestRaw(site, auth, parsed.project, parsed.repo, parsed.id);
  } catch (err) {
    report("FAIL", `Could not fetch ${parsed.project}/${parsed.repo}#${parsed.id}: ${err.message}`);
    return;
  }
  const missingPr = contract.missingKeys(prRaw, normalize.EXPECTED_KEYS.pullRequest);
  report(
    missingPr.length === 0 ? "OK" : "WARN",
    `pullRequest fields: ${missingPr.length === 0 ? "all present." : `missing ${missingPr.join(", ")}.`}`,
  );

  // OPTIONAL_KEYS.pullRequest (properties.openTaskCount/commentCount) is
  // reported as an informational OK, never a WARN: a real 9.4.16 instance
  // doesn't return them on this endpoint at all (see bitbucket-normalize.js's
  // OPTIONAL_KEYS doc comment), and the extension's address-review-comments
  // menu condition already treats that as "show the menu unconditionally"
  // rather than as a broken contract, so there's nothing here to fix.
  const missingOptionalPr = contract.missingKeys(prRaw, normalize.OPTIONAL_KEYS.pullRequest);
  report(
    "OK",
    missingOptionalPr.length === 0
      ? "pullRequest optional fields: all present."
      : `pullRequest optional fields not returned by this Bitbucket version: ${missingOptionalPr.join(", ")} ` +
          "(the Address review comments menu then shows on every PR page).",
  );

  let pages;
  let truncated;
  try {
    ({ pages, truncated } = await bitbucket.fetchAllActivityPages(site, auth, parsed.project, parsed.repo, parsed.id));
  } catch (err) {
    report("FAIL", `Could not fetch activities for ${parsed.project}/${parsed.repo}#${parsed.id}: ${err.message}`);
    return;
  }
  if (truncated) {
    report("WARN", "Hit the activities page cap before the last page — some older comments may be missing from this check.");
  }

  const activities = pages.flatMap((page) => (Array.isArray(page.values) ? page.values : []));
  const commented = activities.filter((a) => a && a.action === "COMMENTED");

  const missingActivity = contract.missingKeysAcross(commented, normalize.EXPECTED_KEYS.activity);
  report(
    missingActivity.length === 0 ? "OK" : "WARN",
    commented.length === 0
      ? "activity fields: no COMMENTED activities found on this PR — nothing to check."
      : `activity fields (${commented.length} sampled): ${
          missingActivity.length === 0 ? "all present." : `missing ${missingActivity.join(", ")}.`
        }`,
  );

  const comments = commented.map((a) => a.comment).filter((c) => c && typeof c === "object");
  const missingComment = contract.missingKeysAcross(comments, normalize.EXPECTED_KEYS.comment);
  report(
    missingComment.length === 0 ? "OK" : "WARN",
    comments.length === 0
      ? "comment fields: no comments found on this PR — nothing to check."
      : `comment fields (${comments.length} sampled): ${
          missingComment.length === 0 ? "all present." : `missing ${missingComment.join(", ")}.`
        }`,
  );

  const anchors = commented.map((a) => a.commentAnchor).filter((c) => c && typeof c === "object");
  const missingAnchor = contract.missingKeysAcross(anchors, normalize.EXPECTED_KEYS.commentAnchor);
  report(
    missingAnchor.length === 0 ? "OK" : "WARN",
    anchors.length === 0
      ? "commentAnchor fields: no anchored comments found on this PR — nothing to check."
      : `commentAnchor fields (${anchors.length} sampled): ${
          missingAnchor.length === 0 ? "all present." : `missing ${missingAnchor.join(", ")}.`
        }`,
  );
}

async function main() {
  if (bitbucketContractFlagIndex !== -1) {
    const config = loadExistingConfig(CONFIG_PATH);
    if (!config) {
      report("FAIL", `${CONFIG_PATH} is missing or invalid — setup hasn't been completed.`, `cd ${__dirname} && npm run setup`);
      return;
    }
    const nextArg = cliArgs[bitbucketContractFlagIndex + 1];
    const prUrlArg = nextArg && !nextArg.startsWith("--") ? nextArg : undefined;
    await runBitbucketContractCheck(config, prUrlArg);
    return;
  }

  console.log(`== ${PRODUCT_NAME} doctor ==\n`);
  const rerunSetup = `cd ${__dirname} && npm run setup`;

  const node = prereqs.checkNodeVersion();
  report(node.ok ? "OK" : "FAIL", node.message, `Install Node ${prereqs.MIN_NODE.major}.${prereqs.MIN_NODE.minor} or newer (https://nodejs.org).`);

  const config = loadExistingConfig(CONFIG_PATH);
  if (!config) {
    report("FAIL", `${CONFIG_PATH} is missing or invalid — setup hasn't been completed.`, rerunSetup);
    return;
  }
  report("OK", `Settings found (${CONFIG_PATH}).`);

  checkCredentials(config);
  checkMcpRegistration(config);
  await checkBackground(config);

  const extConfig = readExtensionConfig();
  const expectedUrl = `http://127.0.0.1:${config.port}`;
  if (!extConfig) {
    report(
      "FAIL",
      `${EXTENSION_CONFIG_PATH} is missing — the extension can't reach the service.`,
      rerunSetup,
    );
  } else if (extConfig.sharedSecret !== config.sharedSecret || extConfig.serviceBaseUrl !== expectedUrl) {
    report("FAIL", "The extension's connection settings don't match config.json.", rerunSetup);
  } else {
    report("OK", "Extension connection settings match config.json.");
  }

  // The manifest's hosts and script list come from the profile, config.json's base URLs and the packs.
  const manifestProblems = require("./core/extension-manifest.js").checkManifest(path.dirname(EXTENSION_CONFIG_PATH), config);
  if (manifestProblems.length) {
    report("FAIL", `The extension's manifest.json doesn't match your settings: ${manifestProblems.join(" ")}`, `${rerunSetup} Then reload the extension at chrome://extensions.`);
  } else {
    report("OK", "Extension manifest matches your servers and features.");
  }

  const enabledIds = registry.enabledFeatureIds(config);
  report(
    enabledIds.length ? "OK" : "WARN",
    `Enabled features: ${enabledIds.join(", ") || "none"}.`,
    rerunSetup,
  );
  for (const name of registry.requiredChecksFor(enabledIds)) {
    const result = prereqs.CHECKS_BY_NAME[name]();
    const needing = registry.featuresRequiring(name, enabledIds).map((d) => d.label);
    report(
      result.ok ? "OK" : "FAIL",
      `${result.message}${result.ok ? "" : ` (needed by ${needing.join(", ")})`}`,
      result.message,
    );
  }

  for (const check of packs.doctorChecks()) {
    const line = check({ config });
    if (line) report(line.status, line.message);
  }

  const riskUrl = config.riskFacts && config.riskFacts.url;
  if (!riskUrl) {
    report("OK", "Shared test history: not configured (optional).");
  } else {
    const r = await require("./core/risk-facts.js").shared.get(riskUrl);
    if (r.ok) {
      report(
        "OK",
        `Shared test history: ${r.facts.tests.size} tests, generated ${r.facts.generatedAt || "at an unknown time"}${r.stale ? " (stale copy)" : ""}.`,
      );
    } else {
      report("WARN", `Shared test history: ${r.reason}`, "Check the URL in ⚙ Settings → Shared test history.");
    }
  }

  if (enabledIds.includes("ticket-to-pr")) {
    const transition = (config.ticketToPr && config.ticketToPr.reviewTransitionName) || "In Review";
    report("OK", `Ticket to PR: on — Create PR moves the ticket with its "${transition}" transition when it has one.`);
    // Create PR pushes with the user's own git credentials. A push that
    // would stop at a password prompt fails instead (GIT_TERMINAL_PROMPT=0),
    // so check each repo's origin answers without one. First 5 repos only.
    const { spawnSync } = require("child_process");
    for (const [key, repoPath] of Object.entries(config.repos || {}).slice(0, 5)) {
      if (!fs.existsSync(path.join(repoPath, ".git"))) {
        report("WARN", `Ticket to PR: ${key} (${repoPath}) isn't a git clone, so Start fix can't use it.`, "Fix the path in ⚙ Settings → Repositories.");
        continue;
      }
      const r = spawnSync("git", ["ls-remote", "--exit-code", "--heads", "origin"], {
        cwd: repoPath,
        encoding: "utf8",
        timeout: 15000,
        env: { ...process.env, GIT_TERMINAL_PROMPT: "0" },
      });
      // ls-remote --exit-code: 2 = reached origin but it has no branches.
      if (r.status === 0) {
        report("OK", `Ticket to PR: ${key} reaches origin without a password prompt.`);
      } else if (r.status === 2) {
        report("WARN", `Ticket to PR: ${key}'s origin answered but has no branches yet.`, "Push a first branch to origin before using Start fix on it.");
      } else {
        report(
          "WARN",
          `Ticket to PR: ${key} can't reach origin without a prompt, so Create PR's push would fail.`,
          `Make \`git -C ${repoPath} push --dry-run\` work in a terminal without a prompt (SSH key or a credential helper).`,
        );
      }
    }
  }

  if (registry.needsRepos(enabledIds)) {
    const repos = Object.entries(config.repos || {});
    if (repos.length === 0) {
      report("OK", "No repos set up yet — each is found, picked or cloned from the browser on first use.");
    }
    for (const [key, repoPath] of repos) {
      const result = prereqs.checkRepoPath(repoPath);
      report(
        result.ok ? "OK" : "WARN",
        `${key}: ${result.message}`,
        `${key} will be looked up again (or offered to pick/clone) the next time it's used.`,
      );
    }
  }

  if (!fs.existsSync(path.join(__dirname, "dist", "server.js"))) {
    report("FAIL", "The service hasn't been built (dist/server.js is missing).", rerunSetup);
  }

  // The PreToolUse hook every readOnly-policy Claude run wires in (see
  // core/claude-args.js, core/mcp-guard.js) has to survive `npm run build`
  // as a plain file under dist/ — tsc's allowJs copies it there, it's
  // never compiled from a .ts source. If it's missing, runClaude() refuses
  // to start rather than silently running that policy without the guard
  // (see core/claude.ts), so this is worth surfacing here too. The
  // protected-path rule count is purely informational (core/claude-args.js's
  // protectedPathRules(), passed to every Claude run's --disallowedTools
  // regardless of policy) — it doesn't change with config, so there's
  // nothing to "fix" if it looks low, just confirmation it's wired up.
  const guardScriptPath = path.join(__dirname, "dist", "core", "mcp-guard.js");
  const guardPresent = fs.existsSync(guardScriptPath);
  const protectedRuleCount = claudeArgs.protectedPathRules({
    stateDir: paths.stateDir(),
    companionDir: claudeArgs.companionServiceDir(),
    home: os.homedir(),
  }).length;
  report(
    guardPresent ? "OK" : "FAIL",
    `MCP read-only guard script ${guardPresent ? "present" : "missing"} (dist/core/mcp-guard.js); ` +
      `${protectedRuleCount} protected-path rule(s) applied to every Claude run.`,
    rerunSetup,
  );

  // Every readOnly/worktreeWrite Claude run now sets `sandbox.enabled` with
  // `failIfUnavailable: true` (core/claude-args.js's isSandboxedPolicy) —
  // if the OS-level sandbox mechanism itself can't start, those runs fail
  // outright rather than silently running Bash unsandboxed. macOS
  // (Seatbelt) needs nothing extra; Linux needs `bwrap` (bubblewrap) and
  // `socat` on PATH; native Windows has no sandbox at all.
  if (IS_MAC) {
    report("OK", "Claude Bash sandbox: enforced (macOS Seatbelt).");
  } else if (IS_LINUX) {
    const missing = ["bwrap", "socat"].filter((bin) => !isOnPath(bin));
    report(
      missing.length === 0 ? "OK" : "FAIL",
      missing.length === 0
        ? "Claude Bash sandbox: enforced (Linux bubblewrap + socat)."
        : `Claude Bash sandbox: ${missing.join(", ")} not found on PATH — Claude runs that use Bash will fail.`,
      `Install bubblewrap and socat (e.g. \`apt install bubblewrap socat\` or \`dnf install bubblewrap socat\`), then come back here.`,
    );
  } else if (IS_WINDOWS) {
    report("WARN", "Claude Bash sandbox: not supported — Claude runs that need Bash will fail.");
  }

  const launchd = IS_MAC ? require("./core/launchd.js") : null;
  const background = launchd && launchd.isInstalled();
  const health = await serviceControl.checkHealth(config.port, config.sharedSecret);

  if (background) {
    const status = launchd.serviceStatus();
    if (status.pid) report("OK", `Background service is running (pid ${status.pid}).`);
    else report("FAIL", "Background service is installed but not running.", rerunSetup);
  } else if (health.state === "ok") {
    report("OK", "Running in the foreground (`npm start`) — it stops when that terminal closes.");
  } else {
    report(
      "FAIL",
      "The service isn't running.",
      IS_MAC
        ? `${rerunSetup}   (answer yes to running it in the background)`
        : `cd ${__dirname} && npm start   (keep that terminal open)`,
    );
  }

  if (health.state === "ok") {
    report("OK", health.message);
  } else if (health.state === "unauthorized") {
    report(
      "FAIL",
      health.message,
      `Stop whatever else is on port ${config.port} (\`lsof -nP -iTCP:${config.port} -sTCP:LISTEN\`), then: ${rerunSetup}`,
    );
  } else {
    report("FAIL", `${health.message} The extension will say "Could not reach the companion service".`);
    if (background) {
      const { out, err } = launchd.logPaths();
      const tail = [serviceControl.tailFile(out, 8), serviceControl.tailFile(err, 8)]
        .filter(Boolean)
        .join("\n");
      if (tail) console.log(`\n     Last lines of the service log:\n${tail.replace(/^/gm, "     ")}\n`);
    }
  }
}

if (require.main !== module) {
  // Loaded by a test: expose the pieces, run nothing.
  module.exports = { checkBackground, checkPluginPath, checkSimilar, fixes };
} else main()
  .catch((err) => {
    console.error(`\nDoctor itself failed: ${err.message}`);
    process.exitCode = 1;
  })
  .finally(() => {
    if (bitbucketContractFlagIndex !== -1) {
      // Its own report()/console.log calls above already say everything
      // worth saying — the ordinary run's "reload the extension" epilogue
      // doesn't apply to a live REST contract check.
      if (fixes.length > 0) process.exitCode = 1;
      return;
    }
    if (fixes.length === 0) {
      console.log(
        "\nEverything looks good. If the ✨ button still misbehaves, reload the extension at\n" +
          "chrome://extensions and refresh the page.",
      );
      return;
    }
    console.log("\nTo fix, start with:");
    for (const fix of [...new Set(fixes)]) console.log(`  • ${fix}`);
    console.log("\nAfter fixing, reload the extension at chrome://extensions and refresh the page.");
    process.exitCode = 1;
  });
