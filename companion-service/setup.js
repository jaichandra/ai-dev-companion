#!/usr/bin/env node
// Interactive setup, start to finish: pick features, check (and help fix)
// the tools they need, find the local clones they work on, ask each
// enabled feature's own questions, write config.json plus the matching
// chrome-extension/companion-config.js, then build, (re)start and verify
// the companion service — so a first run ends with something that works,
// and a re-run (e.g. to add a repo) takes effect without any manual
// restart.
//
// Feature selection drives everything else here — see
// core/feature-registry.js for the descriptors (id/label/description/
// requiredChecks/needsRepos/promptSetup) this file is built around. Adding
// a new feature to the product means adding one descriptor there; nothing
// in this file's prompting/check logic hardcodes feature ids.
//
//   node setup.js                 full interactive run
//   node setup.js --no-service    skip building/starting the service
const fs = require("fs");
const { PRODUCT_NAME } = require("./core/product-name.js");
const os = require("os");
const path = require("path");
const readline = require("readline");
const crypto = require("crypto");
const { spawn, spawnSync } = require("child_process");
const prereqs = require("./core/prereqs.js");
const registry = require("./core/feature-registry.js");
const serviceControl = require("./core/service-control.js");
const settings = require("./core/settings.js");
const credentialStore = require("./core/credential-store.js");
const paths = require("./core/paths.js");
const mcpRegistration = require("./core/mcp-registration.js");
const extensionManifest = require("./core/extension-manifest.js");
const externalTokens = require("./core/external-tokens.js").createExternalTokens();
const siteUrls = require("./core/site-urls.js");
const environment = require("./environment.js");

const IS_MAC = process.platform === "darwin";

const CONFIG_PATH = path.join(__dirname, "config.json");
const CHROME_EXTENSION_DIR = path.join(__dirname, "..", "chrome-extension");
const EXTENSION_CONFIG_PATH = path.join(CHROME_EXTENSION_DIR, "companion-config.js");
const PROVISION_SCRIPT_PATH = path.join(__dirname, "provision-review-worktrees.js");
const PROVISION_LOG_PATH = path.join(__dirname, "review-worktrees-provision.log");
const DEFAULT_PORT = 8787;

const FIX_HINTS = {
  git: "Install git — on macOS run `xcode-select --install` — then come back here.",
  claudeCli: "Install Claude Code (https://claude.com/claude-code), then come back here.",
  claudeAuth: "In another terminal run `claude auth login` and finish logging in, then come back here.",
};

/** "~/gitviews/sample-app" rather than the full home path, for display only. */
function tildify(p) {
  const home = os.homedir();
  return p === home || p.startsWith(home + path.sep) ? `~${p.slice(home.length)}` : p;
}

function heading(title) {
  console.log(`\n── ${title} ${"─".repeat(Math.max(0, 60 - title.length))}\n`);
}

/** The connection file the service worker loads (see core/extension-manifest.js). */
const renderExtensionConfig = extensionManifest.renderExtensionConfig;

/** What the extension folder needs from this config: the connection file and a manifest whose hosts
 * and feature scripts match the profile, the configured base URLs and the packs. */
function writeExtensionFiles(secret, port, config) {
  extensionManifest.writeExtensionFiles(CHROME_EXTENSION_DIR, { ...config, sharedSecret: secret, port });
}

function ask(rl, question, defaultValue) {
  return new Promise((resolve) => {
    const suffix = defaultValue !== undefined && defaultValue !== "" ? ` [${defaultValue}]` : "";
    rl.question(`${question}${suffix}: `, (answer) => resolve(answer.trim() || defaultValue));
  });
}

async function confirm(rl, question, defaultYes) {
  // The capital letter in (Y/n) already shows the default, so no [y] suffix.
  const answer = await new Promise((resolve) =>
    rl.question(`${question} (${defaultYes ? "Y/n" : "y/N"}): `, (a) => resolve(a.trim())),
  );
  return answer ? /^y/i.test(answer) : defaultYes;
}

