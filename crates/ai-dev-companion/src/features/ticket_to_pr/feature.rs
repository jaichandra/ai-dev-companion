//! `ticket-to-pr`: everything the framework needs to know about this feature in one place (port of
//! `features/ticket-to-pr/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;
use std::sync::Arc;

pub fn feature_spec<F>(factory: F, _setup: &SetupContext) -> FeatureSpec<F> {
    let mut d = FeatureDescriptor::new("ticket-to-pr", "Ticket to PR");
    d.summary = Some("Start a fix branch from a ticket, then open the PR".into());
    d.description = "Start fix: a worktree and branch for a Jira ticket, with Claude Code on its analysis. Create PR: push, open the PR with default reviewers, link it on the ticket and move it to review.".into();
    d.required_checks = vec!["git".into(), "claudeCli".into(), "claudeAuth".into()];
    d.needs_repos = true;
    d.prompt_setup = Some(Arc::new(|_, _| json!({})));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/ticket-to-pr.js", 90.0));
    spec.scope_key = ScopeKey::Kind(ScopeKeyKind::JiraIssue);
    spec.history = HistoryFlags { milestone: true, ..Default::default() };
    spec
}
