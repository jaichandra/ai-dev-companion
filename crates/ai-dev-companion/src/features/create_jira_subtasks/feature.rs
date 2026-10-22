//! `create-jira-subtasks`: everything the framework needs to know about this feature in one place (port of
//! `features/create-jira-subtasks/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;
use std::sync::Arc;
use crate::setup_helpers::default_jira_base_url;

pub fn feature_spec<F>(factory: F, setup: &SetupContext) -> FeatureSpec<F> {
    let setup = setup.clone();
    // Setup-wizard-facing name only; the extension feature file's menuLabel ("Create Subtasks") is unchanged.
    let mut d = FeatureDescriptor::new("create-jira-subtasks", "Create JIRA Subtasks (multiple at a time)");
    d.summary = Some("Add several subtasks to a Jira Story or Epic at once".into());
    d.description = "Add subtasks to a Jira Story/Epic, with search-as-you-type assignees.".into();
    d.prompt_setup = Some(Arc::new(move |p, existing| {
        let saved_url = saved_text(existing, "jira", "baseUrl");
        let base_url = if saved_url.is_empty() { default_jira_base_url().to_string() } else { saved_url };
        let source = setup.source("jira", &base_url);
        setup.note(p, "Jira", &source);
        let saved_token = saved_text(existing, "jira", "apiToken");
        let token = if source.is_empty() { ask_or(p, "Jira API token (leave blank to rely on browser SSO)", &saved_token) } else { saved_token };
        json!({ "jira": { "baseUrl": base_url, "apiToken": token } })
    }));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/create-jira-subtasks.js", 30.0));
    spec
}
