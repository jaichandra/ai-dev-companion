//! Packs: how features reach the framework (port of `core/packs.js`). A pack is
//! a named list of features; each feature carries everything the rest of the
//! service needs to know about it, so adding one means writing its own module
//! and listing it in a pack, not editing the registry, the server, the MCP
//! catalog, the scope-key rules, the job persistence list and the history
//! rules one by one.
//!
//! A pack can also contribute, beyond features: extra per-site settings, extra
//! `/targets` data, external token readers, a richer way to diagnose a failed
//! build, extra `doctor` lines, MCP tools a read-only run must never get, and
//! hints about MCP servers the issue analysis should prefer.
//!
//! Where the Node build `require`d pack files named by the profile's `packs`
//! list, a distribution here builds its [`Pack`] values in Rust and hands them
//! to [`PackRegistry::new`]. Because the compiler checks that every factory,
//! hook and flag has the right type, most of `definePack`'s "must be a
//! function" checks are gone; what remains is what the types can't say (id and
//! label shape, the extension script path, duplicates, tool ownership).
//!
//! The feature factory type `F` is a parameter, so this module doesn't depend
//! on the job layer that defines what a running feature is.

pub mod builtin;

use crate::site_urls::Prompter;
use crate::settings_background::{FieldError, TokenOp};
use serde_json::{Map, Value};
use std::sync::Arc;

/// "pr" is a pull request of the distribution's git host; "bitbucket-pr" is its old name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKeyKind {
    Pr,
    BitbucketPr,
    JiraIssue,
}

/// A scope-key rule written as code: the payload's key, or none.
pub type ScopeKeyFn = Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>;

/// How a job of a feature is tagged.
#[derive(Clone, Default)]
pub enum ScopeKey {
    /// Its jobs carry no scope key.
    #[default]
    None,
    Kind(ScopeKeyKind),
    Func(ScopeKeyFn),
}

/// `async promptSetup(rl, helpers, existingConfig)`: the config fragment to
/// shallow-merge into the written config.json. Only called for a feature the
/// user chose to enable.
pub type PromptSetup = Arc<dyn Fn(&mut dyn Prompter, &Value) -> Value + Send + Sync>;

/// A feature's setup/Settings-facing descriptor.
#[derive(Clone)]
pub struct FeatureDescriptor {
    /// Must match the extension feature file's id.
    pub id: String,
    /// Shown in the setup wizard's enable/disable prompt.
    pub label: String,
    pub description: String,
    /// A short line for the setup wizard's list; falls back to the description's first sentence.
    pub summary: Option<String>,
    /// `Some(false)` = in the menu list but off until the user opts in.
    pub enabled_by_default: Option<bool>,
    /// Hidden everywhere at once.
    pub disabled: bool,
    /// Names from the prerequisite checks; only the union across enabled features is run.
    pub required_checks: Vec<String>,
    /// Works on local clones mapped in `config.repos`.
    pub needs_repos: bool,
    pub prompt_setup: Option<PromptSetup>,
}

impl FeatureDescriptor {
    pub fn new(id: &str, label: &str) -> Self {
        FeatureDescriptor {
            id: id.into(),
            label: label.into(),
            description: String::new(),
            summary: None,
            enabled_by_default: None,
            disabled: false,
            required_checks: vec![],
            needs_repos: false,
            prompt_setup: None,
        }
    }
}

/// The content script (relative to the extension folder) that registers a
/// feature's row, and where in the menu it sits.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionSpec {
    pub script: String,
    pub order: Option<f64>,
}

/// How the history layer treats a feature's jobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryFlags {
    pub read_only: bool,
    pub milestone: bool,
    pub event_only: bool,
}

/// An MCP catalog entry a feature owns; it names that feature.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    pub name: String,
    pub feature_id: String,
    /// The rest of the catalog entry (`kind`, `description`, `params`, ...).
    pub def: Value,
}

