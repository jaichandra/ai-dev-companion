//! The setup wizard's steps (port of `companion-service/setup.js`), split into pure pieces that
//! are driven through an injected [`LineInput`] so they can run headless: feature selection, the
//! prerequisite check loop, repo discovery, each feature's own questions, the config a run writes,
//! token migration, MCP registration, and waiting for the extension to connect.
//!
//! Not here (it is the distribution `main`'s job, since it needs the real terminal, `launchd`,
//! Chrome and the build): the build/start/verify of the service, the Chrome walk-through, and the
//! background review-worktree provisioning. Those only call the pieces below.

use crate::credential_store::CredentialStore;
use crate::feature_registry::FeatureRegistry;
use crate::js_text::js_trim;
use crate::mcp_registration::{ensure_mcp_token_default, register_everywhere, McpTokenStore, Registrations, SpawnSync};
use crate::node_path;
use crate::prereqs::{check_repo_path, default_search_roots, discover_local_clones, Check, GitRemoteRules};
use crate::service_control::ExtensionStatus;
use crate::settings::{migrate_tokens_to_store, write_config_file};
use crate::site_urls::Prompter;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub const DEFAULT_PORT: u16 = 8787;

/// How a failed prerequisite is fixed, by check name.
pub fn fix_hint(check: &str) -> &'static str {
    match check {
        "git" => "Install git \u{2014} on macOS run `xcode-select --install` \u{2014} then come back here.",
        "claudeCli" => "Install Claude Code (https://claude.com/claude-code), then come back here.",
        "claudeAuth" => "In another terminal run `claude auth login` and finish logging in, then come back here.",
        _ => "Fix it, then come back here.",
    }
}

/// Where the wizard's answers come from: one line per question; `None` is end of input (a closed
/// terminal), which every question treats as "accept the default".
pub trait LineInput {
    fn read_line(&mut self, prompt: &str) -> Option<String>;
}

/// The wizard's console: input, and where its output lines go.
pub struct Wizard<I: LineInput> {
    pub input: I,
    out: Box<dyn FnMut(&str) + Send>,
}

impl<I: LineInput> Wizard<I> {
    pub fn new(input: I, out: impl FnMut(&str) + Send + 'static) -> Self {
        Self { input, out: Box::new(out) }
    }

    pub fn say(&mut self, line: &str) {
        (self.out)(line);
    }

    /// `question [default]: `, answered with the trimmed line or the default.
    pub fn ask(&mut self, question: &str, default: &str) -> String {
        let suffix = if default.is_empty() { String::new() } else { format!(" [{default}]") };
        let answer = self.input.read_line(&format!("{question}{suffix}: ")).map(|a| js_trim(&a).to_string()).unwrap_or_default();
        if answer.is_empty() { default.to_string() } else { answer }
    }

    /// `question (Y/n): `: the capital letter shows the default, so no `[y]` suffix.
    pub fn confirm(&mut self, question: &str, default_yes: bool) -> bool {
        let answer = self.input.read_line(&format!("{question} ({}): ", if default_yes { "Y/n" } else { "y/N" })).map(|a| js_trim(&a).to_string()).unwrap_or_default();
        if answer.is_empty() { default_yes } else { answer.starts_with(['y', 'Y']) }
    }

    /// Numbered choice: accepts the number or the option's own id, the default on a blank answer,
    /// and asks again on anything else (a typo must never land in config.json). `options` is
    /// `(id, label)`; returns the chosen id.
    pub fn select(&mut self, question: &str, options: &[(&str, &str)], default: &str) -> String {
        self.say(question);
        for (i, (id, label)) in options.iter().enumerate() {
            self.say(&format!("  {}) {label}{}", i + 1, if *id == default { " (default)" } else { "" }));
        }
        let default_index = options.iter().position(|(id, _)| *id == default);
        let default_answer = (default_index.map_or(1, |i| i + 1)).to_string();
        loop {
            let answer = self.ask("Enter a number", &default_answer);
            let by_number = answer.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| options.get(i));
            let by_id = options.iter().find(|(id, _)| *id == js_trim(&answer));
            if let Some((id, _)) = by_number.or(by_id) {
                return (*id).to_string();
            }
            let ids: Vec<&str> = options.iter().map(|(id, _)| *id).collect();
            self.say(&format!("  Please enter a number from 1 to {}, or one of: {}.", options.len(), ids.join(", ")));
        }
    }

    /// Asks a [`parse_number_list`] question until the answer is valid; `default_indices` is what a
    /// blank answer means.
    pub fn ask_number_list(&mut self, question: &str, count: usize, default_indices: &[usize]) -> Vec<usize> {
        let default_text = if default_indices.len() == count {
            "all".to_string()
        } else if default_indices.is_empty() {
            "none".to_string()
        } else {
            default_indices.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(",")
        };
        loop {
            let answer = self.ask(question, &default_text);
            if let Some(indices) = parse_number_list(&answer, count) {
                return indices;
            }
            self.say(&format!("  Please enter \"all\", \"none\", or numbers from 1 to {count} (e.g. 1,3)."));
        }
    }
}

impl<I: LineInput> Prompter for Wizard<I> {
    fn ask(&mut self, question: &str, default: &str) -> String {
        Wizard::ask(self, question, default)
    }
    fn log(&mut self, line: &str) {
        self.say(line);
    }
}

