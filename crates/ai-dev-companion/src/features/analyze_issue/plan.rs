//! Pure logic for analyze-issue (port of `features/analyze-issue/plan.js`): the
//! issue key, the ticket's URLs and linked PRs, which local repo the ticket most
//! likely belongs to (ranking, the Claude pick, the confirmation choices), the
//! analysis prompt and its parsing, the comment text (Markdown, Jira wiki, the
//! short summary), and the on-disk analysis cache.

use crate::app_identity::product_name;
use crate::guard::mcp_classifier::COMPANION_MCP_SERVER;
use crate::jobs::FindSimilar;
use crate::js_text::{is_js_space, js_to_string, js_trim, js_truthy, locale_compare, slice_utf16};
use crate::prereqs::GitRemoteRules;
use crate::similar::prompt_block;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use url::Url;

static ISSUE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Z][A-Z0-9_]*)-([0-9]+)$").unwrap());
static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s<>"')\]]+"#).unwrap());
static STATE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_]{1,20}$").unwrap());
static CONTROL_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\x00-\x1f\x7f]+").unwrap());

/// Upper-cases and validates an issue key (e.g. "proj-34034" -> PROJ-34034): `(key, project_key)`.
pub fn parse_issue_key(raw: &str) -> Result<(String, String), String> {
    let key = js_trim(raw).to_uppercase();
    match ISSUE_KEY_RE.captures(&key) {
        Some(m) => Ok((key.clone(), m[1].to_string())),
        None => Err(format!("\"{raw}\" doesn't look like a Jira issue key (expected e.g. \"PROJ-34034\").")),
    }
}

/// Flatten ADF / wiki / plain text into a searchable string.
pub fn text_from_field(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(_) | Value::Bool(_) => js_to_string(value),
        Value::Array(a) => a.iter().map(text_from_field).collect::<Vec<_>>().join("\n"),
        Value::Object(o) => {
            match o.get("content") {
                Some(Value::String(s)) => return s.clone(),
                Some(Value::Array(a)) => return a.iter().map(text_from_field).collect::<Vec<_>>().join("\n"),
                _ => {}
            }
            match o.get("text") {
                Some(Value::String(s)) => s.clone(),
                _ => value.to_string(),
            }
        }
    }
}

/// Strip trailing punctuation that the URL pattern often captures from prose.
fn clean_url(raw: &str) -> &str {
    raw.trim_end_matches(['.', ',', ';', ':', '!', '?', ')'])
}

/// Pulls https? URLs from description, comments, environment, and remote links, then dedupes
/// (order preserved, first occurrence wins).
pub fn extract_urls(issue: &Value) -> Vec<String> {
    let fields = &issue["fields"];
    let mut chunks = vec![text_from_field(&fields["description"]), text_from_field(&fields["environment"])];
    for c in fields["comment"]["comments"].as_array().into_iter().flatten() {
        chunks.push(text_from_field(&c["body"]));
    }
    for link in issue["remoteLinks"].as_array().into_iter().flatten() {
        let href = [&link["object"]["url"], &link["url"]].into_iter().find(|v| js_truthy(Some(v)));
        if let Some(h) = href {
            chunks.push(js_to_string(h));
        }
    }
    let mut seen: Vec<String> = vec![];
    for chunk in chunks.iter().filter(|c| !c.is_empty()) {
        for m in URL_RE.find_iter(chunk) {
            let url = clean_url(m.as_str());
            if !url.is_empty() && !seen.iter().any(|s| s == url) {
                seen.push(url.to_string());
            }
        }
    }
    seen
}

fn normalize_name(raw: &str) -> String {
    raw.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect()
}

/// Case-insensitive lookup of a "PROJECT/repo" key in the repos map.
fn find_repo_key(repos: &Map<String, Value>, project: &str, repo: &str) -> Option<String> {
    let want = format!("{project}/{repo}").to_lowercase();
    repos.keys().find(|k| k.to_lowercase() == want).cloned()
}

const MAX_LINKED_PRS: usize = 10;

fn clean_pr_title(raw: &Value) -> Option<String> {
    let s = raw.as_str()?;
    let t = slice_utf16(js_trim(&CONTROL_RUN.replace_all(s, " ")), 200);
    (!t.is_empty()).then_some(t)
}

/// A pull request already linked to the ticket.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedPr {
    pub repo_key: String,
    pub project: String,
    pub repo: String,
    pub id: u64,
    pub title: Option<String>,
    pub state: Option<String>,
    pub url: Option<String>,
}

fn pr_state(state: &Value) -> Option<String> {
    let s = if js_truthy(Some(state)) { js_to_string(state) } else { String::new() };
    STATE_RE.is_match(&s).then(|| s.to_uppercase())
}

/// The pull requests already linked to the ticket: those the tracker's development panel lists
/// (`dev_status` is the raw dev-status `pullrequest` answer, or nothing when that isn't available)
/// and any PR address in the ticket's links, description or comments. Each PR's address is rebuilt
/// from its project, repo and number on the configured git host, never taken as given, since it
/// ends up as a link in the panel. Each PR once, at most ten.
pub fn linked_pull_requests(issue: &Value, dev_status: &Value, git_base_url: &str, rules: GitRemoteRules) -> Vec<LinkedPr> {
    // No usable host: the PRs still count, just without a link.
    let base = Url::parse(git_base_url).ok().filter(|u| u.scheme() == "https").map(|u| u.origin().ascii_serialization());
    let mut out: Vec<LinkedPr> = vec![];
    let mut add = |url: &Value, title: &Value, state: &Value| {
        let Some(m) = url.as_str().and_then(|u| rules.parse_pr_url(u)) else { return };
        let repo_key = format!("{}/{}", m.project, m.repo);
        if let Some(existing) = out.iter_mut().find(|p| p.repo_key.to_lowercase() == repo_key.to_lowercase() && p.id == m.id) {
            if existing.title.is_none() {
                existing.title = clean_pr_title(title);
            }
            if existing.state.is_none() {
                existing.state = pr_state(state);
            }
            return;
        }
        if out.len() >= MAX_LINKED_PRS {
            return;
        }
        out.push(LinkedPr { repo_key, project: m.project.clone(), repo: m.repo.clone(), id: m.id, title: clean_pr_title(title), state: pr_state(state), url: base.as_ref().map(|b| format!("{b}{}", rules.pr_path(&m.project, &m.repo, m.id))) });
    };
    for d in dev_status["detail"].as_array().into_iter().flatten() {
        for pr in d["pullRequests"].as_array().into_iter().flatten() {
            let name = if js_truthy(Some(&pr["name"])) { &pr["name"] } else { &pr["title"] };
            add(&pr["url"], name, &pr["status"]);
        }
    }
    for url in extract_urls(issue) {
        add(&json!(url), &Value::Null, &Value::Null);
    }
    out
}

/// A local-repo candidate and why it was ranked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub repo_key: String,
    pub score: u32,
    pub reason: String,
}

fn ticket_names(issue: &Value) -> Vec<String> {
    let fields = &issue["fields"];
    let components = fields["components"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().filter(|n| !n.is_empty()).map(String::from));
    let labels = fields["labels"].as_array().into_iter().flatten().filter(|l| js_truthy(Some(l))).map(js_to_string);
    components.chain(labels).collect()
}

fn mapped_repo<'a>(map: &'a Value, name: &str) -> Option<&'a str> {
    [map.get(name), map.get(name.to_lowercase().as_str())].into_iter().flatten().find(|v| js_truthy(Some(v))).and_then(Value::as_str)
}

/// Ranks local-repo candidates from ticket signals. Sources (highest priority first within a
/// source; later sources only add if not already present at equal-or-higher score):
///   0. Pull requests already linked to the ticket
///   1. Repository links in the ticket matched to the configured repos
///   2. Components/labels via the optional componentRepoMap
///   3. Normalized name match of components/labels against repo names
///
/// Sorted by score desc, then key.
pub fn candidate_repos_from_ticket(issue: &Value, repos: &Value, component_repo_map: &Value, linked_prs: &[LinkedPr], rules: GitRemoteRules) -> Vec<Candidate> {
    let empty = Map::new();
    let repos = repos.as_object().unwrap_or(&empty);
    let mut by_key: Vec<Candidate> = vec![];
    let mut add = |repo_key: &str, score: u32, reason: String| {
        if repo_key.is_empty() || !repos.get(repo_key).is_some_and(|v| js_truthy(Some(v))) {
            return;
        }
        match by_key.iter_mut().find(|c| c.repo_key == repo_key) {
            Some(existing) if score > existing.score => *existing = Candidate { repo_key: repo_key.into(), score, reason },
            Some(_) => {}
            None => by_key.push(Candidate { repo_key: repo_key.into(), score, reason }),
        }
    };
    // A PR already linked to the ticket is the strongest sign of where it lives.
    for pr in linked_prs {
        if let Some(key) = find_repo_key(repos, &pr.project, &pr.repo) {
            add(&key, 120, format!("Linked PR #{} in {}", pr.id, pr.repo_key));
        }
    }
    for url in extract_urls(issue) {
        let Some(m) = rules.parse_repo_url(&url) else { continue };
        if let Some(key) = find_repo_key(repos, &m.project, &m.repo) {
            add(&key, 100, format!("Repository link {}/{}", m.project, m.repo));
        }
    }
    let names = ticket_names(issue);
    for name in &names {
        let Some(mapped) = mapped_repo(component_repo_map, name) else { continue };
        let key = if repos.contains_key(mapped) { Some(mapped.to_string()) } else { repos.keys().find(|k| k.to_lowercase() == mapped.to_lowercase()).cloned() };
        if let Some(k) = key {
            add(&k, 80, format!("Mapped from \"{name}\""));
        }
    }
    for name in &names {
        let norm = normalize_name(name);
        if norm.is_empty() {
            continue;
        }
        for key in repos.keys() {
            let repo_name = if key.contains('/') { key.rsplit('/').next().unwrap_or(key) } else { key };
            if normalize_name(repo_name) == norm {
                add(key, 60, format!("Name match on \"{name}\""));
            }
        }
    }
    by_key.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| locale_compare(&a.repo_key, &b.repo_key)));
    by_key
}

