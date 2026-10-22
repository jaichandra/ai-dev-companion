//! Fail-closed classification of MCP tool names into allow (read-only) or deny
//! (everything else), used by the MCP guard hook at run time (port of
//! `core/mcp-tool-classifier.js`).
//!
//! An MCP tool name looks like `mcp__<server>__<action>`, e.g.
//! `mcp__someserver__jira_get_issue`. The action is everything after the LAST
//! "__". A tool is allowed only when its action's first `_`-separated token is
//! a known read verb, or, for actions namespaced by service (`jira_get_issue`,
//! `confluence_search`), when the first token is a known namespace and the
//! SECOND token is a read verb. Anything that doesn't fit (no action, no verb,
//! an unrecognized namespace) is denied. Deliberately conservative:
//! `jira_batch_get_changelogs` is denied because its first non-namespace token
//! is `batch`, even though the tool is read-only, because guessing wrong the
//! other way (allowing a mutation) is the failure mode that matters. The
//! companion's own MCP server is always denied, so headless Claude runs can't
//! call back into the companion's services.

pub const COMPANION_MCP_SERVER: &str = "ai-companion";

pub const READ_VERBS: [&str; 12] =
    ["get", "list", "search", "query", "read", "grep", "glob", "find", "download", "view", "fetch", "ask"];

/// Service namespaces that legitimately prefix an action before the verb.
const NAMESPACES: [&str; 6] = ["jira", "confluence", "bitbucket", "jenkins", "argocd", "slack"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    Allow,
    Deny,
}

/// `None` (an absent or non-string name) denies, like the empty string.
pub fn classify_mcp_tool(tool_name: Option<&str>) -> ToolClass {
    use ToolClass::{Allow, Deny};
    let name = tool_name.unwrap_or("");
    let Some(sep) = name.rfind("__") else { return Deny };

    // The server is everything between "mcp__" and the last "__".
    if name.starts_with("mcp__") && name.get(5..sep) == Some(COMPANION_MCP_SERVER) {
        return Deny;
    }

    let tokens: Vec<String> = name[sep + 2..].split('_').filter(|t| !t.is_empty()).map(str::to_lowercase).collect();
    let Some(first) = tokens.first() else { return Deny };
    if READ_VERBS.contains(&first.as_str()) {
        return Allow;
    }
    match tokens.get(1) {
        Some(second) if NAMESPACES.contains(&first.as_str()) && READ_VERBS.contains(&second.as_str()) => Allow,
        _ => Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::ToolClass::{Allow, Deny};
    use super::*;

    fn c(name: &str) -> ToolClass {
        classify_mcp_tool(Some(name))
    }

    #[test]
    fn read_verbs_is_the_exact_fail_closed_allowlist_of_read_ish_verbs() {
        assert_eq!(
            READ_VERBS,
            ["get", "list", "search", "query", "read", "grep", "glob", "find", "download", "view", "fetch", "ask"]
        );
    }

    #[test]
    fn allows_read_only_tools() {
        for name in [
            "mcp__acme-jira-confluence__jira_get_issue",
            "mcp__acme-jira-confluence__confluence_search",
            "mcp__acme-jenkins__get_build_console_output",
            "mcp__wiki__query_wiki",
            "mcp__wiki__grep_repo",
            "mcp__acme-bitbucket__list_pull_requests",
            "mcp__acme-jira-confluence__confluence_download_attachment",
        ] {
            assert_eq!(c(name), Allow, "{name}");
        }
    }

    #[test]
    fn denies_everything_else() {
        for name in [
            "mcp__acme-jira-confluence__jira_delete_issue",
            "mcp__acme-jira-confluence__jira_create_issue",
            "mcp__acme-jira-confluence__confluence_delete_page",
            "mcp__acme-jira-confluence__confluence_move_page",
            "mcp__acme-jira-confluence__jira_assign_issue",
            "mcp__acme-bitbucket__decline_pull_request",
            "mcp__acme-bitbucket__delete_branch",
            "mcp__acme-bitbucket__manage_comment",
            "mcp__acme-jenkins__stop_build",
            "mcp__acme-jenkins__set_node_config",
            "mcp__acme-jenkins__run_groovy_script",
            "mcp__acme-argocd__create_application",
            "mcp__acme-argocd__run_resource_action",
            "mcp__acme-jira-confluence__jira_batch_get_changelogs",
            "mcp__x__unknownthing",
            "nounderscores",
            "",
        ] {
            assert_eq!(c(name), Deny, "{name:?}");
        }
    }

    #[test]
    fn fails_closed_on_a_missing_name() {
        assert_eq!(classify_mcp_tool(None), Deny);
    }

    #[test]
    fn is_case_insensitive_on_the_verb_token() {
        assert_eq!(c("mcp__wiki__QUERY_wiki"), Allow);
        assert_eq!(c("mcp__acme-jira-confluence__JIRA_get_issue"), Allow);
    }

    #[test]
    fn denies_the_companions_own_mcp_server() {
        for tool in ["list_jobs", "get_job", "analyze_issue", "forget_item", "open_location"] {
            assert_eq!(c(&format!("mcp__ai-companion__{tool}")), Deny, "{tool}");
        }
    }

    #[test]
    fn allows_servers_with_ai_companion_in_the_name_but_not_an_exact_match() {
        assert_eq!(c("mcp__ai-companion-other__list_x"), Allow);
    }

    // Beyond the ported tests: edges of the slicing the JS did on UTF-16 units.
    #[test]
    fn edge_cases_of_the_name_shape() {
        assert_eq!(c("mcp__x"), Deny); // "__" at index 3, so the server slice is empty
        assert_eq!(c("a___get"), Allow); // the LAST "__" is at index 2: action "_get"
        assert_eq!(c("mcp__srv__"), Deny); // no action tokens
        assert_eq!(c("mcp__srv__ _get"), Deny);
        assert_eq!(c("mcp__srv__jira_"), Deny);
        assert_eq!(c("mcp__srv__jira"), Deny);
        assert_eq!(c("mcp__sérvér__jira_get_x"), Allow);
    }
}