/// The real terminal: prompts on stdout, reads stdin.
pub struct StdinInput;

impl LineInput for StdinInput {
    fn read_line(&mut self, prompt: &str) -> Option<String> {
        use std::io::Write;
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

/// "all", "none", or numbers separated by commas/spaces ("1,3" / "1 3") over `count` numbered items:
/// the chosen 0-based indices in ascending order, or `None` if the answer isn't valid.
pub fn parse_number_list(answer: &str, count: usize) -> Option<Vec<usize>> {
    let text = js_trim(answer).to_lowercase();
    if text == "all" {
        return Some((0..count).collect());
    }
    if text == "none" {
        return Some(vec![]);
    }
    let parts: Vec<&str> = text.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    let mut indices = std::collections::BTreeSet::new();
    for part in parts {
        let n: usize = part.parse().ok()?;
        if n < 1 || n > count {
            return None;
        }
        indices.insert(n - 1);
    }
    Some(indices.into_iter().collect())
}

/// The connection file the service worker loads.
pub fn render_extension_config(secret: &str, port: u16, context_menu_matches: &[String]) -> String {
    crate::extension_manifest::render_extension_config(secret, port, context_menu_matches)
}

/// The parsed config if `path` holds a usable one (a numeric `port` and a string `sharedSecret`),
/// else `None`: an unreadable or malformed file counts as "none yet".
pub fn load_existing_config(path: &Path) -> Option<Value> {
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (config["port"].is_number() && config["sharedSecret"].is_string()).then_some(config)
}

/// A description's first sentence, cut to fit one terminal line.
pub fn one_line(text: &str, max: usize) -> String {
    let first = text.split(". ").next().unwrap_or("");
    let first = first.strip_suffix('.').unwrap_or(first);
    let chars: Vec<char> = first.chars().collect();
    if chars.len() <= max { first.to_string() } else { format!("{}\u{2026}", chars[..max - 1].iter().collect::<String>().trim_end()) }
}

fn refs(ids: &[String]) -> Vec<&str> {
    ids.iter().map(String::as_str).collect()
}

/// Lists every feature once, then takes the whole selection in a single answer ("all", "none", or
/// numbers). A blank answer keeps `currently_enabled`. Returns the selected ids, in registry order.
pub fn prompt_feature_selection<I: LineInput>(wiz: &mut Wizard<I>, registry: &FeatureRegistry, currently_enabled: &[String]) -> Vec<String> {
    let descriptors = registry.descriptors();
    for (i, d) in descriptors.iter().enumerate() {
        wiz.say(&format!("  {:>2}) {}", i + 1, d.label));
        wiz.say(&format!("      {}", d.summary.clone().unwrap_or_else(|| one_line(&d.description, 90))));
    }
    wiz.say("");
    let defaults: Vec<usize> = descriptors.iter().enumerate().filter(|(_, d)| currently_enabled.contains(&d.id)).map(|(i, _)| i).collect();
    let indices = wiz.ask_number_list("Which features do you want? (\"all\", \"none\", or numbers like 1,3)", descriptors.len(), &defaults);
    let enabled: Vec<String> = indices.iter().map(|i| descriptors[*i].id.clone()).collect();
    if enabled.is_empty() {
        wiz.say("\n\u{26a0} No features enabled \u{2014} the \u{2728} button won't offer anything until you re-run setup and pick some.");
    }
    enabled
}

/// Runs the checks the selected features need. For each failure, explains how to fix it and
/// waits: Enter re-checks (so it can be fixed in another terminal), "skip" continues without the
/// features that need it. `checks` runs a check by name. Returns the ids still selected once
/// everything left passes.
pub fn resolve_prerequisites<I: LineInput>(wiz: &mut Wizard<I>, registry: &FeatureRegistry, selected: &[String], checks: &dyn Fn(&str) -> Check) -> Vec<String> {
    let mut remaining: Vec<String> = selected.to_vec();
    let names = registry.required_checks_for(&refs(&remaining));
    if names.is_empty() {
        wiz.say("  Nothing to check \u{2014} the selected features need nothing installed.");
        return remaining;
    }
    for name in names {
        if !registry.required_checks_for(&refs(&remaining)).contains(&name) {
            continue;
        }
        loop {
            let result = checks(&name);
            let shown = if result.ok { result.message.strip_suffix(" OK.").unwrap_or(&result.message).to_string() } else { result.message.clone() };
            wiz.say(&format!("  {} - {shown}", if result.ok { "OK  " } else { "FAIL" }));
            if result.ok {
                break;
            }
            let needing: Vec<(String, String)> = registry.features_requiring(&name, &refs(&remaining)).iter().map(|d| (d.id.clone(), d.label.clone())).collect();
            let labels = needing.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join(", ");
            wiz.say(&format!("         Needed by: {labels}"));
            wiz.say(&format!("         {}", fix_hint(&name)));
            let answer = wiz.ask("         Press Enter to check again, or type \"skip\" to continue without those features", "");
            if answer.starts_with(['s', 'S']) {
                remaining.retain(|id| !needing.iter().any(|(n, _)| n == id));
                wiz.say(&format!("  Skipped: {labels} (re-run setup later to enable)."));
                break;
            }
        }
    }
    remaining
}

/// The folders searched for clones: the common ones plus the folders the mapped repos already live in.
pub fn discovery_roots(existing: &Map<String, Value>, home: &Path) -> Vec<PathBuf> {
    let mut roots = default_search_roots(home);
    for p in existing.values().filter_map(Value::as_str) {
        if let Some(parent) = Path::new(p).parent().map(Path::to_path_buf) {
            if !roots.contains(&parent) {
                roots.push(parent);
            }
        }
    }
    roots
}

/// `existing` (never mutated) plus every clone found in `roots`, keyed from its origin URL, and the
/// keys that were new. Nothing is asked: the map is only a cache. An existing mapping for the
/// same key (any case) or the same folder is kept as it is.
pub fn discover_repos(existing: &Map<String, Value>, roots: &[PathBuf], rules: GitRemoteRules) -> (Map<String, Value>, Vec<String>) {
    let mut repos = existing.clone();
    let mut mapped_keys: HashSet<String> = repos.keys().map(|k| k.to_lowercase()).collect();
    let mut mapped_paths: HashSet<String> = repos.values().filter_map(Value::as_str).map(node_path::resolve).collect();
    let mut added = vec![];
    for clone in discover_local_clones(roots, rules) {
        if mapped_keys.contains(&clone.key.to_lowercase()) || mapped_paths.contains(&node_path::resolve(&clone.path)) {
            continue;
        }
        if !check_repo_path(&clone.path).ok {
            continue;
        }
        repos.insert(clone.key.clone(), json!(clone.path));
        mapped_keys.insert(clone.key.to_lowercase());
        mapped_paths.insert(node_path::resolve(&clone.path));
        added.push(clone.key);
    }
    (repos, added)
}

/// What a feature's `prompt_setup` sees: a feature's name is printed only when it actually says or
/// asks something, and a message several features share is said once.
struct FeatureHelpers<'a, I: LineInput> {
    wiz: &'a mut Wizard<I>,
    label: &'a str,
    said: &'a mut HashSet<String>,
    titled: bool,
}

impl<I: LineInput> FeatureHelpers<'_, I> {
    fn title(&mut self) {
        if !self.titled {
            self.titled = true;
            let line = format!("{}:", self.label);
            self.wiz.say(&line);
        }
    }
}