#[derive(Clone)]
pub struct FeatureSpec<F> {
    pub descriptor: FeatureDescriptor,
    /// A thunk so setup and doctor, which run without the implementation,
    /// never need to build it.
    pub factory: F,
    pub mcp_tools: Vec<McpToolDef>,
    pub scope_key: ScopeKey,
    /// `Some(false)`: jobs are not written to disk (default: they are).
    pub persist: Option<bool>,
    pub extension: Option<ExtensionSpec>,
    pub history: HistoryFlags,
}

impl<F> FeatureSpec<F> {
    pub fn new(descriptor: FeatureDescriptor, factory: F) -> Self {
        FeatureSpec { descriptor, factory, mcp_tools: vec![], scope_key: ScopeKey::None, persist: None, extension: None, history: HistoryFlags::default() }
    }
}

/// `Ok(cleaned value)` or `Err(reason)`.
pub type Validate = Arc<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;
/// A config-to-value function.
pub type FromConfig = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

/// Extra keys on a site's settings: validated, saved and shown like the site's
/// own fields; a value equal to `default_value` is not saved.
#[derive(Clone)]
pub struct SiteSettingExtra {
    pub key: String,
    pub validate: Validate,
    pub default_value: Value,
    /// The effective value for a config.
    pub from: FromConfig,
}

/// A top-level config key a pack owns: how the Settings panel's update to it is validated, merged
/// into the saved config and shown back. Credentials it saves go through [`TokenOp`] (never into the
/// config), and the names it uses are listed in the profile's `credentialNames`.
pub trait SettingsSection: Send + Sync {
    /// The config key (`GET /settings` shows [`SettingsSection::public`] under it, and an update names it).
    fn key(&self) -> &str;
    /// Field errors for an update of the key (`update` is the key's own object).
    fn validate(&self, update: &Map<String, Value>, current: &Value, errors: &mut Vec<FieldError>);
    /// Applies an already-validated update (the whole settings update) onto the saved config `next`.
    fn merge(&self, next: &mut Map<String, Value>, update: &Value, token_ops: &mut Vec<TokenOp>);
    /// What the panel shows.
    fn public(&self, config: &Value, saved_token_names: &[String]) -> Value;
}

/// What an external-token source can read. Grows as the readers are ported.
pub struct ExternalTokenEnv {
    pub home: std::path::PathBuf,
    /// Where files are read from (`read_text` / `read_json` on the env use it).
    pub fs: Arc<dyn crate::external_tokens::TokenFs>,
}

pub type TokenSource = Arc<dyn Fn(&ExternalTokenEnv) -> Value + Send + Sync>;

/// Tokens another tool on this machine already holds, used only when none is
/// saved here and only for the host the entry's origin names.
#[derive(Clone)]
pub struct ExternalTokens {
    pub label: String,
    pub sources: Vec<TokenSource>,
}

/// A value-to-text function.
pub type FromConfigText = Arc<dyn Fn(&Value) -> String + Send + Sync>;

/// Builds the `claude` argv for a build URL.
pub type DiagnoseArgs = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// A richer way to diagnose a failed build than the plain prompt.
#[derive(Clone)]
pub struct Diagnose {
    pub available: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The argv for `claude`, given the build URL.
    pub args: DiagnoseArgs,
    pub summary: FromConfigText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorLine {
    pub ok: bool,
    pub message: String,
}

pub type DoctorCheck = Arc<dyn Fn(&Value) -> Option<DoctorLine> + Send + Sync>;

/// A user-scope MCP server the issue analysis should lean on when configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHint {
    pub server: String,
    pub when_available: String,
    pub when_missing: String,
}

/// A content script block the extension loads on pages that are not a site's (e.g. a local app a
/// pack starts): match patterns and the scripts (paths inside the extension folder).
#[derive(Clone, Debug, PartialEq)]
pub struct PageScript {
    pub matches: Vec<String>,
    pub js: Vec<String>,
}

