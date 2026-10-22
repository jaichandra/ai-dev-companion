//! `address-review-comments`: everything the framework needs to know about this feature in one place (port of
//! `features/address-review-comments/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn feature_spec<F>(factory: F, setup: &SetupContext) -> FeatureSpec<F> {
    let setup = setup.clone();
    let mut d = FeatureDescriptor::new("address-review-comments", "Address review comments");
    d.summary = Some("Have the AI fix a PR's open review comments; you approve the diff".into());
    d.description = "Have Claude fix a Bitbucket PR's open review comments in an isolated worktree; you review the diff, then push and reply.".into();
    d.required_checks = vec!["git".into(), "claudeCli".into(), "claudeAuth".into()];
    d.needs_repos = true;
    d.prompt_setup = Some(Arc::new(move |p, existing| {
        // The profile's git host, whichever it is (its site id is the config key): Bitbucket or GitHub.
        let env = crate::environment::current();
        let Some(git) = env.sites.iter().find(|s| s.kind == "git") else { return json!({}) };
        let base_url = {
            let saved = saved_text(existing, &git.id, "baseUrl");
            if saved.is_empty() { git.base_url.clone() } else { saved }
        };
        let source = setup.source(&git.id, &base_url);
        setup.note(p, git.label.as_deref().unwrap_or(&git.id), &source);
        let mut entry = spread(existing, &git.id);
        entry.insert("baseUrl".into(), json!(base_url));
        json!({ git.id.clone(): Value::Object(entry) })
    }));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/address-review-comments.js", 20.0));
    spec.scope_key = ScopeKey::Kind(ScopeKeyKind::Pr);
    spec.mcp_tools = vec![tool(
        "start_address_review_comments",
        "address-review-comments",
        "pending-start",
        "Queues an Address Review Comments job for a Bitbucket PR. Nothing runs until the user clicks Start on the PR page in Chrome; tell them to open it.",
        json!({
            "project": { "type": "repoSegment", "description": "The Bitbucket project key, e.g. \"CI\"." },
            "repo": { "type": "repoSegment", "description": "The Bitbucket repo slug, e.g. \"sample-app\"." },
            "prId": { "type": "positiveInt", "description": "The pull request number." },
        }),
    )];
    spec
}