impl<I: LineInput> Prompter for FeatureHelpers<'_, I> {
    fn ask(&mut self, question: &str, default: &str) -> String {
        self.title();
        self.wiz.ask(question, default)
    }
    fn log(&mut self, line: &str) {
        if !self.said.insert(line.to_string()) {
            return;
        }
        self.title();
        self.wiz.say(&format!("  {line}"));
    }
}

/// Runs `prompt_setup` for every selected feature that needs it: always for one newly enabled just
/// now, or, for one already enabled and still, only if asked to update it (default no, so a quick
/// re-run doesn't walk through every enabled feature again). Returns the partial config to merge.
pub fn collect_feature_configs<I: LineInput>(wiz: &mut Wizard<I>, registry: &FeatureRegistry, selected: &[String], previously_enabled: &[String], existing: &Value) -> Value {
    let mut said = HashSet::new();
    let mut merged = Map::new();
    for d in registry.descriptors() {
        // A feature with nothing to ask has no prompt_setup.
        let Some(prompt) = d.prompt_setup.as_ref().filter(|_| selected.contains(&d.id)) else { continue };
        let newly_enabled = !previously_enabled.contains(&d.id);
        if !newly_enabled && !wiz.confirm(&format!("Change \"{}\" settings?", d.label), false) {
            continue;
        }
        let mut helpers = FeatureHelpers { wiz: &mut *wiz, label: &d.label, said: &mut said, titled: false };
        let partial = prompt(&mut helpers, existing);
        let titled = helpers.titled;
        if let Value::Object(m) = partial {
            merged.extend(m);
        }
        if titled {
            wiz.say("");
        }
    }
    Value::Object(merged)
}

/// First free port at or above [`DEFAULT_PORT`].
pub fn pick_free_port(is_free: &dyn Fn(u16) -> Check) -> Result<u16, String> {
    (DEFAULT_PORT..DEFAULT_PORT + 50).find(|p| is_free(*p).ok).ok_or_else(|| format!("No free port found between {DEFAULT_PORT} and {}.", DEFAULT_PORT + 49))
}

/// The config an interactive run writes: the existing settings, the chosen port/secret/features,
/// every feature this build knows (so a later update only auto-enables features shipped after this
/// save, not ones just unchecked), the repos, and what the features' questions returned.
pub fn build_config(existing: &Value, port: u16, secret: &str, selected: &[String], registry: &FeatureRegistry, repos: Map<String, Value>, feature_config: &Value) -> Value {
    let mut config = existing.as_object().cloned().unwrap_or_default();
    config.insert("port".into(), json!(port));
    config.insert("sharedSecret".into(), json!(secret));
    config.insert("enabledFeatures".into(), json!(selected));
    config.insert("knownFeatures".into(), json!(registry.all_feature_ids()));
    config.insert("repos".into(), Value::Object(repos));
    if let Some(extra) = feature_config.as_object() {
        config.extend(extra.clone());
    }
    Value::Object(config)
}

/// Maps the mapped repos to the repos plus what discovery found.
pub type RepoDiscovery<'a> = dyn Fn(&Map<String, Value>) -> Map<String, Value> + 'a;