/**
 * Numbered choice prompt: prints each option as "N) label" (marking the
 * default), then accepts either the number or the option's own id typed
 * directly, defaulting to `defaultValue` on a blank answer. Re-prompts —
 * rather than throwing or silently falling back — on anything else: the
 * generic ask() helper has no concept of "must be one of these," and a
 * typo here would otherwise write whatever garbage was typed straight
 * into config.json. `options` is [{id, label}]; returns the chosen id.
 */
async function select(rl, question, options, defaultValue) {
  console.log(question);
  for (const [i, opt] of options.entries()) {
    console.log(`  ${i + 1}) ${opt.label}${opt.id === defaultValue ? " (default)" : ""}`);
  }
  const defaultIndex = options.findIndex((o) => o.id === defaultValue);
  const defaultAnswer = String(defaultIndex >= 0 ? defaultIndex + 1 : 1);
  for (;;) {
    const answer = await ask(rl, "Enter a number", defaultAnswer);
    const byNumber = options[parseInt(answer, 10) - 1];
    const byId = options.find((o) => o.id === answer.trim());
    const chosen = byNumber || byId;
    if (chosen) return chosen.id;
    console.log(
      `  Please enter a number from 1 to ${options.length}, or one of: ${options.map((o) => o.id).join(", ")}.`,
    );
  }
}

/**
 * Parses a multi-choice answer over `count` numbered items: "all", "none",
 * or numbers separated by commas/spaces ("1,3" / "1 3"). Returns the
 * chosen 0-based indices in ascending order, or null if the answer isn't
 * valid (so the caller can re-prompt).
 */
function parseNumberList(answer, count) {
  const text = String(answer || "")
    .trim()
    .toLowerCase();
  if (text === "all") return [...Array(count).keys()];
  if (text === "none") return [];
  const parts = text.split(/[\s,]+/).filter(Boolean);
  if (parts.length === 0) return null;
  const indices = new Set();
  for (const part of parts) {
    const n = Number(part);
    if (!Number.isInteger(n) || n < 1 || n > count) return null;
    indices.add(n - 1);
  }
  return [...indices].sort((a, b) => a - b);
}

/** Asks a parseNumberList question until the answer is valid. `defaultIndices`
 * is what a blank answer means. */
async function askNumberList(rl, question, count, defaultIndices) {
  const defaultText =
    defaultIndices.length === count
      ? "all"
      : defaultIndices.length === 0
        ? "none"
        : defaultIndices.map((i) => i + 1).join(",");
  for (;;) {
    const answer = await ask(rl, question, defaultText);
    const indices = parseNumberList(answer, count);
    if (indices) return indices;
    console.log(`  Please enter "all", "none", or numbers from 1 to ${count} (e.g. 1,3).`);
  }
}

/** setup.js's own encrypted-store instance, over the same two paths
 * core/credentials.ts's singleton resolves to — companionServiceDir() is
 * just `__dirname` here, since setup.js always runs from the top-level
 * companion-service/ folder (never from dist/, unlike server.ts). Built
 * directly against core/credential-store.js rather than requiring
 * core/credentials.ts: that file is TypeScript, and setup.js — like every
 * other file here — has to run as `node setup.js` with no build step. */
let checkedCredentials = false;
function realCredentialStore() {
  const filePath = path.join(__dirname, "credentials.enc");
  const store = credentialStore.createCredentialStore({
    filePath,
    keyPath: path.join(paths.stateDir(), "credentials.key"),
  });
  // A credentials.enc left behind without its key (the key lives in the
  // state dir, so clearing that dir orphans the file) can never be read
  // again, and would fail every later step. Clear it once, saying so.
  if (!checkedCredentials) {
    checkedCredentials = true;
    try {
      store.list();
    } catch (err) {
      if (err && err.code === "undecryptable") {
        fs.rmSync(filePath, { force: true });
        console.log(
          "Note: saved tokens from an earlier install couldn't be unlocked (their key is gone), so they were cleared.",
        );
      }
    }
  }
  return store;
}

