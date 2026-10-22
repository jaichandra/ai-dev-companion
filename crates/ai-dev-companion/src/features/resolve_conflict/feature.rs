//! `resolve-conflict`: everything the framework needs to know about this feature in one place (port of
//! `features/resolve-conflict/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;

pub fn feature_spec<F>(factory: F, _setup: &SetupContext) -> FeatureSpec<F> {
    // Setup-wizard-facing name only; the extension file's own menuLabel ("Resolve Conflict") is left as is.
    let mut d = FeatureDescriptor::new("resolve-conflict", "Resolve Merge Conflicts");
    d.summary = Some("Fix a PR's merge conflicts with Claude, in a throwaway worktree".into());
    d.description = "Detect a merge-conflicted Bitbucket PR and resolve it with Claude in an isolated git worktree.".into();
    d.required_checks = vec!["git".into(), "claudeCli".into(), "claudeAuth".into()];
    d.needs_repos = true;
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/resolve-conflict.js", 10.0));
    spec.scope_key = ScopeKey::Kind(ScopeKeyKind::Pr);
    spec.mcp_tools = vec![tool(
        "start_resolve_conflict",
        "resolve-conflict",
        "pending-start",
        "Queues a Resolve Conflict job for a Bitbucket PR. Nothing runs until the user clicks Start on the PR page in Chrome; tell them to open it.",
        json!({
            "project": { "type": "repoSegment", "description": "The Bitbucket project key, e.g. \"CI\"." },
            "repo": { "type": "repoSegment", "description": "The Bitbucket repo slug, e.g. \"sample-app\"." },
            "prId": { "type": "positiveInt", "description": "The pull request number." },
        }),
    )];
    spec
}