#[derive(Clone)]
pub struct Pack<F> {
    pub id: String,
    /// Top-level config keys this pack's features own, shown and edited in the Settings panel.
    pub settings_sections: Vec<Arc<dyn SettingsSection>>,
    /// Builds the LLM the service asks for triage and embeddings (the first pack that has one wins).
    pub llm: Option<crate::llm::LlmFactory>,
    /// Extension scripts that aren't a feature's (a Settings section's UI): loaded with the features',
    /// in `order`.
    pub extension_scripts: Vec<ExtensionSpec>,
    pub features: Vec<FeatureSpec<F>>,
    /// siteId -> its extra settings keys.
    pub site_settings: Vec<(String, Vec<SiteSettingExtra>)>,
    /// `(config) -> object merged into GET /targets for the extension`.
    pub targets: Option<FromConfig>,
    pub external_tokens: Option<ExternalTokens>,
    pub diagnose: Option<Diagnose>,
    pub doctor_checks: Vec<DoctorCheck>,
    pub mcp_deny_list: Vec<String>,
    pub analysis_server_hints: Vec<ServerHint>,
    pub job_addons: Vec<Arc<dyn crate::addons::JobAddon>>,
    pub page_scripts: Vec<PageScript>,
}

impl<F> Pack<F> {
    pub fn new(id: &str, features: Vec<FeatureSpec<F>>) -> Self {
        Pack {
            id: id.into(),
            settings_sections: vec![],
            llm: None,
            extension_scripts: vec![],
            features,
            site_settings: vec![],
            targets: None,
            external_tokens: None,
            diagnose: None,
            doctor_checks: vec![],
            mcp_deny_list: vec![],
            analysis_server_hints: vec![],
            job_addons: vec![],
            page_scripts: vec![],
        }
    }
}

fn fail<T>(where_: &str, message: impl AsRef<str>) -> Result<T, String> {
    let at = if where_.is_empty() { String::new() } else { format!(" ({where_})") };
    Err(format!("Invalid pack{at}: {}", message.as_ref()))
}

/// `^[a-z][a-z0-9-]*$`
fn is_feature_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty() && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `^[A-Za-z0-9._-]+(\/[A-Za-z0-9._-]+)*\.js$` with no `..` segment.
fn is_extension_script(s: &str) -> bool {
    let segment_ok = |seg: &str| !seg.is_empty() && seg.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    s.ends_with(".js") && s.split('/').all(segment_ok) && !s.split('/').any(|seg| seg == "..")
}

fn check_feature<F>(pack_id: &str, spec: &FeatureSpec<F>) -> Result<(), String> {
    let id = &spec.descriptor.id;
    if !is_feature_id(id) {
        return fail(pack_id, format!("a feature needs descriptor.id (lowercase, dashes), got {}", serde_json::to_string(id).unwrap_or_default()));
    }
    let at = format!("{pack_id}/{id}");
    if spec.descriptor.label.is_empty() {
        return fail(&at, "descriptor.label is required");
    }
    if let Some(ext) = &spec.extension {
        if !is_extension_script(&ext.script) {
            return fail(&at, "extension.script must be a relative .js path inside the extension folder");
        }
    }
    for tool in &spec.mcp_tools {
        if tool.name.is_empty() {
            return fail(&at, "every mcpTools entry needs a name");
        }
        if tool.feature_id != *id {
            return fail(&at, format!("MCP tool {} must name this feature (featureId: \"{id}\"), not {}", tool.name, serde_json::to_string(&tool.feature_id).unwrap_or_default()));
        }
    }
    Ok(())
}