/** Moves any jira/jenkins/bitbucket apiToken sitting in `config` (freshly
 * typed into it by a feature's promptSetup, or left over from before this
 * migration existed) into the encrypted credential store, blanking it in
 * `config` in place. No saveConfig callback here — every caller already
 * writes config.json itself right after this returns, migrated or not
 * (see core/settings.js's migrateTokensToStore for why a caller normally
 * needs one). `store` is injectable so a test can pass a temp-dir store
 * instead of ever touching the real credentials.enc/credentials.key. */
function migrateApiTokens(config, store = realCredentialStore()) {
  settings.migrateTokensToStore(config, store, () => {});
}

/**
 * Registers the companion's own MCP server ("ai-companion") with whatever
 * MCP clients are present on this machine — Claude Code (if the `claude`
 * CLI is on PATH) and Cursor (if `~/.cursor` exists) — see
 * core/mcp-registration.js. Re-run after every setup, interactive or
 * `--yes`, so a freshly created mcp.token or a changed port takes effect
 * without a separate step. Never fails setup: any problem here (a
 * `claude mcp add` failure, an unwritable ~/.cursor/mcp.json) is reported
 * as a WARN line the same way a failed prerequisite check is, not thrown.
 */
function registerMcpEverywhere(config) {
  try {
    const { token } = mcpRegistration.ensureMcpToken(realCredentialStore());
    const result = mcpRegistration.registerEverywhere({
      port: config.port,
      token,
      home: os.homedir(),
      claudeAvailable: prereqs.checkClaudeCli().ok,
      spawnSync,
    });
    for (const [label, entry] of [
      ["Claude Code", result.claude],
      ["Cursor", result.cursor],
    ]) {
      if (!entry) continue;
      console.log(
        entry.ok
          ? `✓ Registered the companion's MCP server with ${label} (ai-companion).`
          : `WARN - couldn't register with ${label}: ${entry.message} — run \`npm run setup\` again later.`,
      );
    }
  } catch (err) {
    console.log(
      `WARN - couldn't register the companion's MCP server: ${err.message} — run \`npm run setup\` again later.`,
    );
  }
}

/** Returns the parsed config if configPath already holds a usable one, else null. */
function loadExistingConfig(configPath) {
  if (!fs.existsSync(configPath)) return null;
  try {
    const config = JSON.parse(fs.readFileSync(configPath, "utf8"));
    if (typeof config.port === "number" && typeof config.sharedSecret === "string") {
      return config;
    }
  } catch {
    // Falls through to null — an unreadable/malformed config.json is treated
    // the same as "none yet" rather than a hard error.
  }
  return null;
}

/**
 * Lists every feature once, then takes the whole selection in a single
 * answer ("all", "none", or numbers). A blank answer keeps
 * `currentlyEnabledIds`, so blitzing Enter preserves the status quo.
 * Returns the selected ids, in registry order.
 */
/** A description's first sentence, cut to fit one terminal line. */
function oneLine(text, max = 90) {
  const first = text.split(/\.\s/)[0].replace(/\.$/, "");
  return first.length <= max ? first : `${first.slice(0, max - 1).trimEnd()}…`;
}

async function promptFeatureSelection(rl, currentlyEnabledIds) {
  const descriptors = registry.FEATURE_DESCRIPTORS;
  for (const [i, d] of descriptors.entries()) {
    console.log(`  ${String(i + 1).padStart(2)}) ${d.label}`);
    console.log(`      ${d.summary || oneLine(d.description)}`);
  }
  console.log();
  const defaults = descriptors.flatMap((d, i) => (currentlyEnabledIds.includes(d.id) ? [i] : []));
  const indices = await askNumberList(
    rl,
    'Which features do you want? ("all", "none", or numbers like 1,3)',
    descriptors.length,
    defaults,
  );
  const enabled = indices.map((i) => descriptors[i].id);
  if (enabled.length === 0) {
    console.log(
      "\n⚠ No features enabled — the ✨ button won't offer anything until you re-run setup and pick some.",
    );
  }
  return enabled;
}

/**
 * Runs the checks the selected features need. For each failure, explains
 * how to fix it and waits: Enter re-checks (so the user can fix it in
 * another terminal without restarting setup), "skip" continues without the
 * features that need it. `checks` is injectable for tests. Returns the ids
 * still selected once everything left passes.
 */
