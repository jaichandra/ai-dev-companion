//! Pure logic for review-in-editor (port of `features/review-in-editor/plan.js`):
//! the payload check, the review prompt, the terminal header and title, the job's
//! summary and review result, and the decisions about a review already running in
//! the worktree or remembered from an earlier click.

use crate::js_text::{is_js_space, js_trim, utf16_len};
use crate::node_path;
use crate::terminal::{HeaderSegment, HeaderStyle};
use crate::uri::encode_uri;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

static UUID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$").unwrap());
static SHA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[0-9a-f]{7,64}$").unwrap());
const TITLE_MAX: usize = 80;
const BADGE_MAX: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Payload {
    pub project: String,
    pub repo: String,
    /// Carried through if present but never required; `Null` when absent.
    pub pr_id: Value,
    pub source_branch: String,
    pub target_branch: Option<String>,
    pub title: Option<String>,
    pub pr_url: Option<String>,
}

impl Payload {
    /// The payload as JSON, leaving out what is unset (as `undefined` is in JSON).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("project".into(), json!(self.project));
        m.insert("repo".into(), json!(self.repo));
        if !self.pr_id.is_null() {
            m.insert("prId".into(), self.pr_id.clone());
        }
        m.insert("sourceBranch".into(), json!(self.source_branch));
        for (k, v) in [("targetBranch", &self.target_branch), ("title", &self.title), ("prUrl", &self.pr_url)] {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        }
        Value::Object(m)
    }
}

/// Validates the extension's /start payload: project, repo and sourceBranch (all strings). `prId`
/// is carried through if present but never required: it's only used for a human-readable
/// summary. `targetBranch`, `title` and `prUrl` are optional too (an older extension doesn't send
/// them); the review prompt copes without them.
pub fn assert_payload(body: &Value) -> Result<Payload, String> {
    let (Some(project), Some(repo), Some(source)) = (body["project"].as_str(), body["repo"].as_str(), body["sourceBranch"].as_str()) else {
        return Err("review-in-editor payload must include project, repo, sourceBranch (strings)".into());
    };
    Ok(Payload {
        project: project.into(),
        repo: repo.into(),
        pr_id: body["prId"].clone(),
        source_branch: source.into(),
        target_branch: body["targetBranch"].as_str().filter(|t| !t.is_empty()).map(String::from),
        title: body["title"].as_str().filter(|t| !js_trim(t).is_empty()).map(String::from),
        pr_url: body["prUrl"].as_str().filter(|u| u.starts_with("http://") || u.starts_with("https://")).map(String::from),
    })
}

/// `text` with every C0/C1 control character and DEL replaced by a space. PR titles and branch
/// names come from the git host's JSON, which can carry raw ESC/BEL/CR bytes; printed verbatim,
/// those would be terminal escape sequences rather than text.
pub fn strip_control_chars(text: &str) -> String {
    text.chars().map(|c| if matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}') { ' ' } else { c }).collect()
}

