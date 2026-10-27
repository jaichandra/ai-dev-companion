//! Everything behind the extension's Settings panel (`GET/PUT /settings`) that
//! doesn't need a running server (port of `core/settings.js`): what the panel
//! may see, what it may change, and how a change is merged into `config.json`.
//!
//! The panel is the in-browser counterpart of the setup wizard, minus the two
//! settings that connect the extension to this service at all: `port` and
//! `sharedSecret` are never accepted from it (changing either from the browser
//! would cut off the very extension that asked), and the secret, like every
//! API token, is never sent back out.

use crate::environment;
use crate::feature_registry::{FeatureRegistry, REVIEW_EDITORS};
use crate::history_schema::DEFAULT_RETENTION_DAYS;
use crate::js_text::{is_js_space, js_to_string, js_trim, utf16_len};
use crate::node_path;
use crate::packs::PackRegistry;
use crate::prereqs::{self, Check};
use crate::settings_background::{self as background, FieldError, TokenOp, BACKGROUND_KEYS};
use crate::targets;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::LazyLock;
use url::Url;

pub const LOCKED_KEYS: [&str; 2] = ["port", "sharedSecret"];
const MAX_REPOS: usize = 500;
const MAX_TOKEN_LENGTH: usize = 1000;
// The in-memory cache of relayed browser sessions. ttlMinutes 0 turns it off
// entirely (a meaningful, persisted value, not "unset"); the cap keeps a
// mistyped setting from caching a session for weeks.
const SESSION_CACHE_TTL_MINUTES_DEFAULT: i64 = 30;
const SESSION_CACHE_HEARTBEAT_DEFAULT: bool = false;
const MAX_SESSION_CACHE_TTL_MINUTES: i64 = 1440;
/// The `/mcp` route's token. `rotateToken` is a one-shot command, not a
/// setting: it becomes an `mcp.token` token op and is never written to config.
pub const MCP_TOKEN_NAME: &str = "mcp.token";
// Passed to `claude --model` or `cursor-agent --model` as a single argument
// (never a shell), but still limited to what model ids and aliases look like,
// e.g. "claude-sonnet-5", "opus", "claude-opus-4-1[1m]".
static MODEL_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:\[\]=,-]{0,99}$").unwrap());
static REPO_KEY_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([^/]+)/([^/]+)$").unwrap());

fn has_control_chars(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'))
}

fn is_http_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()))
}

fn err(errors: &mut Vec<FieldError>, field: impl Into<String>, message: impl Into<String>) {
    errors.push(FieldError { field: field.into(), message: message.into() });
}

fn obj(v: &Value) -> Option<&Map<String, Value>> {
    v.as_object()
}

fn is_whole(v: &Value, min: i64, max: i64) -> bool {
    v.as_f64().is_some_and(|n| n.fract() == 0.0 && n >= min as f64 && n <= max as f64)
}

/// What the panel shows for each feature: the descriptors minus `promptSetup`,
/// which only makes sense in a terminal.
pub fn feature_summaries(features: &FeatureRegistry) -> Vec<Value> {
    features
        .descriptors()
        .iter()
        .map(|d| json!({ "id": d.id, "label": d.label, "description": d.description, "requiredChecks": d.required_checks, "needsRepos": d.needs_repos, "enabledByDefault": d.enabled_by_default != Some(false) }))
        .collect()
}

/// What `public_settings` knows about credentials: which names are saved (never the values)
/// and, per site, which other tool's file a fallback token would come from.
#[derive(Debug, Default, Clone)]
pub struct CredentialView {
    pub saved: Vec<String>,
    pub external: Map<String, Value>,
}

/// The Settings panel's view of the features and packs.
pub struct Settings<'a, F> {
    pub features: &'a FeatureRegistry,
    pub packs: &'a PackRegistry<F>,
}