async function resolvePrerequisites(rl, selectedIds, checks = prereqs.CHECKS_BY_NAME) {
  let remaining = [...selectedIds];
  const names = registry.requiredChecksFor(remaining);
  if (names.length === 0) {
    console.log("  Nothing to check — the selected features only need Node.");
    return remaining;
  }
  for (const name of names) {
    if (!registry.requiredChecksFor(remaining).includes(name)) continue;
    for (;;) {
      const result = checks[name]();
      console.log(`  ${result.ok ? "OK  " : "FAIL"} - ${result.ok ? result.message.replace(/ OK\.$/, "") : result.message}`);
      if (result.ok) break;
      const needing = registry.featuresRequiring(name, remaining);
      console.log(`         Needed by: ${needing.map((d) => d.label).join(", ")}`);
      console.log(`         ${FIX_HINTS[name] || "Fix it, then come back here."}`);
      const answer = await ask(
        rl,
        '         Press Enter to check again, or type "skip" to continue without those features',
        "",
      );
      if (/^s/i.test(answer)) {
        const skipped = new Set(needing.map((d) => d.id));
        remaining = remaining.filter((id) => !skipped.has(id));
        console.log(`  Skipped: ${needing.map((d) => d.label).join(", ")} (re-run setup later to enable).`);
        break;
      }
    }
  }
  return remaining;
}

/**
 * Returns `existingRepos` (never mutated) plus every Bitbucket clone found
 * in common folders (see core/prereqs.js's defaultSearchRoots), keyed from
 * each clone's origin URL, and the keys that were new. Nothing is asked:
 * the repos map is only a cache — anything missed here is found or set up
 * from the browser the first time a PR in it is used. Pre-filling still
 * helps, as it catches clones in folders not named after their repo.
 * `searchRoots` is injectable for tests.
 */
function discoverRepos(existingRepos, searchRoots = undefined) {
  const repos = { ...existingRepos };
  const roots = searchRoots ?? [
    ...new Set([
      ...prereqs.defaultSearchRoots(),
      ...Object.values(existingRepos).map((p) => path.dirname(p)),
    ]),
  ];
  const mappedKeys = new Set(Object.keys(repos).map((k) => k.toLowerCase()));
  const mappedPaths = new Set(Object.values(repos).map((p) => path.resolve(p)));
  const added = [];
  for (const { key, path: clonePath } of prereqs.discoverLocalClones(roots)) {
    if (mappedKeys.has(key.toLowerCase()) || mappedPaths.has(path.resolve(clonePath))) continue;
    if (!prereqs.checkRepoPath(clonePath).ok) continue;
    repos[key] = clonePath;
    mappedKeys.add(key.toLowerCase());
    added.push(key);
  }
  return { repos, added };
}

/**
 * Runs promptSetup for every selected feature that actually needs it —
 * always for one newly enabled just now (nothing to keep, so it must be
 * configured), or, for one that was already enabled and still is, only if
 * asked to update it (defaulting to no, so a quick re-run doesn't force
 * you through every enabled feature's prompts again just because they're
 * still enabled). Returns the partial config to merge in — omitted keys
 * simply keep whatever `existingConfig` already had, via the caller's
 * spread.
 */
async function collectFeatureConfigs(rl, selectedIds, previouslyEnabledIds, existingConfig) {
  // Each feature's name is printed only when it actually says or asks
  // something (many have nothing to configure), and a message several
  // features share — the same site's credentials — is said once.
  const said = new Set();
  const helpersFor = (descriptor) => {
    let titled = false;
    const title = () => {
      if (titled) return;
      titled = true;
      console.log(`${descriptor.label}:`);
    };
    return {
      ask: (...args) => (title(), ask(...args)),
      select: (...args) => (title(), select(...args)),
      log: (msg) => {
        if (said.has(msg)) return;
        said.add(msg);
        title();
        console.log(`  ${msg}`);
      },
      // A token another tool stored for a site (core/external-tokens.js), if one is stored for this exact host.
      externalToken: (site, baseUrl) => externalTokens.tokenFor(`${site}.apiToken`, baseUrl),
      done: () => titled && console.log(),
    };
  };
  let merged = {};
  for (const descriptor of registry.FEATURE_DESCRIPTORS) {
    // A feature with nothing to ask (e.g. Resolve Merge Conflicts) has no promptSetup.
    if (!selectedIds.includes(descriptor.id) || !descriptor.promptSetup) continue;

    const isNewlyEnabled = !previouslyEnabledIds.includes(descriptor.id);
    let shouldPrompt = isNewlyEnabled;
    if (!isNewlyEnabled) {
      shouldPrompt = await confirm(rl, `Change "${descriptor.label}" settings?`, false);
    }
    if (!shouldPrompt) continue;

    const helpers = helpersFor(descriptor);
    const partial = await descriptor.promptSetup(rl, helpers, existingConfig);
    merged = { ...merged, ...partial };
    helpers.done();
  }
  return merged;
}