fn collapse(text: &str) -> String {
    let mut out = String::new();
    let mut in_ws = false;
    for c in text.chars() {
        if is_js_space(c) {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    js_trim(&out).to_string()
}

/// `text` as a single line with no control characters or runs of spaces.
fn one_line(text: &str) -> String {
    collapse(&strip_control_chars(text))
}

fn has_id(pr_id: &Value) -> bool {
    !pr_id.is_null() && pr_id != &json!("")
}

fn id_text(pr_id: &Value) -> String {
    crate::js_text::js_to_string(pr_id)
}

/// `#42 "Title"`, or whichever half is known, or "" (always one line).
fn describe_pr(pr_id: &Value, title: Option<&str>) -> String {
    let id = if !pr_id.is_null() { strip_control_chars(&format!("#{}", id_text(pr_id))) } else { String::new() };
    let t = title.filter(|t| !t.is_empty()).map(|t| format!("\"{}\"", one_line(t))).unwrap_or_default();
    [id, t].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

pub struct ReviewPromptInput<'a> {
    pub source_branch: &'a str,
    pub target_branch: Option<&'a str>,
    pub pr_id: Value,
    pub title: Option<&'a str>,
    pub pr_url: Option<&'a str>,
    /// The sha of the last review: start from the new commits.
    pub since: Option<&'a str>,
}

/// The prompt the editor's AI assistant starts the review with. Always a single line: it's typed
/// into a terminal for Claude Code and Cursor Agent, where a newline would submit it early. It
/// names no particular skill: the agent knows which skills and commands it has, so it is told to
/// use the best review one if there is one, given the PR URL when we have it, and to follow the
/// built-in instructions otherwise. Without a target branch the diff is against origin/HEAD.
pub fn build_review_prompt(i: &ReviewPromptInput<'_>) -> String {
    let base = i.target_branch.filter(|t| !t.is_empty()).map_or_else(|| "origin/HEAD".to_string(), |t| format!("origin/{t}"));
    let pr = describe_pr(&i.pr_id, i.title);
    let where_ = format!("{}branch {}, merging into {}", i.pr_url.map(|u| format!("{u}; ")).unwrap_or_default(), i.source_branch, i.target_branch.filter(|t| !t.is_empty()).unwrap_or("the default branch"));
    let since = i.since.filter(|s| !s.is_empty()).map(|s| {
        let short: String = s.chars().take(7).collect();
        format!(" This PR was already reviewed at {short}: start with `git diff {s}..HEAD` (the new commits) and read the rest of the PR only as far as you need to judge them.")
    });
    let checked_out = format!("The branch is checked out here; run `git diff {base}...HEAD` to see exactly what it changes. Don't modify any files.{}", since.unwrap_or_default());
    let text = format!(
        "Review pull request {}({where_}). {checked_out} If you have a skill or command for reviewing pull requests or code, check your available skills and use the best match{}. Otherwise: look for bugs, edge cases, security problems, missing tests, and anything hard to follow. Report findings grouped by severity, each with a file:line reference.",
        if pr.is_empty() { String::new() } else { format!("{pr} ") },
        if i.pr_url.is_some() { ", passing it the PR URL if it needs one" } else { "" },
    );
    collapse(&strip_control_chars(&text))
}

/// `text` cut to `max` characters, ending in "…" if it was cut.
fn truncate(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max { text.to_string() } else { format!("{}\u{2026}", chars[..max - 1].iter().collect::<String>()) }
}

/// The terminal tab/window title for a terminal-based review, which stays visible once the agent's
/// full-screen UI has covered the printed header, and a one-line iTerm badge for when the agent
/// overwrites that title (cursor-agent does). The source branch is what gets shortened to fit,
/// since the PR number and target are the short, telling parts.
pub fn build_terminal_title(pr_id: &Value, source_branch: &str, target_branch: Option<&str>) -> (String, String) {
    let pr = if !pr_id.is_null() { one_line(&format!("PR #{}", id_text(pr_id))) } else { "PR review".to_string() };
    let target = target_branch.filter(|t| !t.is_empty()).map_or_else(|| "(default branch)".to_string(), one_line);
    let fixed = format!("{pr} \u{b7}  \u{2192} {target}");
    let source = truncate(&one_line(source_branch), 12.max(TITLE_MAX.saturating_sub(utf16_len(&fixed))));
    (truncate(&format!("{pr} \u{b7} {source} \u{2192} {target}"), TITLE_MAX), truncate(&if !pr_id.is_null() { pr } else { one_line(source_branch) }, BADGE_MAX))
}

const RULE_LEN: usize = 64;

pub struct HeaderInput<'a> {
    pub editor: &'a str,
    pub dir: &'a str,
    pub source_branch: &'a str,
    pub target_branch: Option<&'a str>,
    pub pr_id: Value,
    pub title: Option<&'a str>,
    pub pr_url: Option<&'a str>,
}

/// The lines printed at the top of a terminal-based review before the agent starts, so the
/// worktree and branch stay on screen above its output. Each line is a list of styled segments;
/// the style names what the text is (the terminal maps it to a color), so PR-supplied text like
/// the title is only ever printed as data. Every segment is single-line and free of control
/// characters. Only Claude Code's review runs in a terminal.
pub fn build_terminal_header(i: &HeaderInput<'_>) -> Vec<Vec<HeaderSegment>> {
    if i.editor != "claude-code" {
        return vec![];
    }
    let pr = describe_pr(&i.pr_id, i.title);
    let seg = |text: &str, style| HeaderSegment::styled(text, style);
    let row = |label: &str, value: Vec<HeaderSegment>| {
        let mut line = vec![seg(&format!("  {label:<16}"), HeaderStyle::Label)];
        line.extend(value);
        line
    };
    let rule = || vec![seg(&"\u{2500}".repeat(RULE_LEN), HeaderStyle::Rule)];
    let mut title_line = vec![seg("PR review", HeaderStyle::Title)];
    if !pr.is_empty() {
        title_line.push(seg(&format!("  {pr}"), HeaderStyle::Heading));
    }
    let mut lines = vec![
        rule(),
        title_line,
        rule(),
        row("Branch", vec![seg(i.source_branch, HeaderStyle::Branch), seg(" -> ", HeaderStyle::Muted), seg(i.target_branch.filter(|t| !t.is_empty()).unwrap_or("(default branch)"), HeaderStyle::Target)]),
        row("Worktree", vec![seg(i.dir, HeaderStyle::Path)]),
    ];
    if let Some(url) = i.pr_url.filter(|u| !u.is_empty()) {
        lines.push(row("PR", vec![seg(url, HeaderStyle::Link)]));
    }
    lines.extend([rule(), vec![seg("Starting Claude Code...", HeaderStyle::Status)], vec![]]);
    lines.into_iter().map(|l| l.into_iter().map(|s| HeaderSegment { text: strip_control_chars(&s.text), ..s }).collect()).collect()
}

/// Parses `git status --porcelain` output into the list of affected file paths (dropping the
/// two-character status column and the separating space): used to name what got auto-stashed
/// before checking out the PR branch.
pub fn parse_dirty_files(porcelain_stdout: &str) -> Vec<String> {
    porcelain_stdout.split('\n').map(|line| js_trim(&line.chars().skip(3).collect::<String>()).to_string()).filter(|l| !l.is_empty()).collect()
}

/// The one-shot job's `result.summary`: the only thing the extension shows the user, since this
/// feature has no review panel of its own (the point is that you review it in your own editor).
pub fn build_summary(branch: &str, dir: &str, editor_label: Option<&str>, stashed_files: &[String], review_note: Option<&str>) -> String {
    let stash = if stashed_files.is_empty() {
        String::new()
    } else {
        format!(" Your uncommitted changes ({} file{}: {}) were stashed first \u{2014} run `git stash pop` in {dir} to get them back.", stashed_files.len(), if stashed_files.len() == 1 { "" } else { "s" }, stashed_files.join(", "))
    };
    let review = review_note.filter(|r| !r.is_empty()).map(|r| format!(" {r}")).unwrap_or_default();
    let opened = editor_label.filter(|l| !l.is_empty()).map(|l| format!(" and opened it in {l}")).unwrap_or_default();
    format!("Checked out {branch} in {dir}{opened}.{review}{stash}")
}

fn editor_info(editor: &str) -> (String, Option<&'static str>, Option<&'static str>) {
    match editor {
        "cursor" => ("Cursor".into(), Some("Cursor Agent (Ask mode, read-only)"), Some("cursor")),
        "claude-code" => ("Claude Code".into(), Some("Claude Code"), None),
        "vscode" => ("VS Code".into(), Some("Copilot Chat (Ask mode, read-only)"), Some("vscode")),
        other => (other.into(), None, None),
    }
}

pub struct ReviewResultInput<'a> {
    pub payload: &'a Value,
    pub dir: &'a str,
    pub editor: &'a str,
    pub review_started: bool,
    pub review_note: Option<&'a str>,
    pub stashed_files: &'a [String],
    pub tracked: bool,
}