/// The config a `--yes` run writes: the existing settings unchanged except for newly discovered
/// repo clones and any default-on features shipped since the last save. `None` when there is
/// nothing to keep (a first install has to be interactive). `discover` maps the repos to the
/// repos plus what was added.
pub fn non_interactive_config(existing: Option<&Value>, registry: &FeatureRegistry, discover: &RepoDiscovery<'_>) -> Option<Value> {
    let existing = existing?;
    let mut base = existing.as_object().cloned().unwrap_or_default();
    if let Some(m) = registry.migrate_enabled_features(existing) {
        base.insert("enabledFeatures".into(), json!(m.enabled_features));
        base.insert("knownFeatures".into(), json!(m.known_features));
    }
    let base_value = Value::Object(base.clone());
    let enabled = registry.enabled_feature_ids(&base_value);
    let current_repos = base.get("repos").and_then(Value::as_object).cloned().unwrap_or_default();
    let repos = if registry.needs_repos(&refs(&enabled)) { discover(&current_repos) } else { current_repos };
    base.insert("enabledFeatures".into(), json!(enabled));
    base.insert("repos".into(), Value::Object(repos));
    Some(Value::Object(base))
}

/// Moves any site `apiToken` sitting in `config` (freshly typed into it by a feature's prompts, or
/// left over from before the credential store existed) into `store`, blanking it in `config` in
/// place. Every caller writes config.json right after, so there is no save callback here.
pub fn migrate_api_tokens(config: &mut Value, store: &CredentialStore) {
    migrate_tokens_to_store(config, |name, value| store.set_value(name, value).map_err(|e| e.to_string()), |_| Ok(()), |m| eprintln!("{m}"));
}

/// The credential store as the MCP token's home.
pub struct StoreTokens<'a>(pub &'a CredentialStore);

impl McpTokenStore for StoreTokens<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get_value_text(name).ok().flatten()
    }
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.0.set_value(name, value).map_err(|e| e.to_string())
    }
}