/** First free port at or above DEFAULT_PORT. */
async function pickFreePort() {
  for (let port = DEFAULT_PORT; port < DEFAULT_PORT + 50; port++) {
    if ((await prereqs.checkPortFree(port)).ok) return port;
  }
  throw new Error(`No free port found between ${DEFAULT_PORT} and ${DEFAULT_PORT + 49}.`);
}

/** Starts the review-worktree provisioning in the background (see
 * provision-review-worktrees.js) — cloning worktrees for N repos can take
 * minutes, which is no reason to hold up the rest of setup. */
function startWorktreeProvisioning() {
  const out = fs.openSync(PROVISION_LOG_PATH, "a");
  const child = spawn(process.execPath, [PROVISION_SCRIPT_PATH], {
    cwd: __dirname,
    detached: true,
    stdio: ["ignore", out, out],
  });
  child.unref();
  console.log(
    `Preparing a PR-review worktree for each repo in the background. Progress: ${tildify(PROVISION_LOG_PATH)}`,
  );
}

/**
 * Builds the service, (re)starts it as a launchd job on macOS, and waits
 * until it answers with the configured secret. Returns true if it's up.
 */
async function startService(rl, config) {
  process.stdout.write("Building the companion service... ");
  const build = serviceControl.buildServer();
  if (!build.ok) {
    console.log("FAILED\n");
    console.log(build.output);
    console.log(`\nFix the error above, then re-run: cd ${tildify(__dirname)} && npm run setup`);
    return false;
  }
  console.log("done.");

  if (!IS_MAC) {
    console.log(
      "\nBackground mode needs macOS (launchd). Start the service in a terminal you keep open:\n" +
        `  cd ${tildify(__dirname)} && npm start`,
    );
    return false;
  }

  const launchd = require("./core/launchd.js");
  let background = launchd.isInstalled();
  if (!background) {
    console.log(
      "\nThe companion service has to be running for the extension to work. Recommended: run it in the\n" +
        "background — it starts automatically when you log in and restarts itself if it crashes.",
    );
    background = await confirm(rl, "Run it in the background?", true);
  }
  if (!background) {
    console.log(
      `\nOK — start it yourself whenever you use the extension:\n  cd ${tildify(__dirname)} && npm start`,
    );
    return false;
  }

  process.stdout.write("Starting it in the background... ");
  try {
    launchd.installService();
  } catch (err) {
    console.log("FAILED\n");
    console.log(err.message);
    console.log(`\nRun \`cd ${tildify(__dirname)} && npm run doctor\` for more detail.`);
    return false;
  }
  const health = await serviceControl.waitForHealthy(config.port, config.sharedSecret);
  if (health.state === "ok") {
    console.log("done.");
    console.log(`✓ ${health.message}`);
    return true;
  }
  console.log("FAILED\n");
  console.log(health.message);
  const { out, err } = launchd.logPaths();
  const logTail = [serviceControl.tailFile(out, 10), serviceControl.tailFile(err, 10)]
    .filter(Boolean)
    .join("\n");
  if (logTail) console.log(`\nLast lines of the service log:\n${logTail}`);
  console.log(`\nRun \`cd ${tildify(__dirname)} && npm run doctor\` to diagnose.`);
  return false;
}

