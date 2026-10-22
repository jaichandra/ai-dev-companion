//! `analyze-issue`: everything the framework needs to know about this feature in one place (port of
//! `features/analyze-issue/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::{json, Value};
use std::sync::Arc;
use crate::js_text::js_trim;
use crate::setup_helpers::{default_jira_base_url, has_agent};

pub fn feature_spec<F>(factory: F, setup: &SetupContext) -> FeatureSpec<F> {
    let setup = setup.clone();
    // Setup-wizard-facing name only; the extension feature file's menuLabel ("Analyze ticket") is unchanged.
    let mut d = FeatureDescriptor::new("analyze-issue", "Analyze a Jira issue");
    d.summary = Some("Read-only Claude analysis of a Jira issue and the repo it touches".into());
    d.description = "Fetch an issue from the configured Jira projects (default: PROJ Bugs), identify the local repo, and run a read-only Claude analysis (with WebFetch and MCP servers from ~/.claude.json).".into();
    d.required_checks = vec!["claudeCli".into(), "claudeAuth".into()];
    d.needs_repos = true;
    d.prompt_setup = Some(Arc::new(move |p, existing| {
        // Not asked when Claude Code or Cursor is available: the model stays whatever it was (blank =
        // the tool's own default), and Settings can change it later.
        let saved = saved_text(existing, "analyzeIssue", "model");
        let model = if has_agent() { saved.clone() } else { js_trim(&ask_or(p, "Claude model to use for Analyze ticket (Enter keeps Claude Code's default)", &saved)).to_string() };
        let mut analyze_issue = spread(existing, "analyzeIssue");
        if model.is_empty() {
            analyze_issue.shift_remove("model");
        } else {
            analyze_issue.insert("model".into(), json!(model));
        }
        let mut partial = serde_json::Map::new();
        partial.insert("analyzeIssue".into(), Value::Object(analyze_issue));
        // Only ask for the Jira base URL when it isn't already set: create-jira-subtasks (or a prior
        // analyze-issue run) may have written it already.
        if saved_text(existing, "jira", "baseUrl").is_empty() {
            let base_url = default_jira_base_url();
            let source = setup.source("jira", base_url);
            setup.note(p, "Jira", &source);
            let mut jira = spread(existing, "jira");
            jira.insert("baseUrl".into(), json!(base_url));
            partial.insert("jira".into(), Value::Object(jira));
        }
        Value::Object(partial)
    }));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/analyze-issue.js", 70.0));
    spec.scope_key = ScopeKey::Kind(ScopeKeyKind::JiraIssue);
    spec.history = HistoryFlags { read_only: true, ..Default::default() };
    spec.mcp_tools = vec![
        tool(
            "get_issue_analysis",
            "analyze-issue",
            "read",
            "Get the stored analysis for a Jira issue that analyze_issue already ran, without starting a new run.",
            json!({ "issueKey": { "type": "issueKey", "description": "The Jira issue key, e.g. \"PROJ-1\"." } }),
        ),
        tool(
            "analyze_issue",
            "analyze-issue",
            "start",
            "Start a read-only Claude analysis of a Jira issue: fetch the ticket, identify the local repo it concerns, and produce a written analysis. Runs immediately \u{2014} analyze-issue's Claude policy is read-only, so nothing it does can push, comment or otherwise change anything. It accepts issues of the projects and issue types configured in Settings (default: PROJ Bugs).",
            json!({
                "issueKey": { "type": "issueKey", "description": "The Jira issue key, e.g. \"PROJ-1\"." },
                "force": { "type": "boolean", "optional": true, "description": "Re-run even if a cached analysis for this issue already exists." },
            }),
        ),
    ];
    spec
}