impl<F> Settings<'_, F> {
    /// The config as the panel may see it: tokens reduced to whether one is
    /// saved, `sharedSecret` left out entirely, `port` only as a read-only value.
    pub fn public_settings(&self, config: &Value, credentials: &CredentialView) -> Map<String, Value> {
        let env = environment::current();
        let mut out = Map::new();
        if let Some(port) = config.get("port") {
            out.insert("port".into(), port.clone());
        }
        out.insert("enabledFeatures".into(), json!(self.features.enabled_feature_ids(config)));
        out.insert("repos".into(), config["repos"].as_object().map_or_else(|| json!({}), |r| Value::Object(r.clone())));
        // A saved "vscode" (hidden for now) shows as automatic.
        let editor = config["reviewEditor"].as_str().filter(|e| REVIEW_EDITORS.iter().any(|(id, _)| id == e));
        out.insert("reviewEditor".into(), editor.map_or(Value::Null, |e| json!(e)));
        for site in &env.sites {
            let token_name = format!("{}.apiToken", site.id);
            let saved = credentials.saved.contains(&token_name);
            let external = if saved { "" } else { credentials.external.get(&site.id).and_then(Value::as_str).unwrap_or("") };
            let mut s = Map::new();
            s.insert("baseUrl".into(), json!(config[&site.id]["baseUrl"].as_str().unwrap_or("")));
            if site.raw.get("withUsername") == Some(&json!(true)) {
                s.insert("username".into(), json!(config[&site.id]["username"].as_str().unwrap_or("")));
            }
            s.insert("apiTokenSet".into(), json!(saved));
            s.insert("apiTokenExternal".into(), json!(external));
            for extra in self.packs.site_setting_extras(&site.id) {
                s.insert(extra.key.clone(), (extra.from)(config));
            }
            out.insert(site.id.clone(), Value::Object(s));
        }
        let t = targets::analyze_targets_from(config);
        out.insert(
            "analyzeIssue".into(),
            json!({
                "model": config["analyzeIssue"]["model"].as_str().unwrap_or(""),
                "projects": t.projects,
                "issueTypes": t.issue_types,
                "componentRepoMap": config["analyzeIssue"]["componentRepoMap"].as_object().map_or_else(|| json!({}), |m| Value::Object(m.clone())),
            }),
        );
        out.insert("history".into(), json!({ "retentionDays": config["history"].get("retentionDays").filter(|v| !v.is_null()).cloned().unwrap_or(json!(DEFAULT_RETENTION_DAYS)) }));
        out.insert("riskFacts".into(), json!({ "url": config["riskFacts"]["url"].as_str().unwrap_or("") }));
        out.insert("summarizeComments".into(), json!({ "model": config["summarizeComments"]["model"].as_str().unwrap_or("") }));
        out.insert("addressReviewComments".into(), json!({ "model": config["addressReviewComments"]["model"].as_str().unwrap_or("") }));
        let default_transition = env.section("issues").and_then(|i| i["reviewTransitionName"].as_str()).unwrap_or("In Review");
        out.insert(
            "ticketToPr".into(),
            json!({
                "reviewTransitionName": config["ticketToPr"]["reviewTransitionName"].as_str().filter(|s| !s.is_empty()).unwrap_or(default_transition),
                "autoMoveToReview": config["ticketToPr"]["autoMoveToReview"] != json!(false),
                "createWorktreeWithoutAsking": config["ticketToPr"]["createWorktreeWithoutAsking"] == json!(true),
            }),
        );
        let pick = |k: &str, default: Value| config["sessionCache"].get(k).filter(|v| !v.is_null()).cloned().unwrap_or(default);
        out.insert("sessionCache".into(), json!({ "ttlMinutes": pick("ttlMinutes", json!(SESSION_CACHE_TTL_MINUTES_DEFAULT)), "heartbeat": pick("heartbeat", json!(SESSION_CACHE_HEARTBEAT_DEFAULT)) }));
        out.insert("mcp".into(), json!({ "tokenSet": credentials.saved.iter().any(|n| n == MCP_TOKEN_NAME) }));
        out.extend(background::public_background_settings(config, &credentials.saved, &self.packs.settings_sections()));
        out
    }

    fn editable_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = ["enabledFeatures", "repos", "reviewEditor"].iter().map(|k| k.to_string()).collect();
        keys.extend(environment::current().site_ids().into_iter().map(String::from));
        keys.extend(["analyzeIssue", "acknowledgeWarnings", "sessionCache", "history", "riskFacts", "ticketToPr", "summarizeComments", "addressReviewComments", "mcp"].iter().map(|k| k.to_string()));
        keys.extend(BACKGROUND_KEYS.iter().map(|k| k.to_string()));
        keys.extend(self.packs.settings_sections().iter().map(|s| s.key().to_string()));
        keys
    }

    /// Checks a `PUT /settings` body against the config currently saved.
    /// Returns the rejected fields, empty when the update may be saved. Every key
    /// is optional (only the ones present change anything); `repos`, when
    /// present, is the complete new map. A repo mapping that's new or points
    /// somewhere new must be a real clone whose origin is that same
    /// PROJECT/repo: the service runs git in it.
    pub fn validate_settings_update(&self, update: &Value, current: &Value, deps: &RepoDeps<'_>) -> Vec<FieldError> {
        let mut errors = vec![];
        let Some(upd) = obj(update) else {
            return vec![FieldError { field: String::new(), message: "Settings must be a JSON object.".into() }];
        };
        let editable = self.editable_keys();
        for key in upd.keys() {
            if LOCKED_KEYS.contains(&key.as_str()) {
                err(&mut errors, key, "can't be changed from the browser \u{2014} the extension would lose its connection to the service. Re-run `npm run setup` in the companion-service folder instead.");
            } else if !editable.contains(key) {
                err(&mut errors, key, "isn't a known setting.");
            }
        }
        if let Some(v) = upd.get("enabledFeatures") {
            self.validate_enabled_features(&mut errors, v);
        }
        if let Some(v) = upd.get("repos") {
            validate_repos(&mut errors, v, current, deps);
        }
        if let Some(v) = upd.get("reviewEditor").filter(|v| !v.is_null()) {
            if !v.as_str().is_some_and(|e| REVIEW_EDITORS.iter().any(|(id, _)| *id == e)) {
                err(&mut errors, "reviewEditor", format!("must be one of: {}.", REVIEW_EDITORS.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(", ")));
            }
        }
        for site in &environment::current().sites {
            let Some(v) = upd.get(&site.id) else { continue };
            let with_username = site.raw.get("withUsername") == Some(&json!(true));
            let extras = self.packs.site_setting_extras(&site.id);
            if v.is_object() && extras.iter().any(|e| v.get(&e.key).is_some()) {
                // The pack's own keys are checked by the pack; the rest is the site's.
                let mut rest = v.as_object().unwrap().clone();
                for e in &extras {
                    rest.shift_remove(&e.key);
                }
                validate_site(&mut errors, &site.id, &Value::Object(rest), with_username);
                for e in &extras {
                    let Some(value) = v.get(&e.key).filter(|x| !x.is_null()) else { continue };
                    if let Err(reason) = (e.validate)(value) {
                        err(&mut errors, format!("{}.{}", site.id, e.key), format!("{reason}."));
                    }
                }
            } else {
                validate_site(&mut errors, &site.id, v, with_username);
            }
        }
        if let Some(v) = upd.get("analyzeIssue") {
            validate_analyze_issue(&mut errors, v);
        }
        if upd.get("acknowledgeWarnings").is_some_and(|v| !v.is_boolean()) {
            err(&mut errors, "acknowledgeWarnings", "must be true or false.");
        }
        if let Some(v) = upd.get("history") {
            if let Some(h) = obj_or_error(&mut errors, "history", v) {
                for key in h.keys().filter(|k| *k != "retentionDays") {
                    err(&mut errors, format!("history.{key}"), "isn't a known setting.");
                }
                if h.get("retentionDays").is_some_and(|d| !is_whole(d, 7, 3650)) {
                    err(&mut errors, "history.retentionDays", "must be a whole number of days from 7 to 3650.");
                }
            }
        }
        if let Some(v) = upd.get("riskFacts") {
            if let Some(r) = obj_or_error(&mut errors, "riskFacts", v) {
                for key in r.keys().filter(|k| *k != "url") {
                    err(&mut errors, format!("riskFacts.{key}"), "isn't a known setting.");
                }
                if let Some(url) = r.get("url") {
                    let ok = url.as_str().map(js_trim).is_some_and(|t| !has_control_chars(t) && utf16_len(t) <= 500 && (t.is_empty() || is_http_url(t)));
                    if !ok {
                        err(&mut errors, "riskFacts.url", "must be an http:// or https:// URL (or blank to turn it off).");
                    }
                }
            }
        }
        for key in ["summarizeComments", "addressReviewComments"] {
            if let Some(v) = upd.get(key) {
                if let Some(o) = obj_or_error(&mut errors, key, v) {
                    for k in o.keys().filter(|k| *k != "model") {
                        err(&mut errors, format!("{key}.{k}"), "isn't a known setting.");
                    }
                    if let Some(m) = o.get("model") {
                        validate_model(&mut errors, &format!("{key}.model"), m, true);
                    }
                }
            }
        }
        if let Some(v) = upd.get("ticketToPr") {
            validate_ticket_to_pr(&mut errors, v);
        }
        if let Some(v) = upd.get("sessionCache") {
            if let Some(s) = obj_or_error(&mut errors, "sessionCache", v) {
                for key in s.keys().filter(|k| *k != "ttlMinutes" && *k != "heartbeat") {
                    err(&mut errors, format!("sessionCache.{key}"), "isn't a known setting.");
                }
                if s.get("ttlMinutes").is_some_and(|t| !is_whole(t, 0, MAX_SESSION_CACHE_TTL_MINUTES)) {
                    err(&mut errors, "sessionCache.ttlMinutes", format!("must be a whole number of minutes from 0 to {MAX_SESSION_CACHE_TTL_MINUTES} (0 turns caching off)."));
                }
                if s.get("heartbeat").is_some_and(|h| !h.is_boolean()) {
                    err(&mut errors, "sessionCache.heartbeat", "must be true or false.");
                }
            }
        }
        if let Some(v) = upd.get("mcp") {
            if let Some(m) = obj_or_error(&mut errors, "mcp", v) {
                for key in m.keys().filter(|k| *k != "rotateToken") {
                    err(&mut errors, format!("mcp.{key}"), "isn't a known setting.");
                }
                if m.get("rotateToken").is_some_and(|t| *t != json!(true)) {
                    err(&mut errors, "mcp.rotateToken", "can only be true (leave it out to keep the token).");
                }
            }
        }
        background::validate_background_settings(update, current, &mut errors, &self.packs.settings_sections());
        errors
    }

    fn validate_enabled_features(&self, errors: &mut Vec<FieldError>, v: &Value) {
        let known = self.features.all_feature_ids();
        let Some(list) = v.as_array() else {
            err(errors, "enabledFeatures", "must be a list of feature ids.");
            return;
        };
        for id in list {
            if !id.as_str().is_some_and(|s| known.iter().any(|k| k == s)) {
                err(errors, "enabledFeatures", format!("\"{}\" isn't a known feature.", js_to_string(id)));
            }
        }
        let unique: HashSet<String> = list.iter().map(Value::to_string).collect();
        if unique.len() != list.len() {
            err(errors, "enabledFeatures", "lists a feature more than once.");
        }
    }

    /// The config to write for an already-validated update: a copy of `current`
    /// with only the settings the panel owns replaced, plus the token writes and
    /// clears that update implies, NOT yet applied to any store. Returning them
    /// lets the caller apply them only after the prerequisite warnings were
    /// acknowledged and the config write succeeded, so a rejected or failed save
    /// changes nothing at all, token included. Unknown fields, `port` and
    /// `sharedSecret` pass through untouched.
    ///
    /// `saved_token_names` only decides what happens to a legacy token still in
    /// config.json (see `merge_site`). `new_mcp_token` is called only for
    /// `mcp.rotateToken: true`.
    pub fn merge_settings(&self, current: &Value, update: &Value, saved_token_names: Option<&[String]>, new_mcp_token: Option<&dyn Fn() -> String>) -> Result<(Value, Vec<TokenOp>), String> {
        let mut next = current.as_object().cloned().unwrap_or_default();
        let mut token_ops: Vec<TokenOp> = vec![];
        let upd = update.as_object().cloned().unwrap_or_default();
        let env = environment::current();
        if let Some(chosen) = upd.get("enabledFeatures").and_then(Value::as_array) {
            let all = self.features.all_feature_ids();
            next.insert("enabledFeatures".into(), json!(all.iter().filter(|id| chosen.iter().any(|c| c.as_str() == Some(id.as_str()))).collect::<Vec<_>>()));
            // The same snapshot setup writes, so a later update only auto-enables
            // features shipped after this save, not ones just unchecked here.
            next.insert("knownFeatures".into(), json!(all));
        }
        if let Some(repos) = upd.get("repos").and_then(Value::as_object) {
            let resolved: Map<String, Value> = repos.iter().map(|(k, p)| (k.clone(), json!(node_path::resolve(js_trim(p.as_str().unwrap_or("")))))).collect();
            next.insert("repos".into(), Value::Object(resolved));
        }
        if let Some(editor) = upd.get("reviewEditor") {
            if js_truthy_str(editor) {
                next.insert("reviewEditor".into(), editor.clone());
            } else {
                next.shift_remove("reviewEditor");
            }
        }
        for site in &env.sites {
            let Some(u) = upd.get(&site.id) else { continue };
            let merged = merge_site(next.get(&site.id), u, &format!("{}.apiToken", site.id), &mut token_ops, saved_token_names);
            match merged {
                Some(s) => next.insert(site.id.clone(), s),
                None => next.shift_remove(&site.id),
            };
        }
        // The keys packs add to a site: saved only when they differ from the pack's default.
        for site in &env.sites {
            for extra in self.packs.site_setting_extras(&site.id) {
                let Some(raw) = upd.get(&site.id).and_then(|u| u.get(&extra.key)) else { continue };
                let mut s = next.get(&site.id).and_then(Value::as_object).cloned().unwrap_or_default();
                let checked = if raw.is_null() { None } else { (extra.validate)(raw).ok() };
                match checked.filter(|v| *v != extra.default_value) {
                    Some(v) => s.insert(extra.key.clone(), v),
                    None => s.shift_remove(&extra.key),
                };
                if s.is_empty() {
                    next.shift_remove(&site.id);
                } else {
                    next.insert(site.id.clone(), Value::Object(s));
                }
            }
        }
        if let Some(u) = upd.get("analyzeIssue") {
            merge_analyze_issue(&mut next, u);
        }
        if let Some(u) = upd.get("sessionCache") {
            // Merge onto whatever's already saved (an update may set only one of the two fields),
            // then drop the key entirely once both are back at their defaults. ttlMinutes: 0 is a
            // real, meaningful value ("caching off"), never treated as unset.
            let mut merged = next.get("sessionCache").and_then(Value::as_object).cloned().unwrap_or_default();
            for k in ["ttlMinutes", "heartbeat"] {
                if let Some(v) = u.get(k) {
                    merged.insert(k.into(), v.clone());
                }
            }
            let ttl = merged.get("ttlMinutes").and_then(Value::as_i64).unwrap_or(SESSION_CACHE_TTL_MINUTES_DEFAULT);
            let hb = merged.get("heartbeat").and_then(Value::as_bool).unwrap_or(SESSION_CACHE_HEARTBEAT_DEFAULT);
            if ttl == SESSION_CACHE_TTL_MINUTES_DEFAULT && hb == SESSION_CACHE_HEARTBEAT_DEFAULT {
                next.shift_remove("sessionCache");
            } else {
                next.insert("sessionCache".into(), Value::Object(merged));
            }
        }
        if let Some(days) = upd.get("history").and_then(|h| h.get("retentionDays")) {
            if days.as_i64() == Some(DEFAULT_RETENTION_DAYS) {
                next.shift_remove("history");
            } else {
                next.insert("history".into(), json!({ "retentionDays": days }));
            }
        }
        if let Some(url) = upd.get("riskFacts").and_then(|r| r.get("url")).and_then(Value::as_str) {
            let url = js_trim(url);
            if url.is_empty() {
                next.shift_remove("riskFacts");
            } else {
                next.insert("riskFacts".into(), json!({ "url": url }));
            }
        }
        if let Some(model) = upd.get("summarizeComments").and_then(|r| r.get("model")).and_then(Value::as_str) {
            let model = js_trim(model);
            if model.is_empty() {
                next.shift_remove("summarizeComments");
            } else {
                next.insert("summarizeComments".into(), json!({ "model": model }));
            }
        }
        if let Some(model) = upd.get("addressReviewComments").and_then(|r| r.get("model")).and_then(Value::as_str) {
            // checkCommands (config.json only) rides along untouched.
            let mut a = next.get("addressReviewComments").and_then(Value::as_object).cloned().unwrap_or_default();
            let model = js_trim(model);
            if model.is_empty() {
                a.shift_remove("model");
            } else {
                a.insert("model".into(), json!(model));
            }
            if a.is_empty() {
                next.shift_remove("addressReviewComments");
            } else {
                next.insert("addressReviewComments".into(), Value::Object(a));
            }
        }
        if let Some(u) = upd.get("ticketToPr") {
            if ["reviewTransitionName", "autoMoveToReview", "createWorktreeWithoutAsking"].iter().any(|k| u.get(*k).is_some()) {
                // The defaults (In Review, automatic) aren't persisted, like reviewEditor's "auto".
                let saved = next.get("ticketToPr").cloned().unwrap_or(Value::Null);
                let default_name = env.section("issues").and_then(|i| i["reviewTransitionName"].as_str()).unwrap_or("In Review");
                let name = match u.get("reviewTransitionName").and_then(Value::as_str) {
                    Some(n) => js_trim(n).to_string(),
                    None => saved["reviewTransitionName"].as_str().unwrap_or("").to_string(),
                };
                let auto = u.get("autoMoveToReview").and_then(Value::as_bool).unwrap_or(saved["autoMoveToReview"] != json!(false));
                let create = u.get("createWorktreeWithoutAsking").and_then(Value::as_bool).unwrap_or(saved["createWorktreeWithoutAsking"] == json!(true));
                let mut out = Map::new();
                if !name.is_empty() && name != default_name {
                    out.insert("reviewTransitionName".into(), json!(name));
                }
                if !auto {
                    out.insert("autoMoveToReview".into(), json!(false));
                }
                if create {
                    out.insert("createWorktreeWithoutAsking".into(), json!(true));
                }
                if out.is_empty() {
                    next.shift_remove("ticketToPr");
                } else {
                    next.insert("ticketToPr".into(), Value::Object(out));
                }
            }
        }
        if upd.get("mcp").and_then(|m| m.get("rotateToken")) == Some(&json!(true)) {
            let make = new_mcp_token.ok_or("mergeSettings needs newMcpToken to rotate the MCP token.")?;
            token_ops.push(TokenOp { name: MCP_TOKEN_NAME.into(), value: Some(make()) });
        }
        background::merge_background_settings(&mut next, update, &mut token_ops, &self.packs.settings_sections());
        Ok((Value::Object(next), token_ops))
    }

    /// Prerequisite checks failing for any of `ids`: what setup's "Needed by ..." step
    /// shows, for the panel to show before saving. `checks` runs one named check.
    pub fn prerequisite_warnings(&self, ids: &[&str], checks: &dyn Fn(&str) -> Check) -> Vec<Value> {
        let mut warnings = vec![];
        for name in self.features.required_checks_for(ids) {
            let result = checks(&name);
            if result.ok {
                continue;
            }
            let labels: Vec<&str> = self.features.features_requiring(&name, ids).iter().map(|d| d.label.as_str()).collect();
            warnings.push(json!({ "check": name, "message": result.message, "features": labels }));
        }
        warnings
    }

    /// Results of every check any feature can require, keyed by check name:
    /// lets the panel show whether a feature is ready before it's enabled.
    pub fn run_all_checks(&self, checks: &dyn Fn(&str) -> Check) -> Map<String, Value> {
        self.required_check_names().into_iter().map(|n| (n.clone(), { let c = checks(&n); json!({ "ok": c.ok, "message": c.message }) })).collect()
    }

    /// The names of every check some feature requires (what [`Self::run_all_checks`] runs), for a
    /// caller that wants to run them itself, e.g. concurrently.
    pub fn required_check_names(&self) -> Vec<String> {
        let all = self.features.all_feature_ids();
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        self.features.required_checks_for(&refs)
    }

    /// Whether the saved settings differ from what the running service loaded.
    /// Only the enabled feature set needs a restart: the server builds each
    /// feature and its routes once, at startup. Every other setting the panel
    /// edits is read from the shared config whenever a job runs.
    pub fn pending_restart(&self, running_enabled_ids: &[String], saved_config: &Value) -> Value {
        let saved = self.features.enabled_feature_ids(saved_config);
        let added: Vec<&String> = saved.iter().filter(|id| !running_enabled_ids.contains(id)).collect();
        let removed: Vec<&String> = running_enabled_ids.iter().filter(|id| !saved.contains(id)).collect();
        json!({ "required": !added.is_empty() || !removed.is_empty(), "added": added, "removed": removed })
    }
}