/**
 * Polls `isConnected` until the extension has said hello, `timeoutMs`
 * passes, or the user presses Enter to skip — whichever comes first.
 * Resolves { connected, version, skipped }.
 */
function waitForExtension(rl, isConnected, { intervalMs = 1000, timeoutMs = 10 * 60 * 1000 } = {}) {
  return new Promise((resolve) => {
    const controller = new AbortController();
    const deadline = Date.now() + timeoutMs;
    let done = false;
    let timer;
    const finish = (result) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      controller.abort();
      resolve(result);
    };
    rl.question(
      "Waiting for the extension to connect (press Enter to skip)... ",
      { signal: controller.signal },
      () => finish({ connected: false, version: null, skipped: true }),
    );
    const poll = async () => {
      const status = await isConnected();
      if (done) return;
      if (status.connected) {
        finish({ connected: true, version: status.version, skipped: false });
      } else if (Date.now() >= deadline) {
        finish({ connected: false, version: null, skipped: false });
      } else {
        timer = setTimeout(poll, intervalMs);
      }
    };
    poll();
  });
}

/** Bold cyan on a color terminal, so the one thing to act on stands out. */
function highlight(text) {
  return process.stdout.isTTY && !process.env.NO_COLOR ? `\x1b[1;36m${text}\x1b[0m` : text;
}

/**
 * Walks the user through loading the extension — Chrome only lets a person
 * do that (Developer mode + Load unpacked) — with the folder on the
 * clipboard, then, if the service is up, waits until the extension actually
 * connects to it. Chrome is only opened once the user presses Enter, so it
 * doesn't cover the instructions before they've been read. A clickable
 * link isn't an option: nothing on macOS handles chrome:// URLs except
 * Chrome itself, via `open -a`.
 */
async function connectChrome(rl, config, serviceUp) {
  const extensionDir = path.resolve(CHROME_EXTENSION_DIR);
  const copied = IS_MAC && spawnSync("pbcopy", { input: extensionDir }).status === 0;
  heading("5. Connect Chrome");
  console.log("First time? In Chrome (or Edge / Brave):");
  console.log(`  1. Go to   ${highlight("chrome://extensions")}`);
  console.log('  2. Turn on "Developer mode" (top right)');
  console.log('  3. Click "Load unpacked" and choose this folder:');
  console.log(`       ${highlight(extensionDir)}`);
  if (copied) {
    console.log(
      "     (It's on your clipboard — in the folder dialog press Cmd+Shift+G, paste, press Enter.)",
    );
  }
  console.log(`\nAlready loaded it before? Click the ↻ reload icon on ${PRODUCT_NAME} there,`);
  console.log("then refresh any open Bitbucket/Jira/Jenkins tabs.\n");

  if (IS_MAC) {
    const answer = await ask(
      rl,
      `Press Enter to open ${highlight("chrome://extensions")} in Chrome, or type "n" to go there yourself`,
      "",
    );
    if (!/^n/i.test(answer)) {
      const opened = spawnSync("open", ["-a", "Google Chrome", "chrome://extensions"]).status === 0;
      console.log(
        opened
          ? "Opened it in Chrome — follow the steps above, then come back here.\n"
          : "Couldn't open Chrome — type chrome://extensions into your browser's address bar.\n",
      );
    }
  }

  if (serviceUp) {
    const result = await waitForExtension(rl, () =>
      serviceControl.checkExtensionConnected(config.port, config.sharedSecret),
    );
    if (result.connected) {
      console.log(`\n✓ The extension is connected${result.version ? ` (v${result.version})` : ""}.`);
    } else if (!result.skipped) {
      console.log("\nThe extension hasn't connected yet — finish the steps above, then reload it.");
    }
  }
  console.log("\nOpen a Bitbucket PR or a Jira Story and look for the ✨ button at the bottom right.");
  console.log("To change these settings later, use ⚙ in its menu (or re-run this setup).");
  console.log(`Something not working? Run: cd ${tildify(__dirname)} && npm run doctor`);
}

