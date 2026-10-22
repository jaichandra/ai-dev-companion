//! `digest`: everything the framework needs to know about this feature in one place (port of
//! `features/digest/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;
use std::sync::Arc;

pub fn feature_spec<F>(factory: F, _setup: &SetupContext) -> FeatureSpec<F> {
    let mut d = FeatureDescriptor::new("digest", "Morning digest");
    d.summary = Some("Your PRs, reviews and tickets at a glance".into());
    d.description = "Your PRs (conflicts, builds, approvals), PRs waiting on your review, your open tickets and what the background watchers prepared \u{2014} read-only, on a click (and, if you turn it on, every weekday morning).".into();
    d.prompt_setup = Some(Arc::new(|_, _| json!({})));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/digest.js", 110.0));
    spec.persist = Some(false);
    spec.history = HistoryFlags { read_only: true, event_only: true, ..Default::default() };
    spec.mcp_tools = vec![tool(
        "get_digest",
        "digest",
        "read",
        "Today's digest for the developer: their open pull requests (conflicts, failing builds, approvals, needs-work), pull requests waiting on their review, their open Jira tickets, and what the background watchers prepared. Read-only.",
        json!({}),
    )];
    spec
}
