//! Turns raw Bitbucket Server/DC REST JSON into stable internal shapes (port
//! of `core/bitbucket-normalize.js`). The only caller that ever sees a real
//! Bitbucket response is the API client; everything here is pure and defensive
//! so a Bitbucket upgrade that renames/removes/adds a field can't silently
//! break review-comment collection or, worse, panic and take the request
//! down. Every field is read defensively: missing -> a documented default,
//! unrecognized enum value -> "unknown", never a panic.
//!
//! Inputs are untyped JSON (`serde_json::Value`); outputs are typed and
//! serialize to exactly the JSON the Node service produced (the extension
//! reads them), with numbers kept as given.
//!
//! `tests/fixtures/bitbucket/docs-8.x` is hand-modeled on Atlassian's
//! *documented* response shapes, not a real capture. A real capture goes in a
//! sibling `real-<version>/` directory and is tested automatically.

use serde::Serialize;
use serde_json::{Number, Value};
use std::collections::HashMap;

fn obj(v: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    v.and_then(Value::as_object)
}

fn string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn number(v: Option<&Value>) -> Option<Number> {
    match v {
        Some(Value::Number(n)) => Some(n.clone()),
        _ => None,
    }
}

fn float(n: &Option<Number>) -> f64 {
    n.as_ref().and_then(Number::as_f64).unwrap_or(f64::NEG_INFINITY)
}

/// The raw REST keys each normalizer reads, keyed by raw-object type. The
/// single source of truth: the live contract check compares a real Bitbucket
/// response's keys against these lists instead of a second, hand-maintained
/// key list drifting out of sync. Nested reads use a dotted path.
pub struct ExpectedKeys {
    pub pull_request: &'static [&'static str],
    pub activity: &'static [&'static str],
    pub comment: &'static [&'static str],
    pub comment_anchor: &'static [&'static str],
    pub application_properties: &'static [&'static str],
}

pub const EXPECTED_KEYS: ExpectedKeys = ExpectedKeys {
    pull_request: &[
        "id",
        "version",
        "title",
        "state",
        "fromRef.displayId",
        "fromRef.latestCommit",
        "fromRef.repository.slug",
        "fromRef.repository.project.key",
        "toRef.displayId",
        "toRef.latestCommit",
        "toRef.repository.slug",
        "toRef.repository.project.key",
        "author.user.slug",
    ],
    activity: &["action", "comment", "commentAnchor"],
    comment: &[
        "id",
        "version",
        "text",
        "author.slug",
        "author.displayName",
        "author.name",
        "createdDate",
        "state",
        "severity",
        "threadResolved",
        "comments",
    ],
    comment_anchor: &["path", "line", "lineType", "fileType", "diffType", "orphaned"],
    application_properties: &["version"],
};

/// Dotted paths a normalizer reads but that a real Bitbucket Server/DC
/// instance may legitimately not return: confirmed on 9.4.16, whose
/// `GET .../pull-requests/{id}` has no `properties` key at all (only the
/// *list* endpoint's `?withProperties=true` returns per-PR counts). Kept apart
/// from [`EXPECTED_KEYS`] so the contract check can report a missing optional
/// key as informational rather than as a broken contract.
pub const OPTIONAL_PULL_REQUEST_KEYS: &[&str] = &["properties.openTaskCount", "properties.commentCount"];

/// A ref's `repository` as `{projectKey, slug}`, each null when missing or not
/// a string. Kept even though the PR endpoint is already scoped to one repo: a
/// fork PR's `fromRef` lives in a DIFFERENT repository, and
/// address-review-comments must refuse it rather than push a same-named branch
/// in the wrong repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefRepo {
    pub project_key: Option<String>,
    pub slug: Option<String>,
}

fn normalize_ref_repo(r: Option<&serde_json::Map<String, Value>>) -> RefRepo {
    let repo = obj(r.and_then(|r| r.get("repository")));
    let project = obj(repo.and_then(|r| r.get("project")));
    RefRepo { project_key: string(project.and_then(|p| p.get("key"))), slug: string(repo.and_then(|r| r.get("slug"))) }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub id: Option<Number>,
    pub version: Option<Number>,
    pub title: Option<String>,
    pub state: Option<String>,
    pub from_branch: Option<String>,
    pub from_sha: Option<String>,
    pub from_repo: RefRepo,
    pub to_branch: Option<String>,
    pub to_sha: Option<String>,
    pub to_repo: RefRepo,
    pub author_slug: Option<String>,
    /// Omitted (not even null) when `properties` doesn't carry it: callers can
    /// tell "we don't know" apart from "it's zero".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_task_count: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_count: Option<Number>,
}

/// `raw` is a `GET .../pull-requests/{id}` response; `None` if it isn't an object.
pub fn normalize_pull_request(raw: &Value) -> Option<PullRequest> {
    let raw = raw.as_object()?;
    let from_ref = obj(raw.get("fromRef"));
    let to_ref = obj(raw.get("toRef"));
    let author_user = obj(obj(raw.get("author")).and_then(|a| a.get("user")));
    let props = obj(raw.get("properties"));
    fn get<'a>(m: Option<&'a serde_json::Map<String, Value>>, k: &str) -> Option<&'a Value> {
        m.and_then(|m| m.get(k))
    }
    Some(PullRequest {
        id: number(raw.get("id")),
        version: number(raw.get("version")),
        title: string(raw.get("title")),
        state: string(raw.get("state")),
        from_branch: string(get(from_ref, "displayId")),
        from_sha: string(get(from_ref, "latestCommit")),
        from_repo: normalize_ref_repo(from_ref),
        to_branch: string(get(to_ref, "displayId")),
        to_sha: string(get(to_ref, "latestCommit")),
        to_repo: normalize_ref_repo(to_ref),
        author_slug: string(get(author_user, "slug")),
        open_task_count: number(get(props, "openTaskCount")),
        comment_count: number(get(props, "commentCount")),
    })
}