/**
 * The config a `--yes` run writes: the existing settings unchanged except
 * for newly discovered repo clones and any default-on features shipped
 * since the last save (see registry.migrateEnabledFeatures —
 * enabledByDefault:false stays out of enabledFeatures). Returns null when
 * there's nothing to keep (a first install has to be interactive).
 */
function nonInteractiveConfig(existing, discover = discoverRepos) {
  if (!existing) return null;
  const migration = registry.migrateEnabledFeatures(existing);
  const base = migration ? { ...existing, ...migration } : existing;
  const enabledFeatures = registry.enabledFeatureIds(base);
  const repos = registry.needsRepos(enabledFeatures)
    ? discover(base.repos || {}).repos
    : base.repos || {};
  return { ...base, enabledFeatures, repos };
}

/**
 * `setup.js --yes`: keep every existing setting, ask nothing, rebuild, and
 * restart the background service if it's installed (unless --no-service).
 * Used by the in-browser updater (core/update-runner.js), which restarts
 * the service itself. Never opens a readline — stdin may not be a terminal.
 */
async function runNonInteractive(skipService) {
  console.log(`== ${PRODUCT_NAME} setup (keeping your settings) ==\n`);
  const config = nonInteractiveConfig(loadExistingConfig(CONFIG_PATH));
  if (!config) {
    console.log(`No existing settings in ${tildify(CONFIG_PATH)} — run setup without --yes first.`);
    process.exitCode = 1;
    return;
  }
  for (const name of registry.requiredChecksFor(config.enabledFeatures)) {
    const result = prereqs.CHECKS_BY_NAME[name]();
    if (!result.ok) console.log(`WARN - ${result.message}`);
  }
  migrateApiTokens(config);
  registerMcpEverywhere(config);
  settings.writeConfigFile(CONFIG_PATH, config);
  writeExtensionFiles(config.sharedSecret, config.port, config);
  console.log(`Kept your settings (${tildify(CONFIG_PATH)}).`);

  process.stdout.write("Building the companion service... ");
  const build = serviceControl.buildServer();
  if (!build.ok) {
    console.log(`FAILED\n\n${build.output}`);
    process.exitCode = 1;
    return;
  }
  console.log("done.");

  if (skipService) return;
  const launchd = IS_MAC ? require("./core/launchd.js") : null;
  if (!launchd || !launchd.isInstalled()) {
    console.log(
      `The service isn't running in the background. Restart it: cd ${tildify(__dirname)} && npm start`,
    );
    return;
  }
  launchd.installService();
  const health = await serviceControl.waitForHealthy(config.port, config.sharedSecret);
  console.log(health.state === "ok" ? `✓ ${health.message}` : `FAILED - ${health.message}`);
  if (health.state !== "ok") process.exitCode = 1;
}

