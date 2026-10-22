//! The single place that knows "what features exist and what does each one
//! need to be configured/checked" (port of `core/feature-registry.js`).
//!
//! It no longer lists the features itself: each declares its own descriptor in
//! its `feature.js` equivalent, and a pack lists them. Adding a feature means
//! adding it to a pack; nothing in setup, the server or the startup checks
//! changes, since they all drive themselves off this list.

use crate::packs::{FeatureDescriptor, PackRegistry};
use serde_json::Value;

/// The editors the review features can open, as (id, label). A saved "vscode"
/// is ignored and the editor is detected again. Shared by setup's prompt and
/// the Settings panel so the two can't offer different choices.
pub const REVIEW_EDITORS: [(&str, &str); 2] = [("cursor", "Cursor"), ("claude-code", "Claude Code (opens a new terminal)")];

/// A soft-migrated `enabledFeatures` / `knownFeatures` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub enabled_features: Vec<String>,
    pub known_features: Vec<String>,
}

pub struct FeatureRegistry {
    /// The descriptors in play: every descriptor in pack order minus any
    /// marked `disabled`. A disabled id left in an existing config's
    /// `enabledFeatures` is filtered out by the `known` checks below.
    descriptors: Vec<FeatureDescriptor>,
}

fn strings(v: &Value) -> Option<Vec<&str>> {
    // Non-string entries can never name a feature, so they are dropped.
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).collect())
}

impl FeatureRegistry {
    pub fn new(all: Vec<FeatureDescriptor>) -> Self {
        Self { descriptors: all.into_iter().filter(|d| !d.disabled).collect() }
    }

    pub fn from_packs<F>(packs: &PackRegistry<F>) -> Self {
        Self::new(packs.features().map(|s| s.descriptor.clone()).collect())
    }

    pub fn descriptors(&self) -> &[FeatureDescriptor] {
        &self.descriptors
    }

    /// All descriptor ids in declared order: for `knownFeatures` snapshots and
    /// for listing every selectable feature.
    pub fn all_feature_ids(&self) -> Vec<String> {
        self.descriptors.iter().map(|d| d.id.clone()).collect()
    }

    /// Ids that are on by default for a fresh install / soft-migrate
    /// auto-enable: every descriptor except those with `enabledByDefault: false`.
    pub fn default_enabled_feature_ids(&self) -> Vec<String> {
        self.descriptors.iter().filter(|d| d.enabled_by_default != Some(false)).map(|d| d.id.clone()).collect()
    }

    /// Soft-migrate an explicit `enabledFeatures` list when the product ships a
    /// new descriptor. `None` when nothing changed.
    ///
    /// Without this, an existing config that lists older features by id would
    /// permanently hide every feature added later until the user re-runs setup.
    /// - No `enabledFeatures` field: nothing to migrate (everything is on).
    /// - No `knownFeatures` yet (legacy): any known id missing from the enabled
    ///   list is treated as newly shipped. Ones with `enabledByDefault: false`
    ///   are recorded in `knownFeatures` only; the rest turn on.
    /// - With `knownFeatures`: only ids absent from it are candidates for
    ///   auto-enable (still gated by `enabledByDefault`); ids the user
    ///   unchecked stay off.
    pub fn migrate_enabled_features(&self, config: &Value) -> Option<Migration> {
        let enabled_in = strings(&config["enabledFeatures"]).filter(|_| config["enabledFeatures"].is_array())?;
        let all = self.all_feature_ids();
        let default_on = self.default_enabled_feature_ids();
        let known_prior: Vec<&str> = strings(&config["knownFeatures"]).unwrap_or_else(|| enabled_in.clone());

        let mut enabled: Vec<&str> = enabled_in.iter().copied().filter(|id| all.iter().any(|a| a == id)).collect();
        for id in &all {
            // Newly shipped: always enters knownFeatures; only auto-enabled when default-on.
            if !known_prior.contains(&id.as_str()) && default_on.contains(id) && !enabled.contains(&id.as_str()) {
                enabled.push(id);
            }
        }
        let enabled_features: Vec<String> = all.iter().filter(|id| enabled.contains(&id.as_str())).cloned().collect();
        let known_features = all.clone();

        let prev_enabled: Vec<&str> = enabled_in.iter().copied().filter(|id| all.iter().any(|a| a == id)).collect();
        let same_enabled = prev_enabled.len() == enabled_features.len() && enabled_features.iter().all(|id| prev_enabled.contains(&id.as_str()));
        let same_known = strings(&config["knownFeatures"]).is_some_and(|k| k.len() == known_features.len() && known_features.iter().all(|id| k.contains(&id.as_str())));
        if same_enabled && same_known {
            return None;
        }
        Some(Migration { enabled_features, known_features })
    }