/// True when heuristics produced exactly one top-scoring candidate.
pub fn is_strong_repo_match(candidates: &[Candidate]) -> bool {
    match candidates {
        [] => false,
        [_] => true,
        [a, b, ..] => a.score > b.score,
    }
}

/// Repos that similar past tickets were analyzed in, most similar first, limited to keys we still
/// have a clone for. A hint for the pick, not a match.
pub fn repo_hints_from_similar(similar_items: &[Value], repo_keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for s in similar_items {
        let Some(repo) = s["repo"].as_str() else { continue };
        if let Some(key) = repo_keys.iter().find(|k| k.to_lowercase() == repo.to_lowercase()) {
            if !out.contains(key) {
                out.push(key.clone());
            }
        }
    }
    out
}

/// True when the proposed repo is what a component mapping (one the user made or confirmed) gives
/// for one of the ticket's components or labels, so asking again would only be noise. A mapping
/// that points elsewhere settles nothing.
pub fn is_settled_repo_match(issue: &Value, proposal: Option<&str>, component_repo_map: &Value) -> bool {
    let Some(proposal) = proposal.filter(|p| !p.is_empty()) else { return false };
    let want = proposal.to_lowercase();
    ticket_names(issue).iter().any(|name| mapped_repo(component_repo_map, name).is_some_and(|m| m.to_lowercase() == want))
}

const MAX_CHOICE_OPTIONS: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoChoice {
    pub repo_key: String,
    pub reason: String,
}

pub struct ChoiceInput<'a> {
    pub proposal: Option<&'a str>,
    pub proposal_reason: Option<&'a str>,
    pub candidates: &'a [Candidate],
    pub hint_keys: &'a [String],
    pub repo_keys: &'a [String],
    pub linked_prs: &'a [LinkedPr],
}

/// The repos offered when the user confirms the repo: ones that already have a PR for this ticket
/// first, then the proposal, the other ticket-based candidates, the repos of similar past tickets,
/// then every other repo we know, each once, each with why it is offered.
pub fn repo_choice_options(i: &ChoiceInput<'_>) -> Vec<RepoChoice> {
    let mut out: Vec<RepoChoice> = vec![];
    let mut add = |repo_key: &str, reason: &str| {
        if repo_key.is_empty() || out.iter().any(|o| o.repo_key == repo_key) || !i.repo_keys.iter().any(|k| k == repo_key) {
            return;
        }
        out.push(RepoChoice { repo_key: repo_key.into(), reason: reason.into() });
    };
    for pr in i.linked_prs {
        if let Some(key) = i.repo_keys.iter().find(|k| k.to_lowercase() == pr.repo_key.to_lowercase()) {
            add(key, "");
        }
    }
    if let Some(p) = i.proposal {
        add(p, i.proposal_reason.unwrap_or(""));
    }
    for c in i.candidates {
        add(&c.repo_key, &c.reason);
    }
    for k in i.hint_keys {
        add(k, "used for a similar past ticket");
    }
    let mut rest: Vec<&String> = i.repo_keys.iter().collect();
    rest.sort_by(|a, b| locale_compare(a, b));
    for k in rest {
        add(k, "");
    }
    out.truncate(MAX_CHOICE_OPTIONS);
    out
}

/// The reason recorded for the repo the user settled on.
pub fn confirmed_reason(choice: Option<&str>, proposal: Option<&str>, proposal_reason: Option<&str>) -> String {
    match (choice, proposal) {
        (Some(c), Some(p)) if !c.is_empty() && c == p => format!("You confirmed it{}", proposal_reason.filter(|r| !r.is_empty()).map(|r| format!(" ({r})")).unwrap_or_default()),
        _ => "You chose it".into(),
    }
}

/// The component -> repo entries worth remembering after the user confirmed or chose a repo (never
/// from a guess alone): only for a ticket with exactly one component (with several there is no
/// telling which one the repo belongs to), and never over an entry already there.
pub fn component_mappings_to_learn(issue: &Value, repo_key: Option<&str>, reason: &str, component_repo_map: &Value) -> Map<String, Value> {
    let Some(repo_key) = repo_key.filter(|k| !k.is_empty()) else { return Map::new() };
    if !(reason.starts_with("You confirmed") || reason.starts_with("You chose")) {
        return Map::new();
    }
    let components: Vec<&str> = issue["fields"]["components"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().filter(|n| !n.is_empty())).collect();
    if components.len() != 1 {
        return Map::new();
    }
    let name = js_trim(components[0]);
    let taken = component_repo_map.as_object().is_some_and(|m| m.keys().any(|k| k.to_lowercase() == name.to_lowercase()));
    if name.is_empty() || taken {
        return Map::new();
    }
    Map::from_iter([(name.to_string(), json!(repo_key))])
}

/// Model for the "Add as comment" summary: a short rewrite, so the small one.
pub const COMMENT_SUMMARY_MODEL: &str = "haiku";
/// The summary comment stays under this many words, header included.
pub const COMMENT_SUMMARY_MAX_WORDS: usize = 250;
/// What the prompt asks for; below the cap so the header still fits.
const COMMENT_SUMMARY_TARGET_WORDS: usize = 180;

pub fn build_comment_summary_prompt(full_markdown: &str) -> String {
    [
        "Summarize the Jira issue analysis below into a short comment for the ticket.".to_string(),
        String::new(),
        format!("- At most {COMMENT_SUMMARY_TARGET_WORDS} words. Plain Markdown: a few short sentences or bullets, no headings."),
        "- Cover the likely cause (with confidence), where in the code, and the next steps.".into(),
        "- Keep file paths, symbols and ticket keys exactly as written. Add nothing that is not in the analysis.".into(),
        "- Reply with the comment text only: no preamble, no code fence.".into(),
        String::new(),
        "<analysis>".into(),
        js_trim(full_markdown).to_string(),
        "</analysis>".into(),
    ]
    .join("\n")
}

/// Cut `text` to at most `max` words, keeping line breaks in what is kept.
fn clamp_words(text: &str, max: usize) -> String {
    let mut count = 0;
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        let ws = is_js_space(c);
        let mut part = String::new();
        while let Some(&n) = chars.peek() {
            if is_js_space(n) != ws {
                break;
            }
            part.push(n);
            chars.next();
        }
        if !ws {
            count += 1;
            if count > max {
                return format!("{}\u{2026}", out.trim_end_matches(is_js_space));
            }
        }
        out.push_str(&part);
    }
    out
}

fn md_one_line(text: &str) -> String {
    let mut out = String::new();
    let mut in_nl = false;
    for c in text.chars() {
        if c == '\n' {
            if !in_nl {
                out.push(' ');
            }
            in_nl = true;
        } else {
            out.push(c);
            in_nl = false;
        }
    }
    js_trim(&out).to_string()
}

fn md_one(v: &Value) -> String {
    md_one_line(&if js_truthy(Some(v)) { js_to_string(v) } else { String::new() })
}

static FENCED_BODY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^```(?:markdown|md)?\s*\n((?s:.*?))\n```$").unwrap());

/// Header + AI note (as `format_analysis_as_markdown_comment`) over Claude's summary, under
/// `COMMENT_SUMMARY_MAX_WORDS` in all.
pub fn assemble_summary_comment(issue_key: Option<&str>, summary_text: &str) -> String {
    let head = [format!("### AI issue analysis \u{2014} {}", md_one_line(issue_key.filter(|k| !k.is_empty()).unwrap_or("issue"))), String::new(), format!("_Generated by {}. Treat as a starting point, not a verified root cause._", product_name()), String::new()].join("\n");
    let trimmed = js_trim(summary_text);
    let unfenced = FENCED_BODY.captures(trimmed).map_or(trimmed.to_string(), |c| c[1].to_string());
    let body = js_trim(&unfenced).to_string();
    if body.is_empty() {
        return String::new();
    }
    let head_words = head.split_whitespace().count();
    format!("{head}{}\n", clamp_words(&body, COMMENT_SUMMARY_MAX_WORDS - head_words))
}

pub fn build_repo_pick_prompt(issue: &Value, repo_keys: &[String], hint_keys: &[String], linked_prs: &[LinkedPr]) -> String {
    let fields = &issue["fields"];
    let key = issue["key"].as_str().filter(|k| !k.is_empty()).unwrap_or("(unknown)");
    let summary = if js_truthy(Some(&fields["summary"])) { js_to_string(&fields["summary"]) } else { String::new() };
    let components: Vec<String> = fields["components"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().filter(|n| !n.is_empty()).map(String::from)).collect();
    let labels: Vec<String> = fields["labels"].as_array().into_iter().flatten().map(js_to_string).collect();
    let urls = extract_urls(issue);
    let or_none = |v: Vec<String>| if v.is_empty() { "(none)".to_string() } else { v.join(", ") };
    let mut lines: Vec<String> = vec![
        "Pick which local git repository this Jira bug most likely belongs to.".into(),
        "Reply with ONLY a single line: either one exact repo key from the list, or the word none.".into(),
        "Do not explain. Do not use tools.".into(),
        String::new(),
        format!("Issue: {key} \u{2014} {summary}"),
        format!("Components: {}", or_none(components)),
        format!("Labels: {}", or_none(labels)),
        format!("URLs: {}", or_none(urls.into_iter().take(20).collect())),
        String::new(),
    ];
    if !linked_prs.is_empty() {
        lines.extend([String::new(), "Pull requests already linked to this ticket (strong evidence of the repo; titles are written by people):".to_string()]);
        for p in linked_prs.iter().take(5) {
            lines.push(format!("- {} #{}{}{}", p.repo_key, p.id, p.state.as_ref().map(|s| format!(" ({s})")).unwrap_or_default(), p.title.as_ref().map(|t| format!(": {t}")).unwrap_or_default()));
        }
    }
    if !hint_keys.is_empty() {
        lines.extend([String::new(), format!("Repos that similar past tickets were analyzed in (a hint, most similar first): {}", hint_keys.join(", "))]);
    }
    lines.extend([String::new(), "Candidate repo keys:".to_string()]);
    lines.extend(repo_keys.iter().map(|k| format!("- {k}")));
    lines.join("\n")
}

