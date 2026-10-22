//! Small helpers the feature descriptors' setup prompts share (port of `core/setup-helpers.js`).

use crate::environment;
use crate::prereqs::detect_installed_editors;

/// An editor Review PR can open: `config.reviewEditor` must be one of these ids. VS Code is hidden
/// for now (nothing offers or detects it); a saved "vscode" is ignored and the editor is detected
/// again. Shared by setup's prompt and the Settings panel so the two can't offer different choices.
pub struct ReviewEditor {
    pub id: &'static str,
    pub label: &'static str,
}

pub const REVIEW_EDITORS: [ReviewEditor; 2] = [ReviewEditor { id: "cursor", label: "Cursor" }, ReviewEditor { id: "claude-code", label: "Claude Code (opens a new terminal)" }];

// The servers setup assumes instead of asking (the profile); Settings can change them.
pub fn default_jira_base_url() -> &'static str {
    environment::current().default_base_url("jira")
}

pub fn default_jenkins_base_url() -> &'static str {
    environment::current().default_base_url("jenkins")
}

pub fn default_bitbucket_base_url() -> &'static str {
    environment::current().default_base_url("bitbucket")
}

/// Whether Claude Code or Cursor is installed, so model questions can be skipped.
pub fn has_agent() -> bool {
    !detect_installed_editors().is_empty()
}

/// The file the borrowed token (a pack's external tokens) for `site` at `base_url` would come
/// from, or "" when there is none (or setup gave no detector). Never the token.
/// A pack's borrowed-token detector: site id and base URL to the file the token would come from.
pub type TokenDetector<'a> = &'a dyn Fn(&str, &str) -> Option<String>;

pub fn platform_token_source(external_token: Option<TokenDetector<'_>>, site: &str, base_url: &str) -> String {
    external_token.and_then(|f| f(site, base_url)).unwrap_or_default()
}

/// Says how `label` is authenticated, so the prompts match what the companion really does: a
/// borrowed token (`source`) or the browser login. `tool` is the pack's external-token label.
pub fn log_auth_note(log: &mut dyn FnMut(&str), label: &str, source: &str, tool: &str) {
    if source.is_empty() {
        log(&format!("{label}: no {tool} credentials found, so your browser's {label} login is used."));
    } else {
        log(&format!("{label}: using the credentials from {tool} ({source}). No token needed."));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_review_editors_are_cursor_and_claude_code_only() {
        assert_eq!(REVIEW_EDITORS.iter().map(|e| e.id).collect::<Vec<_>>(), ["cursor", "claude-code"]);
    }

    #[test]
    fn the_default_base_urls_come_from_the_profile() {
        assert_eq!(default_jira_base_url(), environment::current().default_base_url("jira"));
        assert_eq!(default_jenkins_base_url(), environment::current().default_base_url("jenkins"));
        assert_eq!(default_bitbucket_base_url(), environment::current().default_base_url("bitbucket"));
    }

    #[test]
    fn a_token_source_is_empty_without_a_detector_or_a_hit() {
        assert_eq!(platform_token_source(None, "jira", "https://j"), "");
        assert_eq!(platform_token_source(Some(&|_, _| None), "jira", "https://j"), "");
        let hit = |site: &str, url: &str| Some(format!("{site}@{url}"));
        assert_eq!(platform_token_source(Some(&hit), "jira", "https://j"), "jira@https://j");
    }

    #[test]
    fn the_auth_note_says_borrowed_or_browser() {
        let mut lines = vec![];
        log_auth_note(&mut |l| lines.push(l.to_string()), "Jira", "~/.tool/config", "Tool X");
        log_auth_note(&mut |l| lines.push(l.to_string()), "Jira", "", "Tool X");
        assert_eq!(lines[0], "Jira: using the credentials from Tool X (~/.tool/config). No token needed.");
        assert_eq!(lines[1], "Jira: no Tool X credentials found, so your browser's Jira login is used.");
    }
}
