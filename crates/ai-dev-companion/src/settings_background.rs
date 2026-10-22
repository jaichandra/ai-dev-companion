//! The Settings panel's background-work keys (port of
//! `core/settings-background.js`), kept apart from `settings` so that file
//! doesn't grow another 300 lines: watchers, the daily Claude budget, quiet
//! hours, notification batching, the morning digest, the pre-push check and
//! similar items (a pack's own sections, such as an LLM's, are handled through
//! [`SettingsSection`]). None needs a restart: the scheduler and
//! analyze-issue read config live. Defaults are never persisted (same
//! convention as reviewEditor's "auto").

use crate::js_text::js_trim;
use crate::packs::SettingsSection;
use crate::schedule;
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub const BACKGROUND_KEYS: [&str; 7] = ["watchers", "budget", "scheduler", "notify", "digest", "prePush", "similar"];
pub const PRE_PUSH_MODES: [&str; 2] = ["warn", "off"];

/// A rejected field and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn to_json(&self) -> Value {
        json!({ "field": self.field, "message": self.message })
    }
}

fn err(errors: &mut Vec<FieldError>, field: impl Into<String>, message: impl Into<String>) {
    errors.push(FieldError { field: field.into(), message: message.into() });
}

fn is_int(v: &Value, min: i64, max: i64) -> bool {
    v.as_f64().is_some_and(|n| n.fract() == 0.0 && n >= min as f64 && n <= max as f64)
}

fn unknown_keys(errors: &mut Vec<FieldError>, prefix: &str, value: &Map<String, Value>, allowed: &[&str]) {
    for key in value.keys() {
        if !allowed.contains(&key.as_str()) {
            err(errors, format!("{prefix}.{key}"), "isn't a known setting.");
        }
    }
}

fn object_or_error<'a>(errors: &mut Vec<FieldError>, field: &str, value: &'a Value) -> Option<&'a Map<String, Value>> {
    let o = value.as_object();
    if o.is_none() {
        err(errors, field, "must be an object.");
    }
    o
}