/// Validates a pack and returns it. An error on a duplicate feature id inside it.
pub fn define_pack<F>(pack: Pack<F>) -> Result<Pack<F>, String> {
    if pack.id.is_empty() {
        return fail("", "a pack needs an id");
    }
    if let Some(t) = &pack.external_tokens {
        if t.label.is_empty() && t.sources.is_empty() {
            return fail(&pack.id, "externalTokens needs a label and a list of source functions");
        }
    }
    if pack.mcp_deny_list.iter().any(String::is_empty) {
        return fail(&pack.id, "mcpDenyList must be a list of tool names");
    }
    if pack.analysis_server_hints.iter().any(|h| h.server.is_empty()) {
        return fail(&pack.id, "analysisServerHints must be a list of { server, whenAvailable, whenMissing }");
    }
    let mut keys: Vec<&str> = Vec::new();
    for section in &pack.settings_sections {
        let key = section.key();
        if key.is_empty() || keys.contains(&key) {
            return fail(&pack.id, format!("settings section \"{key}\" needs a unique, non-empty key"));
        }
        keys.push(key);
    }
    for ext in &pack.extension_scripts {
        if !is_extension_script(&ext.script) {
            return fail(&pack.id, "extension_scripts must be relative .js paths inside the extension folder");
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for spec in &pack.features {
        check_feature(&pack.id, spec)?;
        if seen.contains(&spec.descriptor.id.as_str()) {
            return fail(&pack.id, format!("feature \"{}\" is listed twice", spec.descriptor.id));
        }
        seen.push(&spec.descriptor.id);
    }
    let mut seen_addons = std::collections::HashSet::new();
    for a in &pack.job_addons {
        if !seen_addons.insert(a.id().to_string()) {
            return fail(&pack.id, format!("two add-ons share the id {}", a.id()));
        }
    }
    for s in &pack.page_scripts {
        if s.matches.is_empty() || s.js.is_empty() || !s.js.iter().all(|j| is_extension_script(j)) {
            return fail(&pack.id, "a pageScripts entry needs matches and relative .js paths inside the extension folder");
        }
    }
    Ok(pack)
}

/// The packs, with feature ids unique across all of them.
pub fn merge_packs<F>(packs: Vec<Pack<F>>) -> Result<Vec<Pack<F>>, String> {
    let mut owner: Vec<(String, String)> = Vec::new();
    for pack in &packs {
        for spec in &pack.features {
            let id = &spec.descriptor.id;
            if let Some((_, first)) = owner.iter().find(|(i, _)| i == id) {
                return fail(&pack.id, format!("feature \"{id}\" is already provided by pack \"{first}\""));
            }
            owner.push((id.clone(), pack.id.clone()));
        }
    }
    Ok(packs)
}

/// A job's scope-key rule for a feature, as the caller's named rules resolve it.
pub type KindRules<'a> = &'a dyn Fn(ScopeKeyKind, &Value) -> Option<String>;

/// The validated, merged packs of a distribution, and everything derived from
/// them.
pub struct PackRegistry<F> {
    packs: Vec<Pack<F>>,
}

impl<F> PackRegistry<F> {
    /// Validates every pack and merges them.
    pub fn new(packs: Vec<Pack<F>>) -> Result<Self, String> {
        let defined = packs.into_iter().map(define_pack).collect::<Result<Vec<_>, _>>()?;
        Ok(PackRegistry { packs: merge_packs(defined)? })
    }

    pub fn packs(&self) -> &[Pack<F>] {
        &self.packs
    }

    pub fn job_addons(&self) -> Vec<Arc<dyn crate::addons::JobAddon>> {
        self.packs.iter().flat_map(|p| p.job_addons.iter().cloned()).collect()
    }

    pub fn page_scripts(&self) -> Vec<&PageScript> {
        self.packs.iter().flat_map(|p| p.page_scripts.iter()).collect()
    }

    /// Every feature spec, in pack order then declared order.
    pub fn features(&self) -> impl Iterator<Item = &FeatureSpec<F>> {
        self.packs.iter().flat_map(|p| p.features.iter())
    }

    pub fn feature_spec(&self, id: &str) -> Option<&FeatureSpec<F>> {
        self.features().find(|s| s.descriptor.id == id)
    }

    /// Every pack's Settings sections, in pack order.
    pub fn settings_sections(&self) -> Vec<&Arc<dyn SettingsSection>> {
        self.packs.iter().flat_map(|p| p.settings_sections.iter()).collect()
    }

    /// The first pack's LLM factory, if any pack has one.
    pub fn llm_factory(&self) -> Option<&crate::llm::LlmFactory> {
        self.packs.iter().find_map(|p| p.llm.as_ref())
    }

    /// Extension scripts the packs add besides their features' own.
    pub fn extension_scripts(&self) -> impl Iterator<Item = &ExtensionSpec> {
        self.packs.iter().flat_map(|p| p.extension_scripts.iter())
    }

    /// The MCP catalog entries of every feature, in feature order.
    pub fn mcp_tool_defs(&self) -> Vec<&McpToolDef> {
        self.features().flat_map(|s| s.mcp_tools.iter()).collect()
    }

    /// The extra settings keys packs add to site `site_id`.
    pub fn site_setting_extras(&self, site_id: &str) -> Vec<&SiteSettingExtra> {
        self.packs.iter().flat_map(|p| p.site_settings.iter().filter(|(s, _)| s == site_id).flat_map(|(_, e)| e.iter())).collect()
    }

    /// What the packs add to GET /targets for this config (later packs win on a clash).
    pub fn targets_for(&self, config: &Value) -> Value {
        let mut out = serde_json::Map::new();
        for t in self.packs.iter().filter_map(|p| p.targets.as_ref()) {
            if let Value::Object(m) = t(config) {
                out.extend(m);
            }
        }
        Value::Object(out)
    }

    pub fn external_token_sources(&self) -> Vec<&TokenSource> {
        self.packs.iter().filter_map(|p| p.external_tokens.as_ref()).flat_map(|t| t.sources.iter()).collect()
    }

    /// What to call the tool whose tokens are borrowed, for messages.
    pub fn external_token_label(&self) -> &str {
        self.packs.iter().find_map(|p| p.external_tokens.as_ref()).map_or("another tool on this machine", |t| t.label.as_str())
    }

    /// The first pack-provided way to diagnose a build that is usable right now.
    pub fn diagnose_command(&self) -> Option<&Diagnose> {
        self.packs.iter().filter_map(|p| p.diagnose.as_ref()).find(|d| (d.available)())
    }

    pub fn doctor_checks(&self) -> Vec<&DoctorCheck> {
        self.packs.iter().flat_map(|p| p.doctor_checks.iter()).collect()
    }

    /// MCP tool names the packs say a read-only run must be denied.
    pub fn disallowed_mcp_tools(&self) -> Vec<&str> {
        self.packs.iter().flat_map(|p| p.mcp_deny_list.iter().map(String::as_str)).collect()
    }

    pub fn analysis_server_hints(&self) -> Vec<&ServerHint> {
        self.packs.iter().flat_map(|p| p.analysis_server_hints.iter()).collect()
    }

    /// Ids of the features whose spec says the `flag` history flag.
    pub fn ids_with_history_flag(&self, flag: fn(&HistoryFlags) -> bool) -> Vec<&str> {
        self.features().filter(|s| flag(&s.history)).map(|s| s.descriptor.id.as_str()).collect()
    }

    /// Ids of the features whose jobs are not written to disk.
    pub fn not_persisted_ids(&self) -> Vec<&str> {
        self.features().filter(|s| s.persist == Some(false)).map(|s| s.descriptor.id.as_str()).collect()
    }

    /// The scope key of a job of `feature_id` started with `payload`, or `None`
    /// when the feature has no rule. `kinds` maps the named rules to their
    /// implementations.
    pub fn scope_key_for(&self, feature_id: &str, payload: &Value, kinds: KindRules<'_>) -> Option<String> {
        match &self.feature_spec(feature_id)?.scope_key {
            ScopeKey::None => None,
            ScopeKey::Func(f) => f(payload),
            ScopeKey::Kind(k) => kinds(*k, payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type P = Pack<()>;

    fn feature(id: &str) -> FeatureSpec<()> {
        FeatureSpec::new(FeatureDescriptor::new(id, id), ())
    }

    fn with(id: &str, f: impl FnOnce(&mut FeatureSpec<()>)) -> FeatureSpec<()> {
        let mut s = feature(id);
        f(&mut s);
        s
    }

    fn err(p: P) -> String {
        define_pack(p).err().expect("expected an error")
    }

    #[test]
    fn define_pack_accepts_a_well_formed_pack_and_returns_it() {
        let pack = P::new("p", vec![feature("one"), with("two", |s| {
            s.scope_key = ScopeKey::Kind(ScopeKeyKind::JiraIssue);
            s.persist = Some(false);
        })]);
        let defined = define_pack(pack).unwrap();
        assert_eq!(defined.features.len(), 2);
    }

    #[test]
    fn define_pack_names_the_pack_and_feature_in_every_complaint() {
        assert!(err(P::new("", vec![])).contains("a pack needs an id"));
        assert_eq!(err(P::new("", vec![])), "Invalid pack: a pack needs an id");
        assert_eq!(err(P::new("p", vec![feature("")])), "Invalid pack (p): a feature needs descriptor.id (lowercase, dashes), got \"\"");
        assert!(err(P::new("p", vec![feature("Bad_Id")])).contains("needs descriptor.id") && err(P::new("p", vec![feature("Bad_Id")])).contains("\"Bad_Id\""));
        assert!(err(P::new("p", vec![feature("1abc")])).contains("needs descriptor.id"));
        let unlabeled = FeatureSpec::new(FeatureDescriptor::new("a", ""), ());
        assert_eq!(err(P::new("p", vec![unlabeled])), "Invalid pack (p/a): descriptor.label is required");
    }

    #[test]
    fn define_pack_refuses_a_feature_listed_twice_and_a_tool_that_names_another_feature() {
        assert_eq!(err(P::new("p", vec![feature("a"), feature("a")])), "Invalid pack (p): feature \"a\" is listed twice");
        let stray = with("a", |s| s.mcp_tools = vec![McpToolDef { name: "t".into(), feature_id: "b".into(), def: json!({}) }]);
        assert!(err(P::new("p", vec![stray])).contains("MCP tool t must name this feature (featureId: \"a\"), not \"b\""));
        let nameless = with("a", |s| s.mcp_tools = vec![McpToolDef { name: String::new(), feature_id: "a".into(), def: json!({}) }]);
        assert!(err(P::new("p", vec![nameless])).contains("every mcpTools entry needs a name"));
        let own = with("a", |s| s.mcp_tools = vec![McpToolDef { name: "t".into(), feature_id: "a".into(), def: json!({}) }]);
        assert!(define_pack(P::new("p", vec![own])).is_ok());
    }

    #[test]
    fn merge_packs_refuses_a_feature_id_two_packs_both_provide() {
        let one = define_pack(P::new("one", vec![feature("shared")])).unwrap();
        let two = define_pack(P::new("two", vec![feature("shared")])).unwrap();
        let e = merge_packs(vec![one.clone(), two]).err().unwrap();
        assert_eq!(e, "Invalid pack (two): feature \"shared\" is already provided by pack \"one\"");
        assert_eq!(merge_packs(vec![one]).unwrap().len(), 1);
        assert!(PackRegistry::new(vec![P::new("one", vec![feature("a")]), P::new("two", vec![feature("a")])]).is_err());
    }

    #[test]
    fn define_pack_checks_a_features_extension_script() {
        let ext = |script: &str, order: Option<f64>| with("a", |s| s.extension = Some(ExtensionSpec { script: script.into(), order }));
        assert!(define_pack(P::new("p", vec![ext("features/a.js", Some(5.0))])).is_ok());
        for bad in ["../a.js", "/etc/a.js", "a.txt", "features/../a.js", "", "a b.js", "features//a.js", ".js/x"] {
            let e = err(P::new("p", vec![ext(bad, None)]));
            assert!(e.contains("extension.script must be a relative .js path"), "{bad}: {e}");
        }
        // `./a.js` passes the same pattern the Node build used (a lone dot is a legal segment).
        assert!(define_pack(P::new("p", vec![ext("./a.js", None)])).is_ok());
    }

    #[test]
    fn a_page_script_needs_matches_and_a_relative_js_path() {
        let mut p = Pack::<()>::new("t", vec![]);
        p.page_scripts = vec![PageScript { matches: vec!["http://*.localhost/*".into()], js: vec!["../x.js".into()] }];
        assert!(err(p).contains("pageScripts"));
    }

    #[test]
    fn define_pack_checks_the_hooks_a_pack_contributes() {
        let mut p = P::new("p", vec![]);
        p.mcp_deny_list = vec!["mcp__x__y".into()];
        p.analysis_server_hints = vec![ServerHint { server: "wiki".into(), when_available: "a".into(), when_missing: "b".into() }];
        p.external_tokens = Some(ExternalTokens { label: "x".into(), sources: vec![Arc::new(|_| json!({}))] });
        assert!(define_pack(p).is_ok());
        let mut bad = P::new("p", vec![]);
        bad.mcp_deny_list = vec![String::new()];
        assert!(err(bad).contains("mcpDenyList must be a list of tool names"));
        let mut bad = P::new("p", vec![]);
        bad.analysis_server_hints = vec![ServerHint { server: String::new(), when_available: "a".into(), when_missing: "b".into() }];
        assert!(err(bad).contains("analysisServerHints must be a list"));
        let mut bad = P::new("p", vec![]);
        bad.external_tokens = Some(ExternalTokens { label: String::new(), sources: vec![] });
        assert!(err(bad).contains("externalTokens needs a label"));
    }

    fn registry() -> PackRegistry<()> {
        let mut a = P::new(
            "a",
            vec![
                with("one", |s| {
                    s.scope_key = ScopeKey::Kind(ScopeKeyKind::Pr);
                    s.history = HistoryFlags { read_only: true, ..Default::default() };
                    s.mcp_tools = vec![McpToolDef { name: "t1".into(), feature_id: "one".into(), def: json!({ "kind": "read" }) }];
                }),
                with("two", |s| {
                    s.scope_key = ScopeKey::Func(Arc::new(|p| p.get("k").and_then(Value::as_str).map(str::to_string)));
                    s.persist = Some(false);
                    s.history = HistoryFlags { event_only: true, ..Default::default() };
                }),
                feature("three"),
            ],
        );
        a.site_settings = vec![("jenkins".into(), vec![SiteSettingExtra { key: "k".into(), validate: Arc::new(|v| Ok(v.clone())), default_value: json!(1), from: Arc::new(|_| json!(1)) }])];
        a.targets = Some(Arc::new(|_| json!({ "x": 1, "shared": "a" })));
        a.mcp_deny_list = vec!["deny-a".into()];
        let mut b = P::new("b", vec![with("four", |s| s.history = HistoryFlags { read_only: true, milestone: true, ..Default::default() })]);
        b.targets = Some(Arc::new(|c| json!({ "y": c["n"], "shared": "b" })));
        b.mcp_deny_list = vec!["deny-b".into()];
        b.external_tokens = Some(ExternalTokens { label: "Tool B".into(), sources: vec![Arc::new(|_| json!({})), Arc::new(|_| json!({}))] });
        b.diagnose = Some(Diagnose { available: Arc::new(|| true), args: Arc::new(|u| vec!["-p".into(), u.into()]), summary: Arc::new(|_| "s".into()) });
        b.doctor_checks = vec![Arc::new(|_| Some(DoctorLine { ok: true, message: "fine".into() }))];
        b.analysis_server_hints = vec![ServerHint { server: "wiki".into(), when_available: "a".into(), when_missing: "b".into() }];
        PackRegistry::new(vec![a, b]).unwrap()
    }

    #[test]
    fn the_registry_lists_features_in_pack_order_then_declared_order() {
        let r = registry();
        assert_eq!(r.features().map(|s| s.descriptor.id.as_str()).collect::<Vec<_>>(), ["one", "two", "three", "four"]);
        assert_eq!(r.feature_spec("three").unwrap().descriptor.id, "three");
        assert!(r.feature_spec("nope").is_none());
        assert_eq!(r.mcp_tool_defs().iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["t1"]);
        assert_eq!(r.packs().len(), 2);
    }

    #[test]
    fn history_and_persistence_flags_are_read_off_the_specs() {
        let r = registry();
        assert_eq!(r.ids_with_history_flag(|h| h.read_only), ["one", "four"]);
        assert_eq!(r.ids_with_history_flag(|h| h.event_only), ["two"]);
        assert_eq!(r.ids_with_history_flag(|h| h.milestone), ["four"]);
        assert_eq!(r.not_persisted_ids(), ["two"]);
    }

    #[test]
    fn scope_keys_follow_each_features_declared_rule() {
        let r = registry();
        let kinds = |k: ScopeKeyKind, _: &Value| Some(match k {
            ScopeKeyKind::Pr | ScopeKeyKind::BitbucketPr => "pr-key".to_string(),
            ScopeKeyKind::JiraIssue => "jira-key".to_string(),
        });
        assert_eq!(r.scope_key_for("one", &json!({}), &kinds).as_deref(), Some("pr-key"));
        assert_eq!(r.scope_key_for("two", &json!({ "k": "custom" }), &kinds).as_deref(), Some("custom"));
        assert_eq!(r.scope_key_for("two", &json!({}), &kinds), None);
        assert_eq!(r.scope_key_for("three", &json!({}), &kinds), None, "no rule, no key");
        assert_eq!(r.scope_key_for("not-a-feature", &json!({}), &kinds), None);
    }

    #[test]
    fn pack_hooks_are_collected_across_packs() {
        let r = registry();
        assert_eq!(r.site_setting_extras("jenkins").len(), 1);
        assert!(r.site_setting_extras("jira").is_empty());
        assert_eq!(r.targets_for(&json!({ "n": 7 })), json!({ "x": 1, "y": 7, "shared": "b" }), "later packs win");
        assert_eq!(r.external_token_sources().len(), 2);
        assert_eq!(r.external_token_label(), "Tool B");
        assert_eq!((r.diagnose_command().unwrap().args)("u"), ["-p", "u"]);
        assert_eq!(r.doctor_checks().len(), 1);
        assert_eq!((r.doctor_checks()[0])(&json!({})), Some(DoctorLine { ok: true, message: "fine".into() }));
        assert_eq!(r.disallowed_mcp_tools(), ["deny-a", "deny-b"]);
        assert_eq!(r.analysis_server_hints()[0].server, "wiki");
        let bare = PackRegistry::new(vec![P::new("x", vec![feature("a")])]).unwrap();
        assert_eq!(bare.external_token_label(), "another tool on this machine");
        assert!(bare.diagnose_command().is_none() && bare.targets_for(&json!({})) == json!({}));
    }

    #[test]
    fn a_diagnose_hook_that_is_not_available_is_skipped_for_the_next_pack() {
        let mut a = P::new("a", vec![]);
        a.diagnose = Some(Diagnose { available: Arc::new(|| false), args: Arc::new(|_| vec!["a".into()]), summary: Arc::new(|_| String::new()) });
        let mut b = P::new("b", vec![]);
        b.diagnose = Some(Diagnose { available: Arc::new(|| true), args: Arc::new(|_| vec!["b".into()]), summary: Arc::new(|_| String::new()) });
        let r = PackRegistry::new(vec![a, b]).unwrap();
        assert_eq!((r.diagnose_command().unwrap().args)(""), ["b"]);
    }
}