/// The one-shot job's `result.review`: the same outcome as the summary, as fields for the
/// extension's review panel to lay out. `openUrl` opens the worktree in the configured editor's
/// IDE (none for Claude Code, which has no IDE); `agentLabel` is only set when a review actually
/// started. `tracked` says whether GET /status reports the review's live `session` (terminal-based
/// reviews only).
pub fn build_review_result(i: &ReviewResultInput<'_>) -> Value {
    let (label, agent, scheme) = editor_info(i.editor);
    let mut m = Map::new();
    let p = i.payload;
    m.insert("prId".into(), p["prId"].clone());
    for (k, pk) in [("title", "title"), ("prUrl", "prUrl"), ("sourceBranch", "sourceBranch"), ("targetBranch", "targetBranch")] {
        if !p[pk].is_null() {
            m.insert(k.into(), p[pk].clone());
        }
    }
    m.insert("worktree".into(), json!(i.dir));
    m.insert("editor".into(), json!(i.editor));
    m.insert("editorLabel".into(), json!(label));
    if let Some(s) = scheme {
        m.insert("openUrl".into(), json!(format!("{s}://file{}", encode_uri(i.dir))));
    }
    if let (true, Some(a)) = (i.review_started, agent) {
        m.insert("agentLabel".into(), json!(a));
    }
    m.insert("reviewStarted".into(), json!(i.review_started));
    if let Some(n) = i.review_note {
        m.insert("reviewNote".into(), json!(n));
    }
    m.insert("stashedFiles".into(), json!(i.stashed_files));
    m.insert("tracked".into(), json!(i.tracked));
    Value::Object(m)
}

/// The /start body's `replaceJobId`: the live review the user confirmed cancelling to review this
/// PR instead. Only a well-formed job id.
pub fn parse_replace_job_id(body: &Value) -> Option<String> {
    body["replaceJobId"].as_str().filter(|id| UUID.is_match(id)).map(String::from)
}

