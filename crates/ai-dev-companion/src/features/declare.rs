//! What the `feature.js` declarations share: the context their setup prompts need, and a few helpers
//! that keep each declaration as short as the Node one.

use crate::site_urls::Prompter;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// A pack's borrowed-token detector (site id and base URL to the file the token would come from).
pub type TokenDetector = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// What setup's prompts need beyond the existing config: where borrowed tokens come from and what
/// the tool that leaves them is called (Node's `helpers.externalToken` / `helpers.log`).
#[derive(Clone, Default)]
pub struct SetupContext {
    pub token_detector: Option<TokenDetector>,
    /// The pack's external-token label (e.g. "some-tool"); "" when no pack has one.
    pub token_tool: String,
}

impl SetupContext {
    pub fn source(&self, site: &str, base_url: &str) -> String {
        crate::setup_helpers::platform_token_source(self.token_detector.as_deref().map(|f| f as &dyn Fn(&str, &str) -> Option<String>), site, base_url)
    }

    /// Says how `label` is authenticated (Node's `logAuthNote`).
    pub fn note(&self, p: &mut dyn Prompter, label: &str, source: &str) {
        crate::setup_helpers::log_auth_note(&mut |l| p.log(l), label, source, &self.token_tool);
    }
}

/// `helpers.ask(rl, question, default)`: the typed answer, or the default on Enter.
pub fn ask_or(p: &mut dyn Prompter, question: &str, default: &str) -> String {
    let answer = p.ask(question, default);
    let answer = crate::js_text::js_trim(&answer);
    if answer.is_empty() { default.to_string() } else { answer.to_string() }
}

/// `existing[key]` as an object (what `{ ...existing[key] }` spreads), empty when it is not one.
pub fn spread(existing: &Value, key: &str) -> Map<String, Value> {
    existing.get(key).and_then(Value::as_object).cloned().unwrap_or_default()
}

/// `existing[a][b] || ""`, the text.
pub fn saved_text(existing: &Value, a: &str, b: &str) -> String {
    existing.get(a).and_then(|v| v.get(b)).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A catalog entry for a feature-owned MCP tool.
pub fn tool(name: &str, feature_id: &str, kind: &str, description: &str, params: Value) -> crate::packs::McpToolDef {
    crate::packs::McpToolDef { name: name.into(), feature_id: feature_id.into(), def: json!({ "name": name, "featureId": feature_id, "kind": kind, "generic": false, "description": description, "params": params }) }
}

pub fn extension(script: &str, order: f64) -> crate::packs::ExtensionSpec {
    crate::packs::ExtensionSpec { script: script.into(), order: Some(order) }
}