/// Registers the companion's own MCP server with whatever MCP clients are present (Claude Code if
/// `claude` is available, Cursor if `~/.cursor` exists). Re-run after every setup so a fresh
/// token or a changed port takes effect. Never fails setup: any problem is a WARN line.
pub fn register_mcp_everywhere(port: u16, store: &dyn McpTokenStore, home: &Path, claude_available: bool, spawn: SpawnSync<'_>, log: &mut dyn FnMut(&str)) -> Option<Registrations> {
    let token = match ensure_mcp_token_default(store) {
        Ok((t, _)) => t,
        Err(e) => {
            log(&format!("WARN - couldn't register the companion's MCP server: {e} \u{2014} run `companion setup` again later."));
            return None;
        }
    };
    let result = register_everywhere(port, &token, home, claude_available, spawn);
    for (label, entry) in [("Claude Code", &result.claude), ("Cursor", &result.cursor)] {
        let Some(entry) = entry else { continue };
        if entry.ok {
            log(&format!("\u{2713} Registered the companion's MCP server with {label} (ai-companion)."));
        } else {
            log(&format!("WARN - couldn't register with {label}: {} \u{2014} run `companion setup` again later.", entry.message));
        }
    }
    Some(result)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionWait {
    pub connected: bool,
    pub version: Option<String>,
    pub skipped: bool,
}

/// Polls `is_connected` until the extension has said hello, `timeout` passes, or `skip` is
/// notified (the user pressed Enter), whichever comes first.
pub async fn wait_for_extension<F, Fut>(mut is_connected: F, skip: &Notify, interval: Duration, timeout: Duration) -> ExtensionWait
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ExtensionStatus>,
{
    let deadline = Instant::now() + timeout;
    let skipped = ExtensionWait { connected: false, version: None, skipped: true };
    loop {
        let status = tokio::select! {
            _ = skip.notified() => return skipped,
            s = is_connected() => s,
        };
        if status.connected {
            return ExtensionWait { connected: true, version: status.version, skipped: false };
        }
        if Instant::now() >= deadline {
            return ExtensionWait { connected: false, version: None, skipped: false };
        }
        tokio::select! {
            _ = skip.notified() => return skipped,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// "~/gitviews/sample-app" rather than the full home path, for display only.
pub fn tildify(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.into(),
    }
}

/// Writes `config` as config.json (0600, atomically): what every run does once it has the config.
pub fn save_config(path: &Path, config: &Value) -> std::io::Result<()> {
    write_config_file(path, config)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::packs::FeatureDescriptor;
    use std::collections::VecDeque;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    /// A scripted terminal: answers from a fixed queue in order, and fails loudly when asked more
    /// than was scripted, so a test that gets the interaction wrong doesn't hang.
    struct Scripted(VecDeque<String>);

    impl LineInput for Scripted {
        fn read_line(&mut self, prompt: &str) -> Option<String> {
            Some(self.0.pop_front().unwrap_or_else(|| panic!("ran out of scripted answers at: {prompt}")))
        }
    }

    type Lines = Arc<Mutex<Vec<String>>>;

    fn wiz(answers: &[&str]) -> (Wizard<Scripted>, Lines) {
        let lines: Lines = Arc::new(Mutex::new(vec![]));
        let sink = lines.clone();
        (Wizard::new(Scripted(answers.iter().map(|s| s.to_string()).collect()), move |l| sink.lock().unwrap().push(l.to_string())), lines)
    }

    fn d(id: &str, checks: &[&str], repos: bool, on: Option<bool>) -> FeatureDescriptor {
        let mut f = FeatureDescriptor::new(id, &format!("{id} label"));
        f.required_checks = checks.iter().map(|c| c.to_string()).collect();
        f.needs_repos = repos;
        f.enabled_by_default = on;
        f
    }

    fn registry() -> FeatureRegistry {
        let mut subtasks = d("create-jira-subtasks", &[], false, None);
        subtasks.prompt_setup = Some(Arc::new(|p, _| {
            p.log("Jira credentials are saved in Settings.");
            let model = p.ask("Model", "");
            json!({ "subtasks": { "model": model } })
        }));
        let mut review = d("review-in-editor", &["git"], true, None);
        review.prompt_setup = Some(Arc::new(|p, existing| {
            p.log("Jira credentials are saved in Settings.");
            let editor = p.ask("Editor", existing["reviewEditor"].as_str().unwrap_or("cursor"));
            json!({ "reviewEditor": editor })
        }));
        FeatureRegistry::new(vec![d("resolve-conflict", &["git", "claudeCli", "claudeAuth"], true, None), subtasks, review, d("analyze-issue", &["claudeCli", "claudeAuth"], true, None)])
    }

    fn ids(r: &FeatureRegistry) -> Vec<String> {
        r.all_feature_ids()
    }

    fn pass(_: &str) -> Check {
        Check { ok: true, message: "fine".into() }
    }

    fn fake_clone(dir: &Path, origin: &str) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [vec!["init", "-q"], vec!["remote", "add", "origin", origin]] {
            assert!(Command::new("git").args(&args).current_dir(dir).output().unwrap().status.success());
        }
    }

    #[test]
    fn render_extension_config_sets_the_service_url_secret_and_right_click_hosts() {
        let source = render_extension_config("new-secret", 9999, &["https://git.example.com/*".to_string()]);
        let body = source.split("self.COMPANION_CONFIG = ").nth(1).unwrap().trim().trim_end_matches(';');
        assert_eq!(serde_json::from_str::<Value>(body).unwrap(), json!({ "serviceBaseUrl": "http://127.0.0.1:9999", "sharedSecret": "new-secret", "contextMenuMatches": ["https://git.example.com/*"] }));
        let empty = render_extension_config("s", 1, &[]);
        assert!(empty.contains("\"contextMenuMatches\": []"));
    }

    #[test]
    fn load_existing_config_needs_a_file_with_a_port_and_a_secret() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.json");
        assert!(load_existing_config(&path).is_none());
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(load_existing_config(&path).is_none());
        std::fs::write(&path, r#"{"port":"8787","sharedSecret":"abc"}"#).unwrap();
        assert!(load_existing_config(&path).is_none());
        std::fs::write(&path, r#"{"port":8787,"sharedSecret":"abc","repos":{}}"#).unwrap();
        let c = load_existing_config(&path).unwrap();
        assert_eq!((c["port"].clone(), c["sharedSecret"].clone()), (json!(8787), json!("abc")));
    }

    #[test]
    fn parse_number_list_accepts_all_none_and_comma_or_space_separated_numbers() {
        assert_eq!(parse_number_list("all", 3), Some(vec![0, 1, 2]));
        assert_eq!(parse_number_list("NONE", 3), Some(vec![]));
        assert_eq!(parse_number_list("3,1", 3), Some(vec![0, 2]));
        assert_eq!(parse_number_list(" 1  2 ", 3), Some(vec![0, 1]));
        assert_eq!(parse_number_list("2,2", 3), Some(vec![1]));
    }

    #[test]
    fn parse_number_list_rejects_out_of_range_or_non_numeric_answers() {
        for bad in ["4", "0", "yes", "", "1.5", "-1"] {
            assert_eq!(parse_number_list(bad, 3), None, "{bad}");
        }
    }

    #[test]
    fn discover_repos_returns_the_existing_repos_unchanged_when_nothing_is_found() {
        let existing = json!({ "OTHER/repo": "/some/path" }).as_object().unwrap().clone();
        assert_eq!(discover_repos(&existing, &[], GitRemoteRules::BitbucketDc), (existing, vec![]));
    }

    #[test]
    fn discover_repos_adds_every_discovered_clone_keyed_from_its_origin_without_asking() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        fake_clone(&root.join("sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
        fake_clone(&root.join("billing"), "ssh://git@bitbucket.example.com:7999/acme/sample-service.git");
        fake_clone(&root.join("mirror"), "https://github.com/some/mirror.git");
        std::fs::create_dir(root.join("not-a-repo")).unwrap();
        let existing = json!({ "OTHER/repo": "/some/path" }).as_object().unwrap().clone();

        let (repos, added) = discover_repos(&existing, std::slice::from_ref(&root), GitRemoteRules::BitbucketDc);
        assert_eq!(repos["ACME/sample-service"], json!(root.join("billing").to_string_lossy()));
        assert_eq!(repos["ACME/sample-app"], json!(root.join("sample-app").to_string_lossy()));
        assert_eq!(repos["OTHER/repo"], "/some/path");
        assert_eq!(added, ["ACME/sample-app", "ACME/sample-service"]);
        // On GitHub's rules the clones are keyed by GitHub's naming (the ssh one parses too).
        assert_eq!(discover_repos(&existing, std::slice::from_ref(&root), GitRemoteRules::Github).1, ["acme/sample-service", "some/mirror"]);
        assert_eq!(existing["OTHER/repo"], "/some/path", "doesn't mutate its input");
    }

    #[test]
    fn discover_repos_keeps_an_existing_mapping_rather_than_re_adding_the_key_or_path() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        fake_clone(&root.join("sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
        fake_clone(&root.join("sample-app-copy"), "https://bitbucket.example.com/scm/acme/sample-app.git");
        let existing = json!({ "acme/Sample-App": root.join("sample-app-copy").to_string_lossy() }).as_object().unwrap().clone();
        assert_eq!(discover_repos(&existing, std::slice::from_ref(&root), GitRemoteRules::BitbucketDc), (existing, vec![]));
    }

    #[test]
    fn prompt_feature_selection_a_blank_answer_keeps_every_currently_enabled_feature() {
        let r = registry();
        let (mut w, _) = wiz(&[""]);
        assert_eq!(prompt_feature_selection(&mut w, &r, &ids(&r)), ids(&r));
        let (mut w, lines) = wiz(&[""]);
        assert!(prompt_feature_selection(&mut w, &r, &[]).is_empty());
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("No features enabled")));
    }

    #[test]
    fn prompt_feature_selection_enables_exactly_the_numbers_given_in_registry_order() {
        let r = registry();
        let (mut w, _) = wiz(&["3,1"]);
        assert_eq!(prompt_feature_selection(&mut w, &r, &[]), [ids(&r)[0].clone(), ids(&r)[2].clone()]);
    }

    #[test]
    fn prompt_feature_selection_re_asks_on_an_invalid_answer_and_can_end_with_none() {
        let r = registry();
        let (mut w, lines) = wiz(&["99", "none"]);
        assert!(prompt_feature_selection(&mut w, &r, &ids(&r)).is_empty());
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("Please enter \"all\"")));
    }

    #[test]
    fn resolve_prerequisites_asks_nothing_when_every_check_passes() {
        let r = registry();
        let (mut w, _) = wiz(&[]);
        assert_eq!(resolve_prerequisites(&mut w, &r, &ids(&r), &pass), ids(&r));
    }

    #[test]
    fn resolve_prerequisites_re_checks_on_enter_until_the_check_passes() {
        let r = registry();
        let calls = std::cell::Cell::new(0);
        let checks = |name: &str| {
            if name == "claudeAuth" {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    return Check { ok: false, message: "broken".into() };
                }
            }
            pass(name)
        };
        let (mut w, _) = wiz(&["", ""]);
        assert_eq!(resolve_prerequisites(&mut w, &r, &["resolve-conflict".to_string()], &checks), ["resolve-conflict"]);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn resolve_prerequisites_skip_drops_only_the_features_that_need_the_failing_check() {
        let r = registry();
        let checks = |name: &str| if name == "claudeCli" || name == "claudeAuth" { Check { ok: false, message: "broken".into() } } else { pass(name) };
        let (mut w, lines) = wiz(&["skip"]);
        let selected: Vec<String> = ["resolve-conflict", "review-in-editor", "create-jira-subtasks"].map(String::from).to_vec();
        // claudeAuth is never checked once resolve-conflict (its only user here) is skipped.
        assert_eq!(resolve_prerequisites(&mut w, &r, &selected, &checks), ["review-in-editor", "create-jira-subtasks"]);
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("Skipped: resolve-conflict label")));
    }

    #[test]
    fn collect_feature_configs_never_touches_the_terminal_when_nothing_is_selected_or_nothing_is_asked() {
        let r = registry();
        let (mut w, _) = wiz(&[]);
        assert_eq!(collect_feature_configs(&mut w, &r, &[], &[], &json!({})), json!({}));
        // resolve-conflict has no prompt_setup.
        let rc = vec!["resolve-conflict".to_string()];
        assert_eq!(collect_feature_configs(&mut w, &r, &rc, &[], &json!({})), json!({}));
        assert_eq!(collect_feature_configs(&mut w, &r, &rc, &rc, &json!({})), json!({}));
    }

    #[test]
    fn collect_feature_configs_always_prompts_a_newly_enabled_feature_with_no_change_gate() {
        let r = registry();
        let (mut w, _) = wiz(&["vim"]);
        let out = collect_feature_configs(&mut w, &r, &["review-in-editor".to_string()], &[], &json!({}));
        assert_eq!(out, json!({ "reviewEditor": "vim" }));
        let (mut w, _) = wiz(&[""]);
        let kept = collect_feature_configs(&mut w, &r, &["review-in-editor".to_string()], &[], &json!({ "reviewEditor": "cursor" }));
        assert_eq!(kept, json!({ "reviewEditor": "cursor" }));
    }

    #[test]
    fn collect_feature_configs_skips_an_already_enabled_feature_that_declines_the_change_prompt() {
        let r = registry();
        let (mut w, _) = wiz(&["n"]);
        let one = vec!["create-jira-subtasks".to_string()];
        assert_eq!(collect_feature_configs(&mut w, &r, &one, &one, &json!({})), json!({}));
    }

    #[test]
    fn collect_feature_configs_handles_a_mix_and_says_a_shared_message_once() {
        let r = registry();
        // create-jira-subtasks is gated -> "y" then its question; review-in-editor is new -> straight into its prompt.
        let (mut w, lines) = wiz(&["y", "haiku", ""]);
        let selected: Vec<String> = ["create-jira-subtasks", "review-in-editor"].map(String::from).to_vec();
        let out = collect_feature_configs(&mut w, &r, &selected, &["create-jira-subtasks".to_string()], &json!({ "reviewEditor": "cursor" }));
        assert_eq!(out, json!({ "subtasks": { "model": "haiku" }, "reviewEditor": "cursor" }));
        let said = lines.lock().unwrap().iter().filter(|l| l.contains("Jira credentials are saved in Settings.")).count();
        assert_eq!(said, 1);
    }

    #[test]
    fn select_returns_the_option_chosen_by_number_by_id_or_by_default() {
        let opts = [("a", "A"), ("b", "B")];
        assert_eq!(wiz(&["2"]).0.select("Pick one", &opts, "a"), "b");
        assert_eq!(wiz(&["b"]).0.select("Pick one", &opts, "a"), "b");
        assert_eq!(wiz(&[""]).0.select("Pick one", &opts, "b"), "b");
    }

    #[test]
    fn select_re_prompts_on_an_invalid_answer_before_accepting_a_good_one() {
        let (mut w, lines) = wiz(&["nope", "0", "3", "1"]);
        assert_eq!(w.select("Pick one", &[("a", "A"), ("b", "B")], "a"), "a");
        assert_eq!(lines.lock().unwrap().iter().filter(|l| l.contains("Please enter a number from 1 to 2")).count(), 3);
    }

    #[test]
    fn confirm_reads_yes_no_and_defaults() {
        assert!(wiz(&["Y"]).0.confirm("q", false));
        assert!(!wiz(&["nope"]).0.confirm("q", true));
        assert!(wiz(&[""]).0.confirm("q", true));
        assert!(!wiz(&[""]).0.confirm("q", false));
    }

    #[test]
    fn non_interactive_config_keeps_settings_adds_new_repos_and_soft_migrates_enabled_features() {
        let r = registry();
        let existing = json!({ "port": 8788, "sharedSecret": "s", "enabledFeatures": ["review-in-editor"], "reviewEditor": "cursor", "repos": { "ACME/sample-app": "/w" } });
        let discover = |repos: &Map<String, Value>| {
            let mut r = repos.clone();
            r.insert("ACME/new".into(), json!("/n"));
            r
        };
        let result = non_interactive_config(Some(&existing), &r, &discover).unwrap();
        assert_eq!((result["port"].clone(), result["sharedSecret"].clone(), result["reviewEditor"].clone()), (json!(8788), json!("s"), json!("cursor")));
        assert_eq!(result["repos"], json!({ "ACME/sample-app": "/w", "ACME/new": "/n" }));
        // A legacy list without knownFeatures: every default-on feature.
        assert_eq!(result["enabledFeatures"], json!(r.default_enabled_feature_ids()));
        assert_eq!(result["knownFeatures"], json!(ids(&r)));
        assert!(non_interactive_config(None, &r, &discover).is_none());
    }

    #[test]
    fn non_interactive_config_auto_enables_newly_shipped_default_on_features_on_update() {
        let r = registry();
        let prior: Vec<String> = ids(&r).into_iter().filter(|id| id != "analyze-issue").collect();
        let existing = json!({ "port": 8787, "sharedSecret": "s", "enabledFeatures": ["review-in-editor"], "knownFeatures": prior, "repos": { "ACME/sample-app": "/w" } });
        let result = non_interactive_config(Some(&existing), &r, &|repos| repos.clone()).unwrap();
        assert!(result["knownFeatures"].as_array().unwrap().contains(&json!("analyze-issue")));
        let enabled = result["enabledFeatures"].as_array().unwrap();
        assert!(enabled.contains(&json!("analyze-issue")) && enabled.contains(&json!("review-in-editor")));
    }

    #[test]
    fn non_interactive_config_leaves_repos_alone_when_no_enabled_feature_uses_them() {
        let r = registry();
        let existing = json!({ "port": 1, "sharedSecret": "s", "enabledFeatures": ["create-jira-subtasks"], "knownFeatures": ids(&r), "repos": {} });
        let discover = |_: &Map<String, Value>| -> Map<String, Value> { panic!("shouldn't look for repos") };
        assert_eq!(non_interactive_config(Some(&existing), &r, &discover).unwrap(), existing);
    }

    #[test]
    fn build_config_layers_the_run_over_the_existing_settings() {
        let r = registry();
        let repos = json!({ "A/b": "/p" }).as_object().unwrap().clone();
        let c = build_config(&json!({ "keep": 1, "port": 1 }), 8790, "sec", &["review-in-editor".to_string()], &r, repos, &json!({ "reviewEditor": "cursor" }));
        assert_eq!(c, json!({ "keep": 1, "port": 8790, "sharedSecret": "sec", "enabledFeatures": ["review-in-editor"], "knownFeatures": ids(&r), "repos": { "A/b": "/p" }, "reviewEditor": "cursor" }));
    }

    #[test]
    fn pick_free_port_takes_the_first_free_one_at_or_above_the_default() {
        assert_eq!(pick_free_port(&|p| Check { ok: p >= 8789, message: String::new() }), Ok(8789));
        assert!(pick_free_port(&|_| Check { ok: false, message: String::new() }).unwrap_err().contains("8787 and 8836"));
    }

    #[tokio::test]
    async fn wait_for_extension_resolves_once_the_extension_says_hello() {
        let replies = std::sync::Mutex::new(VecDeque::from([(false, None), (false, None), (true, Some("0.4.2".to_string()))]));
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let skip = Notify::new();
        let result = wait_for_extension(
            || {
                polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (connected, version) = replies.lock().unwrap().pop_front().unwrap();
                async move { ExtensionStatus { connected, version } }
            },
            &skip,
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(result, ExtensionWait { connected: true, version: Some("0.4.2".into()), skipped: false });
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn wait_for_extension_stops_polling_when_the_user_presses_enter() {
        let skip = Notify::new();
        skip.notify_one();
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let result = wait_for_extension(
            || {
                polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { ExtensionStatus { connected: false, version: None } }
            },
            &skip,
            Duration::from_millis(5),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(result, ExtensionWait { connected: false, version: None, skipped: true });
        let at_skip = polls.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), at_skip);
    }

    #[tokio::test]
    async fn wait_for_extension_gives_up_after_the_timeout() {
        let skip = Notify::new();
        let result = wait_for_extension(|| async { ExtensionStatus { connected: false, version: None } }, &skip, Duration::from_millis(1), Duration::from_millis(20)).await;
        assert_eq!(result, ExtensionWait { connected: false, version: None, skipped: false });
    }

    fn temp_store() -> (tempfile::TempDir, CredentialStore) {
        let d = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(d.path().join("credentials.enc"), d.path().join("credentials.key"));
        (d, store)
    }

    #[test]
    fn migrate_api_tokens_moves_a_freshly_typed_token_into_the_store_and_blanks_it_in_config() {
        let (_d, store) = temp_store();
        let mut config = json!({ "jira": { "baseUrl": "https://jira.example.com", "apiToken": "typed-in-wizard" } });
        migrate_api_tokens(&mut config, &store);
        assert_eq!(config["jira"].get("apiToken"), None);
        assert_eq!(store.get_value_text("jira.apiToken").unwrap().as_deref(), Some("typed-in-wizard"));
    }

    #[test]
    fn migrate_api_tokens_is_a_no_op_when_there_is_nothing_to_migrate() {
        let (_d, store) = temp_store();
        let mut config = json!({ "jira": { "baseUrl": "https://jira.example.com" } });
        migrate_api_tokens(&mut config, &store);
        assert_eq!(config, json!({ "jira": { "baseUrl": "https://jira.example.com" } }));
        assert!(store.list_names().unwrap().is_empty());
    }

    #[test]
    fn migrating_plus_token_creation_never_writes_the_mcp_token_into_config_json() {
        let (d, store) = temp_store();
        let mut config = json!({ "port": 8787, "jira": { "baseUrl": "https://jira.example.com", "apiToken": "typed-in-wizard" } });
        migrate_api_tokens(&mut config, &store);
        let (token, created) = ensure_mcp_token_default(&StoreTokens(&store)).unwrap();
        assert!(created && !token.is_empty());
        let path = d.path().join("config.json");
        save_config(&path, &config).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains(&token) && !written.contains("mcp.token") && !written.contains("typed-in-wizard"));
        assert_eq!(store.get_value_text("mcp.token").unwrap().as_deref(), Some(token.as_str()));
    }

    #[test]
    fn register_mcp_everywhere_reports_each_client_and_never_fails() {
        let (d, store) = temp_store();
        let home = d.path().join("home");
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        let ok = |_: &str, _: &[String], _: Duration| crate::mcp_registration::SpawnResult { status: Some(0), ..Default::default() };
        let mut lines = vec![];
        let result = register_mcp_everywhere(8787, &StoreTokens(&store), &home, true, &ok, &mut |l| lines.push(l.to_string())).unwrap();
        assert!(result.claude.unwrap().ok && result.cursor.unwrap().ok);
        assert_eq!(lines.len(), 2);
        let token = store.get_value_text("mcp.token").unwrap().unwrap();
        assert!(lines.iter().all(|l| l.starts_with('\u{2713}') && !l.contains(&token)));
        // Nothing present: no registrations, no lines.
        let mut none = vec![];
        let bare = register_mcp_everywhere(8787, &StoreTokens(&store), &d.path().join("empty"), false, &ok, &mut |l| none.push(l.to_string())).unwrap();
        assert!(bare.claude.is_none() && bare.cursor.is_none() && none.is_empty());
    }

    #[test]
    fn small_helpers_shorten_paths_and_descriptions() {
        assert_eq!(tildify("/home/me/gitviews/x", "/home/me"), "~/gitviews/x");
        assert_eq!(tildify("/home/me", "/home/me"), "~");
        assert_eq!(tildify("/home/meh/x", "/home/me"), "/home/meh/x");
        assert_eq!(one_line("First sentence. Second one.", 90), "First sentence");
        assert_eq!(one_line("A very long sentence indeed", 10), "A very lo\u{2026}");
        assert!(fix_hint("git").contains("xcode-select"));
    }

    #[test]
    fn discovery_roots_add_the_folders_mapped_repos_already_live_in() {
        let d = tempfile::tempdir().unwrap();
        let existing = json!({ "A/b": "/somewhere/else/b" }).as_object().unwrap().clone();
        assert_eq!(discovery_roots(&existing, d.path()), [PathBuf::from("/somewhere/else")]);
    }
}
