//! The Claude Code plugin (`plugin/` in the repo, listed by the repo's own
//! `.claude-plugin/marketplace.json`): the lines `install --plugin` prints, and, for doctor,
//! whether the plugin is installed and enabled, read from Claude Code's own files (port of
//! `core/plugin-info.js`). Pure: callers pass the file texts in.

use crate::environment;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const PLUGIN_NAME: &str = "ai-companion";
/// The mod (`mods/companion-panes/`) with the Run pane and the shell tab.
pub const PANES_NAME: &str = "companion-panes";

/// The marketplace is the repo that holds `.claude-plugin/marketplace.json`, so a distribution
/// names it in its profile (`branding.marketplaceName`); the framework's own repo is
/// `ai-dev-companion`.
pub fn marketplace_name() -> String {
    environment::current().branding_text("marketplaceName").unwrap_or("ai-dev-companion").to_string()
}

pub fn plugin_id() -> String {
    format!("{PLUGIN_NAME}@{}", marketplace_name())
}

pub struct PluginInstall<'a> {
    pub stable_dir: &'a str,
    pub update_url: Option<&'a str>,
    pub update_ref: Option<&'a str>,
}

/// What `install --plugin` prints: nothing is installed, fetched or written.
pub fn plugin_install_text(i: &PluginInstall<'_>) -> String {
    let id = plugin_id();
    let mut lines: Vec<String> = vec![
        "The companion as a Claude Code plugin (its MCP tools, the skills companion-status, resume-ticket and".into(),
        "pre-push-check, and a one-line inbox count when a session starts). In Claude Code, run:".into(),
        String::new(),
        format!("  /plugin marketplace add {}", i.stable_dir),
        format!("  /plugin install {id}"),
    ];
    if let Some(url) = i.update_url.filter(|u| u.starts_with("https://")) {
        let suffix = i.update_ref.filter(|r| !r.is_empty()).map(|r| format!("#{r}")).unwrap_or_default();
        lines.extend([String::new(), "Or, for the team, from the shared repository:".into(), String::new(), format!("  /plugin marketplace add {url}{suffix}"), format!("  /plugin install {id}")]);
    }
    lines.extend([
        String::new(),
        "The Run pane (/dev) and the shell tab (/shell) are loaded automatically in the Claude Code sessions the companion".into(),
        "opens. To have them in every session you start yourself, also run:".into(),
        String::new(),
        format!("  /plugin install {PANES_NAME}@{}", marketplace_name()),
        String::new(),
        "The plugin needs the companion installed here (its `install` command) and talks to the running service.".into(),
        "Setup also registers the same tools with `claude mcp add` (user scope); with the plugin on, remove that".into(),
        "one so the tools don't appear twice: claude mcp remove --scope user ai-companion".into(),
    ]);
    lines.join("\n")
}

/// Claude Code's config folder: `$CLAUDE_CONFIG_DIR` when set, else `~/.claude`.
pub fn claude_config_dir(config_dir_env: Option<&str>, home: &Path) -> PathBuf {
    match config_dir_env.map(crate::js_text::js_trim).filter(|c| !c.is_empty()) {
        Some(custom) => PathBuf::from(custom),
        None => home.join(".claude"),
    }
}