fn js_truthy_str(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.is_empty())
}

/// The git-backed checks of `repos`, injectable for tests.
pub struct RepoDeps<'a> {
    pub check_repo_path: &'a dyn Fn(&str) -> Check,
    pub get_origin_url: &'a dyn Fn(&str) -> Option<String>,
}

impl RepoDeps<'static> {
    pub fn real() -> Self {
        RepoDeps { check_repo_path: &prereqs::check_repo_path, get_origin_url: &prereqs::get_origin_url }
    }
}

fn obj_or_error<'a>(errors: &mut Vec<FieldError>, field: &str, v: &'a Value) -> Option<&'a Map<String, Value>> {
    let o = v.as_object();
    if o.is_none() {
        err(errors, field, "must be an object.");
    }
    o
}

fn validate_model(errors: &mut Vec<FieldError>, field: &str, value: &Value, allow_blank: bool) {
    let Some(s) = value.as_str() else {
        err(errors, field, "must be a string.");
        return;
    };
    let trimmed = js_trim(s);
    if trimmed.is_empty() {
        if !allow_blank {
            err(errors, field, "can't be blank.");
        }
        return;
    }
    if !MODEL_PATTERN.is_match(trimmed) {
        err(errors, field, format!("\"{trimmed}\" doesn't look like a model id (e.g. claude-sonnet-5)."));
    }
}

fn validate_site(errors: &mut Vec<FieldError>, prefix: &str, value: &Value, with_username: bool) {
    let Some(o) = value.as_object() else {
        err(errors, prefix, "must be an object.");
        return;
    };
    let mut allowed = vec!["baseUrl", "apiToken", "clearApiToken"];
    if with_username {
        allowed.push("username");
    }
    for key in o.keys().filter(|k| !allowed.contains(&k.as_str())) {
        err(errors, format!("{prefix}.{key}"), "isn't a known setting.");
    }
    if let Some(b) = o.get("baseUrl") {
        match b.as_str() {
            None => err(errors, format!("{prefix}.baseUrl"), "must be a string."),
            Some(s) if !js_trim(s).is_empty() && !is_http_url(js_trim(s)) => err(errors, format!("{prefix}.baseUrl"), "must be an http:// or https:// URL."),
            _ => {}
        }
    }
    if with_username {
        if let Some(u) = o.get("username") {
            if !u.as_str().is_some_and(|s| !has_control_chars(s) && utf16_len(s) <= 200) {
                err(errors, format!("{prefix}.username"), "must be a short single-line string.");
            }
        }
    }
    if let Some(t) = o.get("apiToken") {
        match t.as_str() {
            Some(s) if utf16_len(s) <= MAX_TOKEN_LENGTH => {
                if js_trim(s).chars().any(is_js_space) {
                    err(errors, format!("{prefix}.apiToken"), "can't contain spaces or line breaks.");
                }
            }
            _ => err(errors, format!("{prefix}.apiToken"), "must be a string."),
        }
    }
    if o.get("clearApiToken").is_some_and(|c| !c.is_boolean()) {
        err(errors, format!("{prefix}.clearApiToken"), "must be true or false.");
    }
}

fn validate_repos(errors: &mut Vec<FieldError>, v: &Value, current: &Value, deps: &RepoDeps<'_>) {
    let Some(repos) = v.as_object() else {
        err(errors, "repos", "must map \"PROJECT/repo\" to a folder.");
        return;
    };
    if repos.len() > MAX_REPOS {
        err(errors, "repos", format!("can't list more than {MAX_REPOS}."));
    }
    let mut seen: HashSet<String> = HashSet::new();
    for (key, clone_path) in repos.iter().take(MAX_REPOS) {
        let field = format!("repos.{key}");
        let Some(m) = REPO_KEY_PATTERN.captures(key).filter(|m| prereqs::is_safe_repo_segment(&m[1]) && prereqs::is_safe_repo_segment(&m[2])) else {
            err(errors, field, "must be \"PROJECT/repo\" (letters, digits, \".\", \"_\" or \"-\").");
            continue;
        };
        if !seen.insert(key.to_lowercase()) {
            err(errors, field, "is listed more than once.");
            continue;
        }
        let Some(p) = clone_path.as_str().filter(|p| !js_trim(p).is_empty() && !has_control_chars(p)) else {
            err(errors, field, "needs the folder of its local clone.");
            continue;
        };
        if !node_path::is_absolute(js_trim(p)) {
            err(errors, field, "must be an absolute folder path (starting with /).");
            continue;
        }
        let resolved = node_path::resolve(js_trim(p));
        if current["repos"][key].as_str().is_some_and(|c| !c.is_empty() && node_path::resolve(c) == resolved) {
            continue;
        }
        let check = (deps.check_repo_path)(&resolved);
        if !check.ok {
            err(errors, field, check.message);
            continue;
        }
        let origin = (deps.get_origin_url)(&resolved);
        if !prereqs::origin_matches_project_repo(origin.as_deref(), &m[1], &m[2]) {
            err(errors, field, format!("{resolved} is a clone of {}, not {key}.", origin.as_deref().filter(|o| !o.is_empty()).unwrap_or("an unknown remote")));
        }
    }
}

fn validate_analyze_issue(errors: &mut Vec<FieldError>, v: &Value) {
    let Some(a) = obj_or_error(errors, "analyzeIssue", v) else { return };
    for key in a.keys().filter(|k| !["model", "projects", "issueTypes", "componentRepoMap"].contains(&k.as_str())) {
        err(errors, format!("analyzeIssue.{key}"), "isn't a known setting.");
    }
    if let Some(m) = a.get("model") {
        validate_model(errors, "analyzeIssue.model", m, true);
    }
    type Check_ = fn(&Value) -> Result<(), String>;
    let checks: [(&str, Check_); 3] = [
        ("projects", |v| targets::validate_projects(v).map(|_| ())),
        ("issueTypes", |v| targets::validate_issue_types(v).map(|_| ())),
        ("componentRepoMap", |v| targets::validate_component_repo_map(v).map(|_| ())),
    ];
    for (key, check) in checks {
        let Some(value) = a.get(key).filter(|v| !v.is_null()) else { continue };
        if let Err(reason) = check(value) {
            err(errors, format!("analyzeIssue.{key}"), format!("{reason}."));
        }
    }
}