    /// Ids actually enabled by `config`: its `enabledFeatures` if present, else
    /// every known feature (legacy configs predating the field). Applies the
    /// migration first so a list frozen before a new default-on feature shipped
    /// still surfaces that feature; default-off new features stay out of the
    /// menu until the user opts in.
    pub fn enabled_feature_ids(&self, config: &Value) -> Vec<String> {
        let known = self.all_feature_ids();
        let list: Option<Vec<String>> = match self.migrate_enabled_features(config) {
            Some(m) => Some(m.enabled_features),
            None => strings(&config["enabledFeatures"]).filter(|_| config["enabledFeatures"].is_array()).map(|l| l.into_iter().map(String::from).collect()),
        };
        match list {
            Some(l) => l.into_iter().filter(|id| known.contains(id)).collect(),
            None => known,
        }
    }

    /// Union of `requiredChecks` across exactly the descriptors whose id is in
    /// `ids`: the checks setup and the server actually need to run.
    pub fn required_checks_for(&self, ids: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = vec![];
        for d in self.descriptors.iter().filter(|d| ids.contains(&d.id.as_str())) {
            for n in &d.required_checks {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
        }
        names
    }

    /// True if any descriptor whose id is in `ids` works on mapped repos.
    pub fn needs_repos(&self, ids: &[&str]) -> bool {
        self.descriptors.iter().any(|d| ids.contains(&d.id.as_str()) && d.needs_repos)
    }

    /// The descriptors in `ids` that require check `check_name`: what setup
    /// offers to switch off when that check can't be satisfied.
    pub fn features_requiring(&self, check_name: &str, ids: &[&str]) -> Vec<&FeatureDescriptor> {
        self.descriptors.iter().filter(|d| ids.contains(&d.id.as_str()) && d.required_checks.iter().any(|c| c == check_name)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(id: &str, checks: &[&str], repos: bool, on: Option<bool>) -> FeatureDescriptor {
        let mut f = FeatureDescriptor::new(id, id);
        f.required_checks = checks.iter().map(|c| c.to_string()).collect();
        f.needs_repos = repos;
        f.enabled_by_default = on;
        f
    }

    /// Features shaped like the generic pack's.
    fn registry() -> FeatureRegistry {
        FeatureRegistry::new(vec![
            d("resolve-conflict", &["git", "claudeCli", "claudeAuth"], true, None),
            d("create-jira-subtasks", &[], false, Some(false)),
            d("review-in-editor", &["git"], true, None),
            d("analyze-issue", &["claudeCli", "claudeAuth"], true, Some(true)),
            d("address-review-comments", &["git", "claudeCli", "claudeAuth"], true, None),
            d("ticket-to-pr", &["git"], true, None),
            { let mut x = d("hidden", &["git"], true, None); x.disabled = true; x },
        ])
    }

    fn ids(v: Vec<String>) -> Vec<String> {
        v
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn ids_are_unique_in_declared_order_and_a_disabled_descriptor_vanishes() {
        let r = registry();
        let all = r.all_feature_ids();
        assert_eq!(all, ["resolve-conflict", "create-jira-subtasks", "review-in-editor", "analyze-issue", "address-review-comments", "ticket-to-pr"]);
        assert!(!all.contains(&"hidden".to_string()));
        assert_eq!(r.default_enabled_feature_ids(), s(&["resolve-conflict", "review-in-editor", "analyze-issue", "address-review-comments", "ticket-to-pr"]));
    }

    #[test]
    fn enabled_feature_ids_defaults_to_every_known_feature_when_the_field_is_absent() {
        let r = registry();
        assert_eq!(r.enabled_feature_ids(&json!({})), r.all_feature_ids());
        assert_eq!(r.enabled_feature_ids(&json!({ "port": 8787 })), r.all_feature_ids());
        assert_eq!(r.enabled_feature_ids(&json!({ "enabledFeatures": "x" })), r.all_feature_ids());
    }

    #[test]
    fn enabled_feature_ids_soft_migrates_a_legacy_list_missing_known_features() {
        let r = registry();
        assert_eq!(r.enabled_feature_ids(&json!({ "enabledFeatures": ["resolve-conflict"] })), r.default_enabled_feature_ids());
        assert_eq!(r.enabled_feature_ids(&json!({ "enabledFeatures": [] })), r.default_enabled_feature_ids());
    }

    #[test]
    fn enabled_feature_ids_respects_an_intentional_disable_once_known_features_is_recorded() {
        let r = registry();
        let known = json!(r.all_feature_ids());
        assert_eq!(r.enabled_feature_ids(&json!({ "enabledFeatures": ["resolve-conflict"], "knownFeatures": known })), ["resolve-conflict"]);
        assert_eq!(r.enabled_feature_ids(&json!({ "enabledFeatures": [], "knownFeatures": known })), ids(vec![]));
        assert_eq!(r.enabled_feature_ids(&json!({ "enabledFeatures": ["resolve-conflict", "some-removed-feature", 5], "knownFeatures": known })), ["resolve-conflict"]);
    }

    #[test]
    fn migrate_returns_none_when_already_up_to_date() {
        let r = registry();
        assert_eq!(r.migrate_enabled_features(&json!({ "enabledFeatures": ["resolve-conflict"], "knownFeatures": r.all_feature_ids() })), None);
        assert_eq!(r.migrate_enabled_features(&json!({})), None);
    }

    #[test]
    fn migrate_enables_default_on_ids_absent_from_a_legacy_list_and_records_known_features() {
        let r = registry();
        let m = r.migrate_enabled_features(&json!({ "enabledFeatures": ["resolve-conflict"] })).unwrap();
        assert_eq!(m.enabled_features, r.default_enabled_feature_ids());
        assert_eq!(m.known_features, r.all_feature_ids());
        assert!(m.enabled_features.contains(&"analyze-issue".to_string()));
    }

    #[test]
    fn migrate_auto_enables_newly_shipped_default_on_features_but_not_default_off_ones() {
        let r = registry();
        let prior: Vec<String> = r.all_feature_ids().into_iter().filter(|id| id != "analyze-issue" && id != "address-review-comments" && id != "ticket-to-pr").collect();
        let m = r.migrate_enabled_features(&json!({ "enabledFeatures": ["resolve-conflict"], "knownFeatures": prior })).unwrap();
        for id in ["resolve-conflict", "analyze-issue", "address-review-comments", "ticket-to-pr"] {
            assert!(m.enabled_features.contains(&id.to_string()), "{id}");
        }
        assert!(!m.enabled_features.contains(&"create-jira-subtasks".to_string()));
        assert_eq!(m.known_features, r.all_feature_ids());
    }

    #[test]
    fn required_checks_for_unions_checks_across_exactly_the_given_ids_deduplicated() {
        let r = registry();
        let mut got = r.required_checks_for(&["resolve-conflict"]);
        got.sort();
        assert_eq!(got, ["claudeAuth", "claudeCli", "git"]);
        assert!(r.required_checks_for(&["create-jira-subtasks"]).is_empty());
        assert_eq!(r.required_checks_for(&["review-in-editor"]), ["git"]);
        assert!(r.required_checks_for(&[]).is_empty());
        assert!(r.required_checks_for(&["not-a-real-feature", "hidden"]).is_empty());
        assert_eq!(r.required_checks_for(&["resolve-conflict", "review-in-editor"]).iter().filter(|c| *c == "git").count(), 1);
    }

    #[test]
    fn needs_repos_and_features_requiring() {
        let r = registry();
        assert!(r.needs_repos(&["resolve-conflict"]));
        assert!(r.needs_repos(&["create-jira-subtasks", "review-in-editor"]));
        assert!(!r.needs_repos(&["create-jira-subtasks"]));
        assert!(!r.needs_repos(&[]));
        let ids = ["resolve-conflict", "review-in-editor", "create-jira-subtasks"];
        assert_eq!(r.features_requiring("git", &ids).iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["resolve-conflict", "review-in-editor"]);
        assert_eq!(r.features_requiring("claudeAuth", &ids).iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["resolve-conflict"]);
        assert!(r.features_requiring("git", &["create-jira-subtasks"]).is_empty());
    }

    #[test]
    fn the_review_editor_choices_are_the_two_the_panel_offers() {
        assert_eq!(REVIEW_EDITORS.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ["cursor", "claude-code"]);
    }
}