/// Pushes `{field, message}` errors for the background keys in `update`.
pub fn validate_background_settings(update: &Value, current: &Value, errors: &mut Vec<FieldError>, sections: &[&Arc<dyn SettingsSection>]) {
    let defined = |k: &str| update.get(k).is_some();
    if defined("watchers") {
        if let Some(w) = object_or_error(errors, "watchers", &update["watchers"]) {
            unknown_keys(errors, "watchers", w, &schedule::WATCHERS);
            for name in schedule::WATCHERS {
                let Some(entry) = w.get(name) else { continue };
                let Some(o) = object_or_error(errors, &format!("watchers.{name}"), entry) else { continue };
                unknown_keys(errors, &format!("watchers.{name}"), o, &["enabled", "intervalMinutes"]);
                if o.get("enabled").is_some_and(|v| !v.is_boolean()) {
                    err(errors, format!("watchers.{name}.enabled"), "must be true or false.");
                }
                if o.get("intervalMinutes").is_some_and(|v| !is_int(v, schedule::MIN_INTERVAL_MINUTES, schedule::MAX_INTERVAL_MINUTES)) {
                    err(errors, format!("watchers.{name}.intervalMinutes"), "must be a whole number of minutes from 1 to 1440.");
                }
            }
        }
    }
    if defined("budget") {
        if let Some(b) = object_or_error(errors, "budget", &update["budget"]) {
            unknown_keys(errors, "budget", b, &["claudeRunsPerDay"]);
            if b.get("claudeRunsPerDay").is_some_and(|n| !is_int(n, 0, schedule::MAX_CLAUDE_RUNS_PER_DAY)) {
                err(errors, "budget.claudeRunsPerDay", format!("must be a whole number from 0 to {} (0 turns background Claude runs off).", schedule::MAX_CLAUDE_RUNS_PER_DAY));
            }
        }
    }
    if defined("scheduler") {
        if let Some(s) = object_or_error(errors, "scheduler", &update["scheduler"]) {
            unknown_keys(errors, "scheduler", s, &["quietHours"]);
            if let Some(q) = s.get("quietHours").filter(|q| !q.is_null()) {
                let (start, end) = (q.as_object().and_then(|_| schedule::parse_hm(q["start"].as_str())), q.as_object().and_then(|_| schedule::parse_hm(q["end"].as_str())));
                match (start, end) {
                    (Some(a), Some(b)) if a == b => err(errors, "scheduler.quietHours", "can't start and end at the same time."),
                    (Some(_), Some(_)) => unknown_keys(errors, "scheduler.quietHours", q.as_object().unwrap(), &["start", "end"]),
                    _ => err(errors, "scheduler.quietHours", "needs a start and an end time like \"19:00\" and \"08:00\" (or null for none)."),
                }
            }
        }
    }
    if defined("notify") {
        if let Some(n) = object_or_error(errors, "notify", &update["notify"]) {
            unknown_keys(errors, "notify", n, &["minIntervalMinutes"]);
            if n.get("minIntervalMinutes").is_some_and(|m| !is_int(m, 0, 1440)) {
                err(errors, "notify.minIntervalMinutes", "must be a whole number of minutes from 0 to 1440.");
            }
        }
    }
    if defined("digest") {
        if let Some(d) = object_or_error(errors, "digest", &update["digest"]) {
            unknown_keys(errors, "digest", d, &["enabled", "time"]);
            if d.get("enabled").is_some_and(|v| !v.is_boolean()) {
                err(errors, "digest.enabled", "must be true or false.");
            }
            if d.get("time").is_some_and(|t| schedule::parse_hm(t.as_str()).is_none()) {
                err(errors, "digest.time", "must be a time like \"08:30\".");
            }
        }
    }
    for section in sections {
        if defined(section.key()) {
            if let Some(p) = object_or_error(errors, section.key(), &update[section.key()]) {
                section.validate(p, current, errors);
            }
        }
    }
    if defined("prePush") {
        if let Some(p) = object_or_error(errors, "prePush", &update["prePush"]) {
            unknown_keys(errors, "prePush", p, &["mode"]);
            if p.get("mode").is_some_and(|m| !m.as_str().is_some_and(|m| PRE_PUSH_MODES.contains(&m))) {
                err(errors, "prePush.mode", format!("must be one of: {}.", PRE_PUSH_MODES.join(", ")));
            }
        }
    }
    if defined("similar") {
        if let Some(s) = object_or_error(errors, "similar", &update["similar"]) {
            unknown_keys(errors, "similar", s, &["enabled"]);
            if s.get("enabled").is_some_and(|v| !v.is_boolean()) {
                err(errors, "similar.enabled", "must be true or false.");
            }
        }
    }
}

/// A credential change a settings update asks for: save `value`, or clear it (`None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenOp {
    pub name: String,
    pub value: Option<String>,
}

