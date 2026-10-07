// The Claude Code plugin (plugin/ in the repo, listed by the repo's own
// .claude-plugin/marketplace.json): the lines `node install.js --plugin`
// prints, and — for doctor — whether the plugin is installed and enabled,
// read from Claude Code's own files. Pure: callers pass the file texts in.
const PLUGIN_NAME = "ai-companion";
const MARKETPLACE_NAME = "ai-dev-companion";
const PLUGIN_ID = `${PLUGIN_NAME}@${MARKETPLACE_NAME}`;

/** What `node install.js --plugin` prints: nothing is installed, fetched or written. */
function pluginInstallText({ stableDir, updateUrl, updateRef }) {
  const lines = [
    "The companion as a Claude Code plugin (its MCP tools, the skills companion-status, resume-ticket and",
    "pre-push-check, and a one-line inbox count when a session starts). In Claude Code, run:",
    "",
    `  /plugin marketplace add ${stableDir}`,
    `  /plugin install ${PLUGIN_ID}`,
  ];
  if (typeof updateUrl === "string" && /^https:\/\//.test(updateUrl)) {
    lines.push("", "Or, for the team, from the shared repository:", "", `  /plugin marketplace add ${updateUrl}${updateRef ? `#${updateRef}` : ""}`, `  /plugin install ${PLUGIN_ID}`);
  }
  lines.push(
    "",
    "The plugin needs the companion installed here (node install.js) and talks to the running service.",
    "Setup also registers the same tools with `claude mcp add` (user scope); with the plugin on, remove that",
    "one so the tools don't appear twice: claude mcp remove --scope user ai-companion",
  );
  return lines.join("\n");
}

/** Claude Code's config folder: $CLAUDE_CONFIG_DIR when set, else ~/.claude. */
function claudeConfigDir(env, home) {
  const custom = env && typeof env.CLAUDE_CONFIG_DIR === "string" ? env.CLAUDE_CONFIG_DIR.trim() : "";
  return custom || require("path").join(home, ".claude");
}

const parse = (text) => {
  if (typeof text !== "string" || !text.trim()) return null;
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
};

/** Whether our plugin is installed (<config dir>/plugins/installed_plugins.json)
 * and enabled (<config dir>/settings.json `enabledPlugins`), under any marketplace name. */
function pluginStatus({ installedPluginsText, settingsText }) {
  const installed = parse(installedPluginsText);
  const plugins = installed && typeof installed.plugins === "object" && installed.plugins ? Object.keys(installed.plugins) : [];
  const ours = plugins.filter((id) => id.split("@")[0] === PLUGIN_NAME);
  const settings = parse(settingsText);
  const enabledMap = settings && typeof settings.enabledPlugins === "object" && settings.enabledPlugins ? settings.enabledPlugins : {};
  const enabled = ours.some((id) => enabledMap[id] === true);
  return { installed: ours.length > 0, enabled, ids: ours };
}

/**
 * Doctor's one line on how Claude Code reaches the companion's tools:
 * `claudeState` is core/mcp-registration.js's state for Claude Code
 * ("ok", "missing", "wrong-url", "stale-token", "unreadable") or null when
 * Claude Code isn't installed; `plugin` is pluginStatus().
 */
function mcpPathSummary({ claudeState, plugin }) {
  const viaAdd = claudeState === "ok";
  const viaPlugin = !!(plugin && plugin.enabled);
  if (viaAdd && viaPlugin) {
    return {
      level: "WARN",
      message: "Claude Code reaches the companion both through `claude mcp add` and the ai-companion plugin, so its tools appear twice.",
      fix: "claude mcp remove --scope user ai-companion (or disable the plugin)",
    };
  }
  if (viaPlugin) return { level: "OK", message: "Claude Code reaches the companion through the ai-companion plugin." };
  if (viaAdd) {
    return {
      level: "OK",
      message: `Claude Code reaches the companion through \`claude mcp add\` (user scope)${plugin && plugin.installed ? "; the ai-companion plugin is installed but disabled" : ""}.`,
    };
  }
  if (plugin && plugin.installed) {
    return { level: "WARN", message: "The ai-companion plugin is installed but disabled, and `claude mcp add` isn't set up either.", fix: "/plugin enable ai-companion (in Claude Code), or npm run setup" };
  }
  return null;
}

module.exports = {
  claudeConfigDir, PLUGIN_NAME, MARKETPLACE_NAME, PLUGIN_ID, pluginInstallText, pluginStatus, mcpPathSummary };