/// Parses Claude's one-line repo pick: an exact key (any case), or `None` for "none"/garbage.
pub fn parse_repo_pick(text: &str, repo_keys: &[String]) -> Option<String> {
    let line = js_trim(text).split('\n').map(|l| js_trim(l.trim_end_matches('\r'))).find(|l| !l.is_empty() && !l.starts_with("```")).unwrap_or("");
    let cleaned = js_trim(line.trim_matches(|c| c == '"' || c == '\'' || c == '`'));
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("none") {
        return None;
    }
    repo_keys.iter().find(|k| *k == cleaned).or_else(|| repo_keys.iter().find(|k| k.to_lowercase() == cleaned.to_lowercase())).cloned()
}

/// How a run is set up, chosen by the one flag that says nobody clicked: a background run (the
/// watchers') uses the stricter policy, no MCP servers and no reference links, since it can fetch
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisRunOptions {
    pub policy: &'static str,
    pub mcp_servers: Vec<String>,
    pub extra_allowed_tools: Vec<String>,
    pub use_urls: bool,
}

pub fn analysis_run_options(background: bool, read_mcp_servers: impl FnOnce() -> Vec<String>) -> AnalysisRunOptions {
    let mcp_servers = if background { vec![] } else { read_mcp_servers() };
    AnalysisRunOptions { policy: if background { "readOnlyBackground" } else { "readOnly" }, extra_allowed_tools: mcp_servers.iter().map(|s| format!("mcp__{s}")).collect(), mcp_servers, use_urls: !background }
}

/// The text a similar-ticket search compares: the summary and the start of the description (the
/// search clips and masks it again before embedding).
pub fn similar_query_text(issue: &Value) -> String {
    let fields = &issue["fields"];
    let summary = if js_truthy(Some(&fields["summary"])) { js_to_string(&fields["summary"]) } else { String::new() };
    [summary, slice_utf16(&text_from_field(&fields["description"]), 2000)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n")
}

/// One MCP server hint line, and which server it is for.
#[derive(Debug, Clone)]
pub struct ServerHint {
    pub server: String,
    pub line: String,
}

pub struct AnalysisPromptInput<'a> {
    pub issue: &'a Value,
    pub urls: &'a [String],
    pub repo_key: Option<&'a str>,
    pub mcp_servers: &'a [String],
    pub server_hints: &'a [ServerHint],
    pub background: bool,
    pub similar: &'a [Value],
}

fn names_of(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x["name"].as_str().filter(|n| !n.is_empty()).map(String::from)).collect()
}