fn obj_or_empty(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// Applies an already-validated update's background keys onto `next` (a copy
/// of the saved config), pushing any credential change onto `token_ops`.
pub fn merge_background_settings(next: &mut Map<String, Value>, update: &Value, token_ops: &mut Vec<TokenOp>, sections: &[&Arc<dyn SettingsSection>]) {
    if let Some(upd) = update.get("watchers") {
        let mut merged = Map::new();
        for (i, name) in schedule::WATCHERS.iter().enumerate() {
            let mut w = obj_or_empty(next.get("watchers").and_then(|w| w.get(*name)));
            w.extend(obj_or_empty(upd.get(*name)));
            let mut out = Map::new();
            if w.get("enabled") == Some(&json!(true)) {
                out.insert("enabled".into(), json!(true));
            }
            if let Some(m) = w.get("intervalMinutes").filter(|m| m.as_i64() != Some(schedule::WATCHER_DEFAULT_MINUTES[i])) {
                out.insert("intervalMinutes".into(), m.clone());
            }
            if !out.is_empty() {
                merged.insert(name.to_string(), Value::Object(out));
            }
        }
        set_or_remove(next, "watchers", (!merged.is_empty()).then(|| Value::Object(merged)));
    }
    if let Some(n) = update.get("budget").and_then(|b| b.get("claudeRunsPerDay")) {
        set_or_remove(next, "budget", (n.as_i64() != Some(schedule::DEFAULT_CLAUDE_RUNS_PER_DAY)).then(|| json!({ "claudeRunsPerDay": n })));
    }
    if let Some(q) = update.get("scheduler").and_then(|s| s.get("quietHours")) {
        set_or_remove(next, "scheduler", (!q.is_null()).then(|| json!({ "quietHours": { "start": js_trim(q["start"].as_str().unwrap_or("")), "end": js_trim(q["end"].as_str().unwrap_or("")) } })));
    }
    if let Some(m) = update.get("notify").and_then(|n| n.get("minIntervalMinutes")) {
        set_or_remove(next, "notify", (m.as_i64() != Some(schedule::DEFAULT_NOTIFY_MIN_INTERVAL_MINUTES)).then(|| json!({ "minIntervalMinutes": m })));
    }
    if let Some(upd) = update.get("digest") {
        let mut d = obj_or_empty(next.get("digest"));
        d.extend(obj_or_empty(Some(upd)));
        let mut out = Map::new();
        if d.get("enabled") == Some(&json!(true)) {
            out.insert("enabled".into(), json!(true));
        }
        if let Some(t) = d.get("time").and_then(Value::as_str).map(js_trim).filter(|t| *t != schedule::DEFAULT_DIGEST_TIME) {
            out.insert("time".into(), json!(t));
        }
        set_or_remove(next, "digest", (!out.is_empty()).then(|| Value::Object(out)));
    }
    for section in sections {
        if update.get(section.key()).is_some() {
            section.merge(next, update, token_ops);
        }
    }
    if let Some(mode) = update.get("prePush").and_then(|p| p.get("mode")) {
        set_or_remove(next, "prePush", (mode != "warn").then(|| json!({ "mode": mode })));
    }
    if let Some(enabled) = update.get("similar").and_then(|s| s.get("enabled")) {
        // On is the default: only "off" is saved.
        set_or_remove(next, "similar", (enabled != &json!(true)).then(|| json!({ "enabled": false })));
    }
}

fn set_or_remove(next: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(v) => {
            next.insert(key.to_string(), v);
        }
        None => {
            next.shift_remove(key);
        }
    }
}

