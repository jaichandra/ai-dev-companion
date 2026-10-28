//! Pure logic for the ticket workspace (port of `features/ticket-workspace/plan.js`):
//! which history keys to follow, and how the live git scan, git-host reads and
//! history combine into one view of a ticket (worktrees, branches, PRs with build
//! state, analyses, sessions) plus what to offer next. The job runner does the I/O.

use crate::bitbucket_endpoints::is_safe_branch_name;
use crate::js_text::{js_trim, js_truthy};
use crate::prereqs::GitRemoteRules;
use crate::resume::validate_session;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;
use url::Url;

static ISSUE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]*-[1-9][0-9]*$").unwrap());
static PR_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:bitbucket|github):((?:[A-Za-z0-9]|\.[A-Za-z0-9])[A-Za-z0-9._-]*)/((?:[A-Za-z0-9]|\.[A-Za-z0-9])[A-Za-z0-9._-]*)#([1-9][0-9]*)$").unwrap()
});
const MAX_RELATED: usize = 20;
const MAX_SESSIONS: usize = 10;

/// `{issueKey}` -> the upper-cased key, or an error.
pub fn parse_workspace_payload(payload: &Value) -> Result<String, String> {
    match payload.get("issueKey").and_then(Value::as_str) {
        Some(raw) if ISSUE_KEY_RE.is_match(js_trim(raw)) => Ok(js_trim(raw).to_uppercase()),
        _ => Err("ticket-workspace payload must include an issueKey such as \"PROJ-1234\".".into()),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelatedKeys {
    pub worktree: Vec<String>,
    pub pr: Vec<String>,
    pub analysis: Vec<String>,
    pub job: Vec<String>,
    pub session: Vec<String>,
}

/// The neighbour keys of a history detail worth a second look, by kind.
pub fn related_keys(detail: &Value) -> RelatedKeys {
    let mut out = RelatedKeys::default();
    for e in detail["edges"].as_array().into_iter().flatten() {
        let (Some(key), Some(kind)) = (e["key"].as_str(), e["kind"].as_str()) else { continue };
        let list = match kind {
            "worktree" => &mut out.worktree,
            "pr" => &mut out.pr,
            "analysis" => &mut out.analysis,
            "job" => &mut out.job,
            "session" => &mut out.session,
            _ => continue,
        };
        if !list.iter().any(|k| k == key) {
            list.push(key.to_string());
        }
    }
    for list in [&mut out.worktree, &mut out.pr, &mut out.analysis, &mut out.job, &mut out.session] {
        list.truncate(MAX_RELATED);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrKey {
    pub project: String,
    pub repo: String,
    pub id: u64,
}

pub fn parse_pr_key(key: &str) -> Option<PrKey> {
    let m = PR_KEY_RE.captures(key)?;
    Some(PrKey { project: m[1].to_string(), repo: m[2].to_string(), id: m[3].parse().ok()? })
}

/// A branch name from the git host is untrusted: one that isn't safe becomes unknown (null).
fn safe_branch(name: &Value) -> Value {
    match name.as_str() {
        Some(n) if is_safe_branch_name(n) => json!(n),
        _ => Value::Null,
    }
}

/// Only a PR page of the git host on its configured address is ever offered as a link.
pub fn pr_url_allowed(url: &str, git_base_url: &str, rules: GitRemoteRules) -> bool {
    let (Ok(u), Ok(base)) = (Url::parse(url), Url::parse(git_base_url)) else { return false };
    u.scheme() == "https" && u.origin() == base.origin() && rules.parse_pr_url(u.path()).is_some()
}

/// `(repo_key, dir)`: whether the folder is that repo's own ticket worktree.
pub type WorktreeOk<'a> = &'a dyn Fn(Option<&str>, &str) -> bool;

/// What `history_facts` may trust: a folder in a history row is only used if the policy allows it.
#[derive(Default)]
pub struct HistoryPolicy<'a> {
    /// The folder must be that repo's own ticket worktree: `(repo_key, dir)`.
    pub worktree_ok: Option<WorktreeOk<'a>>,
    /// The job-file allow-list for a session's folder.
    pub session_cwd_ok: Option<&'a dyn Fn(&str) -> bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryFacts {
    pub worktrees: Vec<Value>,
    pub prs: Vec<PrFact>,
    pub analyses: Vec<Value>,
    pub sessions: Vec<Value>,
    /// The ticket's Jira title as the history recorded it.
    pub title: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrFact {
    pub project: String,
    pub repo: String,
    pub id: u64,
    pub title: Value,
    pub url: Value,
}

fn item_of<'a>(related: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    related.get(key).and_then(|r| r.get("item")).filter(|i| js_truthy(Some(i)))
}

/// What the history knows, from the ticket's detail and the details of its neighbours
/// (`related`: key -> getItem result, for up to two hops). A history row is untrusted (the
/// database is a local file), so a folder in it is only used if `policy` allows it; each dropped
/// row becomes a line in `notes`. With no policy nothing is allowed.
pub fn history_facts(ticket: Option<&Value>, related: &Map<String, Value>, policy: &HistoryPolicy<'_>) -> HistoryFacts {
    let mut facts = HistoryFacts::default();
    let Some(ticket) = ticket.filter(|t| js_truthy(Some(t))) else { return facts };
    facts.title = ticket["item"]["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(String::from);
    let keys = related_keys(ticket);
    for key in &keys.worktree {
        let Some(d) = item_of(related, key).map(|i| &i["data"]) else { continue };
        let Some(dir) = d["dir"].as_str() else { continue };
        let allowed = policy.worktree_ok.is_some_and(|f| f(d["repoKey"].as_str(), dir));
        if !allowed {
            facts.notes.push(format!("Local history: ignored a worktree ({dir}) that isn't this ticket's worktree in a configured repository."));
            continue;
        }
        let or_null = |v: &Value| if js_truthy(Some(v)) { v.clone() } else { Value::Null };
        facts.worktrees.push(json!({ "dir": dir, "repoKey": or_null(&d["repoKey"]), "branch": or_null(&d["branch"]), "base": or_null(&d["base"]), "fixStartedAt": d["fixStartedAt"] }));
    }
    for key in &keys.pr {
        let Some(pr) = parse_pr_key(key) else { continue };
        let item = item_of(related, key);
        facts.prs.push(PrFact { project: pr.project, repo: pr.repo, id: pr.id, title: item.map_or(Value::Null, |i| i["title"].clone()), url: item.map_or(Value::Null, |i| i["url"].clone()) });
    }
    for key in &keys.analysis {
        if let Some(item) = item_of(related, key) {
            facts.analyses.push(json!({ "key": key, "title": item["title"], "excerpt": item["excerpt"], "updatedAt": item["updatedAt"] }));
        }
    }
    let mut session_keys: Vec<String> = vec![];
    for d in related.values() {
        for k in related_keys(d).session {
            if !session_keys.contains(&k) {
                session_keys.push(k);
            }
        }
    }
    for key in session_keys {
        let Some(item) = item_of(related, &key).filter(|i| js_truthy(i.get("data"))) else { continue };
        let session = json!({ "id": key.strip_prefix("session:").unwrap_or(&key), "cwd": item["data"]["cwd"], "permissionMode": item["data"]["permissionMode"] });
        if !validate_session(Some(&session)) {
            continue;
        }
        let cwd = session["cwd"].as_str().unwrap_or("");
        if !policy.session_cwd_ok.is_some_and(|f| f(cwd)) {
            facts.notes.push(format!("Local history: ignored a Claude session whose folder ({cwd}) isn't one this service uses."));
            continue;
        }
        let mut s = session.clone();
        s["at"] = if js_truthy(item.get("updatedAt")) { item["updatedAt"].clone() } else { json!(0) };
        facts.sessions.push(s);
    }
    facts
}

/// Keys to fetch for the second hop: the ticket's neighbours.
pub fn second_hop_keys(ticket: &Value) -> Vec<String> {
    let k = related_keys(ticket);
    [k.worktree, k.pr, k.analysis, k.job].concat()
}

/// The third hop: the neighbours' sessions.
pub fn session_keys_of(details: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for d in details {
        for key in related_keys(d).session {
            if !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out.truncate(MAX_SESSIONS);
    out
}

pub struct WorkspaceInput<'a> {
    pub issue_key: &'a str,
    pub git_base_url: &'a str,
    pub rules: GitRemoteRules,
    /// `[{ repoKey, branches, worktrees, prs }]` from the live scan (each PR may carry `build`).
    pub repos: &'a [Value],
    pub history: Option<&'a HistoryFacts>,
    pub notes: Vec<String>,
    /// Whether a folder is still there.
    pub exists: &'a dyn Fn(&str) -> bool,
    /// Whether a session may be started in a folder (the resume fallback folder): with no
    /// predicate there is none.
    pub cwd_allowed: &'a dyn Fn(&str) -> bool,
}

fn merged(repo_key: &Value, entry: &Value) -> Value {
    let mut m = Map::new();
    m.insert("repoKey".into(), repo_key.clone());
    for (k, v) in entry.as_object().into_iter().flatten() {
        m.insert(k.clone(), v.clone());
    }
    Value::Object(m)
}

/// The workspace model.
pub fn build_workspace(input: WorkspaceInput<'_>) -> Value {
    let (mut worktrees, mut branches, mut prs): (Vec<Value>, Vec<Value>, Vec<Value>) = (vec![], vec![], vec![]);
    for r in input.repos {
        let repo_key = &r["repoKey"];
        worktrees.extend(r["worktrees"].as_array().into_iter().flatten().map(|w| merged(repo_key, w)));
        branches.extend(r["branches"].as_array().into_iter().flatten().map(|b| merged(repo_key, b)));
        for pr in r["prs"].as_array().into_iter().flatten() {
            if !pr["url"].as_str().is_some_and(|u| pr_url_allowed(u, input.git_base_url, input.rules)) {
                continue;
            }
            if prs.iter().any(|p| p["repoKey"] == *repo_key && p["id"] == pr["id"]) {
                continue;
            }
            let build = if js_truthy(pr.get("build")) {
                let b = &pr["build"];
                let builds: Vec<Value> = b["builds"].as_array().into_iter().flatten().map(|x| json!({ "state": x["state"], "name": if x["name"].is_string() { x["name"].clone() } else { Value::Null }, "url": if x["url"].is_string() { x["url"].clone() } else { Value::Null } })).collect();
                json!({ "state": b["state"], "counts": b["counts"], "builds": builds })
            } else {
                Value::Null
            };
            prs.push(json!({
                "repoKey": repo_key, "id": pr["id"], "title": if pr["title"].is_null() { Value::Null } else { pr["title"].clone() },
                "state": pr["state"], "url": pr["url"], "fromBranch": safe_branch(&pr["fromBranch"]), "toBranch": safe_branch(&pr["toBranch"]),
                "build": build, "source": "bitbucket",
            }));
        }
    }
    let empty = HistoryFacts::default();
    let h = input.history.unwrap_or(&empty);
    let mut notes = input.notes;
    for n in &h.notes {
        if !notes.contains(n) {
            notes.push(n.clone());
        }
    }
    for w in &h.worktrees {
        let dir = w["dir"].as_str().unwrap_or("");
        if worktrees.iter().any(|x| x["dir"].as_str() == Some(dir)) {
            continue;
        }
        worktrees.push(json!({ "repoKey": w["repoKey"], "dir": dir, "branch": w["branch"], "exists": (input.exists)(dir), "dirty": false, "changed": 0, "ahead": 0, "behind": 0, "isTicketWorktree": true, "fromHistory": true }));
    }
    let base = Url::parse(input.git_base_url).ok().map(|u| u.origin().ascii_serialization());
    for p in &h.prs {
        let repo_key = format!("{}/{}", p.project, p.repo);
        if prs.iter().any(|x| x["id"] == json!(p.id) && x["repoKey"].as_str().is_some_and(|k| k.to_lowercase() == repo_key.to_lowercase())) {
            continue;
        }
        let from_history = p.url.as_str().filter(|u| pr_url_allowed(u, input.git_base_url, input.rules)).map(String::from);
        let Some(url) = from_history.or_else(|| base.as_ref().map(|b| format!("{b}{}", input.rules.pr_path(&p.project, &p.repo, p.id)))) else { continue };
        prs.push(json!({ "repoKey": repo_key, "id": p.id, "title": p.title, "state": Value::Null, "url": url, "fromBranch": Value::Null, "toBranch": Value::Null, "build": Value::Null, "source": "history" }));
    }
    let mut sorted: Vec<&Value> = h.sessions.iter().collect();
    sorted.sort_by(|a, b| b["at"].as_f64().unwrap_or(0.0).partial_cmp(&a["at"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    let mut sessions: Vec<Value> = vec![];
    for s in sorted {
        if !sessions.iter().any(|x| x["id"] == s["id"]) {
            sessions.push(s.clone());
        }
    }
    sessions.truncate(MAX_SESSIONS);

    let live: Vec<&Value> = worktrees.iter().filter(|w| w["exists"] != json!(false)).collect();
    let ticket_worktree = live.iter().find(|w| js_truthy(w.get("isTicketWorktree"))).copied();
    let open_for = |w: &Value| prs.iter().any(|p| p["state"] == "OPEN" && p["fromBranch"] == w["branch"] && p["repoKey"] == w["repoKey"]);
    let create_pr = ticket_worktree
        .filter(|w| js_truthy(w.get("branch")) && js_truthy(w.get("repoKey")) && !open_for(w))
        .map_or(Value::Null, |w| json!({ "repoKey": w["repoKey"], "dir": w["dir"], "branch": w["branch"] }));
    let start_fix = live.is_empty() && branches.is_empty();
    let preferred = ticket_worktree.and_then(|w| sessions.iter().find(|s| s["cwd"] == w["dir"]));
    let resume_dir = ticket_worktree.into_iter().chain(live.iter().copied()).find(|w| w["dir"].as_str().is_some_and(|d| (input.cwd_allowed)(d))).and_then(|w| w["dir"].as_str());
    json!({
        "issueKey": input.issue_key, "summary": h.title, "worktrees": worktrees, "branches": branches, "prs": prs, "analyses": h.analyses, "sessions": sessions, "notes": notes,
        "suggestions": { "startFix": start_fix, "createPr": create_pr },
        "resume": { "session": preferred.or(sessions.first()).cloned().unwrap_or(Value::Null), "worktreeDir": resume_dir },
    })
}

/// One line for the job's result and the MCP/CLI output.
pub fn workspace_summary(ws: &Value) -> String {
    let arr = |k: &str| ws[k].as_array().cloned().unwrap_or_default();
    let n = |count: usize, word: &str| format!("{count} {word}{}", if count == 1 { "" } else { "s" });
    let open = arr("prs").iter().filter(|p| p["state"] == "OPEN").count();
    let live = arr("worktrees").iter().filter(|w| w["exists"] != json!(false)).count();
    let prs = arr("prs").len();
    let parts = [
        n(live, "worktree"),
        n(arr("branches").len(), "branch").replace("branchs", "branches"),
        format!("{}{}", n(prs, "PR"), if open > 0 { format!(" ({open} open)") } else { String::new() }),
        n(arr("analyses").len(), "analysis").replace("analysiss", "analyses"),
        n(arr("sessions").len(), "Claude session"),
    ];
    format!("{}: {}.", ws["issueKey"].as_str().unwrap_or(""), parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use GitRemoteRules::BitbucketDc;

    const BB: &str = "https://bb.example";
    const WT: &str = "/Users/me/gitviews/sample-app.worktrees/PROJ-7";
    const S1: &str = "11111111-1111-4111-8111-111111111111";
    const S2: &str = "22222222-2222-4222-8222-222222222222";

    fn ticket_detail() -> Value {
        json!({
            "item": { "kind": "ticket", "key": "jira:PROJ-7", "title": "Login fails" },
            "edges": [
                { "rel": "worktree", "kind": "worktree", "key": format!("worktree:{WT}"), "direction": "out" },
                { "rel": "links", "kind": "pr", "key": "bitbucket:ACME/sample-app#12", "direction": "out" },
                { "rel": "analyzed", "kind": "analysis", "key": "analysis:jira:PROJ-7", "direction": "out" },
                { "rel": "links", "kind": "job", "key": "job:abc", "direction": "in" },
            ],
            "facts": [],
        })
    }

    fn related() -> Map<String, Value> {
        let v = json!({
            format!("worktree:{WT}"): { "item": { "kind": "worktree", "data": { "dir": WT, "repoKey": "ACME/sample-app", "branch": "bugfix/PROJ-7-login", "base": "master", "fixStartedAt": 5 } }, "edges": [{ "kind": "session", "key": format!("session:{S1}"), "rel": "links", "direction": "out" }] },
            "bitbucket:ACME/sample-app#12": { "item": { "kind": "pr", "title": "PROJ-7: fix", "url": format!("{BB}/projects/ACME/repos/sample-app/pull-requests/12") }, "edges": [] },
            "analysis:jira:PROJ-7": { "item": { "kind": "analysis", "title": "Analysis of PROJ-7", "excerpt": "Null check", "updatedAt": 9 }, "edges": [] },
            "job:abc": { "item": { "kind": "job" }, "edges": [{ "kind": "session", "key": format!("session:{S2}"), "rel": "links", "direction": "out" }] },
            format!("session:{S1}"): { "item": { "kind": "session", "updatedAt": 20, "data": { "cwd": WT, "permissionMode": "plan" } }, "edges": [] },
            format!("session:{S2}"): { "item": { "kind": "session", "updatedAt": 30, "data": { "cwd": "/Users/me/gitviews/sample-app", "permissionMode": "plan" } }, "edges": [] },
        });
        v.as_object().unwrap().clone()
    }

    fn allow() -> HistoryPolicy<'static> {
        HistoryPolicy { worktree_ok: Some(&|_, _| true), session_cwd_ok: Some(&|_| true) }
    }

    fn live_repo(over: Value) -> Value {
        let mut r = json!({
            "repoKey": "ACME/sample-app",
            "branches": [{ "name": "bugfix/PROJ-7-login", "local": true, "remote": true, "ahead": 1, "behind": 0 }],
            "worktrees": [{ "dir": WT, "branch": "bugfix/PROJ-7-login", "exists": true, "dirty": true, "changed": 2, "ahead": 1, "behind": 0, "isTicketWorktree": true }],
            "prs": [],
        });
        for (k, v) in over.as_object().into_iter().flatten() {
            r[k] = v.clone();
        }
        r
    }

    fn input<'a>(repos: &'a [Value], history: Option<&'a HistoryFacts>) -> WorkspaceInput<'a> {
        WorkspaceInput { issue_key: "PROJ-7", git_base_url: BB, rules: BitbucketDc, repos, history, notes: vec![], exists: &|_| true, cwd_allowed: &|_| false }
    }

    #[test]
    fn parse_workspace_payload_upper_cases_the_key_and_refuses_anything_else() {
        assert_eq!(parse_workspace_payload(&json!({ "issueKey": " proj-7 " })), Ok("PROJ-7".into()));
        assert!(parse_workspace_payload(&json!({ "issueKey": "PROJ" })).is_err());
        assert!(parse_workspace_payload(&json!(null)).is_err());
        assert!(parse_workspace_payload(&json!({ "issueKey": "PROJ-0" })).is_err());
    }

    #[test]
    fn second_hop_keys_and_session_keys_of_walk_ticket_neighbours_sessions() {
        assert_eq!(second_hop_keys(&ticket_detail()), [format!("worktree:{WT}"), "bitbucket:ACME/sample-app#12".into(), "analysis:jira:PROJ-7".into(), "job:abc".into()]);
        let r = related();
        assert_eq!(session_keys_of(&[r[&format!("worktree:{WT}")].clone(), r["job:abc"].clone()]), [format!("session:{S1}"), format!("session:{S2}")]);
        let many: Vec<Value> = (0..15).map(|i| json!({ "edges": [{ "kind": "session", "key": format!("session:{i}") }] })).collect();
        assert_eq!(session_keys_of(&many).len(), 10);
    }

    #[test]
    fn history_facts_collects_worktrees_prs_analyses_and_valid_sessions() {
        let f = history_facts(Some(&ticket_detail()), &related(), &allow());
        assert_eq!(f.worktrees, [json!({ "dir": WT, "repoKey": "ACME/sample-app", "branch": "bugfix/PROJ-7-login", "base": "master", "fixStartedAt": 5 })]);
        assert_eq!(f.prs, [PrFact { project: "ACME".into(), repo: "sample-app".into(), id: 12, title: json!("PROJ-7: fix"), url: json!(format!("{BB}/projects/ACME/repos/sample-app/pull-requests/12")) }]);
        assert_eq!(f.analyses[0]["excerpt"], "Null check");
        let mut ids: Vec<&str> = f.sessions.iter().map(|s| s["id"].as_str().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, [S1, S2]);
        let mut bad = related();
        bad.insert(format!("session:{S1}"), json!({ "item": { "data": { "cwd": "relative", "permissionMode": "plan" } }, "edges": [] }));
        assert_eq!(history_facts(Some(&ticket_detail()), &bad, &allow()).sessions.iter().map(|s| s["id"].as_str().unwrap().to_string()).collect::<Vec<_>>(), [S2]);
        assert_eq!(history_facts(None, &Map::new(), &HistoryPolicy::default()), HistoryFacts::default());
    }

    #[test]
    fn pr_url_allowed_only_accepts_an_https_pr_page_on_the_configured_host() {
        assert!(pr_url_allowed(&format!("{BB}/projects/ACME/repos/sample-app/pull-requests/12"), BB, BitbucketDc));
        assert!(!pr_url_allowed("https://evil.example/projects/ACME/repos/sample-app/pull-requests/12", BB, BitbucketDc));
        assert!(!pr_url_allowed("javascript:alert(1)", BB, BitbucketDc));
        assert!(!pr_url_allowed(&format!("{BB}/plugins/servlet/x"), BB, BitbucketDc));
        assert!(!pr_url_allowed("http://bb.example/projects/A/repos/b/pull-requests/1", BB, BitbucketDc));
    }

    #[test]
    fn build_workspace_merges_live_and_history_and_offers_create_pr_when_the_ticket_worktree_has_no_open_pr() {
        let h = history_facts(Some(&ticket_detail()), &related(), &allow());
        let repos = [live_repo(json!({}))];
        let ws = build_workspace(WorkspaceInput { cwd_allowed: &|_| true, ..input(&repos, Some(&h)) });
        assert_eq!((ws["worktrees"].as_array().unwrap().len(), ws["prs"].as_array().unwrap().len()), (1, 1));
        assert_eq!(ws["prs"][0]["source"], "history");
        assert_eq!(ws["summary"], "Login fails", "the ticket's Jira title rides along for the terminal title");
        assert_eq!(ws["suggestions"]["startFix"], false);
        assert_eq!(ws["suggestions"]["createPr"], json!({ "repoKey": "ACME/sample-app", "dir": WT, "branch": "bugfix/PROJ-7-login" }));
        assert_eq!(ws["resume"]["session"]["id"], S1, "the session in the ticket worktree wins over a newer one elsewhere");
        assert_eq!(ws["resume"]["worktreeDir"], WT);
        assert_eq!(workspace_summary(&ws), "PROJ-7: 1 worktree, 1 branch, 1 PR, 1 analysis, 2 Claude sessions.");
    }

    #[test]
    fn an_open_pr_from_the_worktrees_branch_turns_create_pr_off_and_carries_its_build_state() {
        let pr = json!({
            "id": 12, "title": "PROJ-7: fix", "state": "OPEN", "url": format!("{BB}/projects/ACME/repos/sample-app/pull-requests/12"), "fromBranch": "bugfix/PROJ-7-login", "toBranch": "master",
            "build": { "state": "FAILED", "counts": { "FAILED": 1 }, "builds": [{ "state": "FAILED", "name": "sample-app » b #6", "url": "https://ci/job/6", "key": "k", "at": 1 }, { "state": "SUCCESSFUL", "name": 7, "url": null }] },
        });
        let repos = [live_repo(json!({ "prs": [pr] }))];
        let ws = build_workspace(input(&repos, None));
        assert_eq!(ws["prs"].as_array().unwrap().len(), 1);
        assert_eq!(ws["prs"][0]["build"], json!({ "state": "FAILED", "counts": { "FAILED": 1 }, "builds": [{ "state": "FAILED", "name": "sample-app » b #6", "url": "https://ci/job/6" }, { "state": "SUCCESSFUL", "name": null, "url": null }] }));
        assert_eq!(ws["suggestions"]["createPr"], Value::Null);
        assert!(workspace_summary(&ws).contains("1 PR (1 open)"));
    }

    #[test]
    fn nothing_found_offers_start_fix_a_pr_on_another_host_is_dropped_a_vanished_history_worktree_is_marked() {
        let empty = build_workspace(input(&[], None));
        assert_eq!(empty["suggestions"]["startFix"], true);
        assert_eq!(empty["suggestions"]["createPr"], Value::Null);
        assert_eq!(empty["resume"], json!({ "session": null, "worktreeDir": null }));

        let evil_repos = [live_repo(json!({ "worktrees": [], "branches": [], "prs": [{ "id": 1, "state": "OPEN", "url": "https://evil.example/projects/A/repos/b/pull-requests/1" }] }))];
        assert!(build_workspace(input(&evil_repos, None))["prs"].as_array().unwrap().is_empty());

        let h = history_facts(Some(&ticket_detail()), &related(), &allow());
        let gone = build_workspace(WorkspaceInput { exists: &|_| false, cwd_allowed: &|_| true, ..input(&[], Some(&h)) });
        assert_eq!(gone["worktrees"][0]["exists"], false);
        assert_eq!(gone["suggestions"]["createPr"], Value::Null);
        assert_eq!(gone["suggestions"]["startFix"], true);
    }

    #[test]
    fn a_branch_name_that_isnt_safe_degrades_to_unknown_instead_of_failing_the_scan() {
        let pr = json!({ "id": 3, "title": "t", "state": "OPEN", "url": format!("{BB}/projects/ACME/repos/sample-app/pull-requests/3"), "fromBranch": "feat/a#b", "toBranch": "master" });
        let repos = [live_repo(json!({ "prs": [pr] }))];
        let ws = build_workspace(input(&repos, None));
        assert_eq!(ws["prs"].as_array().unwrap().len(), 1);
        assert_eq!((ws["prs"][0]["fromBranch"].clone(), ws["prs"][0]["toBranch"].clone()), (Value::Null, json!("master")));
    }

    #[test]
    fn history_facts_drops_a_tampered_row_a_folder_the_policy_refuses_with_a_note_and_no_policy_allows_nothing() {
        let mut evil = related();
        evil.insert(format!("worktree:{WT}"), json!({ "item": { "kind": "worktree", "data": { "dir": "/tmp/evil", "repoKey": "ACME/sample-app", "branch": "b" } }, "edges": [{ "kind": "session", "key": format!("session:{S1}"), "rel": "links", "direction": "out" }] }));
        evil.insert(format!("session:{S1}"), json!({ "item": { "kind": "session", "updatedAt": 20, "data": { "cwd": "/tmp/evil", "permissionMode": "plan" } }, "edges": [] }));
        let policy = HistoryPolicy { worktree_ok: Some(&|_, dir| dir == WT), session_cwd_ok: Some(&|cwd| cwd != "/tmp/evil") };
        let f = history_facts(Some(&ticket_detail()), &evil, &policy);
        assert!(f.worktrees.is_empty());
        assert_eq!(f.sessions.iter().map(|s| s["id"].as_str().unwrap().to_string()).collect::<Vec<_>>(), [S2]);
        assert_eq!(f.notes.len(), 2);
        let none = history_facts(Some(&ticket_detail()), &related(), &HistoryPolicy::default());
        assert_eq!((none.worktrees.len(), none.sessions.len()), (0, 0));
    }

    #[test]
    fn the_resume_folder_fallback_needs_cwd_allowed_a_refused_folder_is_skipped_for_the_next_allowed_one() {
        let two = live_repo(json!({ "worktrees": [{ "dir": "/somewhere/else", "branch": "x", "exists": true, "isTicketWorktree": true }, { "dir": WT, "branch": "y", "exists": true }] }));
        let repos = [two];
        assert_eq!(build_workspace(input(&repos, None))["resume"]["worktreeDir"], Value::Null);
        assert_eq!(build_workspace(WorkspaceInput { cwd_allowed: &|d| d == WT, ..input(&repos, None) })["resume"]["worktreeDir"], WT);
    }

    #[test]
    fn the_summary_pluralizes_branches_and_analyses() {
        let ws = json!({ "issueKey": "PROJ-1", "worktrees": [], "branches": [{}, {}], "prs": [], "analyses": [{}, {}], "sessions": [{}] });
        assert_eq!(workspace_summary(&ws), "PROJ-1: 0 worktrees, 2 branches, 0 PRs, 2 analyses, 1 Claude session.");
    }
}