fn validate_ticket_to_pr(errors: &mut Vec<FieldError>, v: &Value) {
    let Some(t) = obj_or_error(errors, "ticketToPr", v) else { return };
    for key in t.keys().filter(|k| !["reviewTransitionName", "autoMoveToReview", "createWorktreeWithoutAsking"].contains(&k.as_str())) {
        err(errors, format!("ticketToPr.{key}"), "isn't a known setting.");
    }
    for key in ["autoMoveToReview", "createWorktreeWithoutAsking"] {
        if t.get(key).is_some_and(|b| !b.is_boolean()) {
            err(errors, format!("ticketToPr.{key}"), "must be true or false.");
        }
    }
    if let Some(name) = t.get("reviewTransitionName") {
        if !name.as_str().is_some_and(|n| !has_control_chars(n) && utf16_len(js_trim(n)) <= 100) {
            err(errors, "ticketToPr.reviewTransitionName", "must be a Jira transition or status name of at most 100 characters (blank for the default, In Review).");
        }
    }
}

/// Applies a site update (`{baseUrl, username?, apiToken?, clearApiToken?}`)
/// over `existing`: blank fields are removed (readers fall back to their
/// defaults / SSO), a blank or absent apiToken keeps the saved one.
///
/// The token itself is never part of the returned object and is not written to
/// the credential store from in here either: any token change is pushed onto
/// `token_ops` (`value: None` meaning "clear") for the caller to apply once it's
/// safe to.
///
/// A legacy `apiToken` still in config.json (the migration failed for it, say an
/// unwritable store) is the ONLY copy of that token, so it is not simply
/// dropped. With a new token or a clear in this update, that wins and the legacy
/// field goes. Otherwise, when `saved_token_names` says the store already has
/// this token, the store's copy is authoritative and the legacy field goes; when
/// the store lacks it, a token op moves the legacy value into the store and the
/// field goes with it; and when `saved_token_names` wasn't given (can't tell),
/// the field is kept as-is.
fn merge_site(existing: Option<&Value>, update: &Value, token_name: &str, token_ops: &mut Vec<TokenOp>, saved_token_names: Option<&[String]>) -> Option<Value> {
    let mut next = existing.and_then(Value::as_object).cloned().unwrap_or_default();
    let legacy = next.get("apiToken").and_then(Value::as_str).map(js_trim).unwrap_or("").to_string();
    let legacy_raw = next.shift_remove("apiToken");
    for key in ["baseUrl", "username"] {
        let Some(raw) = update.get(key).and_then(Value::as_str) else { continue };
        let mut value = js_trim(raw).to_string();
        if key == "baseUrl" {
            value = value.trim_end_matches('/').to_string();
        }
        if value.is_empty() {
            next.shift_remove(key);
        } else {
            next.insert(key.into(), json!(value));
        }
    }
    let truthy = |k: &str| update.get(k).is_some_and(|v| v.as_bool().unwrap_or_else(|| !v.is_null() && v != &json!(0) && v != &json!("")));
    if truthy("clearApiToken") {
        token_ops.push(TokenOp { name: token_name.into(), value: None });
    } else if let Some(t) = update.get("apiToken").and_then(Value::as_str).map(js_trim).filter(|t| !t.is_empty()) {
        token_ops.push(TokenOp { name: token_name.into(), value: Some(t.to_string()) });
    } else if !legacy.is_empty() {
        match saved_token_names {
            None => {
                next.insert("apiToken".into(), legacy_raw.unwrap_or(Value::Null));
            }
            Some(saved) if !saved.iter().any(|n| n == token_name) => token_ops.push(TokenOp { name: token_name.into(), value: Some(legacy) }),
            Some(_) => {}
        }
    }
    (!next.is_empty()).then(|| Value::Object(next))
}

fn merge_analyze_issue(next: &mut Map<String, Value>, u: &Value) {
    let mut a = next.get("analyzeIssue").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(m) = u.get("model").and_then(Value::as_str) {
        let m = js_trim(m);
        if m.is_empty() {
            a.shift_remove("model");
        } else {
            a.insert("model".into(), json!(m));
        }
    }
    type Validated = Result<Value, String>;
    type Rule = (&'static str, fn(&Value) -> Validated, Value);
    let rules: [Rule; 3] = [
        ("projects", |v| targets::validate_projects(v).map(|p| json!(p)), json!(targets::default_projects())),
        ("issueTypes", |v| targets::validate_issue_types(v).map(|p| json!(p)), json!(targets::default_issue_types())),
        ("componentRepoMap", |v| targets::validate_component_repo_map(v).map(Value::Object), json!({})),
    ];
    for (key, validate, default) in rules {
        let Some(value) = u.get(key) else { continue };
        let checked = if value.is_null() { None } else { validate(value).ok() };
        match checked.filter(|v| *v != default) {
            Some(v) => a.insert(key.into(), v),
            None => a.shift_remove(key),
        };
    }
    if a.is_empty() {
        next.shift_remove("analyzeIssue");
    } else {
        next.insert("analyzeIssue".into(), Value::Object(a));
    }
}

/// The outcome of `apply_token_ops`.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenOpsResult {
    Ok,
    /// The op that failed and why; the ops before it stay applied.
    Failed { failed_op: TokenOp, error: String },
}

/// Applies `token_ops` in order via `set_token(name, value)`, stopping at the
/// first failure rather than trying the rest. Each call is a complete write, so
/// there's no "half applied" for a single op to roll back: ops before the
/// failing one stay applied exactly as they were; this only reports where
/// things stopped, it never undoes anything.
pub fn apply_token_ops(token_ops: &[TokenOp], mut set_token: impl FnMut(&str, Option<&str>) -> Result<(), String>) -> TokenOpsResult {
    for op in token_ops {
        if let Err(error) = set_token(&op.name, op.value.as_deref()) {
            return TokenOpsResult::Failed { failed_op: op.clone(), error };
        }
    }
    TokenOpsResult::Ok
}

/// A local clone found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clone_ {
    pub key: String,
    pub path: String,
}

/// Local clones not already mapped by key or by folder: offered in the panel
/// as one-click additions.
pub fn suggested_repos(existing: &Map<String, Value>, clones: &[Clone_]) -> Vec<Clone_> {
    let keys: HashSet<String> = existing.keys().map(|k| k.to_lowercase()).collect();
    let paths: HashSet<String> = existing.values().filter_map(Value::as_str).map(node_path::resolve).collect();
    clones.iter().filter(|c| !keys.contains(&c.key.to_lowercase()) && !paths.contains(&node_path::resolve(&c.path))).cloned().collect()
}

