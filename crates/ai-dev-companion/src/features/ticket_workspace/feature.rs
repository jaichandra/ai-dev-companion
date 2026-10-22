//! `ticket-workspace`: everything the framework needs to know about this feature in one place (port of
//! `features/ticket-workspace/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;
use std::sync::Arc;

pub fn feature_spec<F>(factory: F, _setup: &SetupContext) -> FeatureSpec<F> {
    let mut d = FeatureDescriptor::new("ticket-workspace", "Ticket workspace");
    d.summary = Some("See a ticket's branches, PRs, builds and past analyses".into());
    d.description = "On a Jira ticket, show its local branches and worktrees, their Bitbucket PRs and builds, and earlier analyses and Claude sessions \u{2014} read-only, on a click.".into();
    d.required_checks = vec!["git".into()];
    d.needs_repos = true;
    d.prompt_setup = Some(Arc::new(|_, _| json!({})));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/ticket-workspace.js", 80.0));
    spec.scope_key = ScopeKey::Kind(ScopeKeyKind::JiraIssue);
    spec.persist = Some(false);
    spec.history = HistoryFlags { read_only: true, event_only: true, ..Default::default() };
    spec.mcp_tools = vec![tool(
        "ticket_workspace",
        "ticket-workspace",
        "read",
        "Everything the companion can find for a Jira ticket on this machine: local branches and worktrees whose name has the key (uncommitted changes, commits ahead), their Bitbucket pull requests and build state, and earlier analyses and Claude Code sessions from the local history. Read-only. Use it to pick up work on a ticket.",
        json!({ "issueKey": { "type": "issueKey", "description": "The Jira issue key, e.g. \"PROJ-1\"." } }),
    )];
    spec
}