/// Whether payloads `a` and `b` are the same pull request: same repo and PR id, or the same
/// source branch when either has no id.
pub fn same_pr(a: &Value, b: &Value) -> bool {
    if a["project"] != b["project"] || a["repo"] != b["repo"] {
        return false;
    }
    if has_id(&a["prId"]) && has_id(&b["prId"]) {
        return id_text(&a["prId"]) == id_text(&b["prId"]);
    }
    a["sourceBranch"] == b["sourceBranch"]
}

/// What /start does, given the session record of the review still live in the worktree: "start"
/// as usual; "reuse" the live one when it's this PR; "replace" it when the request names that very
/// review as the one to cancel first; otherwise "conflict", since checking out another branch
/// would change the files under the running agent.
pub fn decide_start(payload: &Value, live: Option<&Value>, replace_job_id: Option<&str>) -> &'static str {
    let Some(live) = live else { return "start" };
    if replace_job_id.is_some_and(|r| live["jobId"].as_str() == Some(r)) {
        return "replace";
    }
    if same_pr(&live["payload"], payload) { "reuse" } else { "conflict" }
}

/// "PR #42 (branch)" for the live review in `payload`, on one line.
pub fn describe_live_review(payload: &Value) -> String {
    let branch = one_line(payload["sourceBranch"].as_str().unwrap_or(""));
    let pr = if has_id(&payload["prId"]) { one_line(&format!("PR #{}", id_text(&payload["prId"]))) } else { "a pull request".to_string() };
    if branch.is_empty() { pr } else { format!("{pr} ({branch})") }
}

/// The body of the 409 /start answers a "conflict" with: enough for the panel to name the live
/// review and offer to go to it or replace it.
pub fn build_conflict(live: &Value, state: &str) -> Value {
    json!({
        "code": "review-in-progress",
        "error": format!("A review of {} is in progress in {}. Checking out this pull request there would change the files under it.", describe_live_review(&live["payload"]), live["worktree"].as_str().unwrap_or("")),
        "existing": { "jobId": live["jobId"], "state": state, "worktree": live["worktree"], "payload": live["payload"] },
    })
}

/// The result /start returns when it "reuse"s the live review: that review's own, marked
/// `switched` with how bringing its terminal to the front went (`focus`: "focused", "missing",
/// "unknown" or an error).
pub fn build_switched_result(live: &Value, focus: &str) -> Value {
    let note = match focus {
        "focused" => "Its terminal window is in front.".to_string(),
        "missing" => "Its terminal window couldn't be found; it may have just been closed.".to_string(),
        "unknown" => "Which terminal window it's in isn't known, so it wasn't brought to the front.".to_string(),
        other => format!("Couldn't bring its terminal window to the front: {other}"),
    };
    let mut review = live["review"].clone();
    review["switched"] = json!({ "focused": focus == "focused", "note": note });
    json!({ "summary": format!("Switched to the review of {} already running in {}. {note}", describe_live_review(&live["payload"]), live["worktree"].as_str().unwrap_or("")), "review": review })
}