/// Writes config.json via a temp file + rename in the same folder, so a crash
/// or a concurrent reader never sees half a file. Keeps the existing file's
/// permissions (it holds the shared secret); a new file is created owner-only.
pub fn write_config_file(file: &Path, config: &Value) -> io::Result<()> {
    let mode = fs::metadata(file).map_or(0o600, |m| m.permissions().mode() & 0o777);
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let tmp = file.with_file_name(format!(".{name}.{}.{stamp}.tmp", std::process::id()));
    let result = (|| {
        let mut text = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
        text.push('\n');
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        drop(f);
        // `mode` is subject to the umask; make it exact, as the old file's was.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// One-time move of any `apiToken` still sitting in config.json into the
/// encrypted credential store. Runs at service startup (after loading the
/// config) and in setup, both of which build `config` from scratch each time,
/// so idempotent is what makes repeat runs harmless: nothing left to migrate
/// means nothing is written to the store and `save_config` is never called.
///
/// Order matters for safety: every non-empty token is set in the store FIRST;
/// only once every one of those writes has succeeded are the config fields
/// deleted and `save_config` called. A mid-way store failure therefore leaves
/// config.json exactly as it was (the tokens stay right where they are, still
/// readable by the SSO-first fallback chain, not silently dropped) and is
/// reported with a single warning rather than one per token.
pub fn migrate_tokens_to_store(config: &mut Value, mut store_set: impl FnMut(&str, &str) -> Result<(), String>, save_config: impl FnOnce(&Value) -> io::Result<()>, warn: impl FnOnce(&str)) -> Vec<String> {
    let to_migrate: Vec<(String, String)> = environment::current()
        .site_ids()
        .into_iter()
        .filter(|site| config[*site]["apiToken"].as_str().is_some_and(|t| !js_trim(t).is_empty()))
        .map(|site| (site.to_string(), format!("{site}.apiToken")))
        .collect();
    if to_migrate.is_empty() {
        return vec![];
    }
    for (site, name) in &to_migrate {
        let token = config[site]["apiToken"].as_str().unwrap_or("").to_string();
        if let Err(e) = store_set(name, &token) {
            warn(&format!(
                "[credentials] Could not move {} into the encrypted credential store ({e}); left as-is in config.json for now \u{2014} this will be retried on the next startup.",
                to_migrate.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")
            ));
            return vec![];
        }
    }
    for (site, _) in &to_migrate {
        if let Some(s) = config.get_mut(site).and_then(Value::as_object_mut) {
            s.shift_remove("apiToken");
        }
    }
    // A failed save is the caller's to report; the tokens are in the store either way.
    let _ = save_config(config);
    to_migrate.into_iter().map(|(_, n)| n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::{FeatureDescriptor, Pack};
    use std::cell::RefCell;

    fn descriptor(id: &str, label: &str, checks: &[&str], repos: bool, on: Option<bool>) -> FeatureDescriptor {
        let mut f = FeatureDescriptor::new(id, label);
        f.required_checks = checks.iter().map(|c| c.to_string()).collect();
        f.needs_repos = repos;
        f.enabled_by_default = on;
        f
    }

    fn features() -> FeatureRegistry {
        FeatureRegistry::new(vec![
            descriptor("resolve-conflict", "Resolve Merge Conflicts", &["git", "claudeCli", "claudeAuth"], true, None),
            descriptor("create-jira-subtasks", "Create subtasks", &[], false, Some(false)),
            descriptor("review-in-editor", "Review in editor", &["git"], true, None),
            descriptor("analyze-issue", "Analyze ticket", &["claudeCli", "claudeAuth"], true, None),
        ])
    }

    fn packs() -> PackRegistry<()> {
        PackRegistry::new(vec![Pack::new("t", vec![])]).unwrap()
    }

    fn current() -> Value {
        json!({
            "port": 8787,
            "sharedSecret": "s3cret",
            "repos": { "ACME/sample-app": "/Users/me/gitviews/sample-app" },
            "enabledFeatures": ["resolve-conflict", "review-in-editor"],
            "knownFeatures": features().all_feature_ids(),
            "reviewEditor": "cursor",
            "jira": { "baseUrl": "https://jira.example.com" },
            "jenkins": { "baseUrl": "https://jenkins.example.com", "username": "me", "maxBuildsToScan": 9 },
            "analyzeIssue": { "model": "claude-opus-5", "componentRepoMap": { "UI": "ACME/sample-app" } },
            "someFutureField": { "keep": true },
        })
    }

    fn with_settings<R>(f: impl FnOnce(&Settings<'_, ()>) -> R) -> R {
        let (features, packs) = (features(), packs());
        f(&Settings { features: &features, packs: &packs })
    }

    fn ok_check(_: &str) -> Check {
        Check { ok: true, message: "OK.".into() }
    }

    /// Every repo path "exists" and has the origin its folder name implies, unless overridden.
    fn validate_with(update: Value, cur: &Value, check: &dyn Fn(&str) -> Check, origin: &dyn Fn(&str) -> Option<String>) -> Vec<FieldError> {
        with_settings(|s| s.validate_settings_update(&update, cur, &RepoDeps { check_repo_path: check, get_origin_url: origin }))
    }

    fn default_origin(p: &str) -> Option<String> {
        Some(format!("https://github.com/acme/{}.git", Path::new(p).file_name().unwrap().to_string_lossy()))
    }

    fn validate(update: Value) -> Vec<FieldError> {
        validate_with(update, &current(), &ok_check, &default_origin)
    }

    fn fields(update: Value) -> Vec<String> {
        let mut f: Vec<String> = validate(update).into_iter().map(|e| e.field).collect();
        f.sort();
        f
    }

    fn public(config: &Value, saved: &[&str]) -> Map<String, Value> {
        with_settings(|s| s.public_settings(config, &CredentialView { saved: saved.iter().map(|x| x.to_string()).collect(), external: Map::new() }))
    }

    fn merge(cur: &Value, update: Value) -> (Value, Vec<TokenOp>) {
        with_settings(|s| s.merge_settings(cur, &update, None, None)).unwrap()
    }

    fn merge_with_saved(cur: &Value, update: Value, saved: &[&str]) -> (Value, Vec<TokenOp>) {
        let saved: Vec<String> = saved.iter().map(|x| x.to_string()).collect();
        with_settings(|s| s.merge_settings(cur, &update, Some(&saved), None)).unwrap()
    }

    fn op(name: &str, value: Option<&str>) -> TokenOp {
        TokenOp { name: name.into(), value: value.map(String::from) }
    }

    // ---- publicSettings ----

    #[test]
    fn public_settings_never_exposes_the_shared_secret_or_any_token() {
        let view = public(&current(), &["jira.apiToken", "jenkins.apiToken"]);
        let text = serde_json::to_string(&view).unwrap();
        assert!(!text.contains("s3cret"));
        assert!(!view.contains_key("sharedSecret"));
        assert_eq!(view["jira"]["apiTokenSet"], true);
        assert_eq!(view["jenkins"]["apiTokenSet"], true);
        assert_eq!(view["port"], 8787);
    }

    #[test]
    fn public_settings_reports_no_token_and_blank_fields_for_a_minimal_config() {
        let view = public(&json!({ "port": 1, "sharedSecret": "x" }), &[]);
        assert_eq!(view["jira"], json!({ "baseUrl": "", "apiTokenSet": false, "apiTokenExternal": "" }));
        assert_eq!(view["github"], json!({ "baseUrl": "", "apiTokenSet": false, "apiTokenExternal": "" }));
        assert_eq!(view["jenkins"], json!({ "baseUrl": "", "username": "", "apiTokenSet": false, "apiTokenExternal": "" }));
        assert_eq!(view["repos"], json!({}));
        assert_eq!(view["reviewEditor"], Value::Null);
        // No enabledFeatures field means "every known feature" (legacy configs).
        assert_eq!(view["enabledFeatures"], json!(features().all_feature_ids()));
        assert_eq!(view["sessionCache"], json!({ "ttlMinutes": 30, "heartbeat": false }));
        assert_eq!(view["mcp"], json!({ "tokenSet": false }));
    }

    #[test]
    fn public_settings_reports_token_set_per_name_from_the_store_not_from_config() {
        let view = public(&current(), &["jenkins.apiToken"]);
        assert_eq!((view["jira"]["apiTokenSet"].clone(), view["jenkins"]["apiTokenSet"].clone(), view["github"]["apiTokenSet"].clone()), (json!(false), json!(true), json!(false)));
        let gh = public(&json!({ "github": { "baseUrl": "https://github.com" } }), &["github.apiToken"]);
        assert_eq!(gh["github"], json!({ "baseUrl": "https://github.com", "apiTokenSet": true, "apiTokenExternal": "" }));
    }

    #[test]
    fn public_settings_names_the_other_tool_file_a_site_borrows_its_token_from_only_when_none_is_saved() {
        let external = json!({ "jira": "~/.tool/.jira.env", "github": "~/.claude.json (acme-github)" }).as_object().unwrap().clone();
        let view = with_settings(|s| s.public_settings(&current(), &CredentialView { saved: vec!["github.apiToken".into()], external }));
        assert_eq!(view["jira"]["apiTokenExternal"], "~/.tool/.jira.env");
        assert_eq!(view["github"]["apiTokenExternal"], "");
        assert_eq!(view["jenkins"]["apiTokenExternal"], "");
    }

    #[test]
    fn public_settings_key_order_follows_the_node_service() {
        let keys: Vec<String> = public(&current(), &[]).keys().cloned().collect();
        assert_eq!(
            keys,
            ["port", "enabledFeatures", "repos", "reviewEditor", "github", "jira", "jenkins", "analyzeIssue", "history", "riskFacts", "summarizeComments", "addressReviewComments", "ticketToPr", "sessionCache", "mcp",
             "watchers", "budget", "scheduler", "notify", "digest", "prePush", "similar"]
        );
    }

    #[test]
    fn public_settings_for_history_risk_facts_ticket_to_pr_models_and_the_hidden_editor() {
        assert_eq!(public(&current(), &[])["history"]["retentionDays"], 180);
        assert_eq!(public(&json!({ "history": { "retentionDays": 30 } }), &[])["history"]["retentionDays"], 30);
        assert_eq!(public(&current(), &[])["riskFacts"]["url"], "");
        assert_eq!(public(&json!({ "riskFacts": { "url": "https://facts.example/risk.json" } }), &[])["riskFacts"]["url"], "https://facts.example/risk.json");
        assert_eq!(public(&current(), &[])["ticketToPr"], json!({ "reviewTransitionName": "In Review", "autoMoveToReview": true, "createWorktreeWithoutAsking": false }));
        assert_eq!(public(&json!({ "ticketToPr": { "reviewTransitionName": "Code Review", "autoMoveToReview": false } }), &[])["ticketToPr"]["reviewTransitionName"], "Code Review");
        assert_eq!(public(&current(), &[])["summarizeComments"]["model"], "");
        assert_eq!(public(&current(), &[])["addressReviewComments"]["model"], "");
        assert_eq!(public(&json!({ "reviewEditor": "vscode" }), &[])["reviewEditor"], Value::Null);
        assert_eq!(public(&json!({ "sessionCache": { "ttlMinutes": 5, "heartbeat": true } }), &[])["sessionCache"], json!({ "ttlMinutes": 5, "heartbeat": true }));
        assert_eq!(public(&current(), &[])["analyzeIssue"]["projects"], json!(["PROJ"]));
        assert_eq!(public(&current(), &[])["analyzeIssue"]["componentRepoMap"], json!({ "UI": "ACME/sample-app" }));
    }

    #[test]
    fn feature_summaries_list_the_descriptors_without_prompt_setup() {
        let s = feature_summaries(&features());
        assert_eq!(s.len(), 4);
        assert_eq!(s[1], json!({ "id": "create-jira-subtasks", "label": "Create subtasks", "description": "", "requiredChecks": [], "needsRepos": false, "enabledByDefault": false }));
    }

    // ---- validateSettingsUpdate ----

    #[test]
    fn validate_accepts_a_full_valid_update() {
        let errors = validate(json!({
            "enabledFeatures": ["resolve-conflict", "create-jira-subtasks"],
            "repos": { "ACME/sample-app": "/Users/me/gitviews/sample-app", "ACME/sample-service": "/Users/me/gitviews/sample-service" },
            "reviewEditor": "cursor",
            "jira": { "baseUrl": "https://jira.example.com/", "apiToken": "", "clearApiToken": false },
            "jenkins": { "baseUrl": "", "username": "", "apiToken": "abc" },
            "github": { "baseUrl": "https://github.com/", "apiToken": "bb-tok" },
            "analyzeIssue": { "model": "" },
            "acknowledgeWarnings": true,
        }));
        assert_eq!(errors, vec![]);
    }

    #[test]
    fn validate_refuses_port_and_shared_secret_and_unknown_keys() {
        let errors = validate(json!({ "port": 9999, "sharedSecret": "new", "bogus": 1 }));
        assert_eq!(errors.iter().map(|e| e.field.as_str()).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>(), ["bogus", "port", "sharedSecret"]);
        assert!(errors.iter().find(|e| e.field == "port").unwrap().message.contains("npm run setup"));
    }

    #[test]
    fn validate_rejects_a_non_object_body() {
        for body in [Value::Null, json!([]), json!("x")] {
            assert_eq!(validate(body).len(), 1);
        }
        assert_eq!(validate(json!(5))[0].message, "Settings must be a JSON object.");
    }

    #[test]
    fn validate_rejects_unknown_and_duplicate_feature_ids() {
        let errors = validate(json!({ "enabledFeatures": ["resolve-conflict", "resolve-conflict", "nope"] }));
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|e| e.message.contains("\"nope\"")));
        assert!(errors.iter().any(|e| e.message.contains("more than once")));
        assert_eq!(validate(json!({ "enabledFeatures": "all" })).len(), 1);
        assert_eq!(validate(json!({ "enabledFeatures": [5] }))[0].message, "\"5\" isn't a known feature.");
    }

    #[test]
    fn validate_rejects_unsafe_repo_keys_and_relative_paths() {
        let f = fields(json!({ "repos": { "ACME/../etc": "/tmp/x", "no-slash": "/tmp/x", "ACME/a/b": "/tmp/x", "ACME/relative": "gitviews/relative", "ACME/blank": "  " } }));
        assert_eq!(f, ["repos.ACME/../etc", "repos.ACME/a/b", "repos.ACME/blank", "repos.ACME/relative", "repos.no-slash"]);
        assert_eq!(fields(json!({ "repos": [] })), ["repos"]);
    }

    #[test]
    fn validate_rejects_case_insensitive_duplicate_repo_keys() {
        assert_eq!(fields(json!({ "repos": { "ACME/sample-app": "/Users/me/gitviews/sample-app", "acme/sample-app": "/Users/me/gitviews/sample-app" } })), ["repos.acme/sample-app"]);
    }

    #[test]
    fn validate_checks_only_new_or_moved_repo_mappings() {
        let checked = RefCell::new(vec![]);
        let check = |p: &str| {
            checked.borrow_mut().push(p.to_string());
            Check { ok: false, message: format!("{p} does not exist.") }
        };
        let errors = validate_with(json!({ "repos": { "ACME/sample-app": "/Users/me/gitviews/sample-app", "ACME/new": "/Users/me/gitviews/new" } }), &current(), &check, &default_origin);
        assert_eq!(*checked.borrow(), ["/Users/me/gitviews/new"]);
        assert_eq!(errors, [FieldError { field: "repos.ACME/new".into(), message: "/Users/me/gitviews/new does not exist.".into() }]);
    }

    #[test]
    fn validate_rejects_a_clone_whose_origin_is_a_different_repo() {
        let origin = |_: &str| Some("https://github.com/acme/sample-app.git".to_string());
        let errors = validate_with(json!({ "repos": { "ACME/other": "/Users/me/gitviews/sample-app2" } }), &current(), &ok_check, &origin);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("not ACME/other"), "{}", errors[0].message);
        let none = |_: &str| None;
        let unknown = validate_with(json!({ "repos": { "ACME/other": "/x/y" } }), &current(), &ok_check, &none);
        assert!(unknown[0].message.contains("an unknown remote"));
    }

    #[test]
    fn validate_validates_editor_models_urls_and_tokens() {
        let f = fields(json!({
            "reviewEditor": "emacs",
            "analyzeIssue": { "model": "rm -rf /", "extra": 1 },
            "jira": { "baseUrl": "javascript:alert(1)", "apiToken": "has space", "clearApiToken": "yes" },
            "jenkins": { "baseUrl": "ftp://jenkins", "username": "a\nb", "surprise": true },
            "github": { "baseUrl": "not a url", "username": "shouldn't be allowed" },
        }));
        assert_eq!(
            f,
            ["analyzeIssue.extra", "analyzeIssue.model", "github.baseUrl", "github.username", "jenkins.baseUrl", "jenkins.surprise", "jenkins.username", "jira.apiToken", "jira.baseUrl", "jira.clearApiToken", "reviewEditor"]
        );
        assert_eq!(fields(json!({ "jira": { "apiToken": "k".repeat(1001) } })), ["jira.apiToken"]);
        assert_eq!(fields(json!({ "jira": "x" })), ["jira"]);
        assert!(validate(json!({ "reviewEditor": null })).is_empty());
    }

    #[test]
    fn validate_checks_the_analyze_targets() {
        assert_eq!(fields(json!({ "analyzeIssue": { "projects": [], "issueTypes": ["a\nb"], "componentRepoMap": { "UI": "nope" } } })), ["analyzeIssue.componentRepoMap", "analyzeIssue.issueTypes", "analyzeIssue.projects"]);
        assert!(validate(json!({ "analyzeIssue": { "projects": ["ci"], "issueTypes": [], "componentRepoMap": null } })).is_empty());
        assert_eq!(validate(json!({ "analyzeIssue": { "projects": [] } }))[0].message, "must be a list of 1 to 20 project keys.");
    }

    #[test]
    fn validate_session_cache() {
        assert!(validate(json!({ "sessionCache": { "ttlMinutes": 30, "heartbeat": true } })).is_empty());
        for bad in [json!(-1), json!(1.5), json!("30"), json!(1441)] {
            assert_eq!(fields(json!({ "sessionCache": { "ttlMinutes": bad } })), ["sessionCache.ttlMinutes"], "{bad}");
        }
        assert_eq!(fields(json!({ "sessionCache": { "heartbeat": "yes" } })), ["sessionCache.heartbeat"]);
        let unknown = validate(json!({ "sessionCache": { "cookie": "a=b" } }));
        assert_eq!((unknown[0].field.as_str(), unknown[0].message.as_str()), ("sessionCache.cookie", "isn't a known setting."));
        assert_eq!(fields(json!({ "sessionCache": "off" })), ["sessionCache"]);
    }

    #[test]
    fn validate_mcp() {
        assert!(validate(json!({ "mcp": { "rotateToken": true } })).is_empty());
        for bad in [json!(false), json!("true"), json!(1)] {
            assert_eq!(fields(json!({ "mcp": { "rotateToken": bad } })), ["mcp.rotateToken"], "{bad}");
        }
        let unknown = validate(json!({ "mcp": { "token": "abc" } }));
        assert_eq!(unknown.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(), ["mcp.token"]);
        assert!(!serde_json::to_string(&unknown.iter().map(FieldError::to_json).collect::<Vec<_>>()).unwrap().contains("abc"));
        assert_eq!(fields(json!({ "mcp": "never" })), ["mcp"]);
    }

    #[test]
    fn validate_history_risk_facts_models_and_ticket_to_pr() {
        assert!(validate(json!({ "history": { "retentionDays": 30 } })).is_empty());
        for bad in [json!(3), json!(30.5), json!("30"), json!(3651)] {
            assert_eq!(fields(json!({ "history": { "retentionDays": bad } })), ["history.retentionDays"], "{bad}");
        }
        assert_eq!(fields(json!({ "history": { "nope": 1 } })), ["history.nope"]);
        assert!(validate(json!({ "riskFacts": { "url": "https://facts.example/risk.json" } })).is_empty());
        assert!(validate(json!({ "riskFacts": { "url": "" } })).is_empty());
        for bad in [json!("ftp://x/y"), json!("not a url"), json!("https://a/b\nc"), json!(format!("https://x/{}", "a".repeat(600))), json!(5)] {
            assert_eq!(fields(json!({ "riskFacts": { "url": bad } })), ["riskFacts.url"], "{bad}");
        }
        assert_eq!(fields(json!({ "riskFacts": { "nope": 1 } })), ["riskFacts.nope"]);
        for key in ["summarizeComments", "addressReviewComments"] {
            assert!(validate(json!({ key: { "model": "claude-opus-4-8[context=1m,effort=high]" } })).is_empty());
            assert!(validate(json!({ key: { "model": "" } })).is_empty());
            assert_eq!(fields(json!({ key: { "model": "rm -rf /" } })), [format!("{key}.model")]);
            assert_eq!(fields(json!({ key: { "checkCommands": ["npm test"] } })), [format!("{key}.checkCommands")]);
            assert_eq!(fields(json!({ key: "x" })), [key]);
            assert_eq!(fields(json!({ key: { "model": 5 } })), [format!("{key}.model")]);
        }
        assert!(validate(json!({ "ticketToPr": { "reviewTransitionName": "Code Review" } })).is_empty());
        for bad in [json!("a\nb"), json!("x".repeat(101)), json!(5)] {
            assert_eq!(fields(json!({ "ticketToPr": { "reviewTransitionName": bad } })), ["ticketToPr.reviewTransitionName"]);
        }
        assert_eq!(fields(json!({ "ticketToPr": { "other": 1 } })), ["ticketToPr.other"]);
        assert_eq!(fields(json!({ "ticketToPr": "x" })), ["ticketToPr"]);
        assert_eq!(fields(json!({ "ticketToPr": { "autoMoveToReview": "no" } })), ["ticketToPr.autoMoveToReview"]);
        assert_eq!(fields(json!({ "ticketToPr": { "createWorktreeWithoutAsking": 1 } })), ["ticketToPr.createWorktreeWithoutAsking"]);
        assert_eq!(fields(json!({ "reviewEditor": "vscode" })), ["reviewEditor"]);
        assert_eq!(fields(json!({ "acknowledgeWarnings": "yes" })), ["acknowledgeWarnings"]);
    }

    #[test]
    fn validate_includes_the_background_keys() {
        assert_eq!(fields(json!({ "budget": { "claudeRunsPerDay": 51 } })), ["budget.claudeRunsPerDay"]);
        assert_eq!(fields(json!({ "similar": { "enabled": "no" } })), ["similar.enabled"]);
    }

    #[test]
    fn validate_accepts_a_real_clone_with_a_matching_origin() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("sample-app");
        fs::create_dir(&clone).unwrap();
        let git = |args: &[&str]| crate::git_cli::testing::git(&clone, args);
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "https://github.com/acme/sample-app.git"]);
        let real = RepoDeps::real();
        let run = |key: &str| with_settings(|s| s.validate_settings_update(&json!({ "repos": { key: clone.to_str().unwrap() } }), &json!({ "repos": {} }), &real));
        assert_eq!(run("ACME/sample-app"), vec![]);
        assert_eq!(run("ACME/api").len(), 1);
    }

    // ---- mergeSettings ----

    #[test]
    fn merge_keeps_tokens_secret_port_and_unknown_fields_and_reports_no_token_ops_for_a_blank_token() {
        let cur = current();
        let (next, ops) = merge(&cur, json!({ "jira": { "baseUrl": "https://jira2.example.com/", "apiToken": "" }, "jenkins": { "baseUrl": "https://jenkins.example.com", "username": "me" } }));
        assert_eq!((next["sharedSecret"].clone(), next["port"].clone()), (json!("s3cret"), json!(8787)));
        assert_eq!(next["someFutureField"], json!({ "keep": true }));
        assert_eq!(next["jira"], json!({ "baseUrl": "https://jira2.example.com" }));
        assert!(next["jenkins"].get("apiToken").is_none());
        assert_eq!(ops, []);
        assert_eq!(next["jenkins"]["maxBuildsToScan"], 9);
        assert_eq!(cur["jira"]["baseUrl"], "https://jira.example.com", "the input is never mutated");
    }

    #[test]
    fn merge_reports_a_token_set_or_clear_as_a_pending_op_and_never_applies_it() {
        let (replaced, set_ops) = merge(&current(), json!({ "jira": { "apiToken": " new-token " } }));
        assert!(replaced["jira"].get("apiToken").is_none());
        assert_eq!(set_ops, [op("jira.apiToken", Some("new-token"))]);
        let (cleared, clear_ops) = merge(&current(), json!({ "jira": { "apiToken": "ignored", "clearApiToken": true } }));
        assert!(cleared["jira"].get("apiToken").is_none());
        assert_eq!(cleared["jira"]["baseUrl"], "https://jira.example.com");
        assert_eq!(clear_ops, [op("jira.apiToken", None)]);
    }

    fn legacy(token: &str) -> Value {
        let mut c = current();
        c["jira"] = json!({ "baseUrl": "https://jira.example.com", "apiToken": token });
        c
    }

    #[test]
    fn merge_handles_a_legacy_token_left_in_config() {
        let update = || json!({ "jira": { "baseUrl": "https://jira.example.com" } });
        // The store already has one: its copy is authoritative.
        let (next, ops) = merge_with_saved(&legacy("leftover-from-before"), update(), &["jira.apiToken"]);
        assert!(next["jira"].get("apiToken").is_none() && ops.is_empty());
        // The store lacks it: move it there.
        let (next, ops) = merge_with_saved(&legacy(" leftover "), update(), &[]);
        assert!(next["jira"].get("apiToken").is_none());
        assert_eq!(ops, [op("jira.apiToken", Some("leftover"))]);
        // A new token or a clear wins over a legacy one.
        let (set, ops) = merge_with_saved(&legacy("leftover"), json!({ "jira": { "apiToken": "fresh" } }), &[]);
        assert!(set["jira"].get("apiToken").is_none());
        assert_eq!(ops, [op("jira.apiToken", Some("fresh"))]);
        let (clear, ops) = merge_with_saved(&legacy("leftover"), json!({ "jira": { "clearApiToken": true } }), &[]);
        assert!(clear["jira"].get("apiToken").is_none());
        assert_eq!(ops, [op("jira.apiToken", None)]);
        // Can't tell whether the store has one: keep the legacy field untouched.
        let (next, ops) = merge(&legacy("leftover"), update());
        assert_eq!(next["jira"]["apiToken"], "leftover");
        assert!(ops.is_empty());
        // Blank legacy values are just dropped either way.
        assert!(merge(&legacy("  "), json!({ "jira": {} })).0["jira"].get("apiToken").is_none());
        // Pure: current is never mutated.
        let before = legacy("leftover");
        let copy = before.clone();
        merge_with_saved(&before, json!({ "jira": { "baseUrl": "https://x.example.com" } }), &[]);
        assert_eq!(before, copy);
    }

    #[test]
    fn merge_drops_blank_site_fields_and_the_whole_object_once_empty() {
        let (next, _) = merge(&json!({ "port": 1, "sharedSecret": "x", "jira": { "baseUrl": "https://jira.example.com" } }), json!({ "jira": { "baseUrl": " " } }));
        assert!(next.get("jira").is_none());
    }

    #[test]
    fn merge_handles_github_like_the_other_sites() {
        let (with_url, set_ops) = merge(&current(), json!({ "github": { "baseUrl": "https://github.com/", "apiToken": " bb-token " } }));
        assert_eq!(with_url["github"], json!({ "baseUrl": "https://github.com" }));
        assert_eq!(set_ops, [op("github.apiToken", Some("bb-token"))]);
        let (cleared, clear_ops) = merge(&with_url, json!({ "github": { "apiToken": "ignored", "clearApiToken": true } }));
        assert_eq!(cleared["github"], json!({ "baseUrl": "https://github.com" }));
        assert_eq!(clear_ops, [op("github.apiToken", None)]);
        assert!(merge(&cleared, json!({ "github": { "baseUrl": " " } })).0.get("github").is_none());
    }

    #[test]
    fn merge_orders_enabled_features_like_the_registry_and_snapshots_known_features() {
        let (next, _) = merge(&current(), json!({ "enabledFeatures": ["review-in-editor", "resolve-conflict"] }));
        assert_eq!(next["enabledFeatures"], json!(["resolve-conflict", "review-in-editor"]));
        assert_eq!(next["knownFeatures"], json!(features().all_feature_ids()));
    }

    #[test]
    fn merge_replaces_repos_with_resolved_paths_and_clears_the_editor_and_model() {
        let (next, _) = merge(&current(), json!({ "repos": { "ACME/new": " /Users/me/gitviews/new/ " }, "reviewEditor": null, "analyzeIssue": { "model": "" } }));
        assert_eq!(next["repos"], json!({ "ACME/new": "/Users/me/gitviews/new" }));
        assert!(next.get("reviewEditor").is_none());
        assert_eq!(next["analyzeIssue"], json!({ "componentRepoMap": { "UI": "ACME/sample-app" } }));
        assert!(next.get("claudeModel").is_none());
    }

    #[test]
    fn merge_analyze_targets_are_saved_only_when_they_differ_from_the_defaults() {
        let (next, _) = merge(&current(), json!({ "analyzeIssue": { "projects": ["ci"], "issueTypes": ["Bug"], "componentRepoMap": {} } }));
        assert_eq!(next["analyzeIssue"]["projects"], json!(["CI"]));
        assert_eq!(next["analyzeIssue"]["issueTypes"], json!(["Bug"]));
        assert!(next["analyzeIssue"].get("componentRepoMap").is_none(), "an empty map is the default");
        let (back, _) = merge(&next, json!({ "analyzeIssue": { "projects": ["proj"], "issueTypes": [], "model": " m " } }));
        assert_eq!(back["analyzeIssue"], json!({ "model": "m" }));
        assert!(merge(&json!({ "analyzeIssue": { "model": "m" } }), json!({ "analyzeIssue": { "model": "" } })).0.get("analyzeIssue").is_none());
    }

    #[test]
    fn merge_session_cache() {
        assert!(merge(&current(), json!({ "sessionCache": { "ttlMinutes": 30, "heartbeat": false } })).0.get("sessionCache").is_none());
        let mut with_ttl = current();
        with_ttl["sessionCache"] = json!({ "ttlMinutes": 45 });
        assert_eq!(merge(&with_ttl, json!({ "sessionCache": { "heartbeat": true } })).0["sessionCache"], json!({ "ttlMinutes": 45, "heartbeat": true }));
        assert_eq!(merge(&current(), json!({ "sessionCache": { "ttlMinutes": 0 } })).0["sessionCache"], json!({ "ttlMinutes": 0 }));
    }

    #[test]
    fn merge_history_risk_facts_models_and_ticket_to_pr() {
        let (set, _) = merge(&current(), json!({ "history": { "retentionDays": 30 } }));
        assert_eq!(set["history"], json!({ "retentionDays": 30 }));
        assert!(merge(&set, json!({ "history": { "retentionDays": 180 } })).0.get("history").is_none());
        let (set, _) = merge(&current(), json!({ "riskFacts": { "url": "  https://facts.example/risk.json  " } }));
        assert_eq!(set["riskFacts"], json!({ "url": "https://facts.example/risk.json" }));
        assert!(merge(&set, json!({ "riskFacts": { "url": "" } })).0.get("riskFacts").is_none());
        let (set, _) = merge(&current(), json!({ "summarizeComments": { "model": " haiku " } }));
        assert_eq!(set["summarizeComments"], json!({ "model": "haiku" }));
        assert!(merge(&set, json!({ "summarizeComments": { "model": "" } })).0.get("summarizeComments").is_none());
        let mut with_checks = current();
        with_checks["addressReviewComments"] = json!({ "checkCommands": ["npm test"] });
        let (set, _) = merge(&with_checks, json!({ "addressReviewComments": { "model": " gpt-5 " } }));
        assert_eq!(set["addressReviewComments"], json!({ "checkCommands": ["npm test"], "model": "gpt-5" }));
        assert_eq!(merge(&set, json!({ "addressReviewComments": { "model": "" } })).0["addressReviewComments"], json!({ "checkCommands": ["npm test"] }));

        let (set, _) = merge(&current(), json!({ "ticketToPr": { "reviewTransitionName": "  Code Review " } }));
        assert_eq!(set["ticketToPr"], json!({ "reviewTransitionName": "Code Review" }));
        assert!(merge(&set, json!({ "ticketToPr": { "reviewTransitionName": "In Review" } })).0.get("ticketToPr").is_none());
        assert!(merge(&set, json!({ "ticketToPr": { "reviewTransitionName": "" } })).0.get("ticketToPr").is_none());
        let (off, _) = merge(&current(), json!({ "ticketToPr": { "autoMoveToReview": false } }));
        assert_eq!(off["ticketToPr"], json!({ "autoMoveToReview": false }));
        // The panel sends both fields together: neither wipes the other.
        let (both, _) = merge(&off, json!({ "ticketToPr": { "reviewTransitionName": "Code Review", "autoMoveToReview": false } }));
        assert_eq!(both["ticketToPr"], json!({ "reviewTransitionName": "Code Review", "autoMoveToReview": false }));
        assert_eq!(merge(&both, json!({ "ticketToPr": { "reviewTransitionName": "In Review" } })).0["ticketToPr"], json!({ "autoMoveToReview": false }));
        assert!(merge(&both, json!({ "ticketToPr": { "reviewTransitionName": "", "autoMoveToReview": true } })).0.get("ticketToPr").is_none());
        assert_eq!(merge(&current(), json!({ "ticketToPr": { "createWorktreeWithoutAsking": true } })).0["ticketToPr"], json!({ "createWorktreeWithoutAsking": true }));
    }

    #[test]
    fn merge_turns_rotate_token_into_an_mcp_token_op_never_into_config() {
        let (next, ops) = with_settings(|s| s.merge_settings(&current(), &json!({ "mcp": { "rotateToken": true } }), Some(&[]), Some(&|| "fresh-mcp-token".to_string()))).unwrap();
        assert_eq!(ops, [op("mcp.token", Some("fresh-mcp-token"))]);
        assert!(next.get("mcp").is_none());
        let text = next.to_string();
        assert!(!text.contains("rotateToken") && !text.contains("fresh-mcp-token"));
        let boom = |_: &str| -> String { unreachable!() };
        let _ = boom;
        let (_, none) = with_settings(|s| s.merge_settings(&current(), &json!({ "mcp": {} }), None, Some(&|| panic!("should not be called")))).unwrap();
        assert!(none.is_empty());
        assert!(with_settings(|s| s.merge_settings(&current(), &json!({ "mcp": { "rotateToken": true } }), None, None)).unwrap_err().contains("newMcpToken"));
    }

    #[test]
    fn merge_includes_the_background_keys() {
        let (next, ops) = merge(&current(), json!({ "similar": { "enabled": false } }));
        assert_eq!(next["similar"], json!({ "enabled": false }));
        assert!(ops.is_empty());
    }

    /// A pack's own section: validated, merged, shown and its key accepted like a built-in one.
    struct Fake;

    impl crate::packs::SettingsSection for Fake {
        fn key(&self) -> &str {
            "extra"
        }
        fn validate(&self, update: &Map<String, Value>, _: &Value, errors: &mut Vec<FieldError>) {
            if update.get("n").is_some_and(|n| !n.is_number()) {
                err(errors, "extra.n", "must be a number.");
            }
        }
        fn merge(&self, next: &mut Map<String, Value>, update: &Value, token_ops: &mut Vec<TokenOp>) {
            next.insert("extra".into(), json!({ "n": update["extra"]["n"] }));
            if let Some(k) = update["extra"]["key"].as_str() {
                token_ops.push(TokenOp { name: "extra.key".into(), value: Some(k.into()) });
            }
        }
        fn public(&self, config: &Value, saved: &[String]) -> Value {
            json!({ "n": config["extra"]["n"], "keySet": saved.iter().any(|n| n == "extra.key") })
        }
    }

    fn with_section<R>(f: impl FnOnce(&Settings<'_, ()>) -> R) -> R {
        let features = features();
        let mut pack = Pack::new("t", vec![]);
        pack.settings_sections = vec![std::sync::Arc::new(Fake)];
        let packs = PackRegistry::new(vec![pack]).unwrap();
        f(&Settings { features: &features, packs: &packs })
    }

    #[test]
    fn a_packs_settings_section_is_accepted_validated_merged_and_shown_like_a_built_in_key() {
        let check = |_: &str| Check { ok: true, message: "OK.".into() };
        let origin = |_: &str| None;
        let deps = RepoDeps { check_repo_path: &check, get_origin_url: &origin };
        assert!(with_section(|s| s.validate_settings_update(&json!({ "extra": { "n": 3 } }), &current(), &deps)).is_empty());
        let bad = with_section(|s| s.validate_settings_update(&json!({ "extra": { "n": "x" } }), &current(), &deps));
        assert_eq!(bad.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(), ["extra.n"]);
        let wrong = with_section(|s| s.validate_settings_update(&json!({ "extra": 1 }), &current(), &deps));
        assert_eq!(wrong.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(), ["extra"]);
        // Without the pack, the key is unknown.
        assert!(!validate(json!({ "extra": { "n": 3 } })).is_empty());
        let (next, ops) = with_section(|s| s.merge_settings(&current(), &json!({ "extra": { "n": 3, "key": "k" } }), None, None)).unwrap();
        assert_eq!((next["extra"].clone(), ops), (json!({ "n": 3 }), vec![op("extra.key", Some("k"))]));
        let credentials = CredentialView { saved: vec!["extra.key".into()], external: Map::new() };
        let shown = with_section(|s| s.public_settings(&next, &credentials));
        assert_eq!(shown["extra"], json!({ "n": 3, "keySet": true }));
        assert_eq!(shown.keys().next_back().map(String::as_str), Some("extra"), "a pack's sections follow the built-in ones");
    }

    // ---- applyTokenOps ----

    #[test]
    fn apply_token_ops_applies_every_op_in_order_and_reports_ok() {
        let mut applied = vec![];
        let result = apply_token_ops(&[op("jira.apiToken", Some("jira-tok")), op("jenkins.apiToken", None)], |n, v| {
            applied.push((n.to_string(), v.map(String::from)));
            Ok(())
        });
        assert_eq!(result, TokenOpsResult::Ok);
        assert_eq!(applied, [("jira.apiToken".to_string(), Some("jira-tok".to_string())), ("jenkins.apiToken".to_string(), None)]);
        assert_eq!(apply_token_ops(&[], |_, _| panic!("should not be called")), TokenOpsResult::Ok);
    }

    #[test]
    fn apply_token_ops_stops_at_the_first_failure_ops_before_it_stay_applied() {
        let mut applied = vec![];
        let ops = [op("jira.apiToken", Some("jira-tok")), op("jenkins.apiToken", Some("jenkins-tok")), op("github.apiToken", Some("never-reached"))];
        let result = apply_token_ops(&ops, |n, v| {
            if n == "jenkins.apiToken" {
                return Err("disk full".into());
            }
            applied.push((n.to_string(), v.map(String::from)));
            Ok(())
        });
        assert_eq!(applied, [("jira.apiToken".to_string(), Some("jira-tok".to_string()))]);
        assert_eq!(result, TokenOpsResult::Failed { failed_op: op("jenkins.apiToken", Some("jenkins-tok")), error: "disk full".into() });
    }

    // ---- checks, restart, suggestions ----

    #[test]
    fn prerequisite_warnings_list_failing_checks_with_the_features_needing_them() {
        let checks = |name: &str| match name {
            "git" => Check { ok: true, message: "git OK.".into() },
            "claudeCli" => Check { ok: false, message: "claude CLI not found.".into() },
            _ => Check { ok: false, message: "not logged in.".into() },
        };
        let w = with_settings(|s| s.prerequisite_warnings(&["resolve-conflict", "review-in-editor"], &checks));
        assert_eq!(w.iter().map(|x| x["check"].as_str().unwrap()).collect::<Vec<_>>(), ["claudeCli", "claudeAuth"]);
        assert_eq!(w[0]["features"], json!(["Resolve Merge Conflicts"]));
        assert_eq!(w[0]["message"], "claude CLI not found.");
        assert!(with_settings(|s| s.prerequisite_warnings(&["review-in-editor"], &checks)).is_empty());
    }

    #[test]
    fn run_all_checks_runs_every_check_some_feature_requires() {
        let r = with_settings(|s| s.run_all_checks(&|n: &str| Check { ok: n != "claudeAuth", message: n.into() }));
        let mut names: Vec<&str> = r.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["claudeAuth", "claudeCli", "git"]);
        assert_eq!(r["claudeAuth"], json!({ "ok": false, "message": "claudeAuth" }));
    }

    #[test]
    fn pending_restart_only_cares_about_the_enabled_feature_set() {
        let running: Vec<String> = vec!["resolve-conflict".into(), "review-in-editor".into()];
        let mut other_repos = current();
        other_repos["repos"] = json!({ "ACME/other": "/x" });
        assert_eq!(with_settings(|s| s.pending_restart(&running, &other_repos)), json!({ "required": false, "added": [], "removed": [] }));
        let mut changed = current();
        changed["enabledFeatures"] = json!(["resolve-conflict", "analyze-issue"]);
        assert_eq!(with_settings(|s| s.pending_restart(&running, &changed)), json!({ "required": true, "added": ["analyze-issue"], "removed": ["review-in-editor"] }));
        let mut heartbeat = current();
        heartbeat["sessionCache"] = json!({ "heartbeat": true });
        assert_eq!(with_settings(|s| s.pending_restart(&running, &heartbeat))["required"], false, "every reader is live");
    }

    #[test]
    fn suggested_repos_skips_clones_already_mapped_by_key_or_folder() {
        let clones = [
            Clone_ { key: "ACME/Sample-App".into(), path: "/elsewhere/sample-app".into() },
            Clone_ { key: "ACME/alias".into(), path: "/Users/me/gitviews/sample-app/".into() },
            Clone_ { key: "ACME/new".into(), path: "/Users/me/gitviews/new".into() },
        ];
        assert_eq!(suggested_repos(current()["repos"].as_object().unwrap(), &clones), [Clone_ { key: "ACME/new".into(), path: "/Users/me/gitviews/new".into() }]);
    }

    // ---- writeConfigFile and the token migration ----

    #[test]
    fn write_config_file_replaces_the_file_atomically_and_keeps_its_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        fs::write(&file, "{}").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        write_config_file(&file, &json!({ "a": 1 })).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\n  \"a\": 1\n}\n");
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        let fresh = dir.path().join("new.json");
        write_config_file(&fresh, &json!({ "b": 2 })).unwrap();
        assert_eq!(fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(write_config_file(&dir.path().join("missing").join("x.json"), &json!({})).is_err());
    }

    #[test]
    fn migrate_moves_non_empty_apitokens_into_the_store_blanks_and_saves_config() {
        let mut stored: Vec<(String, String)> = vec![];
        let mut config = json!({
            "jira": { "baseUrl": "https://jira.example.com", "apiToken": "jira-tok" },
            "jenkins": { "baseUrl": "https://jenkins.example.com", "apiToken": "jenkins-tok" },
            "github": { "apiToken": "bb-tok" },
        });
        let saved = RefCell::new(None);
        let migrated = migrate_tokens_to_store(&mut config, |n, v| { stored.push((n.into(), v.into())); Ok(()) }, |c| { *saved.borrow_mut() = Some(c.clone()); Ok(()) }, |_| {});
        let mut sorted = migrated.clone();
        sorted.sort();
        assert_eq!(sorted, ["github.apiToken", "jenkins.apiToken", "jira.apiToken"]);
        assert_eq!(stored.len(), 3);
        assert!(stored.contains(&("jira.apiToken".to_string(), "jira-tok".to_string())));
        for site in ["jira", "jenkins", "github"] {
            assert!(config[site].get("apiToken").is_none());
        }
        assert_eq!(config["jira"]["baseUrl"], "https://jira.example.com", "other fields untouched");
        assert_eq!(saved.borrow().as_ref(), Some(&config), "saveConfig is called with the (now blanked) config");
    }

    #[test]
    fn migrate_ignores_blank_or_missing_tokens_and_other_credentials() {
        let mut config = json!({ "jira": { "baseUrl": "x", "apiToken": "" }, "jenkins": { "baseUrl": "y" }, "mcp": { "token": "keep" }, "slack": { "token": "keep too" } });
        let calls = RefCell::new(0);
        let migrated = migrate_tokens_to_store(&mut config, |_, _| panic!("no store write"), |_| { *calls.borrow_mut() += 1; Ok(()) }, |_| {});
        assert!(migrated.is_empty() && *calls.borrow() == 0);
        assert_eq!((config["mcp"]["token"].clone(), config["slack"]["token"].clone()), (json!("keep"), json!("keep too")));
    }

    #[test]
    fn migrate_is_idempotent_and_leaves_config_untouched_with_one_warning_on_a_store_failure() {
        let mut config = json!({ "jira": { "baseUrl": "https://jira.example.com", "apiToken": "jira-tok" } });
        let saves = RefCell::new(0);
        migrate_tokens_to_store(&mut config, |_, _| Ok(()), |_| { *saves.borrow_mut() += 1; Ok(()) }, |_| {});
        assert_eq!(*saves.borrow(), 1);
        assert!(migrate_tokens_to_store(&mut config, |_, _| Ok(()), |_| { *saves.borrow_mut() += 1; Ok(()) }, |_| {}).is_empty());
        assert_eq!(*saves.borrow(), 1, "saveConfig was not called again");

        let mut config = json!({ "jira": { "baseUrl": "https://jira.example.com", "apiToken": "jira-tok" }, "jenkins": { "apiToken": "jenkins-tok" } });
        let warnings = RefCell::new(vec![]);
        let migrated = migrate_tokens_to_store(&mut config, |_, _| Err("disk full".into()), |_| panic!("not saved"), |m| warnings.borrow_mut().push(m.to_string()));
        assert!(migrated.is_empty());
        assert_eq!((config["jira"]["apiToken"].clone(), config["jenkins"]["apiToken"].clone()), (json!("jira-tok"), json!("jenkins-tok")), "config untouched on failure");
        assert_eq!(warnings.borrow().len(), 1, "exactly one warning, not one per token");
        assert!(warnings.borrow()[0].contains("jira.apiToken, jenkins.apiToken") || warnings.borrow()[0].contains("jenkins.apiToken"), "{:?}", warnings.borrow());
    }
}