pub fn build_analysis_prompt(i: &AnalysisPromptInput<'_>) -> String {
    let fields = &i.issue["fields"];
    let key = i.issue["key"].as_str().filter(|k| !k.is_empty()).unwrap_or("(unknown)");
    let text_or_empty = |v: &Value| if js_truthy(Some(v)) { js_to_string(v) } else { String::new() };
    let summary = text_or_empty(&fields["summary"]);
    let description = slice_utf16(&text_from_field(&fields["description"]), 8000);
    let environment = slice_utf16(&text_from_field(&fields["environment"]), 2000);
    let components = names_of(&fields["components"]);
    let labels: Vec<String> = fields["labels"].as_array().into_iter().flatten().map(js_to_string).collect();
    let priority = fields["priority"]["name"].as_str().unwrap_or("").to_string();
    let versions = names_of(&fields["versions"]);
    let fix_versions = names_of(&fields["fixVersions"]);
    let all_comments = fields["comment"]["comments"].as_array().cloned().unwrap_or_default();
    let comments = all_comments[all_comments.len().saturating_sub(10)..]
        .iter()
        .map(|c| {
            let author = [&c["author"]["displayName"], &c["author"]["name"]].into_iter().find(|v| js_truthy(Some(v))).map_or_else(|| "unknown".to_string(), js_to_string);
            format!("[{author}] {}", slice_utf16(&text_from_field(&c["body"]), 1500))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mcp_list = if i.mcp_servers.is_empty() { "(none detected)".to_string() } else { i.mcp_servers.join(", ") };
    // Earlier tickets from the local history: fenced, clipped, labelled as untrusted (titles by
    // people, analyses earlier AI output). Background runs get it too: it is data, not a tool, and
    // gives no way out.
    let similar_block = prompt_block(i.similar);
    let hint_lines: Vec<String> = i.server_hints.iter().map(|h| h.line.clone()).collect();
    let preferred_schema = i.server_hints.iter().map(|h| format!("{}: true", serde_json::to_string(&h.server).expect("a string serializes"))).collect::<Vec<_>>().join(", ");
    let or_none = |v: &[String]| if v.is_empty() { "(none)".to_string() } else { v.join(", ") };

    let mut lines: Vec<String> = vec![
        "You are doing a quick, read-only initial analysis of a Jira Bug. Do NOT propose or apply a fix.".into(),
        "Stay read-only: never edit files, never commit, never push, never create/update Jira/Confluence/Bitbucket/Jenkins resources.".into(),
    ];
    if i.background {
        lines.push("No MCP servers or web access are available in this run: work only from the ticket text and the local files.".into());
    } else {
        lines.extend(hint_lines);
        lines.push("Use Jira, Confluence, Bitbucket, and Jenkins MCP servers (or WebFetch) for the reference URLs as needed.".into());
    }
    lines.extend([
        "You may use Bash only for `git log`, `git blame`, and `git show` \u{2014} nothing else.".to_string(),
        String::new(),
        format!("Repo context: {}.", match i.repo_key.filter(|k| !k.is_empty()) {
            Some(k) => format!("working in local clone for \"{k}\""),
            None => format!("NO local repo resolved \u{2014} reason from ticket text{} only", if i.background { "" } else { " and URLs" }),
        }),
    ]);
    if !i.background {
        lines.push(format!("MCP servers loaded from user config: {mcp_list}"));
    }
    lines.extend([
        String::new(),
        "Return ONE fenced JSON object (and nothing else after it) matching this schema:".to_string(),
        "{".into(),
        "  \"tldr\": \"string\",".into(),
        "  \"affectedArea\": [{ \"path\": \"string\", \"symbol\": \"string\", \"why\": \"string\" }],".into(),
        "  \"hypotheses\": [{ \"hypothesis\": \"string\", \"confidence\": \"high|medium|low\", \"evidence\": \"string\" }],".into(),
        "  \"reproScope\": \"string\",".into(),
        "  \"referencesReviewed\": [{ \"url\": \"string\", \"takeaway\": \"string\" }],".into(),
        "  \"nextSteps\": [\"string\"],".into(),
        "  \"openQuestions\": [\"string\"],".into(),
        format!("  \"toolsUsed\": {{ \"preferred\": {{ {preferred_schema} }}, \"mcpServers\": [\"string\"] }}"),
        "}".into(),
        String::new(),
        format!("Issue: {key}"),
        format!("Summary: {summary}"),
        format!("Priority: {priority}"),
        format!("Components: {}", or_none(&components)),
        format!("Labels: {}", or_none(&labels)),
        format!("Affects versions: {}", or_none(&versions)),
        format!("Fix versions: {}", or_none(&fix_versions)),
        String::new(),
        "Description:".into(),
        if description.is_empty() { "(empty)".into() } else { description },
        String::new(),
        "Environment:".into(),
        if environment.is_empty() { "(empty)".into() } else { environment },
        String::new(),
        "Recent comments:".into(),
        if comments.is_empty() { "(none)".into() } else { comments },
    ]);
    if !similar_block.is_empty() {
        lines.extend([String::new(), similar_block]);
    }
    if !i.background {
        lines.extend([String::new(), "Reference URLs:".to_string(), if i.urls.is_empty() { "(none)".to_string() } else { i.urls.iter().map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n") }]);
    }
    lines.join("\n")
}

fn empty_analysis() -> Value {
    json!({ "tldr": "", "affectedArea": [], "hypotheses": [], "reproScope": "", "referencesReviewed": [], "nextSteps": [], "openQuestions": [], "toolsUsed": { "preferred": {}, "mcpServers": [] } })
}

fn str_of(v: &Value) -> String {
    if js_truthy(Some(v)) { js_to_string(v) } else { String::new() }
}

fn as_string_array(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().map(|x| js_trim(&if x.is_null() { String::new() } else { js_to_string(x) }).to_string()).filter(|s| !s.is_empty()).collect()
}

/// `{<server>: bool}` from a toolsUsed object. Older saved analyses kept each flag at the top level.
fn preferred_flags(tools_used: &Map<String, Value>) -> Map<String, Value> {
    let mut flags = Map::new();
    for (k, v) in tools_used {
        if let Value::Bool(_) = v {
            flags.insert(k.clone(), v.clone());
        }
    }
    if let Some(p) = tools_used.get("preferred").and_then(Value::as_object) {
        for (k, v) in p {
            flags.insert(k.clone(), json!(js_truthy(Some(v))));
        }
    }
    flags
}

fn objects(v: &Value) -> Vec<&Value> {
    v.as_array().into_iter().flatten().filter(|x| x.is_object()).collect()
}

fn normalize_analysis(parsed: &Value) -> Value {
    if !parsed.is_object() {
        return empty_analysis();
    }
    let affected: Vec<Value> = objects(&parsed["affectedArea"]).iter().map(|a| json!({ "path": str_of(&a["path"]), "symbol": str_of(&a["symbol"]), "why": str_of(&a["why"]) })).collect();
    let hypotheses: Vec<Value> = objects(&parsed["hypotheses"])
        .iter()
        .map(|h| {
            let conf = if js_truthy(Some(&h["confidence"])) { js_to_string(&h["confidence"]).to_lowercase() } else { "medium".to_string() };
            json!({ "hypothesis": str_of(&h["hypothesis"]), "confidence": if ["high", "medium", "low"].contains(&conf.as_str()) { conf } else { "medium".into() }, "evidence": str_of(&h["evidence"]) })
        })
        .collect();
    let refs: Vec<Value> = objects(&parsed["referencesReviewed"]).iter().map(|r| json!({ "url": str_of(&r["url"]), "takeaway": str_of(&r["takeaway"]) })).collect();
    let tools = match parsed["toolsUsed"].as_object() {
        Some(t) => json!({ "preferred": preferred_flags(t), "mcpServers": as_string_array(&t.get("mcpServers").cloned().unwrap_or(Value::Null)) }),
        None => json!({ "preferred": {}, "mcpServers": [] }),
    };
    json!({
        "tldr": str_of(&parsed["tldr"]), "affectedArea": affected, "hypotheses": hypotheses, "reproScope": str_of(&parsed["reproScope"]),
        "referencesReviewed": refs, "nextSteps": as_string_array(&parsed["nextSteps"]), "openQuestions": as_string_array(&parsed["openQuestions"]), "toolsUsed": tools,
    })
}

static JSON_FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)```(?:json)?\s*((?s:.*?))```").unwrap());

/// Extract a fenced or raw JSON object from Claude's text; on failure `{raw}`.
pub fn parse_analysis(text: &str) -> Value {
    let raw = js_trim(text);
    if raw.is_empty() {
        return json!({ "raw": "" });
    }
    let mut candidates: Vec<String> = vec![];
    if let Some(f) = JSON_FENCE.captures(raw) {
        candidates.push(js_trim(&f[1]).to_string());
    }
    if let (Some(a), Some(b)) = (raw.find('{'), raw.rfind('}')) {
        if b > a {
            candidates.push(raw[a..=b].to_string());
        }
    }
    candidates.push(raw.to_string());
    for c in candidates {
        if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&c) {
            return normalize_analysis(&v);
        }
    }
    json!({ "raw": raw })
}

/// User-scope MCP server names from `claude_json_path` (normally `~/.claude.json`). Filters out
/// the companion's own MCP server to prevent headless Claude runs from recursively calling the
/// companion's services. `[]` if the file is missing or invalid: analysis still runs without MCP.
pub fn read_mcp_servers(claude_json_path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(claude_json_path) else { return vec![] };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else { return vec![] };
    parsed["mcpServers"].as_object().map(|m| m.keys().filter(|s| !s.is_empty() && *s != COMPANION_MCP_SERVER).cloned().collect()).unwrap_or_default()
}

pub fn build_summary(issue_key: &str, repo_key: Option<&str>, analysis: &Value) -> String {
    let repo_bit = repo_key.filter(|k| !k.is_empty()).map_or(" (no repo matched)".to_string(), |k| format!(" (repo {k})"));
    if let Some(tldr) = analysis["tldr"].as_str().filter(|t| !js_trim(t).is_empty()) {
        let flat = js_trim(tldr).split_whitespace().collect::<Vec<_>>().join(" ");
        let short = if crate::js_text::utf16_len(&flat) > 160 { format!("{}\u{2026}", slice_utf16(&flat, 157)) } else { flat };
        return format!("Analyzed {issue_key}{repo_bit}: {short}");
    }
    format!("Analyzed {issue_key}{repo_bit}.")
}

pub struct CommentInput<'a> {
    pub issue_key: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub repo_key: Option<&'a str>,
    pub repo_match: Option<&'a str>,
    pub analysis: &'a Value,
}

/// Format a structured (or raw) analysis as Markdown for the extension's edit-before-post compose
/// view. Posted comments convert this to Jira Server/DC wiki markup via
/// [`markdown_to_jira_wiki`] (API v2 string body, not ADF).
pub fn format_analysis_as_markdown_comment(i: &CommentInput<'_>) -> String {
    let key = i.issue_key.filter(|k| !k.is_empty()).unwrap_or("issue");
    let mut lines: Vec<String> = vec![format!("### AI issue analysis \u{2014} {}", md_one_line(key)), String::new(), format!("_Generated by {}. Treat as a starting point, not a verified root cause._", product_name()), String::new()];
    if let Some(s) = i.summary.filter(|s| !s.is_empty()) {
        lines.extend([format!("**Summary:** {}", md_one_line(s)), String::new()]);
    }
    match (i.repo_key.filter(|k| !k.is_empty()), i.repo_match.filter(|m| !m.is_empty())) {
        (Some(k), m) => lines.extend([format!("**Repo:** `{}`{}", md_one_line(k), m.map(|m| format!(" ({})", md_one_line(m))).unwrap_or_default()), String::new()]),
        (None, Some(m)) => lines.extend([format!("**Repo:** none matched ({})", md_one_line(m)), String::new()]),
        (None, None) => {}
    }
    let a = i.analysis;
    if !a.is_object() {
        lines.push("_No analysis content was available._".into());
        return lines.join("\n");
    }
    // Parse-failure path: only raw Claude text.
    if let Some(raw) = a["raw"].as_str().filter(|r| !r.is_empty()) {
        if !js_truthy(Some(&a["tldr"])) {
            lines.extend(["#### Raw analysis".to_string(), "```".to_string(), js_trim(raw).to_string(), "```".to_string()]);
            return lines.join("\n") + "\n";
        }
    }
    if js_truthy(Some(&a["tldr"])) {
        lines.extend(["#### Initial assessment".to_string(), md_one(&a["tldr"]), String::new()]);
    }
    let list = |k: &str| a[k].as_array().cloned().unwrap_or_default();
    let affected = list("affectedArea");
    if !affected.is_empty() {
        lines.push("#### Likely affected area".into());
        for e in &affected {
            let parts: Vec<String> = [&e["path"], &e["symbol"]].into_iter().filter(|v| js_truthy(Some(v))).map(md_one).collect();
            let where_ = if parts.is_empty() { "(unknown)".to_string() } else { parts.join(" \u{b7} ") };
            let why = if js_truthy(Some(&e["why"])) { format!(" \u{2014} {}", md_one(&e["why"])) } else { String::new() };
            lines.push(format!("- `{where_}`{why}"));
        }
        lines.push(String::new());
    }
    let hypotheses = list("hypotheses");
    if !hypotheses.is_empty() {
        lines.push("#### Root-cause hypotheses".into());
        for e in &hypotheses {
            let conf = if js_truthy(Some(&e["confidence"])) { format!("**[{}]** ", md_one(&e["confidence"])) } else { String::new() };
            let evidence = if js_truthy(Some(&e["evidence"])) { format!(" \u{2014} _{}_", md_one(&e["evidence"])) } else { String::new() };
            lines.push(format!("- {conf}{}{evidence}", md_one(&e["hypothesis"])));
        }
        lines.push(String::new());
    }
    if js_truthy(Some(&a["reproScope"])) {
        lines.extend(["#### Repro and scope".to_string(), md_one(&a["reproScope"]), String::new()]);
    }
    let refs = list("referencesReviewed");
    if !refs.is_empty() {
        lines.push("#### References reviewed".into());
        for e in &refs {
            let url = e["url"].as_str().map(|u| js_trim(u).to_string()).unwrap_or_default();
            let link = if url.is_empty() { "(no url)".to_string() } else { format!("[{url}]({url})") };
            let takeaway = if js_truthy(Some(&e["takeaway"])) { format!(" \u{2014} {}", md_one(&e["takeaway"])) } else { String::new() };
            lines.push(format!("- {link}{takeaway}"));
        }
        lines.push(String::new());
    }
    let steps = list("nextSteps");
    if !steps.is_empty() {
        lines.push("#### Suggested next steps".into());
        for (n, s) in steps.iter().enumerate() {
            lines.push(format!("{}. {}", n + 1, md_one(s)));
        }
        lines.push(String::new());
    }
    let questions = list("openQuestions");
    if !questions.is_empty() {
        lines.push("#### Open questions".into());
        lines.extend(questions.iter().map(|q| format!("- {}", md_one(q))));
        lines.push(String::new());
    }
    format!("{}\n", js_trim(&lines.join("\n")))
}

static CODE_FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"```([^\n]*)\n?((?s:.*?))```").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)\s]+)\)").unwrap());
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*]+)\*\*").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.*)$").unwrap());
static ORDERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]+)\.\s+(.*)$").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-*+]\s+(.*)$").unwrap());
static HELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x00INL([0-9]+)\x00").unwrap());
static HELD_FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x00FENCE([0-9]+)\x00").unwrap());