/// Key of a PR's remembered review: repo and PR id, or the branch when there is no id.
pub fn pr_key(project: &str, repo: &str, pr_id: &Value, source_branch: &str) -> String {
    if has_id(pr_id) { format!("{project}/{repo}#{}", id_text(pr_id)) } else { format!("{project}/{repo}@{source_branch}") }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reuse {
    /// Reopen the session recorded for this head.
    Resume { session_id: String, reviewed_sha: String, reviewed_at: Value },
    /// A new review, told the sha to start from when that is still an ancestor of the head.
    Review { since: Option<String> },
}

/// What to do about this PR's last review (`record`: `{sessionId, cwd, reviewedSha, reviewedAt}`)
/// given the branch's `head` now: "resume" that session when the head hasn't moved and its
/// transcript is still there, in the folder this review would use (`dir`); otherwise a new
/// "review", told the sha to start from when that sha is still an ancestor of `head` (not after a
/// force-push). Resuming an old session to review new commits is deliberately not an option: its
/// whole transcript would be re-read, usually uncached, to review less than a fresh session would.
pub fn decide_reuse(record: Option<&Value>, head: &str, dir: &str, transcript_exists: bool, is_ancestor: bool) -> Reuse {
    let Some(record) = record.filter(|r| r["reviewedSha"].as_str().is_some_and(|s| SHA.is_match(s))) else { return Reuse::Review { since: None } };
    let sha = record["reviewedSha"].as_str().unwrap_or("");
    if sha == head && transcript_exists && record["sessionId"].as_str().is_some_and(|s| UUID.is_match(s)) && record["cwd"].as_str().is_some_and(|c| node_path::resolve(c) == node_path::resolve(dir)) {
        return Reuse::Resume { session_id: record["sessionId"].as_str().unwrap_or("").into(), reviewed_sha: sha.into(), reviewed_at: record["reviewedAt"].clone() };
    }
    Reuse::Review { since: (sha != head && is_ancestor).then(|| sha.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PR_URL: &str = "https://bitbucket.example.com/projects/ACME/repos/sample-app/pull-requests/42";
    const LIVE_JOB: &str = "11111111-2222-4333-8444-555555555555";

    fn plain(lines: &[Vec<HeaderSegment>]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect()
    }
    fn prompt(source: &str, target: Option<&str>, id: Value, title: Option<&str>, url: Option<&str>, since: Option<&str>) -> String {
        build_review_prompt(&ReviewPromptInput { source_branch: source, target_branch: target, pr_id: id, title, pr_url: url, since })
    }
    fn header(editor: &str, source: &str, target: Option<&str>, id: Value, title: Option<&str>, url: Option<&str>) -> Vec<Vec<HeaderSegment>> {
        build_terminal_header(&HeaderInput { editor, dir: "/wt", source_branch: source, target_branch: target, pr_id: id, title, pr_url: url })
    }
    fn has_control(s: &str) -> bool {
        s.chars().any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'))
    }

    #[test]
    fn assert_payload_accepts_keeps_and_drops_the_right_fields() {
        let r = assert_payload(&json!({ "project": "ACME", "repo": "sample-app", "prId": 42, "sourceBranch": "feature/x" })).unwrap();
        assert_eq!(r, Payload { project: "ACME".into(), repo: "sample-app".into(), pr_id: json!(42), source_branch: "feature/x".into(), target_branch: None, title: None, pr_url: None });
        let base = json!({ "project": "ACME", "repo": "sample-app", "sourceBranch": "x" });
        let with = |k: &str, v: Value| {
            let mut b = base.clone();
            b[k] = v;
            assert_payload(&b).unwrap()
        };
        assert_eq!(with("prUrl", json!(PR_URL)).pr_url.as_deref(), Some(PR_URL));
        assert_eq!(with("prUrl", json!("javascript:alert(1)")).pr_url, None);
        let both = assert_payload(&json!({ "project": "ACME", "repo": "sample-app", "sourceBranch": "feature/x", "targetBranch": "master", "title": "Fix bars" })).unwrap();
        assert_eq!((both.target_branch.as_deref(), both.title.as_deref()), (Some("master"), Some("Fix bars")));
        let dropped = assert_payload(&json!({ "project": "ACME", "repo": "sample-app", "sourceBranch": "x", "targetBranch": 5, "title": "  " })).unwrap();
        assert_eq!((dropped.target_branch, dropped.title), (None, None));
        for bad in [json!({ "repo": "r", "sourceBranch": "x" }), json!({ "project": "P", "sourceBranch": "x" }), json!({ "project": "P", "repo": "r" }), json!(null)] {
            assert!(assert_payload(&bad).unwrap_err().contains("project, repo, sourceBranch"), "{bad}");
        }
        assert_eq!(r.to_json(), json!({ "project": "ACME", "repo": "sample-app", "prId": 42, "sourceBranch": "feature/x" }));
    }

    #[test]
    fn the_review_prompt_uses_whichever_skill_exists_with_the_url_diffs_the_target_and_stays_on_one_line() {
        let p = prompt("x", Some("master"), json!(42), None, Some(PR_URL), None);
        assert!(p.starts_with("Review pull request #42 (https://bitbucket.example.com/projects/ACME/repos/sample-app/pull-requests/42; branch x, merging into master)."), "{p}");
        for needle in ["git diff origin/master...HEAD", "Don't modify any files", "If you have a skill or command for reviewing", "available skills", "Otherwise: look for bugs", "grouped by severity", "passing it the PR URL"] {
            assert!(p.contains(needle), "{needle}");
        }
        assert!(!p.contains("review-pr"));
        assert!(prompt("x", Some("master"), json!(42), None, None, None).starts_with("Review pull request #42 (branch x, merging into master)."));
        let t = prompt("feature/x", Some("develop"), json!(42), Some("Fix bars"), None, None);
        assert!(t.contains("git diff origin/develop...HEAD") && t.contains("#42 \"Fix bars\""));
        let d = prompt("feature/x", None, json!(42), None, None, None);
        assert!(d.contains("git diff origin/HEAD...HEAD") && d.contains("the default branch"));
        let multi = prompt("x", Some("master"), json!(1), Some("a\nb\r\n\tc"), None, None);
        assert!(!multi.contains(['\r', '\n', '\t']) && multi.contains("\"a b c\""));
        let ctl = prompt("x", None, json!(1), Some("a\u{1b}]0;t\u{7}b"), None, None);
        assert!(!has_control(&ctl) && ctl.contains("\"a ]0;t b\""));
        let sha = format!("{:0<40}", "abcdef1234567890");
        let s = prompt("f", Some("main"), json!(1), None, None, Some(&sha));
        assert!(s.contains("already reviewed at abcdef1") && s.contains("git diff abcdef1234567890") && !s.contains('\n'));
        assert!(!prompt("f", None, Value::Null, None, None, None).contains("already reviewed"));
    }

    #[test]
    fn build_summary_covers_the_review_note_the_editor_and_the_stash() {
        let s = build_summary("feature/x", "/repo.worktrees/pr-review", Some("Cursor"), &[], Some("Cursor Agent is reviewing it."));
        assert!(s.contains("opened it in Cursor. Cursor Agent is reviewing it.") && !s.contains("skill"));
        let n = build_summary("feature/x", "/wt", None, &[], Some("Cursor Agent is reviewing it."));
        assert!(n.starts_with("Checked out feature/x in /wt. Cursor Agent is reviewing it.") && !n.contains("opened it in"));
        let two = build_summary("feature/x", "/repo.worktrees/pr-review", Some("Cursor"), &["a.ts".into(), "b.ts".into()], None);
        assert!(two.contains("Checked out feature/x") && two.contains("2 files") && two.contains("git stash pop"));
        assert!(build_summary("b", "/d", Some("Cursor"), &["a.ts".into()], None).contains("1 file:"));
        assert!(!build_summary("b", "/d", Some("Cursor"), &[], None).contains("stash"));
    }

    #[test]
    fn the_terminal_header_shows_the_pr_branch_worktree_and_link_styled_for_color() {
        let lines = header("claude-code", "feature/x", Some("master"), json!(42), Some("Fix bars"), Some(PR_URL));
        let text = plain(&lines);
        assert!(text.contains(&"PR review  #42 \"Fix bars\"".to_string()));
        assert!(text.iter().any(|l| Regex::new(r"^ {2}Branch +feature/x -> master$").unwrap().is_match(l)));
        assert!(text.iter().any(|l| l == "  Worktree        /wt"));
        assert!(text.iter().any(|l| *l == format!("  {:<16}{PR_URL}", "PR")));
        assert!(text.iter().any(|l| l.starts_with("Starting Claude Code")));
        let style_of = |t: &str| lines.iter().flatten().find(|s| s.text == t).and_then(|s| s.style);
        assert_eq!(style_of("PR review"), Some(HeaderStyle::Title));
        assert_eq!(style_of("  #42 \"Fix bars\""), Some(HeaderStyle::Heading));
        assert_eq!((style_of("feature/x"), style_of("master"), style_of("/wt"), style_of(PR_URL)), (Some(HeaderStyle::Branch), Some(HeaderStyle::Target), Some(HeaderStyle::Path), Some(HeaderStyle::Link)));
        assert!(header("cursor", "x", None, Value::Null, None, None).is_empty());
        assert!(header("vscode", "x", None, Value::Null, None, None).is_empty());
        let bare = plain(&header("claude-code", "x", None, Value::Null, Some("a\nb"), None));
        assert!(bare.contains(&"PR review  \"a b\"".to_string()));
        assert!(bare.iter().any(|l| Regex::new(r"^ {2}Branch +x -> \(default branch\)$").unwrap().is_match(l)));
        assert!(bare.iter().all(|l| !l.starts_with("  PR ") && !l.contains("Open in")));
    }

    #[test]
    fn the_terminal_header_strips_terminal_escapes_from_pr_supplied_text() {
        let lines = header("claude-code", "feat\u{1b}]0;pwned\u{7}", Some("mas\u{9b}ter"), json!(42), Some("Fix \u{1b}[2Jbars\u{7}"), Some(&format!("{PR_URL}\u{1b}[0m")));
        assert!(lines.iter().flatten().all(|s| !has_control(&s.text)));
        assert!(plain(&lines).contains(&"PR review  #42 \"Fix [2Jbars\"".to_string()));
        assert_eq!(strip_control_chars("a\u{1b}[31mb\u{7}c\rd\u{7f}e\u{9b}f"), "a [31mb c d e f");
        assert_eq!(strip_control_chars("plain \u{2192} text"), "plain \u{2192} text");
    }

    #[test]
    fn parse_dirty_files_reads_porcelain_output() {
        assert_eq!(parse_dirty_files(" M src/api/client.ts\n?? notes.md\n"), ["src/api/client.ts", "notes.md"]);
        assert!(parse_dirty_files("").is_empty());
    }

    #[test]
    fn the_terminal_title_summarises_the_pr_number_and_branches() {
        let (title, badge) = build_terminal_title(&json!(5429), "proj-31764-activity-color-is-not-displayed-in-tooltip", Some("master"));
        assert_eq!(title, "PR #5429 \u{b7} proj-31764-activity-color-is-not-displayed-in-tooltip \u{2192} master");
        assert_eq!(badge, "PR #5429");
        let (long, _) = build_terminal_title(&json!(1), &"b".repeat(200), Some("release/2026.09"));
        assert_eq!(long.chars().count(), 80);
        assert!(Regex::new(r"^PR #1 \u{b7} b+\u{2026} \u{2192} release/2026\.09$").unwrap().is_match(&long), "{long}");
        let (t, b) = build_terminal_title(&Value::Null, "x\u{1b}]0;y\u{7}", None);
        assert_eq!(t, "PR review \u{b7} x ]0;y \u{2192} (default branch)");
        assert!(!has_control(&t));
        assert_eq!(b, "x ]0;y");
        assert_eq!(build_terminal_title(&Value::Null, "abc-31764-activity-color-is-not-displayed-in-tooltip", None).1, "abc-31764-activity-\u{2026}");
    }

    #[test]
    fn the_review_result_lays_out_the_outcome_for_the_panel() {
        let payload = json!({ "prId": 42, "title": "Fix bars", "prUrl": PR_URL, "sourceBranch": "feature/x", "targetBranch": "master" });
        let review = build_review_result(&ReviewResultInput { payload: &payload, dir: "/my repos/pr-review", editor: "cursor", review_started: true, review_note: Some("Cursor Agent is reviewing it."), stashed_files: &["a.ts".into()], tracked: true });
        assert_eq!(review, json!({
            "prId": 42, "title": "Fix bars", "prUrl": PR_URL, "sourceBranch": "feature/x", "targetBranch": "master", "worktree": "/my repos/pr-review",
            "editor": "cursor", "editorLabel": "Cursor", "openUrl": "cursor://file/my%20repos/pr-review", "agentLabel": "Cursor Agent (Ask mode, read-only)",
            "reviewStarted": true, "reviewNote": "Cursor Agent is reviewing it.", "stashedFiles": ["a.ts"], "tracked": true,
        }));
        let p = json!({ "sourceBranch": "x" });
        let claude = build_review_result(&ReviewResultInput { payload: &p, dir: "/wt", editor: "claude-code", review_started: true, review_note: None, stashed_files: &[], tracked: false });
        assert!(claude.get("openUrl").is_none());
        assert_eq!((claude["agentLabel"].clone(), claude["stashedFiles"].clone()), (json!("Claude Code"), json!([])));
        let failed = build_review_result(&ReviewResultInput { payload: &p, dir: "/wt", editor: "vscode", review_started: false, review_note: None, stashed_files: &[], tracked: false });
        assert_eq!((failed["openUrl"].clone(), failed.get("agentLabel").is_none(), failed["tracked"].clone()), (json!("vscode://file/wt"), true, json!(false)));
    }

    fn pr(extra: Value) -> Value {
        let mut p = json!({ "project": "ACME", "repo": "sample-app", "prId": 5429, "sourceBranch": "proj-31764-tooltip" });
        for (k, v) in extra.as_object().into_iter().flatten() {
            p[k] = v.clone();
        }
        p
    }
    fn live_record(extra: Value) -> Value {
        let mut r = json!({ "jobId": LIVE_JOB, "worktree": "/wt/sample-app.worktrees/pr-review", "payload": pr(json!({})), "summary": "Checked out.", "review": { "prId": 5429, "tracked": true, "worktree": "/wt/sample-app.worktrees/pr-review" } });
        for (k, v) in extra.as_object().into_iter().flatten() {
            r[k] = v.clone();
        }
        r
    }

    #[test]
    fn replace_job_id_and_same_pr_and_decide_start() {
        assert_eq!(parse_replace_job_id(&json!({ "replaceJobId": LIVE_JOB })).as_deref(), Some(LIVE_JOB));
        for bad in [json!({ "replaceJobId": "../../x" }), json!({ "replaceJobId": 5 }), json!({}), json!(null)] {
            assert_eq!(parse_replace_job_id(&bad), None);
        }
        assert!(same_pr(&pr(json!({})), &pr(json!({ "sourceBranch": "renamed", "title": "x" }))));
        assert!(same_pr(&pr(json!({})), &pr(json!({ "prId": "5429" }))));
        assert!(!same_pr(&pr(json!({})), &pr(json!({ "prId": 5430 }))));
        assert!(!same_pr(&pr(json!({})), &pr(json!({ "repo": "sample-app-rest" }))));
        assert!(same_pr(&pr(json!({ "prId": null })), &pr(json!({ "prId": 7 }))));
        assert!(!same_pr(&pr(json!({ "prId": null })), &pr(json!({ "prId": 7, "sourceBranch": "other" }))));

        assert_eq!(decide_start(&pr(json!({})), None, None), "start");
        assert_eq!(decide_start(&pr(json!({})), None, Some(LIVE_JOB)), "start");
        assert_eq!(decide_start(&pr(json!({})), Some(&live_record(json!({}))), None), "reuse");
        let other = pr(json!({ "prId": 6000, "sourceBranch": "other-branch" }));
        assert_eq!(decide_start(&other, Some(&live_record(json!({}))), None), "conflict");
        assert_eq!(decide_start(&other, Some(&live_record(json!({}))), Some(LIVE_JOB)), "replace");
        assert_eq!(decide_start(&other, Some(&live_record(json!({}))), Some("99999999-2222-4333-8444-555555555555")), "conflict");
    }

    #[test]
    fn conflicts_and_switched_results_name_the_live_review() {
        let c = build_conflict(&live_record(json!({})), "running");
        assert_eq!(c["code"], "review-in-progress");
        assert!(c["error"].as_str().unwrap().starts_with("A review of PR #5429 (proj-31764-tooltip) is in progress in /wt/sample-app"));
        assert_eq!(c["existing"], json!({ "jobId": LIVE_JOB, "state": "running", "worktree": "/wt/sample-app.worktrees/pr-review", "payload": pr(json!({})) }));
        let ctl = build_conflict(&live_record(json!({ "payload": pr(json!({ "sourceBranch": "a\u{1b}]0;x\u{7}b" })) })), "running");
        assert!(!has_control(ctl["error"].as_str().unwrap()));

        let focused = build_switched_result(&live_record(json!({})), "focused");
        assert!(focused["summary"].as_str().unwrap().starts_with("Switched to the review of PR #5429 (proj-31764-tooltip) already running in "));
        let mut want = live_record(json!({}))["review"].clone();
        want["switched"] = json!({ "focused": true, "note": "Its terminal window is in front." });
        assert_eq!(focused["review"], want);
        let missing = build_switched_result(&live_record(json!({})), "missing");
        assert_eq!(missing["review"]["switched"]["focused"], false);
        assert!(missing["summary"].as_str().unwrap().contains("couldn't be found"));
        assert!(build_switched_result(&live_record(json!({})), "unknown")["summary"].as_str().unwrap().contains("isn't known"));
        assert!(build_switched_result(&live_record(json!({})), "osascript failed: x")["summary"].as_str().unwrap().ends_with("front: osascript failed: x"));
        assert_eq!(describe_live_review(&json!({ "sourceBranch": "" })), "a pull request");
    }

    #[test]
    fn pr_key_and_decide_reuse() {
        assert_eq!(pr_key("P", "r", &json!(7), "b"), "P/r#7");
        assert_eq!(pr_key("P", "r", &Value::Null, "b"), "P/r@b");
        let id = "0f8b2d0e-1c3a-4b5d-9e7f-123456789abc";
        let sha = "a".repeat(40);
        let record = json!({ "sessionId": id, "cwd": "/wt/pr-review", "reviewedSha": sha, "reviewedAt": 1 });
        let reuse = |rec: Option<&Value>, head: &str, dir: &str, transcript: bool, ancestor: bool| decide_reuse(rec, head, dir, transcript, ancestor);
        assert_eq!(reuse(Some(&record), &sha, "/wt/pr-review", true, true), Reuse::Resume { session_id: id.into(), reviewed_sha: sha.clone(), reviewed_at: json!(1) });
        assert_eq!(reuse(None, &sha, "/wt/pr-review", true, true), Reuse::Review { since: None });
        let moved = "b".repeat(40);
        assert_eq!(reuse(Some(&record), &moved, "/wt/pr-review", true, true), Reuse::Review { since: Some(sha.clone()) });
        assert_eq!(reuse(Some(&record), &moved, "/wt/pr-review", true, false), Reuse::Review { since: None });
        assert!(matches!(reuse(Some(&record), &sha, "/wt/pr-review", false, true), Reuse::Review { .. }));
        assert!(matches!(reuse(Some(&record), &sha, "/elsewhere", true, true), Reuse::Review { .. }));
        let bad = json!({ "sessionId": "not-a-uuid", "cwd": "/wt/pr-review", "reviewedSha": sha, "reviewedAt": 1 });
        assert!(matches!(reuse(Some(&bad), &sha, "/wt/pr-review", true, true), Reuse::Review { .. }));
        let bad_sha = json!({ "sessionId": id, "cwd": "/wt/pr-review", "reviewedSha": "zz", "reviewedAt": 1 });
        assert_eq!(reuse(Some(&bad_sha), &sha, "/wt/pr-review", true, true), Reuse::Review { since: None });
    }
}
