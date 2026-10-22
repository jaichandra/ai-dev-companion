//! The `--permission-mode` Claude Code is started with (port of
//! `core/permission-mode.js`). The framework default is "auto"; a distribution
//! can change it for everything (`claude.permissionMode`) or for one feature
//! (`claude.permissionModes[featureId]`) in its environment profile.

use crate::environment_base::Environment;
use serde_json::Value;

pub const DEFAULT_PERMISSION_MODE: &str = "auto";

fn non_empty(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The mode for the Claude Code session a feature starts: its override, else the profile's, else "auto".
pub fn permission_mode_for(env: &Environment, feature_id: &str) -> String {
    let claude = env.profile.get("claude");
    non_empty(claude.and_then(|c| c.get("permissionModes")).and_then(|m| m.get(feature_id)))
        .or_else(|| non_empty(claude.and_then(|c| c.get("permissionMode"))))
        .unwrap_or(DEFAULT_PERMISSION_MODE)
        .to_string()
}

/// [`permission_mode_for`] against the loaded environment.
pub fn permission_mode_for_current(feature_id: &str) -> String {
    permission_mode_for(crate::environment::current(), feature_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment_base::define_environment;
    use serde_json::json;

    fn env(claude: Value) -> Environment {
        let mut profile = crate::environment::placeholder_profile();
        if !claude.is_null() {
            profile["claude"] = claude;
        }
        define_environment(&profile).unwrap()
    }

    #[test]
    fn defaults_to_auto_with_the_placeholder_profile() {
        assert_eq!(DEFAULT_PERMISSION_MODE, "auto");
        assert_eq!(permission_mode_for(&env(Value::Null), "review-in-editor"), "auto");
        assert_eq!(permission_mode_for_current("review-in-editor"), "auto");
    }

    #[test]
    fn a_feature_override_wins_over_the_profile_mode_which_wins_over_the_default() {
        let e = env(json!({ "permissionMode": "plan", "permissionModes": { "ticket-to-pr": "default" } }));
        assert_eq!(permission_mode_for(&e, "ticket-to-pr"), "default");
        assert_eq!(permission_mode_for(&e, "digest"), "plan");
        assert_eq!(permission_mode_for(&env(json!({ "permissionModes": { "digest": "plan" } })), "other"), "auto");
    }
}