async function main() {
  const skipService = process.argv.includes("--no-service");
  if (process.argv.includes("--yes")) {
    await runNonInteractive(skipService);
    return;
  }
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  let finished = false;
  let saved = false;
  rl.on("close", () => {
    if (!finished) {
      const state = saved
        ? "your settings were saved, but the service may not be running"
        : "nothing was changed";
      console.log(`\n\nSetup stopped before finishing — ${state}. Re-run it with: npm run setup`);
      process.exit(1);
    }
  });

  console.log(`== ${PRODUCT_NAME} setup ==\n`);
  console.log("This will: choose features, check the tools they need, then start the companion");
  console.log("service and connect the Chrome extension to it.");
  console.log("Press Enter at any question to accept the [default] shown.");

  const nodeCheck = prereqs.checkNodeVersion();
  if (!nodeCheck.ok) {
    console.log(`\nFAIL - ${nodeCheck.message} Install a newer Node (https://nodejs.org), then re-run this.`);
    finished = true;
    rl.close();
    process.exitCode = 1;
    return;
  }

  // Re-running this later (e.g. via `npx git+...` to pick up an update)
  // should feel like an update, not a re-install — reuse whatever's
  // already configured (including which features were enabled) rather
  // than asking the same questions again from scratch.
  const existingOrNull = loadExistingConfig(CONFIG_PATH);
  const existing = existingOrNull || {};
  // Deliberately not just registry.enabledFeatureIds(existing) here: that
  // treats a *missing* enabledFeatures field as "everything," which is
  // the right backward-compatible reading for an existing config.json
  // that predates this field — but a brand new install has no
  // enabledFeatures because nothing has been configured yet, and every
  // feature must count as newly enabled so its questions get asked.
  const previouslyEnabledIds = existingOrNull ? registry.enabledFeatureIds(existing) : [];
  if (existingOrNull) {
    console.log(`\nFound your existing settings (${tildify(CONFIG_PATH)}) — they're the defaults below.`);
  }

  heading("1. Features");
  // Defaults: previously enabled ids, plus any default-on feature shipped
  // since the last save (enabledFeatureIds already soft-migrates;
  // enabledByDefault:false stays off). Fresh install: every default-on
  // feature — default-off ones are still listed so the user can opt in.
  const selectionDefaultIds = existingOrNull
    ? previouslyEnabledIds
    : registry.defaultEnabledFeatureIds();
  let selectedIds = await promptFeatureSelection(rl, selectionDefaultIds);

  heading("2. Checking the tools these features need");
  selectedIds = await resolvePrerequisites(rl, selectedIds);

  heading("3. Feature settings");
  // Before the features' own questions, which would otherwise record the profile's placeholder address.
  const siteConfig = await siteUrls.promptSiteUrls(environment, existing, { ask: (q, d) => ask(rl, q, d) });
  Object.assign(existing, siteConfig);
  const featureConfig = await collectFeatureConfigs(rl, selectedIds, previouslyEnabledIds, existing);
  if (selectedIds.length === 0) console.log("  (no features selected)");

  let port = existing.port;
  if (port === undefined) port = await pickFreePort();
  const sharedSecret = existing.sharedSecret || crypto.randomBytes(24).toString("hex");

  let repos = existing.repos || {};
  let addedRepos = [];
  if (registry.needsRepos(selectedIds)) ({ repos, added: addedRepos } = discoverRepos(repos));

  const config = {
    ...existing,
    port,
    sharedSecret,
    enabledFeatures: selectedIds,
    // Record every descriptor we know about so a later --yes / loadConfig
    // migrate only auto-enables features shipped *after* this save — not
    // ones the user just unchecked above.
    knownFeatures: registry.allFeatureIds(),
    repos,
    ...featureConfig,
  };
  migrateApiTokens(config);
  registerMcpEverywhere(config);
  settings.writeConfigFile(CONFIG_PATH, config);
  writeExtensionFiles(sharedSecret, port, config);
  saved = true;

  heading("4. Saving and starting the companion service");
  console.log(`Saved ${tildify(CONFIG_PATH)}`);
  console.log(
    `Saved ${tildify(EXTENSION_CONFIG_PATH)} (connects the extension to the service on port ${port})`,
  );
  if (registry.needsRepos(selectedIds)) {
    const found = addedRepos.length > 0 ? `Found ${addedRepos.length} more local repo clone(s). ` : "";
    console.log(
      `${found}Any repo without a local clone is set up from the browser the first time you use it — ` +
        "you'll be offered to pick its folder or have it cloned.",
    );
  }
  console.log();

  let serviceUp = false;
  if (skipService) {
    console.log(
      `Not starting the service (--no-service). To start it: cd ${tildify(__dirname)} && npm start`,
    );
  } else {
    serviceUp = await startService(rl, config);
  }

  if (selectedIds.includes("review-in-editor") && Object.keys(repos).length > 0) {
    console.log();
    startWorktreeProvisioning();
  }

  await connectChrome(rl, config, serviceUp);

  finished = true;
  rl.close();
}

if (require.main === module) {
  main().catch((err) => {
    console.error("\nSetup failed:", err.message);
    process.exit(1);
  });
}

module.exports = {
  renderExtensionConfig,
  writeExtensionFiles,
  loadExistingConfig,
  parseNumberList,
  discoverRepos,
  nonInteractiveConfig,
  promptFeatureSelection,
  resolvePrerequisites,
  collectFeatureConfigs,
  select,
  waitForExtension,
  migrateApiTokens,
  EXTENSION_CONFIG_PATH,
};