/// What the panel shows for the background keys: the key only as `apiKeySet`.
pub fn public_background_settings(config: &Value, saved_token_names: &[String], sections: &[&Arc<dyn SettingsSection>]) -> Map<String, Value> {
    let quiet = schedule::quiet_hours_from(config);
    let mode = config["prePush"]["mode"].as_str().filter(|m| PRE_PUSH_MODES.contains(m)).unwrap_or("warn");
    let watchers: Map<String, Value> = schedule::watcher_settings(config).into_iter().map(|(n, w)| (n.to_string(), json!({ "enabled": w.enabled, "intervalMinutes": w.interval_minutes }))).collect();
    let digest = schedule::digest_settings(config);
    let mut out = Map::new();
    out.insert("watchers".into(), Value::Object(watchers));
    out.insert("budget".into(), json!({ "claudeRunsPerDay": schedule::budget_limit(config) }));
    out.insert(
        "scheduler".into(),
        json!({ "quietHours": quiet.map(|_| json!({ "start": js_trim(config["scheduler"]["quietHours"]["start"].as_str().unwrap_or("")), "end": js_trim(config["scheduler"]["quietHours"]["end"].as_str().unwrap_or("")) })) }),
    );
    out.insert("notify".into(), json!({ "minIntervalMinutes": crate::js_number::json_number(schedule::notify_min_interval_ms(config) / 60000.0) }));
    out.insert("digest".into(), json!({ "enabled": digest.enabled, "time": digest.time }));
    out.insert("prePush".into(), json!({ "mode": mode }));
    out.insert("similar".into(), json!({ "enabled": crate::similar::similar_enabled(config) }));
    for section in sections {
        out.insert(section.key().to_string(), section.public(config, saved_token_names));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Value {
        json!({ "port": 8787, "sharedSecret": "s3cret", "repos": {} })
    }

    fn fields(update: Value) -> Vec<String> {
        fields_with(update, &base())
    }

    fn fields_with(update: Value, current: &Value) -> Vec<String> {
        let mut errors = vec![];
        validate_background_settings(&update, current, &mut errors, &[]);
        let mut f: Vec<String> = errors.into_iter().map(|e| e.field).collect();
        f.sort();
        f
    }

    fn merged(saved: &Value, update: Value) -> (Value, Vec<TokenOp>) {
        let mut next = saved.as_object().unwrap().clone();
        let mut ops = vec![];
        merge_background_settings(&mut next, &update, &mut ops, &[]);
        (Value::Object(next), ops)
    }

    fn public(config: &Value) -> Map<String, Value> {
        public_background_settings(config, &[], &[])
    }

    #[test]
    fn out_of_range_unknown_and_wrongly_typed_background_values_are_refused() {
        assert_eq!(fields(json!({ "watchers": { "conflicts": { "intervalMinutes": 0 }, "builds": {} } })), ["watchers.builds", "watchers.conflicts.intervalMinutes"]);
        assert_eq!(fields(json!({ "watchers": { "conflicts": { "enabled": "yes" } } })), ["watchers.conflicts.enabled"]);
        assert_eq!(fields(json!({ "budget": { "claudeRunsPerDay": 51 } })), ["budget.claudeRunsPerDay"]);
        assert_eq!(fields(json!({ "scheduler": { "quietHours": { "start": "7pm", "end": "08:00" } } })), ["scheduler.quietHours"]);
        assert_eq!(fields(json!({ "scheduler": { "quietHours": { "start": "08:00", "end": "08:00" } } })), ["scheduler.quietHours"]);
        assert_eq!(fields(json!({ "scheduler": { "quietHours": { "start": "08:00", "end": "09:00", "tz": "x" } } })), ["scheduler.quietHours.tz"]);
        assert_eq!(fields(json!({ "scheduler": { "quietHours": "late" } })), ["scheduler.quietHours"]);
        assert_eq!(fields(json!({ "notify": { "minIntervalMinutes": -1 } })), ["notify.minIntervalMinutes"]);
        assert_eq!(fields(json!({ "digest": { "time": "8:30" } })), ["digest.time"]);
        assert_eq!(fields(json!({ "prePush": { "mode": "block" } })), ["prePush.mode"]);
        assert_eq!(fields(json!({ "budget": 5 })), ["budget"]);
        assert!(fields(json!({ "scheduler": { "quietHours": null }, "digest": { "enabled": true, "time": "09:15" } })).is_empty());
    }

    #[test]
    fn fractional_string_and_out_of_range_numbers_are_refused_where_a_whole_number_is_needed() {
        for bad in [json!(1.5), json!(0.5), json!("5"), json!(""), json!(null), json!(true), json!([5]), json!(1441)] {
            assert_eq!(fields(json!({ "watchers": { "conflicts": { "intervalMinutes": bad } } })), ["watchers.conflicts.intervalMinutes"], "{bad}");
        }
        for bad in [json!(2.5), json!("3"), json!(null), json!(false), json!(51), json!(-1)] {
            assert_eq!(fields(json!({ "budget": { "claudeRunsPerDay": bad } })), ["budget.claudeRunsPerDay"], "{bad}");
        }
        for bad in [json!(0.5), json!("30"), json!(null), json!(1441), json!(-1)] {
            assert_eq!(fields(json!({ "notify": { "minIntervalMinutes": bad } })), ["notify.minIntervalMinutes"], "{bad}");
        }
        assert!(fields(json!({ "watchers": { "conflicts": { "intervalMinutes": 1440 } }, "budget": { "claudeRunsPerDay": 50 }, "notify": { "minIntervalMinutes": 1440 } })).is_empty());
    }

    #[test]
    fn merge_merges_onto_saved_values_clears_back_to_defaults_and_can_clear_the_key() {
        let mut saved = base();
        saved["watchers"] = json!({ "conflicts": { "enabled": true, "intervalMinutes": 5 } });
        saved["digest"] = json!({ "enabled": true, "time": "07:00" });
        let (config, ops) = merged(&saved, json!({ "watchers": { "conflicts": { "enabled": false } }, "digest": { "time": "08:30" }, "scheduler": { "quietHours": null } }));
        assert_eq!(config["watchers"], json!({ "conflicts": { "intervalMinutes": 5 } }));
        assert_eq!(config["digest"], json!({ "enabled": true }));
        assert!(config.get("scheduler").is_none());
        assert!(ops.is_empty());
        let mut only = base();
        only["watchers"] = json!({ "reviewRequests": { "enabled": true } });
        let (off, _) = merged(&only, json!({ "watchers": { "reviewRequests": { "enabled": false } } }));
        assert!(off.get("watchers").is_none());
    }

    #[test]
    fn defaults_are_never_persisted_and_other_values_are() {
        let (c, _) = merged(&base(), json!({ "budget": { "claudeRunsPerDay": 5 }, "notify": { "minIntervalMinutes": 30 }, "prePush": { "mode": "warn" } }));
        for k in ["budget", "notify", "prePush"] {
            assert!(c.get(k).is_none(), "{k}");
        }
        let (c, _) = merged(&base(), json!({ "budget": { "claudeRunsPerDay": 0 }, "notify": { "minIntervalMinutes": 0 }, "prePush": { "mode": "off" }, "scheduler": { "quietHours": { "start": " 19:00 ", "end": "08:00" } }, "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 15 } } }));
        assert_eq!(c["budget"], json!({ "claudeRunsPerDay": 0 }));
        assert_eq!(c["notify"], json!({ "minIntervalMinutes": 0 }));
        assert_eq!(c["prePush"], json!({ "mode": "off" }));
        assert_eq!(c["scheduler"], json!({ "quietHours": { "start": "19:00", "end": "08:00" } }));
        assert_eq!(c["watchers"], json!({ "conflicts": { "enabled": true } }), "the default interval isn't saved");
    }

    #[test]
    fn similar_enabled_on_by_default_only_off_is_saved_and_only_a_boolean_is_accepted() {
        assert_eq!(public(&base())["similar"], json!({ "enabled": true }));
        let mut off_cfg = base();
        off_cfg["similar"] = json!({ "enabled": false });
        assert_eq!(public(&off_cfg)["similar"], json!({ "enabled": false }));
        assert!(fields(json!({ "similar": { "enabled": false } })).is_empty());
        assert_eq!(fields(json!({ "similar": { "enabled": "no" } })), ["similar.enabled"]);
        assert_eq!(fields(json!({ "similar": { "k": 3 } })), ["similar.k"]);
        assert_eq!(fields(json!({ "similar": true })), ["similar"]);
        let (off, _) = merged(&base(), json!({ "similar": { "enabled": false } }));
        assert_eq!(off["similar"], json!({ "enabled": false }));
        let (on, _) = merged(&off, json!({ "similar": { "enabled": true } }));
        assert!(on.get("similar").is_none());
    }

    #[test]
    fn similar_an_update_without_it_or_with_an_empty_object_leaves_the_saved_off_null_and_arrays_are_refused() {
        let mut saved = base();
        saved["similar"] = json!({ "enabled": false });
        assert_eq!(merged(&saved, json!({})).0["similar"], json!({ "enabled": false }));
        assert_eq!(merged(&saved, json!({ "similar": {} })).0["similar"], json!({ "enabled": false }));
        for bad in [json!(null), json!([]), json!([{ "enabled": false }])] {
            assert_eq!(fields(json!({ "similar": bad })), ["similar"]);
        }
    }

    #[test]
    fn the_public_view_shows_effective_values_never_the_key() {
        let mut cfg = base();
        cfg["scheduler"] = json!({ "quietHours": { "start": " 19:00 ", "end": "08:00" } });
        let p = public_background_settings(&cfg, &[], &[]);
        assert_eq!(p["scheduler"], json!({ "quietHours": { "start": "19:00", "end": "08:00" } }));
        assert_eq!(p["notify"], json!({ "minIntervalMinutes": 30 }));
        assert_eq!(p["budget"], json!({ "claudeRunsPerDay": 5 }));
        assert_eq!(p["digest"], json!({ "enabled": false, "time": "08:30" }));
        assert_eq!(p["prePush"], json!({ "mode": "warn" }));
        assert!(!serde_json::to_string(&p).unwrap().contains("sk-"));
        assert_eq!(p["watchers"]["assignedBugs"], json!({ "enabled": false, "intervalMinutes": 60 }));
        assert!(public(&base())["scheduler"]["quietHours"].is_null());
    }
}