fn known(raw: Option<&Value>, set: &[&str]) -> Option<String> {
    raw.and_then(Value::as_str).filter(|s| set.contains(s)).map(str::to_string)
}

/// A comment with no `severity` at all is Bitbucket's ordinary case: default
/// "NORMAL". A `severity` that IS present but isn't one Bitbucket documents
/// maps to "unknown".
fn normalize_severity(raw: Option<&Value>) -> String {
    match raw {
        None | Some(Value::Null) => "NORMAL".into(),
        r => known(r, &["NORMAL", "BLOCKER"]).unwrap_or_else(|| "unknown".into()),
    }
}

/// Missing `state` defaults to "OPEN": an unresolved/undetermined comment
/// should keep showing up as needing attention rather than silently
/// disappearing. An unrecognized value maps to "unknown".
fn normalize_state(raw: Option<&Value>) -> String {
    match raw {
        None | Some(Value::Null) => "OPEN".into(),
        r => known(r, &["OPEN", "RESOLVED", "PENDING"]).unwrap_or_else(|| "unknown".into()),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Anchor {
    pub path: Option<String>,
    pub line: Option<Number>,
    pub line_type: String,
    pub file_type: String,
    pub diff_type: String,
    pub orphaned: bool,
}

/// `None` when the activity carries no anchor (an ordinary top-level PR
/// comment, or a reply): never an anchor-shaped object with guessed fields.
fn normalize_anchor(raw: Option<&Value>) -> Option<Anchor> {
    let a = obj(raw)?;
    let unknown = |v: Option<String>| v.unwrap_or_else(|| "unknown".into());
    Some(Anchor {
        path: string(a.get("path")),
        line: number(a.get("line")),
        line_type: unknown(known(a.get("lineType"), &["ADDED", "REMOVED", "CONTEXT"])),
        file_type: unknown(known(a.get("fileType"), &["FROM", "TO"])),
        diff_type: unknown(known(a.get("diffType"), &["EFFECTIVE", "COMMIT", "RANGE"])),
        orphaned: a.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    pub id: Option<Number>,
    pub version: Option<Number>,
    pub text: Option<String>,
    pub author_slug: Option<String>,
    pub author_name: Option<String>,
    /// Bitbucket's own epoch-millis number, kept as-is.
    pub created_at: Option<Number>,
    pub severity: String,
    pub state: String,
    /// "We don't know" is a real, distinct third value, so a missing or
    /// non-boolean raw value stays null.
    pub thread_resolved: Option<bool>,
    pub anchor: Option<Anchor>,
    pub replies: Vec<ReviewComment>,
}

/// Normalizes one raw `comment` (from an activity, or a parent's `comments[]`).
/// `raw_anchor` is only passed for the top-level call: a reply is recursed into
/// with no anchor, since Bitbucket never anchors a reply independently of its
/// parent.
fn normalize_comment(raw: Option<&Value>, raw_anchor: Option<&Value>) -> Option<ReviewComment> {
    let c = obj(raw)?;
    let author = obj(c.get("author"));
    let a = |k: &str| author.and_then(|a| a.get(k));
    let replies = c
        .get("comments")
        .and_then(Value::as_array)
        .map(|rs| rs.iter().filter_map(|r| normalize_comment(Some(r), None)).collect())
        .unwrap_or_default();
    Some(ReviewComment {
        id: number(c.get("id")),
        version: number(c.get("version")),
        text: string(c.get("text")),
        author_slug: string(a("slug")),
        author_name: string(a("displayName")).or_else(|| string(a("name"))),
        created_at: number(c.get("createdDate")),
        severity: normalize_severity(c.get("severity")),
        state: normalize_state(c.get("state")),
        thread_resolved: c.get("threadResolved").and_then(Value::as_bool),
        anchor: normalize_anchor(raw_anchor),
        replies,
    })
}

/// `pages` is an array of paged `GET .../activities` responses, or a single
/// such object, or anything else (gives `[]`). Only `COMMENTED` activities
/// contribute; comments are deduped by id (across pages and activities: the
/// same comment can appear more than once as it gets edited) and the highest
/// `version` wins, in the position of the first sighting.
pub fn normalize_activities(pages: &Value) -> Vec<ReviewComment> {
    let single;
    let page_list: &[Value] = match pages {
        Value::Array(a) => a,
        Value::Object(_) => {
            single = [pages.clone()];
            &single
        }
        _ => &[],
    };
    let mut ordered: Vec<ReviewComment> = Vec::new();
    let mut index: HashMap<u64, usize> = HashMap::new();
    for page in page_list {
        let Some(values) = obj(Some(page)).and_then(|p| p.get("values")).and_then(Value::as_array) else { continue };
        for activity in values {
            let Some(act) = activity.as_object() else { continue };
            if act.get("action").and_then(Value::as_str) != Some("COMMENTED") {
                continue;
            }
            let Some(comment) = normalize_comment(act.get("comment"), act.get("commentAnchor")) else { continue };
            let Some(id) = comment.id.as_ref().and_then(Number::as_f64) else { continue };
            match index.get(&id.to_bits()) {
                Some(&i) => {
                    if float(&comment.version) > float(&ordered[i].version) {
                        ordered[i] = comment;
                    }
                }
                None => {
                    index.insert(id.to_bits(), ordered.len());
                    ordered.push(comment);
                }
            }
        }
    }
    ordered
}

/// Parses "8.19.1" (or any "MAJOR.MINOR..." prefix) into `(major, minor)`.
/// Anything unparseable maps to (0, 0): deliberately below every capability
/// threshold, so an unrecognized version is treated as the oldest possible one.
fn parse_version(version: Option<&str>) -> (u64, u64) {
    let Some(v) = version else { return (0, 0) };
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let major_len = digits(v);
    if major_len == 0 || !v[major_len..].starts_with('.') {
        return (0, 0);
    }
    let rest = &v[major_len + 1..];
    let minor_len = digits(rest);
    if minor_len == 0 {
        return (0, 0);
    }
    let num = |s: &str| s.parse::<u64>().unwrap_or(u64::MAX);
    (num(&v[..major_len]), num(&rest[..minor_len]))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppProperties {
    pub version: Option<String>,
    pub major: u64,
    pub minor: u64,
}

/// `raw` is a `GET /rest/api/1.0/application-properties` response.
pub fn normalize_app_properties(raw: &Value) -> Option<AppProperties> {
    let r = raw.as_object()?;
    let version = string(r.get("version"));
    let (major, minor) = parse_version(version.as_deref());
    Some(AppProperties { version, major, minor })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// `threadResolved` (on a comment) appears in Bitbucket Server/DC 8.x+.
    pub thread_resolved_field: bool,
    /// `/rest/api/latest/.../commits/{sha}/builds` needs 7.14+.
    pub commit_builds_endpoint: bool,
}

pub fn capabilities_for_parts(major: u64, minor: u64) -> Capabilities {
    Capabilities { thread_resolved_field: major >= 8, commit_builds_endpoint: major > 7 || (major == 7 && minor >= 14) }
}

/// `version` is either the `{major, minor}` [`normalize_app_properties`]
/// produces, or a raw version string, accepted directly so a caller with only
/// the string needn't round-trip it. Anything else (missing, malformed) is
/// (0, 0), which is below every threshold: the conservative, everything-off
/// answer when the server's version isn't known.
pub fn capabilities_for(version: &Value) -> Capabilities {
    match version {
        Value::String(s) => {
            let (major, minor) = parse_version(Some(s));
            capabilities_for_parts(major, minor)
        }
        Value::Object(o) => match (o.get("major").and_then(Value::as_f64), o.get("minor").and_then(Value::as_f64)) {
            (Some(major), Some(minor)) => Capabilities {
                thread_resolved_field: major >= 8.0,
                commit_builds_endpoint: major > 7.0 || (major == 7.0 && minor >= 14.0),
            },
            _ => capabilities_for_parts(0, 0),
        },
        _ => capabilities_for_parts(0, 0),
    }
}

// ---- Ticket workspace / ticket to PR ----

/// The first `links.self[].href` of a raw PR (its page URL), or `None`.
fn self_href(raw: &serde_json::Map<String, Value>) -> Option<String> {
    obj(raw.get("links"))?
        .get("self")?
        .as_array()?
        .iter()
        .find_map(|l| obj(Some(l)).and_then(|l| l.get("href")).and_then(Value::as_str))
        .map(str::to_string)
}

/// A PR plus the two fields a ticket view needs: its page `url` and `updatedAt`
/// (epoch ms).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestSummary {
    #[serde(flatten)]
    pub base: PullRequest,
    pub url: Option<String>,
    pub updated_at: Option<Number>,
}

pub fn normalize_pull_request_summary(raw: &Value) -> Option<PullRequestSummary> {
    let base = normalize_pull_request(raw)?;
    let r = raw.as_object()?;
    Some(PullRequestSummary { base, url: self_href(r), updated_at: number(r.get("updatedDate")) })
}

/// A paged `GET .../pull-requests` response -> PR summaries (entries with no
/// numeric id are dropped). Anything else -> `[]`.
pub fn normalize_pull_request_list(raw: &Value) -> Vec<PullRequestSummary> {
    let Some(values) = obj(Some(raw)).and_then(|r| r.get("values")).and_then(Value::as_array) else { return vec![] };
    values.iter().filter_map(normalize_pull_request_summary).filter(|pr| pr.base.id.is_some()).collect()
}

const BUILD_STATES: [&str; 3] = ["FAILED", "INPROGRESS", "SUCCESSFUL"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Build {
    pub state: String,
    pub key: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub at: Option<Number>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub struct BuildCounts {
    pub successful: u32,
    pub failed: u32,
    pub inprogress: u32,
    #[serde(rename = "unknown")]
    pub unknown: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildStatuses {
    pub state: Option<String>,
    pub counts: BuildCounts,
    pub builds: Vec<Build>,
}

/// A paged build-status response (either endpoint) -> `{state, counts,
/// builds}`; `state` is the worst of the builds (FAILED, then INPROGRESS, then
/// SUCCESSFUL), or null when there are none. Unknown states count as "unknown"
/// and never make the summary look green.
pub fn normalize_build_statuses(raw: &Value) -> BuildStatuses {
    let values = obj(Some(raw)).and_then(|r| r.get("values")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let builds: Vec<Build> = values
        .iter()
        .filter_map(|b| obj(Some(b)))
        .map(|b| Build {
            state: known(b.get("state"), &BUILD_STATES).unwrap_or_else(|| "unknown".into()),
            key: string(b.get("key")),
            name: string(b.get("name")),
            url: string(b.get("url")),
            at: number(b.get("dateAdded")),
        })
        .collect();
    let mut counts = BuildCounts::default();
    for b in &builds {
        match b.state.as_str() {
            "SUCCESSFUL" => counts.successful += 1,
            "FAILED" => counts.failed += 1,
            "INPROGRESS" => counts.inprogress += 1,
            _ => counts.unknown += 1,
        }
    }
    let state = if counts.failed > 0 {
        Some("FAILED")
    } else if counts.inprogress > 0 || counts.unknown > 0 {
        Some("INPROGRESS")
    } else if counts.successful > 0 {
        Some("SUCCESSFUL")
    } else {
        None
    };
    BuildStatuses { state: state.map(str::to_string), counts, builds: builds.into_iter().take(10).collect() }
}

/// A default-branch response (`{id: "refs/heads/master", displayId}`) -> the
/// plain branch name, or `None`.
pub fn normalize_default_branch(raw: &Value) -> Option<String> {
    let r = raw.as_object()?;
    if let Some(d) = r.get("displayId").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        return Some(d.to_string());
    }
    let id = r.get("id").and_then(Value::as_str)?.strip_prefix("refs/heads/")?;
    (!id.is_empty()).then(|| id.to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub id: Number,
    pub slug: Option<String>,
    pub project_key: Option<String>,
}

/// `GET .../repos/{slug}` -> `{id, slug, projectKey}`, or `None` without a numeric id.
pub fn normalize_repository(raw: &Value) -> Option<Repository> {
    let r = raw.as_object()?;
    let id = number(r.get("id"))?;
    Some(Repository { id, slug: string(r.get("slug")), project_key: string(obj(r.get("project")).and_then(|p| p.get("key"))) })
}

/// The default-reviewers plugin's user array -> unique usernames (`name`, else
/// `slug`) of active users. Anything else -> `[]`.
pub fn normalize_default_reviewers(raw: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for u in raw.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let Some(u) = u.as_object() else { continue };
        if u.get("active") == Some(&Value::Bool(false)) {
            continue;
        }
        let name = string(u.get("name")).filter(|n| !n.is_empty()).or_else(|| string(u.get("slug")));
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    names
}

// ---- The watchers and the digest ----

const REVIEWER_STATUSES: [&str; 3] = ["APPROVED", "NEEDS_WORK", "UNAPPROVED"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reviewer {
    pub name: String,
    pub status: String,
}

/// A dashboard PR: the summary plus its repo (from toRef), reviewers
/// (`{name, status}`, unknown statuses as "unknown") and approval count.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DashboardPullRequest {
    #[serde(flatten)]
    pub summary: PullRequestSummary,
    pub project: String,
    pub repo: String,
    pub reviewers: Vec<Reviewer>,
    pub approvals: usize,
}

/// `None` without an id, project or repo.
pub fn normalize_dashboard_pull_request(raw: &Value) -> Option<DashboardPullRequest> {
    let summary = normalize_pull_request_summary(raw)?;
    summary.base.id.as_ref()?;
    let project = summary.base.to_repo.project_key.clone().filter(|p| !p.is_empty())?;
    let repo = summary.base.to_repo.slug.clone().filter(|s| !s.is_empty())?;
    let reviewers: Vec<Reviewer> = raw
        .get("reviewers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|r| {
            let r = r.as_object()?;
            let user = obj(r.get("user"))?;
            let name = string(user.get("name")).or_else(|| string(user.get("slug"))).filter(|n| !n.is_empty())?;
            let status = known(r.get("status"), &REVIEWER_STATUSES).unwrap_or_else(|| {
                if r.get("approved") == Some(&Value::Bool(true)) { "APPROVED".into() } else { "unknown".into() }
            });
            Some(Reviewer { name, status })
        })
        .collect();
    let approvals = reviewers.iter().filter(|r| r.status == "APPROVED").count();
    Some(DashboardPullRequest { summary, project, repo, reviewers, approvals })
}

pub fn normalize_dashboard_pull_requests(raw: &Value) -> Vec<DashboardPullRequest> {
    let Some(values) = obj(Some(raw)).and_then(|r| r.get("values")).and_then(Value::as_array) else { return vec![] };
    values.iter().filter_map(normalize_dashboard_pull_request).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeStatus {
    pub conflicted: Option<bool>,
    pub can_merge: Option<bool>,
    pub outcome: Option<String>,
}

/// `GET .../merge` -> `{conflicted, canMerge, outcome}` (null when absent).
pub fn normalize_merge_status(raw: &Value) -> MergeStatus {
    let Some(r) = raw.as_object() else { return MergeStatus { conflicted: None, can_merge: None, outcome: None } };
    MergeStatus {
        conflicted: r
            .get("conflicted")
            .and_then(Value::as_bool)
            .or_else(|| (r.get("outcome").and_then(Value::as_str) == Some("CONFLICTED")).then_some(true)),
        can_merge: r.get("canMerge").and_then(Value::as_bool),
        outcome: string(r.get("outcome")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn fixtures_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bitbucket")
    }

    /// Every fixture directory (docs-8.x today, a future real-9.x/ automatically).
    fn fixture_dirs() -> Vec<String> {
        let mut dirs: Vec<String> = fs::read_dir(fixtures_root())
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        dirs.sort();
        dirs
    }

    fn read_json(dir: &str, name: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(fixtures_root().join(dir).join(name)).unwrap()).unwrap()
    }

    /// All activities-pageN.json files in a fixture dir, in filename order.
    fn read_activity_pages(dir: &str) -> Value {
        let mut names: Vec<String> = fs::read_dir(fixtures_root().join(dir))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("activities-page") && n.ends_with(".json"))
            .collect();
        names.sort();
        Value::Array(names.iter().map(|n| read_json(dir, n)).collect())
    }

    fn get<'a>(v: &'a Value, dotted: &str) -> Option<&'a Value> {
        dotted.split('.').try_fold(v, |acc, k| acc.as_object().and_then(|o| o.get(k)))
    }

    fn n(v: i64) -> Option<Number> {
        Some(Number::from(v))
    }

    #[test]
    fn every_fixture_normalizes_to_the_stable_shape() {
        for dir in fixture_dirs() {
            let pr = normalize_pull_request(&read_json(&dir, "pull-request.json")).unwrap();
            assert!(pr.id.is_some() && pr.version.is_some(), "{dir}");
            for s in [&pr.title, &pr.state, &pr.from_branch, &pr.from_sha, &pr.to_branch, &pr.to_sha, &pr.author_slug] {
                assert!(s.is_some(), "{dir}");
            }

            let pages = read_activity_pages(&dir);
            let comments = normalize_activities(&pages);
            assert!(!comments.is_empty(), "{dir}");
            let ids: Vec<f64> = comments.iter().map(|c| c.id.as_ref().unwrap().as_f64().unwrap()).collect();
            let unique: std::collections::HashSet<u64> = ids.iter().map(|i| i.to_bits()).collect();
            assert_eq!(ids.len(), unique.len(), "no duplicate top-level comment ids");
            for c in &comments {
                assert!(["NORMAL", "BLOCKER", "unknown"].contains(&c.severity.as_str()));
                assert!(["OPEN", "RESOLVED", "PENDING", "unknown"].contains(&c.state.as_str()));
                if let Some(a) = &c.anchor {
                    assert!(["ADDED", "REMOVED", "CONTEXT", "unknown"].contains(&a.line_type.as_str()));
                    assert!(["FROM", "TO", "unknown"].contains(&a.file_type.as_str()));
                    assert!(["EFFECTIVE", "COMMIT", "RANGE", "unknown"].contains(&a.diff_type.as_str()));
                }
            }
            // Only COMMENTED activities contribute.
            let mut commented = std::collections::HashSet::new();
            for page in pages.as_array().unwrap() {
                for act in page["values"].as_array().unwrap() {
                    if act["action"] == "COMMENTED" {
                        commented.insert(act["comment"]["id"].as_f64().unwrap().to_bits());
                    }
                }
            }
            for c in &comments {
                assert!(commented.contains(&c.id.as_ref().unwrap().as_f64().unwrap().to_bits()), "{dir}");
            }

            let props = normalize_app_properties(&read_json(&dir, "application-properties.json")).unwrap();
            let _ = capabilities_for_parts(props.major, props.minor);
        }
    }

    #[test]
    fn docs_8x_pull_request_maps_every_field_verbatim() {
        let pr = normalize_pull_request(&read_json("docs-8.x", "pull-request.json")).unwrap();
        assert_eq!(
            serde_json::to_value(&pr).unwrap(),
            json!({
                "id": 42, "version": 3, "title": "Fix widget rendering on Safari", "state": "OPEN",
                "fromBranch": "fix/widget-flicker", "fromSha": "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
                "fromRepo": { "projectKey": "ACME", "slug": "sample-app" },
                "toBranch": "master", "toSha": "0f1e2d3c4b5a69788796a5b4c3d2e1f0a9b8c7d6",
                "toRepo": { "projectKey": "ACME", "slug": "sample-app" },
                "authorSlug": "jsmith", "openTaskCount": 1, "commentCount": 8,
            })
        );
    }

    #[test]
    fn docs_8x_activities_cover_open_resolved_blocker_reply_anchors_dedup_and_unknown_enums() {
        let comments = normalize_activities(&read_activity_pages("docs-8.x"));
        assert_eq!(comments.len(), 9, "9 distinct top-level comments (ids 1,2,3,4,5,6,7,9,10)");
        let by = |id: i64| comments.iter().find(|c| c.id == n(id)).unwrap_or_else(|| panic!("comment {id}"));

        let open = by(1);
        assert_eq!((open.state.as_str(), open.severity.as_str(), open.thread_resolved), ("OPEN", "NORMAL", Some(false)));
        assert_eq!(open.version, n(2), "id 1 appears on both pages (v1, v2): the higher version wins");
        assert_eq!(open.text.as_deref(), Some("Please rename this variable for clarity \u{2014} done, thanks!"));
        assert_eq!(open.created_at, n(1_706_000_100_000));
        assert_eq!((open.author_slug.as_deref(), open.author_name.as_deref()), (Some("asmith"), Some("Alice Smith")));

        let resolved = by(2);
        assert_eq!((resolved.state.as_str(), resolved.thread_resolved), ("RESOLVED", Some(true)));
        assert_eq!(by(3).severity, "BLOCKER");

        let with_reply = by(4);
        assert_eq!(with_reply.replies.len(), 1);
        let reply = &with_reply.replies[0];
        assert_eq!(reply.id, n(8));
        assert_eq!(reply.text.as_deref(), Some("Good idea, added in the latest push."));
        assert!(reply.anchor.is_none(), "a reply has no anchor of its own");
        assert_eq!(reply.created_at, n(1_706_000_450_000));
        assert_eq!((reply.author_slug.as_deref(), reply.author_name.as_deref()), (Some("asmith"), Some("Alice Smith")));

        assert!(by(5).anchor.as_ref().unwrap().orphaned);
        let removed = by(6).anchor.as_ref().unwrap();
        assert_eq!((removed.line_type.as_str(), removed.file_type.as_str()), ("REMOVED", "FROM"));
        assert_eq!(by(7).anchor.as_ref().unwrap().diff_type, "COMMIT");
        let weird = by(9);
        assert_eq!((weird.severity.as_str(), weird.state.as_str()), ("unknown", "unknown"));
        assert!(by(10).anchor.is_none());
    }

    /// The Node implementation's own output for the fixtures, byte for byte as
    /// JSON values: the strongest check that the port changed no behavior.
    #[test]
    fn output_matches_what_the_node_implementation_produced_for_the_fixtures() {
        let golden = |name: &str| read_json("docs-8.x", name);
        let pr = normalize_pull_request(&read_json("docs-8.x", "pull-request.json")).unwrap();
        assert_eq!(serde_json::to_value(pr).unwrap(), golden("golden-pull-request.json"));
        let comments = normalize_activities(&read_activity_pages("docs-8.x"));
        assert_eq!(serde_json::to_value(comments).unwrap(), golden("golden-activities.json"));
        let props = normalize_app_properties(&read_json("docs-8.x", "application-properties.json")).unwrap();
        let caps = capabilities_for_parts(props.major, props.minor);
        assert_eq!(json!({ "props": props, "caps": caps }), golden("golden-app-properties.json"));
    }

    #[test]
    fn docs_8x_capabilities_and_app_properties() {
        let props = normalize_app_properties(&read_json("docs-8.x", "application-properties.json")).unwrap();
        assert_eq!((props.version.as_deref(), props.major, props.minor), (Some("8.19.1"), 8, 19));
        assert_eq!(capabilities_for_parts(props.major, props.minor), Capabilities { thread_resolved_field: true, commit_builds_endpoint: true });
    }

    #[test]
    fn capabilities_follow_the_version_thresholds() {
        let c = capabilities_for_parts;
        assert!(c(8, 0).thread_resolved_field && c(9, 0).thread_resolved_field && !c(7, 20).thread_resolved_field);
        assert!(c(7, 14).commit_builds_endpoint && !c(7, 13).commit_builds_endpoint);
        assert!(c(8, 0).commit_builds_endpoint && !c(6, 99).commit_builds_endpoint);
        assert_eq!(c(0, 0), Capabilities { thread_resolved_field: false, commit_builds_endpoint: false });
    }

    #[test]
    fn app_properties_parse_prefixes_and_reject_junk() {
        for (v, want) in [("8.19.1", (8, 19)), ("9.4", (9, 4)), ("not-a-version", (0, 0)), ("8", (0, 0)), ("8.x", (0, 0)), (".5", (0, 0)), ("v8.1", (0, 0))] {
            let p = normalize_app_properties(&json!({ "version": v })).unwrap();
            assert_eq!((p.major, p.minor), want, "{v}");
        }
        assert_eq!(normalize_app_properties(&json!({})).unwrap(), AppProperties { version: None, major: 0, minor: 0 });
        for bad in [json!(null), json!("nope")] {
            assert!(normalize_app_properties(&bad).is_none());
        }
    }

    #[test]
    fn capabilities_for_accepts_a_string_a_parts_object_and_degrades_for_anything_else() {
        let off = Capabilities { thread_resolved_field: false, commit_builds_endpoint: false };
        assert_eq!(capabilities_for(&json!("8.19.1")), capabilities_for_parts(8, 19));
        assert_eq!(capabilities_for(&json!({ "major": 7, "minor": 14 })), capabilities_for_parts(7, 14));
        for bad in [json!(null), json!({}), json!({ "major": 8 }), json!({ "major": "8", "minor": 1 }), json!(8)] {
            assert_eq!(capabilities_for(&bad), off, "{bad}");
        }
    }

    #[test]
    fn pull_request_tolerates_non_objects_and_missing_fields() {
        for bad in [json!(null), json!("nope"), json!(42), json!([])] {
            assert!(normalize_pull_request(&bad).is_none());
        }
        let pr = serde_json::to_value(normalize_pull_request(&json!({})).unwrap()).unwrap();
        assert_eq!(
            pr,
            json!({
                "id": null, "version": null, "title": null, "state": null, "fromBranch": null, "fromSha": null,
                "fromRepo": { "projectKey": null, "slug": null }, "toBranch": null, "toSha": null,
                "toRepo": { "projectKey": null, "slug": null }, "authorSlug": null,
            })
        );
        let fork = normalize_pull_request(&json!({
            "fromRef": { "repository": { "slug": "sample-app", "project": { "key": "~JSMITH" } } },
            "toRef": { "repository": { "slug": "sample-app", "project": { "key": "ACME" } } },
        }))
        .unwrap();
        assert_eq!(fork.from_repo, RefRepo { project_key: Some("~JSMITH".into()), slug: Some("sample-app".into()) });
        assert_eq!(fork.to_repo, RefRepo { project_key: Some("ACME".into()), slug: Some("sample-app".into()) });
        let junk = normalize_pull_request(&json!({ "fromRef": { "repository": "nope" }, "toRef": { "repository": { "slug": 7, "project": [] } } })).unwrap();
        let none = RefRepo { project_key: None, slug: None };
        assert_eq!((junk.from_repo, junk.to_repo), (none.clone(), none));
    }

    #[test]
    fn activities_tolerate_odd_inputs() {
        for bad in [json!(null), json!("nope"), json!(42)] {
            assert!(normalize_activities(&bad).is_empty());
        }
        let page = read_json("docs-8.x", "activities-page1.json");
        assert_eq!(normalize_activities(&page), normalize_activities(&json!([page.clone()])), "a single page object is accepted");
        for pages in [json!([{ "isLastPage": true }]), json!([{ "values": "nope" }]), json!([null, null, { "values": [] }])] {
            assert!(normalize_activities(&pages).is_empty(), "{pages}");
        }
        for act in [json!({ "action": "COMMENTED" }), json!({ "action": "COMMENTED", "comment": "nope" })] {
            assert!(normalize_activities(&json!([{ "values": [act] }])).is_empty());
        }
    }

    fn one(activity: Value) -> ReviewComment {
        normalize_activities(&json!([{ "values": [activity] }])).remove(0)
    }

    #[test]
    fn comment_defaults_and_fallbacks() {
        let c = one(json!({ "action": "COMMENTED", "comment": { "id": 1, "version": 1, "text": "no author or date" } }));
        assert_eq!((c.created_at, c.author_slug, c.author_name), (None, None, None));
        let c = one(json!({ "action": "COMMENTED", "comment": { "id": 1, "version": 1, "text": "x", "author": { "name": "bwhite", "slug": "bwhite" } } }));
        assert_eq!((c.author_slug.as_deref(), c.author_name.as_deref()), (Some("bwhite"), Some("bwhite")), "authorName falls back to author.name");
        let anchor = json!({ "path": "src/x.js", "line": 1, "lineType": "CONTEXT", "fileType": "TO", "diffType": "EFFECTIVE" });
        let c = one(json!({ "action": "COMMENTED", "comment": { "id": 1, "version": 1, "text": "x" }, "commentAnchor": anchor }));
        assert!(!c.anchor.unwrap().orphaned, "orphaned defaults to false");
        let c = one(json!({ "action": "COMMENTED", "comment": { "id": 1, "version": 1, "text": "x", "comments": "not-an-array" } }));
        assert!(c.replies.is_empty());
        let c = one(json!({ "action": "COMMENTED", "comment": { "id": 1, "version": 1, "text": "x" }, "commentAnchor": "nope" }));
        assert!(c.anchor.is_none());
        // A null severity/state is the ordinary default; a present non-string is unknown.
        let c = one(json!({ "action": "COMMENTED", "comment": { "id": 1, "severity": null, "state": 5 } }));
        assert_eq!((c.severity.as_str(), c.state.as_str(), c.thread_resolved), ("NORMAL", "unknown", None));
    }

    #[test]
    fn dedup_keeps_the_first_position_and_the_highest_version() {
        let page = |v: Value| json!({ "values": v });
        let c = |id: i64, ver: Value, text: &str| json!({ "action": "COMMENTED", "comment": { "id": id, "version": ver, "text": text } });
        let got = normalize_activities(&json!([
            page(json!([c(1, json!(1), "a1"), c(2, json!(1), "b1")])),
            page(json!([c(1, json!(3), "a3"), c(2, json!(0), "b0"), c(3, json!(null), "c-null"), c(3, json!(null), "c-null-again")])),
        ]));
        let texts: Vec<&str> = got.iter().map(|c| c.text.as_deref().unwrap()).collect();
        assert_eq!(texts, ["a3", "b1", "c-null"]);
    }

    #[test]
    fn expected_keys_are_complete_for_every_fixture() {
        for dir in fixture_dirs() {
            let pr = read_json(&dir, "pull-request.json");
            for p in EXPECTED_KEYS.pull_request {
                assert!(get(&pr, p).is_some(), "{dir}/pull-request.json has no value at {p}");
            }
            assert!(read_json(&dir, "application-properties.json").get("version").is_some());
        }
        for p in OPTIONAL_PULL_REQUEST_KEYS {
            assert!(!EXPECTED_KEYS.pull_request.contains(p), "{p} must not also be required");
            assert!(get(&read_json("docs-8.x", "pull-request.json"), p).is_some(), "{p}");
        }
        // Every key a comment/anchor is read from is listed, and some fixture supplies each.
        let comment_paths = ["id", "version", "text", "author.slug", "author.displayName", "createdDate", "state", "severity", "threadResolved", "comments"];
        let anchor_paths = ["path", "line", "lineType", "fileType", "diffType", "orphaned"];
        for p in comment_paths {
            assert!(EXPECTED_KEYS.comment.contains(&p), "{p}");
        }
        for p in anchor_paths {
            assert!(EXPECTED_KEYS.comment_anchor.contains(&p), "{p}");
        }
        for p in ["action", "comment", "commentAnchor"] {
            assert!(EXPECTED_KEYS.activity.contains(&p));
        }
        let (mut seen_c, mut seen_a) = (std::collections::HashSet::new(), std::collections::HashSet::new());
        for dir in fixture_dirs() {
            for page in read_activity_pages(&dir).as_array().unwrap() {
                for act in page["values"].as_array().unwrap().iter().filter(|a| a["action"] == "COMMENTED") {
                    comment_paths.iter().filter(|p| get(&act["comment"], p).is_some()).for_each(|p| {
                        seen_c.insert(*p);
                    });
                    if let Some(anchor) = act.get("commentAnchor").filter(|a| !a.is_null()) {
                        anchor_paths.iter().filter(|p| get(anchor, p).is_some()).for_each(|p| {
                            seen_a.insert(*p);
                        });
                    }
                }
            }
        }
        assert_eq!(seen_c.len(), comment_paths.len(), "no fixture comment supplies some path");
        assert_eq!(seen_a.len(), anchor_paths.len(), "no fixture anchor supplies some path");
    }

    // ---- Ticket workspace / digest normalizers ----

    fn raw_pr(id: Value, extra: Value) -> Value {
        let mut pr = json!({
            "id": id, "version": 1, "title": "T", "state": "OPEN", "updatedDate": 1700000000000_i64,
            "fromRef": { "displayId": "feat", "latestCommit": "a", "repository": { "slug": "app", "project": { "key": "ACME" } } },
            "toRef": { "displayId": "master", "latestCommit": "b", "repository": { "slug": "app", "project": { "key": "ACME" } } },
            "author": { "user": { "slug": "jsmith" } },
            "links": { "self": [{ "href": "https://bb.example/projects/ACME/repos/app/pull-requests/7" }] },
        });
        for (k, v) in extra.as_object().unwrap() {
            pr[k] = v.clone();
        }
        pr
    }

    #[test]
    fn pull_request_summary_and_list() {
        let s = normalize_pull_request_summary(&raw_pr(json!(7), json!({}))).unwrap();
        assert_eq!(s.url.as_deref(), Some("https://bb.example/projects/ACME/repos/app/pull-requests/7"));
        assert_eq!(s.updated_at, n(1_700_000_000_000));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!((v["id"].clone(), v["url"].clone(), v["updatedAt"].clone()), (json!(7), json!("https://bb.example/projects/ACME/repos/app/pull-requests/7"), json!(1700000000000_i64)));
        let no_links = normalize_pull_request_summary(&raw_pr(json!(7), json!({ "links": { "self": [{}, { "href": 5 }] } }))).unwrap();
        assert_eq!(no_links.url, None);
        assert!(normalize_pull_request_summary(&json!(null)).is_none());
        let list = normalize_pull_request_list(&json!({ "values": [raw_pr(json!(1), json!({})), raw_pr(json!("x"), json!({})), json!("junk"), raw_pr(json!(2), json!({}))] }));
        assert_eq!(list.iter().map(|p| p.base.id.clone()).collect::<Vec<_>>(), [n(1), n(2)]);
        assert!(normalize_pull_request_list(&json!({ "values": "x" })).is_empty());
        assert!(normalize_pull_request_list(&json!(null)).is_empty());
    }

    #[test]
    fn build_statuses_report_the_worst_state_and_never_look_green_for_unknowns() {
        let b = |state: Value| json!({ "state": state, "key": "k", "name": "n", "url": "https://ci/1", "dateAdded": 5 });
        let summary = |states: Vec<Value>| normalize_build_statuses(&json!({ "values": states.into_iter().map(b).collect::<Vec<_>>() }));
        assert_eq!(summary(vec![]).state, None);
        assert_eq!(summary(vec![json!("SUCCESSFUL"), json!("SUCCESSFUL")]).state.as_deref(), Some("SUCCESSFUL"));
        assert_eq!(summary(vec![json!("SUCCESSFUL"), json!("INPROGRESS")]).state.as_deref(), Some("INPROGRESS"));
        assert_eq!(summary(vec![json!("SUCCESSFUL"), json!("WEIRD")]).state.as_deref(), Some("INPROGRESS"), "unknown never looks green");
        let failed = summary(vec![json!("SUCCESSFUL"), json!("FAILED"), json!("INPROGRESS"), json!(7)]);
        assert_eq!(failed.state.as_deref(), Some("FAILED"));
        assert_eq!(
            serde_json::to_value(&failed.counts).unwrap(),
            json!({ "SUCCESSFUL": 1, "FAILED": 1, "INPROGRESS": 1, "unknown": 1 })
        );
        assert_eq!(failed.builds[3].state, "unknown");
        assert_eq!(serde_json::to_value(&failed.builds[0]).unwrap(), json!({ "state": "SUCCESSFUL", "key": "k", "name": "n", "url": "https://ci/1", "at": 5 }));
        let many = summary((0..15).map(|_| json!("SUCCESSFUL")).collect());
        assert_eq!((many.builds.len(), many.counts.successful), (10, 15), "only 10 builds are listed but all are counted");
        assert_eq!(normalize_build_statuses(&json!("x")).state, None);
    }

    #[test]
    fn default_branch_repository_and_reviewers() {
        assert_eq!(normalize_default_branch(&json!({ "id": "refs/heads/master", "displayId": "master" })).as_deref(), Some("master"));
        assert_eq!(normalize_default_branch(&json!({ "id": "refs/heads/release/1.x" })).as_deref(), Some("release/1.x"));
        for bad in [json!({ "id": "refs/tags/v1" }), json!({ "id": "refs/heads/" }), json!({ "displayId": "" }), json!(null), json!("x")] {
            assert_eq!(normalize_default_branch(&bad), None, "{bad}");
        }
        let repo = normalize_repository(&json!({ "id": 12, "slug": "app", "project": { "key": "ACME" } })).unwrap();
        assert_eq!(serde_json::to_value(repo).unwrap(), json!({ "id": 12, "slug": "app", "projectKey": "ACME" }));
        assert!(normalize_repository(&json!({ "slug": "app" })).is_none());
        assert!(normalize_repository(&json!({ "id": "12" })).is_none());
        assert_eq!(
            normalize_default_reviewers(&json!([{ "name": "a" }, { "slug": "b" }, { "name": "a" }, { "name": "gone", "active": false }, { "name": "", "slug": "c" }, "x", {}])),
            ["a", "b", "c"]
        );
        assert!(normalize_default_reviewers(&json!({})).is_empty());
    }

    #[test]
    fn dashboard_pull_requests_carry_reviewers_and_an_approval_count() {
        let reviewers = json!([
            { "user": { "name": "alice" }, "status": "APPROVED" },
            { "user": { "slug": "bob" }, "status": "NEEDS_WORK" },
            { "user": { "name": "carol" }, "approved": true },
            { "user": { "name": "dave" }, "status": "WEIRD" },
            { "user": { "name": "" } },
            { "status": "APPROVED" },
            "junk",
        ]);
        let pr = normalize_dashboard_pull_request(&raw_pr(json!(7), json!({ "reviewers": reviewers }))).unwrap();
        let got: Vec<(&str, &str)> = pr.reviewers.iter().map(|r| (r.name.as_str(), r.status.as_str())).collect();
        assert_eq!(got, [("alice", "APPROVED"), ("bob", "NEEDS_WORK"), ("carol", "APPROVED"), ("dave", "unknown")]);
        assert_eq!((pr.approvals, pr.project.as_str(), pr.repo.as_str()), (2, "ACME", "app"));
        let v = serde_json::to_value(&pr).unwrap();
        assert_eq!((v["project"].clone(), v["repo"].clone(), v["approvals"].clone(), v["id"].clone()), (json!("ACME"), json!("app"), json!(2), json!(7)));
        assert!(normalize_dashboard_pull_request(&raw_pr(json!(null), json!({}))).is_none(), "no id");
        assert!(normalize_dashboard_pull_request(&raw_pr(json!(7), json!({ "toRef": { "repository": { "slug": "app" } } }))).is_none(), "no project");
        assert!(normalize_dashboard_pull_request(&raw_pr(json!(7), json!({ "toRef": { "repository": { "project": { "key": "ACME" } } } }))).is_none(), "no repo");
        let list = normalize_dashboard_pull_requests(&json!({ "values": [raw_pr(json!(1), json!({})), raw_pr(json!(null), json!({}))] }));
        assert_eq!(list.len(), 1);
        assert!(normalize_dashboard_pull_requests(&json!(null)).is_empty());
    }

    #[test]
    fn merge_status() {
        let m = |v: Value| serde_json::to_value(normalize_merge_status(&v)).unwrap();
        assert_eq!(m(json!({ "conflicted": true, "canMerge": false, "outcome": "CONFLICTED" })), json!({ "conflicted": true, "canMerge": false, "outcome": "CONFLICTED" }));
        assert_eq!(m(json!({ "outcome": "CONFLICTED" })), json!({ "conflicted": true, "canMerge": null, "outcome": "CONFLICTED" }));
        assert_eq!(m(json!({ "outcome": "CLEAN", "canMerge": true })), json!({ "conflicted": null, "canMerge": true, "outcome": "CLEAN" }));
        assert_eq!(m(json!({ "conflicted": "yes" })), json!({ "conflicted": null, "canMerge": null, "outcome": null }));
        assert_eq!(m(json!(null)), json!({ "conflicted": null, "canMerge": null, "outcome": null }));
    }
}