fn parse(text: Option<&str>) -> Option<Value> {
    let text = text.filter(|t| !crate::js_text::js_trim(t).is_empty())?;
    serde_json::from_str(text).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStatus {
    pub installed: bool,
    pub enabled: bool,
    pub ids: Vec<String>,
}

/// Whether our plugin is installed (`<config dir>/plugins/installed_plugins.json`) and enabled
/// (`<config dir>/settings.json` `enabledPlugins`), under any marketplace name.
pub fn plugin_status(installed_plugins_text: Option<&str>, settings_text: Option<&str>) -> PluginStatus {
    let installed = parse(installed_plugins_text);
    let ours: Vec<String> = installed.as_ref().and_then(|i| i["plugins"].as_object()).map(|m| m.keys().filter(|id| id.split('@').next() == Some(PLUGIN_NAME)).cloned().collect()).unwrap_or_default();
    let settings = parse(settings_text);
    let enabled_map = settings.as_ref().and_then(|s| s["enabledPlugins"].as_object());
    let enabled = ours.iter().any(|id| enabled_map.is_some_and(|m| m.get(id) == Some(&Value::Bool(true))));
    PluginStatus { installed: !ours.is_empty(), enabled, ids: ours }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPath {
    pub level: Level,
    pub message: String,
    pub fix: Option<&'static str>,
}

/// Doctor's one line on how Claude Code reaches the companion's tools: `claude_state` is
/// `mcp_registration`'s state for Claude Code ("ok", "missing", "wrong-url", "stale-token",
/// "unreadable") or `None` when Claude Code isn't installed.
pub fn mcp_path_summary(claude_state: Option<&str>, plugin: Option<&PluginStatus>) -> Option<McpPath> {
    let via_add = claude_state == Some("ok");
    let via_plugin = plugin.is_some_and(|p| p.enabled);
    let installed = plugin.is_some_and(|p| p.installed);
    if via_add && via_plugin {
        return Some(McpPath { level: Level::Warn, message: "Claude Code reaches the companion both through `claude mcp add` and the ai-companion plugin, so its tools appear twice.".into(), fix: Some("claude mcp remove --scope user ai-companion (or disable the plugin)") });
    }
    if via_plugin {
        return Some(McpPath { level: Level::Ok, message: "Claude Code reaches the companion through the ai-companion plugin.".into(), fix: None });
    }
    if via_add {
        let note = if installed { "; the ai-companion plugin is installed but disabled" } else { "" };
        return Some(McpPath { level: Level::Ok, message: format!("Claude Code reaches the companion through `claude mcp add` (user scope){note}."), fix: None });
    }
    if installed {
        return Some(McpPath { level: Level::Warn, message: "The ai-companion plugin is installed but disabled, and `claude mcp add` isn't set up either.".into(), fix: Some("/plugin enable ai-companion (in Claude Code), or npm run setup") });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(installed: bool, enabled: bool) -> PluginStatus {
        PluginStatus { installed, enabled, ids: vec![] }
    }

    #[test]
    fn the_install_text_names_the_plugin_and_adds_the_shared_repo_only_for_an_https_source() {
        let id = plugin_id();
        let local = plugin_install_text(&PluginInstall { stable_dir: "/home/me/ai-dev-companion", update_url: Some("/local/repo"), update_ref: None });
        assert!(local.contains("/plugin marketplace add /home/me/ai-dev-companion") && local.contains(&format!("/plugin install {id}")));
        assert!(!local.contains("for the team"));
        assert!(local.contains(&format!("/plugin install companion-panes@{}", marketplace_name())));
        let team = plugin_install_text(&PluginInstall { stable_dir: "/s", update_url: Some("https://git.example.com/x.git"), update_ref: Some("release") });
        assert!(team.contains("/plugin marketplace add https://git.example.com/x.git#release"));
        assert!(plugin_install_text(&PluginInstall { stable_dir: "/s", update_url: Some("https://h/x.git"), update_ref: None }).contains("add https://h/x.git\n"));
    }

    #[test]
    fn the_config_dir_is_the_env_var_when_set_else_dot_claude() {
        let home = Path::new("/home/me");
        assert_eq!(claude_config_dir(Some(" /custom "), home), PathBuf::from("/custom"));
        assert_eq!(claude_config_dir(Some("  "), home), PathBuf::from("/home/me/.claude"));
        assert_eq!(claude_config_dir(None, home), PathBuf::from("/home/me/.claude"));
    }

    #[test]
    fn plugin_status_reads_installed_and_enabled_under_any_marketplace_and_survives_junk() {
        let installed = r#"{"plugins":{"ai-companion@acme-tools":[{}],"other@x":[{}]}}"#;
        let on = r#"{"enabledPlugins":{"ai-companion@acme-tools":true}}"#;
        let off = r#"{"enabledPlugins":{"ai-companion@acme-tools":false}}"#;
        assert_eq!(plugin_status(Some(installed), Some(on)), PluginStatus { installed: true, enabled: true, ids: vec!["ai-companion@acme-tools".into()] });
        assert!(!plugin_status(Some(installed), Some(off)).enabled);
        assert!(!plugin_status(Some(installed), None).enabled);
        for junk in [None, Some(""), Some("not json"), Some("[]"), Some("{\"plugins\":null}")] {
            assert_eq!(plugin_status(junk, junk), PluginStatus { installed: false, enabled: false, ids: vec![] });
        }
    }

    #[test]
    fn the_mcp_path_summary_warns_on_duplicates_and_on_a_disabled_plugin_with_nothing_else() {
        assert_eq!(mcp_path_summary(Some("ok"), Some(&status(true, true))).unwrap().level, Level::Warn);
        assert!(mcp_path_summary(Some("ok"), Some(&status(true, true))).unwrap().fix.unwrap().contains("claude mcp remove"));
        assert!(mcp_path_summary(Some("missing"), Some(&status(true, true))).unwrap().message.contains("through the ai-companion plugin"));
        let add = mcp_path_summary(Some("ok"), Some(&status(true, false))).unwrap();
        assert_eq!(add.level, Level::Ok);
        assert!(add.message.ends_with("installed but disabled."));
        assert!(!mcp_path_summary(Some("ok"), None).unwrap().message.contains("disabled"));
        let disabled = mcp_path_summary(Some("missing"), Some(&status(true, false))).unwrap();
        assert_eq!(disabled.level, Level::Warn);
        assert!(disabled.fix.unwrap().starts_with("/plugin enable"));
        assert_eq!(mcp_path_summary(None, Some(&status(false, false))), None);
        assert_eq!(mcp_path_summary(Some("wrong-url"), None), None);
    }
}
