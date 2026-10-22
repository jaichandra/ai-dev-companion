//! `review-in-editor`: everything the framework needs to know about this feature in one place (port of
//! `features/review-in-editor/feature.js`): its setup descriptor, the MCP tools, scope-key rule, persistence and
//! history behaviour, and where its extension file sits. Listed in a pack (`packs::builtin`).

use super::super::declare::*;
use crate::packs::*;
use serde_json::json;
use std::sync::Arc;
use crate::prereqs::detect_installed_editors;
use crate::setup_helpers::REVIEW_EDITORS;

/// An editor's label without the ` (...)` at its end.
fn short_label(label: &str) -> &str {
    match label.find(" (") {
        Some(i) if label.ends_with(')') => &label[..i],
        _ => label,
    }
}

pub fn feature_spec<F>(factory: F, _setup: &SetupContext) -> FeatureSpec<F> {
    // Setup-wizard-facing name only; the extension feature file's menuLabel ("Review PR") is unchanged.
    let mut d = FeatureDescriptor::new("review-in-editor", "Review PRs in your editor");
    d.summary = Some("Check out a PR and have your editor's AI review it".into());
    d.description = "Check out a PR's source branch into a dedicated per-repo worktree and open it in your editor.".into();
    // A missing editor isn't a hard prerequisite: the feature fails with a clear message until setup
    // is re-run to pick one. Git, which it can't function without, is.
    d.required_checks = vec!["git".into()];
    d.needs_repos = true;
    d.prompt_setup = Some(Arc::new(|p, existing| {
        // No question when the answer is obvious: a saved choice, else Claude Code if installed, else
        // Cursor (detect_installed_editors lists them in that order). Only a machine with neither is asked.
        let saved = existing["reviewEditor"].as_str().filter(|s| REVIEW_EDITORS.iter().any(|e| e.id == *s)).map(String::from);
        let picked = saved.clone().or_else(|| detect_installed_editors().first().map(|e| e.id.to_string()));
        if let Some(picked) = picked {
            let label = REVIEW_EDITORS.iter().find(|e| e.id == picked).map_or("", |e| short_label(e.label));
            p.log(&format!("Review PR editor: {label}{}.", if saved.is_some() { "" } else { " (detected)" }));
            return json!({ "reviewEditor": picked });
        }
        let options: Vec<String> = REVIEW_EDITORS.iter().map(|e| format!("{} ({})", e.label, e.id)).collect();
        let answer = ask_or(p, &format!("Which editor should \"Review PR\" open PR branches in? (Neither Claude Code nor Cursor was found.) Choose one of: {}", options.join(", ")), "claude-code");
        let chosen = REVIEW_EDITORS.iter().find(|e| e.id == answer).map_or("claude-code", |e| e.id);
        json!({ "reviewEditor": chosen })
    }));
    let mut spec = FeatureSpec::new(d, factory);
    spec.extension = Some(extension("features/review-in-editor.js", 50.0));
    spec.persist = Some(false);
    spec
}

#[cfg(test)]
mod tests {
    use super::short_label;

    #[test]
    fn the_editor_label_loses_its_parenthesis() {
        assert_eq!(short_label("Claude Code (opens a new terminal)"), "Claude Code");
        assert_eq!(short_label("Cursor"), "Cursor");
    }
}
