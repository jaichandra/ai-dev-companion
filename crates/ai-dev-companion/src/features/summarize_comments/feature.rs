//! `summarize-comments`: everything the framework needs to know about this feature in one place (port of
//! `features/summarize-comments/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;
use std::sync::Arc;
use crate::config::DEFAULT_SUMMARIZE_COMMENTS_MODEL;
use crate::setup_helpers::has_agent;

pub fn feature_spec<F>(factory: F, _setup: &SetupContext) -> FeatureSpec<F> {
    let mut d = FeatureDescriptor::new("summarize-comments", "Summarize comments");
    d.summary = Some("Short summary of a Jira ticket's comment thread".into());
    d.description = "A short summary of a Jira ticket's comment thread (decisions, open questions, next steps), written by Claude Haiku. Read-only; only offered on tickets that have comments.".into();
    d.required_checks = vec!["claudeCli".into(), "claudeAuth".into()];
    d.prompt_setup = Some(Arc::new(|p, existing| {
        let saved = saved_text(existing, "summarizeComments", "model");
        let current = if saved.is_empty() { DEFAULT_SUMMARIZE_COMMENTS_MODEL.to_string() } else { saved };
        let model = if has_agent() { current } else { ask_or(p, "Claude model to use for Summarize comments (Enter keeps Haiku)", &current) };
        json!({ "summarizeComments": { "model": model } })
    }));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/summarize-comments.js", 100.0));
    spec
}