/// Replaces every `delim inner delim` where `inner` is a non-empty run of characters that are
/// neither `delim` nor a newline, and the characters just outside the pair satisfy `outside_ok`
/// (the lookbehind and lookahead of the regexes this stands in for).
fn replace_delimited(text: &str, delim: char, outside_ok: impl Fn(Option<char>) -> bool, mut f: impl FnMut(&str) -> String) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == delim && outside_ok(i.checked_sub(1).map(|p| chars[p])) {
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && chars[j] != delim && chars[j] != '\n' {
                j += 1;
            }
            if j > start && j < chars.len() && chars[j] == delim && outside_ok(chars.get(j + 1).copied()) {
                out.push_str(&f(&chars[start..j].iter().collect::<String>()));
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Convert a small Markdown subset to Jira Server/DC wiki markup for
/// `POST /rest/api/2/issue/{key}/comment` (plain string body, not ADF). Supports headings, bold,
/// italic, lists, links, inline code, and fences.
pub fn markdown_to_jira_wiki(md: &str) -> String {
    let src = md.replace("\r\n", "\n");
    if js_trim(&src).is_empty() {
        return String::new();
    }
    let mut fences: Vec<String> = vec![];
    let mut text = CODE_FENCE
        .replace_all(&src, |c: &regex::Captures<'_>| {
            let code = &c[2];
            fences.push(format!("{{code}}\n{}\n{{code}}", code.strip_suffix('\n').unwrap_or(code)));
            format!("\0FENCE{}\0", fences.len() - 1)
        })
        .into_owned();
    let mut inlines: Vec<String> = vec![];
    let mut hold = |wiki: String| {
        inlines.push(wiki);
        format!("\0INL{}\0", inlines.len() - 1)
    };
    // Links before other inline so [text](url) isn't mangled by bold/italic.
    text = LINK.replace_all(&text, |c: &regex::Captures<'_>| hold(format!("[{}|{}]", &c[1], &c[2]))).into_owned();
    text = INLINE_CODE.replace_all(&text, |c: &regex::Captures<'_>| hold(format!("{{{{{}}}}}", c[1].replace("{{", "{ {")))).into_owned();
    text = BOLD.replace_all(&text, |c: &regex::Captures<'_>| hold(format!("*{}*", &c[1]))).into_owned();
    text = replace_delimited(&text, '*', |c| c != Some('*'), |inner| hold(format!("_{inner}_")));
    text = replace_delimited(&text, '_', |c| !c.is_some_and(|c| c.is_ascii_alphanumeric()), |inner| hold(format!("_{inner}_")));

    let mut out: Vec<String> = vec![];
    for line in text.split('\n') {
        if let Some(h) = HEADING.captures(line) {
            out.push(format!("h{}. {}", h[1].len(), &h[2]));
        } else if let Some(o) = ORDERED.captures(line) {
            out.push(format!("# {}", &o[2]));
        } else if let Some(u) = BULLET.captures(line) {
            out.push(format!("* {}", &u[1]));
        } else {
            out.push(line.to_string());
        }
    }
    let mut result = out.join("\n");
    result = HELD.replace_all(&result, |c: &regex::Captures<'_>| inlines.get(c[1].parse::<usize>().unwrap_or(usize::MAX)).cloned().unwrap_or_default()).into_owned();
    result = HELD_FENCE.replace_all(&result, |c: &regex::Captures<'_>| fences.get(c[1].parse::<usize>().unwrap_or(usize::MAX)).cloned().unwrap_or_default()).into_owned();
    let trimmed = js_trim(&result);
    format!("{trimmed}{}", if trimmed.is_empty() { "" } else { "\n" })
}

/// The Markdown comment as Jira wiki markup.
pub fn format_analysis_as_wiki_comment(i: &CommentInput<'_>) -> String {
    markdown_to_jira_wiki(&format_analysis_as_markdown_comment(i))
}

/// Durable analyses live next to config.json under analysis-cache/.
pub fn analysis_cache_dir(root_dir: &Path) -> PathBuf {
    root_dir.join("analysis-cache")
}

pub fn analysis_cache_path(issue_key: &str, root_dir: &Path) -> Result<PathBuf, String> {
    let (key, _) = parse_issue_key(issue_key)?;
    Ok(analysis_cache_dir(root_dir).join(format!("{key}.json")))
}

/// True when a cache entry has the fields needed to reopen the report panel without re-running
/// Claude. `analysis` must be a non-empty object (structured or `{raw}`).
pub fn is_valid_analysis_cache(entry: &Value, expected_key: Option<&str>) -> bool {
    let Some(key) = entry["issueKey"].as_str().filter(|k| !k.is_empty()) else { return false };
    if expected_key.is_some_and(|e| key.to_uppercase() != e.to_uppercase()) {
        return false;
    }
    entry["analysis"].is_object() && entry["completedAt"].as_str().is_some_and(|c| !c.is_empty())
}

/// A cache file is read back as plain JSON, not the typed session, so a hand-edited or
/// pre-this-feature file could have anything under `claudeSession`: dropped unless it has the exact
/// shape a real session has.
fn sanitize_claude_session(raw: &Value) -> Option<Value> {
    let id = raw["id"].as_str().filter(|s| !s.is_empty())?;
    let cwd = raw["cwd"].as_str().filter(|s| !s.is_empty())?;
    let mode = raw["permissionMode"].as_str().filter(|m| ["plan", "auto", "default"].contains(m))?;
    Some(json!({ "id": id, "cwd": cwd, "permissionMode": mode }))
}

/// Load a cached analysis for `issue_key`, or `None` if missing/invalid.
pub fn read_analysis_cache(issue_key: &str, root_dir: &Path) -> Option<Value> {
    let (key, _) = parse_issue_key(issue_key).ok()?;
    let parsed: Value = serde_json::from_str(&std::fs::read_to_string(analysis_cache_path(&key, root_dir).ok()?).ok()?).ok()?;
    if !is_valid_analysis_cache(&parsed, Some(&key)) {
        return None;
    }
    let mut out = json!({
        "issueKey": key,
        "summary": parsed["summary"].as_str().unwrap_or(""),
        "repoKey": if parsed["repoKey"].is_null() { Value::Null } else { json!(js_to_string(&parsed["repoKey"])) },
        "repoMatch": if parsed["repoMatch"].is_null() { Value::Null } else { json!(js_to_string(&parsed["repoMatch"])) },
        "analysis": parsed["analysis"],
        "completedAt": parsed["completedAt"],
    });
    if let Some(s) = sanitize_claude_session(&parsed["claudeSession"]) {
        out["claudeSession"] = s;
    }
    Some(out)
}

/// Persist a successful analysis next to config.json.
pub fn write_analysis_cache(entry: &Value, root_dir: &Path) -> Result<(), String> {
    let (key, _) = parse_issue_key(entry["issueKey"].as_str().unwrap_or(""))?;
    let mut with_key = entry.clone();
    with_key["issueKey"] = json!(key);
    if !is_valid_analysis_cache(&with_key, Some(&key)) {
        return Err("writeAnalysisCache: entry is missing required fields".into());
    }
    let mut payload = json!({
        "issueKey": key,
        "summary": entry["summary"].as_str().unwrap_or(""),
        "repoKey": if entry["repoKey"].is_null() { Value::Null } else { entry["repoKey"].clone() },
        "repoMatch": if entry["repoMatch"].is_null() { Value::Null } else { entry["repoMatch"].clone() },
        "analysis": entry["analysis"],
        "completedAt": entry["completedAt"],
    });
    if let Some(s) = sanitize_claude_session(&entry["claudeSession"]) {
        payload["claudeSession"] = s;
    }
    std::fs::create_dir_all(analysis_cache_dir(root_dir)).map_err(|e| e.to_string())?;
    std::fs::write(analysis_cache_path(&key, root_dir)?, serde_json::to_string_pretty(&payload).expect("a value serializes") + "\n").map_err(|e| e.to_string())
}

pub const SIMILAR_TIMEOUT: Duration = Duration::from_millis(4000);
const SIMILAR_ASK: f64 = 20.0;
const SIMILAR_KEEP: usize = 5;

/// The similar past tickets for a run's prompt: asks for more than will be shown (PR results are
/// dropped by the prompt block, so asking for exactly five could leave fewer), keeps the tickets,
/// at most five, and gives up after `timeout`. Never fails: any failure or timeout is an empty
/// list and one `warn` line.
pub async fn fetch_similar_tickets(find: Option<&dyn FindSimilar>, key: Option<&str>, text: Option<&str>, timeout: Duration, warn: &(dyn Fn(&str) + Sync)) -> Vec<Value> {
    let Some(find) = find else { return vec![] };
    match tokio::time::timeout(timeout, find.find(key.map(String::from), text.map(String::from), Some(SIMILAR_ASK))).await {
        Err(_) => {
            warn("timed out");
            vec![]
        }
        Ok(Err(e)) => {
            warn(&e);
            vec![]
        }
        Ok(Ok(result)) => result["items"].as_array().into_iter().flatten().filter(|i| i["key"].as_str().is_some_and(|k| k.starts_with("jira:"))).take(SIMILAR_KEEP).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::FindSimilar;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use GitRemoteRules::BitbucketDc as BB;

    const BASE: &str = "https://bitbucket.example.com";

    fn candidates(issue: &Value, repos: Value, map: Value) -> Vec<Candidate> {
        candidate_repos_from_ticket(issue, &repos, &map, &[], BB)
    }

    #[test]
    fn parse_issue_key_upper_cases_and_splits_the_project_key() {
        assert_eq!(parse_issue_key("proj-34034"), Ok(("PROJ-34034".into(), "PROJ".into())));
        assert!(parse_issue_key("nope").unwrap_err().contains("doesn't look like a Jira issue key"));
    }

    #[test]
    fn extract_urls_pulls_from_description_comments_environment_and_remote_links() {
        let issue = json!({
            "fields": { "description": "See https://example.com/a and also https://example.com/a again.", "environment": "Log at https://example.com/env", "comment": { "comments": [{ "body": "Related: https://jira.example/browse/PROJ-1" }] } },
            "remoteLinks": [{ "object": { "url": "https://bitbucket.example/projects/PROJ/repos/sample-app" } }],
        });
        assert_eq!(extract_urls(&issue), ["https://example.com/a", "https://example.com/env", "https://jira.example/browse/PROJ-1", "https://bitbucket.example/projects/PROJ/repos/sample-app"]);
    }

    #[test]
    fn candidates_rank_links_then_mappings_then_name_matches() {
        let repos = json!({ "PROJ/sample-app": "/tmp/sample-app", "PROJ/other": "/tmp/other" });
        let issue = json!({ "fields": { "description": "Code in https://bitbucket.example/projects/PROJ/repos/sample-app/browse", "components": [{ "name": "other" }] } });
        let ranked = candidates(&issue, repos, json!({}));
        assert_eq!((ranked[0].repo_key.as_str(), ranked[0].score), ("PROJ/sample-app", 100));
        assert!(ranked[0].reason.contains("Repository link PROJ/sample-app"));
        let mapped = candidates(&json!({ "fields": { "components": [{ "name": "REST API" }], "labels": [] } }), json!({ "PROJ/sample-app-rest": "/tmp/r" }), json!({ "REST API": "PROJ/sample-app-rest" }));
        assert_eq!((mapped.len(), mapped[0].repo_key.as_str(), mapped[0].score), (1, "PROJ/sample-app-rest", 80));
        let named = candidates(&json!({ "fields": { "labels": ["sample-app-browser"], "components": [] } }), json!({ "CI/sample-app-browser": "/tmp/b" }), json!({}));
        assert_eq!((named.len(), named[0].repo_key.as_str(), named[0].score), (1, "CI/sample-app-browser", 60));
        assert!(candidates(&json!({ "fields": { "description": "no links", "components": [], "labels": [] } }), json!({ "PROJ/sample-app": "/tmp/s" }), json!({})).is_empty());
    }

    fn cand(key: &str, score: u32) -> Candidate {
        Candidate { repo_key: key.into(), score, reason: String::new() }
    }

    #[test]
    fn is_strong_repo_match_needs_a_unique_top_score() {
        assert!(is_strong_repo_match(&[cand("a", 100)]));
        assert!(is_strong_repo_match(&[cand("a", 100), cand("b", 60)]));
        assert!(!is_strong_repo_match(&[cand("a", 80), cand("b", 80)]));
        assert!(!is_strong_repo_match(&[]));
    }

    #[test]
    fn parse_repo_pick_accepts_an_exact_key_any_case_or_none() {
        let keys = vec!["PROJ/sample-app".to_string(), "PROJ/other".to_string()];
        assert_eq!(parse_repo_pick("PROJ/sample-app", &keys).as_deref(), Some("PROJ/sample-app"));
        assert_eq!(parse_repo_pick("proj/sample-app\n", &keys).as_deref(), Some("PROJ/sample-app"));
        assert_eq!(parse_repo_pick("```\n\"PROJ/other\"\n```", &keys).as_deref(), Some("PROJ/other"));
        assert_eq!((parse_repo_pick("none", &keys), parse_repo_pick("not-a-repo", &keys), parse_repo_pick("", &keys)), (None, None, None));
    }

    #[test]
    fn parse_analysis_extracts_fenced_json_fills_defaults_and_falls_back_to_raw() {
        let r = parse_analysis("Here you go:\n```json\n{\"tldr\":\"Bug in foo\",\"hypotheses\":[{\"hypothesis\":\"null deref\",\"confidence\":\"HIGH\",\"evidence\":\"stack\"}]}\n```");
        assert_eq!((r["tldr"].clone(), r["hypotheses"][0]["confidence"].clone()), (json!("Bug in foo"), json!("high")));
        assert_eq!((r["affectedArea"].clone(), r["nextSteps"].clone(), r["toolsUsed"]["preferred"].clone()), (json!([]), json!([]), json!({})));
        let raw = parse_analysis("sorry, I could not produce JSON");
        assert_eq!(raw, json!({ "raw": "sorry, I could not produce JSON" }));
        assert_eq!(parse_analysis(""), json!({ "raw": "" }));
        let flags = parse_analysis("{\"toolsUsed\":{\"wiki\":true,\"preferred\":{\"jira\":1},\"mcpServers\":[\"a\",\" b \",null]}}");
        assert_eq!(flags["toolsUsed"], json!({ "preferred": { "wiki": true, "jira": true }, "mcpServers": ["a", "b"] }));
        assert_eq!(parse_analysis("[1,2]"), json!({ "raw": "[1,2]" }));
    }

    #[test]
    fn read_mcp_servers_names_the_servers_and_never_the_companions_own() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("claude.json");
        std::fs::write(&file, json!({ "mcpServers": { "wiki": {}, "acme-bitbucket": {}, "ai-companion": {} } }).to_string()).unwrap();
        let mut got = read_mcp_servers(&file);
        got.sort();
        assert_eq!(got, ["acme-bitbucket", "wiki"]);
        assert!(read_mcp_servers(&d.path().join("missing.json")).is_empty());
        std::fs::write(&file, "{not json").unwrap();
        assert!(read_mcp_servers(&file).is_empty());
    }

    #[test]
    fn build_summary_prefers_the_analysis_tldr() {
        let s = build_summary("PROJ-1", Some("PROJ/sample-app"), &json!({ "tldr": "Likely a race in the cache layer." }));
        assert!(s.contains("PROJ-1") && s.contains("PROJ/sample-app") && s.contains("race in the cache"));
        assert_eq!(build_summary("PROJ-1", None, &json!({})), "Analyzed PROJ-1 (no repo matched).");
        assert!(build_summary("P-1", None, &json!({ "tldr": "x".repeat(300) })).ends_with("\u{2026}"));
    }

    fn entry(key: &str, extra: Value) -> Value {
        let mut e = json!({ "issueKey": key, "summary": "Something broke", "repoKey": "PROJ/sample-app", "repoMatch": "Bitbucket link", "analysis": { "tldr": "Null deref in Foo" }, "completedAt": "2026-09-24T12:00:00.000Z" });
        for (k, v) in extra.as_object().into_iter().flatten() {
            e[k] = v.clone();
        }
        e
    }

    #[test]
    fn the_analysis_cache_round_trips_with_and_without_a_claude_session_and_drops_a_malformed_one() {
        let root = tempfile::tempdir().unwrap();
        write_analysis_cache(&entry("proj-99", json!({})), root.path()).unwrap();
        let loaded = read_analysis_cache("PROJ-99", root.path()).unwrap();
        assert_eq!((loaded["issueKey"].clone(), loaded["summary"].clone(), loaded["repoKey"].clone(), loaded["repoMatch"].clone(), loaded["analysis"]["tldr"].clone()), (json!("PROJ-99"), json!("Something broke"), json!("PROJ/sample-app"), json!("Bitbucket link"), json!("Null deref in Foo")));
        assert!(loaded.get("claudeSession").is_none());
        assert_eq!(read_analysis_cache("PROJ-1", root.path()), None);

        let session = json!({ "id": "session-abc", "cwd": "/tmp/sample-app", "permissionMode": "plan" });
        write_analysis_cache(&entry("proj-100", json!({ "claudeSession": session })), root.path()).unwrap();
        assert_eq!(read_analysis_cache("PROJ-100", root.path()).unwrap()["claudeSession"], session);

        for (key, bad) in [("PROJ-102", json!({ "cwd": "/tmp/x", "permissionMode": "plan" })), ("PROJ-103", json!({ "id": "s1", "cwd": "/tmp/x", "permissionMode": "bypassPermissions" })), ("PROJ-104", json!({ "id": "s1", "cwd": 42, "permissionMode": "default" }))] {
            let path = analysis_cache_path(key, root.path()).unwrap();
            std::fs::write(&path, entry(key, json!({ "claudeSession": bad })).to_string()).unwrap();
            assert!(read_analysis_cache(key, root.path()).unwrap().get("claudeSession").is_none(), "{key}");
        }
        let legacy = analysis_cache_path("PROJ-101", root.path()).unwrap();
        std::fs::write(&legacy, json!({ "issueKey": "PROJ-101", "summary": "Legacy entry", "analysis": { "tldr": "x" }, "completedAt": "2026-01-01T00:00:00.000Z" }).to_string()).unwrap();
        assert_eq!(read_analysis_cache("PROJ-101", root.path()).unwrap()["summary"], "Legacy entry");
    }

    #[test]
    fn the_cache_reads_null_for_missing_or_invalid_files_and_refuses_an_incomplete_write() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_analysis_cache("PROJ-1", root.path()), None);
        std::fs::create_dir_all(analysis_cache_dir(root.path())).unwrap();
        std::fs::write(analysis_cache_path("PROJ-2", root.path()).unwrap(), "{not json").unwrap();
        assert_eq!(read_analysis_cache("PROJ-2", root.path()), None);
        std::fs::write(analysis_cache_path("PROJ-3", root.path()).unwrap(), json!({ "issueKey": "PROJ-3", "analysis": { "tldr": "x" } }).to_string()).unwrap();
        assert_eq!(read_analysis_cache("PROJ-3", root.path()), None);
        assert!(write_analysis_cache(&json!({ "issueKey": "PROJ-4" }), root.path()).unwrap_err().contains("missing required fields"));
        assert!(write_analysis_cache(&json!({ "issueKey": "nope" }), root.path()).is_err());
        let v = json!({ "issueKey": "PROJ-1", "analysis": { "tldr": "ok" }, "completedAt": "2026-01-01T00:00:00.000Z" });
        assert!(is_valid_analysis_cache(&v, None) && !is_valid_analysis_cache(&Value::Null, None));
        assert!(!is_valid_analysis_cache(&json!({ "issueKey": "PROJ-2", "analysis": { "tldr": "ok" }, "completedAt": "2026-01-01T00:00:00.000Z" }), Some("PROJ-1")));
    }

    fn comment_of(issue_key: &str, summary: Option<&str>, repo_key: Option<&str>, repo_match: Option<&str>, analysis: &Value) -> String {
        format_analysis_as_markdown_comment(&CommentInput { issue_key: Some(issue_key), summary, repo_key, repo_match, analysis })
    }

    #[test]
    fn the_markdown_comment_is_readable_with_the_ai_note_and_falls_back_to_raw_fenced_text() {
        let body = comment_of("PROJ-1", Some("Null deref"), Some("PROJ/sample-app"), Some("Bitbucket link"), &json!({
            "tldr": "Likely NPE in Foo.bar", "affectedArea": [{ "path": "src/Foo.java", "symbol": "bar", "why": "stack points here" }],
            "hypotheses": [{ "hypothesis": "missing null check", "confidence": "high", "evidence": "NPE at L42" }], "reproScope": "Only on tenant X",
            "referencesReviewed": [{ "url": "https://example.com/doc", "takeaway": "mentions Foo" }], "nextSteps": ["Add null guard", "Add unit test"], "openQuestions": ["Is tenant X on a fork?"],
        }));
        for needle in [product_name(), "### AI issue analysis \u{2014} PROJ-1", "**Summary:** Null deref", "`PROJ/sample-app`", "#### Initial assessment", "Likely NPE in Foo.bar", "missing null check", "1. Add null guard", "[https://example.com/doc](https://example.com/doc)", "- `src/Foo.java \u{b7} bar` \u{2014} stack points here", "- **[high]** missing null check \u{2014} _NPE at L42_"] {
            assert!(body.contains(needle), "{needle}");
        }
        let raw = comment_of("PROJ-2", None, None, None, &json!({ "raw": "Could not produce JSON\nline 2" }));
        assert!(raw.contains("```") && raw.contains("Could not produce JSON"));
        assert!(comment_of("PROJ-2", None, None, Some("none fit"), &json!({ "tldr": "x" })).contains("**Repo:** none matched (none fit)"));
        assert!(comment_of("PROJ-2", None, None, None, &Value::Null).contains("_No analysis content was available._"));
    }

    #[test]
    fn markdown_to_jira_wiki_converts_headings_emphasis_lists_links_and_code() {
        let wiki = markdown_to_jira_wiki(&["### Title", "", "_italic note_", "", "**Summary:** value", "", "#### Section", "- `path/file` \u{2014} why", "- **[high]** hyp \u{2014} _ev_", "1. First step", "2. Second step", "- [https://ex.com](https://ex.com) \u{2014} takeaway", "", "```", "raw line", "```", ""].join("\n"));
        for needle in ["h3. Title", "h4. Section", "_italic note_", "*Summary:* value", "* {{path/file}}", "* *[high]* hyp \u{2014} _ev_", "# First step", "# Second step", "[https://ex.com|https://ex.com]", "{code}\nraw line\n{code}"] {
            assert!(wiki.contains(needle), "{needle} in {wiki}");
        }
        assert_eq!(markdown_to_jira_wiki("  "), "");
        assert_eq!(markdown_to_jira_wiki("a * b * c and snake_case_name"), "a _ b _ c and snake_case_name\n", "as the Node regex does: spaces inside are still a pair");
        assert_eq!(markdown_to_jira_wiki("*one* and **two**"), "_one_ and *two*\n");
        let round = format_analysis_as_wiki_comment(&CommentInput { issue_key: Some("PROJ-1"), summary: Some("Null deref"), repo_key: Some("PROJ/sample-app"), repo_match: None, analysis: &json!({ "tldr": "Likely NPE", "nextSteps": ["Add null guard"] }) });
        assert!(round.contains(product_name()) && round.contains("h3. AI issue analysis \u{2014} PROJ-1") && round.contains("Likely NPE") && round.contains("# Add null guard"));
    }

    #[test]
    fn background_runs_are_strict_and_never_read_the_mcp_config() {
        let read = || vec!["wiki".to_string(), "jira".to_string()];
        assert_eq!(analysis_run_options(false, read), AnalysisRunOptions { policy: "readOnly", mcp_servers: vec!["wiki".into(), "jira".into()], extra_allowed_tools: vec!["mcp__wiki".into(), "mcp__jira".into()], use_urls: true });
        let called = Mutex::new(false);
        let bg = analysis_run_options(true, || {
            *called.lock().unwrap() = true;
            vec![]
        });
        assert_eq!(bg, AnalysisRunOptions { policy: "readOnlyBackground", mcp_servers: vec![], extra_allowed_tools: vec![], use_urls: false });
        assert!(!*called.lock().unwrap());
    }

    fn prompt_args<'a>(issue: &'a Value, urls: &'a [String], hints: &'a [ServerHint], mcp: &'a [String], similar: &'a [Value], background: bool) -> AnalysisPromptInput<'a> {
        AnalysisPromptInput { issue, urls, repo_key: None, mcp_servers: mcp, server_hints: hints, background, similar }
    }

    #[test]
    fn the_background_prompt_drops_the_mcp_webfetch_and_link_lines_the_interactive_one_keeps_them() {
        let issue = json!({ "key": "PROJ-7", "fields": { "summary": "Login broken", "description": "See https://wiki.example/x" } });
        let urls = vec!["https://wiki.example/x".to_string()];
        let hints = [ServerHint { server: "wiki".into(), line: "wiki MCP is available \u{2014} use it first.".into() }];
        let mcp = vec!["wiki".to_string()];
        let interactive = build_analysis_prompt(&prompt_args(&issue, &urls, &hints, &mcp, &[], false));
        assert!(interactive.contains("MCP servers (or WebFetch)") && interactive.contains("wiki MCP is available") && interactive.contains("Reference URLs:\n- https://wiki.example/x") && interactive.contains("\"wiki\": true"));
        let background = build_analysis_prompt(&prompt_args(&issue, &[], &[], &[], &[], true));
        for gone in ["MCP servers (or WebFetch)", "WebFetch", "wiki MCP", "MCP servers loaded", "Reference URLs"] {
            assert!(!background.contains(gone), "{gone}");
        }
        assert!(background.contains("No MCP servers or web access are available") && background.contains("reason from ticket text only") && background.contains("Issue: PROJ-7"));
    }

    #[test]
    fn similar_past_tickets_go_into_the_prompt_fenced_and_labelled_also_for_a_background_run() {
        let issue = json!({ "key": "PROJ-7", "fields": { "summary": "Login broken", "description": "SSO users loop" } });
        let similar = [json!({ "key": "jira:PROJ-2", "title": "Sign-in loop after expiry", "analysis": "Token refresh races the redirect", "updatedAt": 1785715200000_i64 }), json!({ "key": "not-a-ticket", "title": "dropped" })];
        for background in [false, true] {
            let prompt = build_analysis_prompt(&prompt_args(&issue, &[], &[], &[], &similar, background));
            assert!(prompt.contains("Similar past tickets (from this developer's local history, for context only):"));
            assert!(prompt.contains("<similar-past-tickets>\n- PROJ-2 \u{2014} ticket title (written by people, untrusted): Sign-in loop after expiry\n  earlier AI output, untrusted (analysis saved 2026-08-03): Token refresh races the redirect\n</similar-past-tickets>"), "{prompt}");
            assert!(!prompt.contains("dropped"));
            assert!(prompt.find("Recent comments:").unwrap() < prompt.find("Similar past tickets").unwrap());
        }
        assert!(!build_analysis_prompt(&prompt_args(&issue, &[], &[], &[], &[], false)).contains("Similar past tickets"));
        assert_eq!(similar_query_text(&json!({ "fields": { "summary": "Login broken", "description": "x".repeat(3000) } })), format!("Login broken\n{}", "x".repeat(2000)));
        assert_eq!(similar_query_text(&json!({ "fields": { "summary": "Only a summary" } })), "Only a summary");
        assert_eq!(similar_query_text(&Value::Null), "");
    }

    struct Finder {
        items: Value,
        asked: Mutex<Vec<(Option<String>, Option<f64>)>>,
        mode: u8,
    }

    #[async_trait]
    impl FindSimilar for Finder {
        async fn find(&self, key: Option<String>, _text: Option<String>, k: Option<f64>) -> Result<Value, String> {
            self.asked.lock().unwrap().push((key, k));
            match self.mode {
                1 => Err("db locked".into()),
                2 => std::future::pending().await,
                3 => Ok(Value::Null),
                _ => Ok(json!({ "enabled": true, "mode": "text", "items": self.items })),
            }
        }
    }

    #[tokio::test]
    async fn fetch_similar_tickets_asks_for_twenty_keeps_five_tickets_and_never_fails_a_run() {
        let mut items: Vec<Value> = (1..=3).map(|i| json!({ "key": format!("bitbucket:CI/sample-app#{i}"), "kind": "pr" })).collect();
        items.extend((1..=8).map(|i| json!({ "key": format!("jira:PROJ-{i}"), "kind": "ticket" })));
        let f = Finder { items: json!(items), asked: Mutex::new(vec![]), mode: 0 };
        let got = fetch_similar_tickets(Some(&f), Some("PROJ-9"), Some("x"), SIMILAR_TIMEOUT, &|_| {}).await;
        assert_eq!(f.asked.lock().unwrap()[0].1, Some(20.0));
        assert_eq!(got.iter().map(|i| i["key"].as_str().unwrap()).collect::<Vec<_>>(), ["jira:PROJ-1", "jira:PROJ-2", "jira:PROJ-3", "jira:PROJ-4", "jira:PROJ-5"]);

        let warned = Mutex::new(vec![]);
        let warn = |m: &str| warned.lock().unwrap().push(m.to_string());
        for mode in [1, 3] {
            let f = Finder { items: json!([]), asked: Mutex::new(vec![]), mode };
            assert!(fetch_similar_tickets(Some(&f), Some("PROJ-1"), Some("x"), SIMILAR_TIMEOUT, &warn).await.is_empty());
        }
        assert!(fetch_similar_tickets(None, Some("PROJ-1"), Some("x"), SIMILAR_TIMEOUT, &warn).await.is_empty());
        let hang = Finder { items: json!([]), asked: Mutex::new(vec![]), mode: 2 };
        let started = std::time::Instant::now();
        assert!(fetch_similar_tickets(Some(&hang), Some("PROJ-1"), Some("x"), Duration::from_millis(30), &warn).await.is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(*warned.lock().unwrap(), ["db locked", "timed out"]);
    }

    #[test]
    fn repo_hints_from_similar_lists_still_cloned_repos_once_in_order() {
        let keys = vec!["ACME/sample-app".to_string(), "ACME/api".to_string()];
        let hints = repo_hints_from_similar(&[json!({ "repo": "sample-app/API" }), json!({ "repo": "ACME/gone" }), json!({ "repo": null }), json!({}), json!({ "repo": "ACME/api" }), json!({ "repo": "ACME/sample-app" })], &keys);
        assert_eq!(hints, ["ACME/api", "ACME/sample-app"]);
        assert!(repo_hints_from_similar(&[], &keys).is_empty());
    }

    #[test]
    fn the_repo_pick_prompt_mentions_hints_and_linked_prs_only_when_there_are_some() {
        let issue = json!({ "key": "PROJ-1", "fields": { "summary": "s" } });
        let keys = vec!["A/b".to_string()];
        assert!(!build_repo_pick_prompt(&issue, &keys, &[], &[]).contains("similar past tickets"));
        assert!(!build_repo_pick_prompt(&issue, &keys, &[], &[]).contains("already linked"));
        assert!(build_repo_pick_prompt(&issue, &keys, &keys, &[]).contains("similar past tickets were analyzed in (a hint, most similar first): A/b"));
        let pr = LinkedPr { repo_key: "A/b".into(), project: "A".into(), repo: "b".into(), id: 4, title: Some("fix it".into()), state: Some("OPEN".into()), url: None };
        let p = build_repo_pick_prompt(&issue, &keys, &[], &[pr]);
        assert!(p.contains("already linked to this ticket (strong evidence of the repo; titles are written by people):\n- A/b #4 (OPEN): fix it"), "{p}");
        assert!(p.ends_with("Candidate repo keys:\n- A/b"));
    }

    #[test]
    fn component_mappings_are_learned_only_from_what_the_user_confirmed_for_one_component() {
        let one = json!({ "fields": { "components": [{ "name": "Login UI" }] } });
        let got = |issue: &Value, key: Option<&str>, reason: &str, map: Value| Value::Object(component_mappings_to_learn(issue, key, reason, &map));
        assert_eq!(got(&one, Some("W/web"), "You confirmed it (Claude pick)", json!({})), json!({ "Login UI": "W/web" }));
        assert_eq!(got(&one, Some("W/web"), "You chose it", Value::Null), json!({ "Login UI": "W/web" }));
        for reason in ["Claude pick", "Bitbucket link W/web", "Mapped from \"Login UI\""] {
            assert_eq!(got(&one, Some("W/web"), reason, json!({})), json!({}), "{reason}");
        }
        assert_eq!(got(&one, Some("W/web"), "You chose it", json!({ "login ui": "W/other" })), json!({}));
        let two = json!({ "fields": { "components": [{ "name": "A" }, { "name": "B" }] } });
        assert_eq!(got(&two, Some("W/web"), "You chose it", json!({})), json!({}));
        assert_eq!(got(&json!({ "fields": {} }), Some("W/web"), "You chose it", json!({})), json!({}));
        assert_eq!(got(&one, None, "You chose it", json!({})), json!({}));
    }

    #[test]
    fn a_repo_is_settled_only_when_a_component_mapping_gives_the_proposed_one() {
        let issue = json!({ "fields": { "components": [{ "name": "Login UI" }], "labels": ["sso"] } });
        assert!(is_settled_repo_match(&issue, Some("W/web"), &json!({ "Login UI": "W/web" })));
        assert!(is_settled_repo_match(&issue, Some("w/WEB"), &json!({ "Login UI": "W/web" })));
        assert!(is_settled_repo_match(&issue, Some("W/web"), &json!({ "sso": "W/web" })));
        assert!(!is_settled_repo_match(&issue, Some("W/api"), &json!({ "Login UI": "W/web" })));
        assert!(!is_settled_repo_match(&issue, Some("W/web"), &json!({})) && !is_settled_repo_match(&issue, Some("W/web"), &Value::Null));
        assert!(!is_settled_repo_match(&issue, None, &json!({ "Login UI": "W/web" })) && !is_settled_repo_match(&json!({ "fields": {} }), Some("W/web"), &json!({ "Login UI": "W/web" })));
    }

    #[test]
    fn repo_choice_options_list_the_proposal_first_then_candidates_hints_and_the_rest_each_once() {
        let repo_keys: Vec<String> = ["W/zed", "W/api", "W/web", "W/docs"].iter().map(|s| s.to_string()).collect();
        let cands = [Candidate { repo_key: "W/api".into(), score: 1, reason: "Name match".into() }, Candidate { repo_key: "W/web".into(), score: 1, reason: "Name match".into() }, Candidate { repo_key: "W/gone".into(), score: 1, reason: "x".into() }];
        let hints = vec!["W/docs".to_string(), "W/api".to_string()];
        let out = repo_choice_options(&ChoiceInput { proposal: Some("W/web"), proposal_reason: Some("Claude pick"), candidates: &cands, hint_keys: &hints, repo_keys: &repo_keys, linked_prs: &[] });
        let pairs: Vec<(&str, &str)> = out.iter().map(|o| (o.repo_key.as_str(), o.reason.as_str())).collect();
        assert_eq!(pairs, [("W/web", "Claude pick"), ("W/api", "Name match"), ("W/docs", "used for a similar past ticket"), ("W/zed", "")]);
        let sorted = repo_choice_options(&ChoiceInput { proposal: None, proposal_reason: None, candidates: &[], hint_keys: &[], repo_keys: &["B/b".to_string(), "A/a".to_string()], linked_prs: &[] });
        assert_eq!(sorted.iter().map(|o| o.repo_key.as_str()).collect::<Vec<_>>(), ["A/a", "B/b"]);
        let many: Vec<String> = (0..50).map(|i| format!("P/r{i}")).collect();
        assert_eq!(repo_choice_options(&ChoiceInput { proposal: None, proposal_reason: None, candidates: &[], hint_keys: &[], repo_keys: &many, linked_prs: &[] }).len(), 30);
        assert_eq!(confirmed_reason(Some("W/web"), Some("W/web"), Some("Claude pick")), "You confirmed it (Claude pick)");
        assert_eq!(confirmed_reason(Some("W/web"), Some("W/web"), Some("")), "You confirmed it");
        assert_eq!(confirmed_reason(Some("W/api"), Some("W/web"), Some("Claude pick")), "You chose it");
        assert_eq!(confirmed_reason(Some("W/api"), None, None), "You chose it");
    }

    fn dev_status(prs: Vec<Value>) -> Value {
        json!({ "detail": [{ "pullRequests": prs }] })
    }

    #[test]
    fn linked_pull_requests_read_the_development_panel_and_pr_addresses_in_the_ticket_once_each() {
        let issue = json!({ "fields": { "description": format!("See {BASE}/projects/ACME/repos/sample-app/pull-requests/12/overview and {BASE}/projects/ACME/repos/api/pull-requests/7") }, "remoteLinks": [{ "object": { "url": format!("{BASE}/projects/ACME/repos/sample-app/pull-requests/12") } }] });
        let out = linked_pull_requests(&issue, &dev_status(vec![json!({ "url": format!("{BASE}/projects/ACME/repos/sample-app/pull-requests/12"), "name": "PROJ-1: fix\nlogin", "status": "merged" }), json!({ "url": "https://evil.example/x" }), Value::Null]), BASE, BB);
        let want = json!([
            { "repoKey": "ACME/sample-app", "project": "ACME", "repo": "sample-app", "id": 12, "title": "PROJ-1: fix login", "state": "MERGED", "url": format!("{BASE}/projects/ACME/repos/sample-app/pull-requests/12") },
            { "repoKey": "ACME/api", "project": "ACME", "repo": "api", "id": 7, "title": null, "state": null, "url": format!("{BASE}/projects/ACME/repos/api/pull-requests/7") },
        ]);
        assert_eq!(serde_json::to_value(&out).unwrap(), want);
    }

    #[test]
    fn linked_pull_requests_rebuild_the_link_on_the_configured_host_cap_the_list_and_cope_with_nothing() {
        let foreign = linked_pull_requests(&json!({}), &dev_status(vec![json!({ "url": "https://other.example/projects/ACME/repos/sample-app/pull-requests/3" })]), BASE, BB);
        assert_eq!(foreign[0].url.as_deref(), Some(&*format!("{BASE}/projects/ACME/repos/sample-app/pull-requests/3")));
        assert_eq!(linked_pull_requests(&json!({}), &dev_status(vec![json!({ "url": "https://o/projects/ACME/repos/sample-app/pull-requests/3" })]), "not a url", BB)[0].url, None);
        let many = dev_status((1..=30).map(|i| json!({ "url": format!("{BASE}/projects/ACME/repos/sample-app/pull-requests/{i}") })).collect());
        assert_eq!(linked_pull_requests(&json!({}), &many, BASE, BB).len(), 10);
        for bad in [Value::Null, json!({}), json!({ "detail": "x" }), json!({ "detail": [null, { "pullRequests": "x" }] })] {
            assert!(linked_pull_requests(&json!({}), &bad, BASE, BB).is_empty(), "{bad}");
        }
    }

    #[test]
    fn a_repo_with_a_linked_pr_outranks_every_other_signal_and_is_the_top_offered_option() {
        let repos = json!({ "ACME/sample-app": "/c/sample-app", "ACME/api": "/c/api" });
        let issue = json!({ "fields": { "components": [{ "name": "api" }] } });
        let prs = linked_pull_requests(&issue, &dev_status(vec![json!({ "url": format!("{BASE}/projects/acme/repos/Sample-App/pull-requests/12"), "name": "fix", "status": "OPEN" })]), BASE, BB);
        let c = candidate_repos_from_ticket(&issue, &repos, &json!({ "api": "ACME/api" }), &prs, BB);
        assert_eq!(c[0], Candidate { repo_key: "ACME/sample-app".into(), score: 120, reason: "Linked PR #12 in acme/Sample-App".into() });
        assert!(is_strong_repo_match(&c));
        let keys = vec!["ACME/sample-app".to_string(), "ACME/api".to_string()];
        let options = repo_choice_options(&ChoiceInput { proposal: Some("ACME/api"), proposal_reason: Some("Mapped"), candidates: &c, hint_keys: &[], repo_keys: &keys, linked_prs: &prs });
        assert_eq!(options.iter().map(|o| o.repo_key.as_str()).collect::<Vec<_>>(), ["ACME/sample-app", "ACME/api"]);
        assert_eq!(options[0], RepoChoice { repo_key: "ACME/sample-app".into(), reason: String::new() });
    }

    #[test]
    fn the_comment_summary_prompt_carries_the_analysis_and_the_assembled_comment_keeps_the_header_under_the_word_cap() {
        assert!(build_comment_summary_prompt("FULL TEXT").contains("<analysis>\nFULL TEXT\n</analysis>"));
        let short = assemble_summary_comment(Some("PROJ-1"), "```markdown\n- NPE in Foo.bar\n```");
        assert!(short.starts_with("### AI issue analysis \u{2014} PROJ-1\n") && short.contains(product_name()) && short.ends_with("- NPE in Foo.bar\n") && !short.contains("```"));
        let long = assemble_summary_comment(Some("PROJ-1"), &["word"; 600].join(" "));
        assert!(long.split_whitespace().count() <= COMMENT_SUMMARY_MAX_WORDS);
        assert!(long.ends_with("\u{2026}\n"));
        assert_eq!(assemble_summary_comment(Some("PROJ-1"), "  "), "");
        assert_eq!(COMMENT_SUMMARY_MODEL, "haiku");
    }
}
