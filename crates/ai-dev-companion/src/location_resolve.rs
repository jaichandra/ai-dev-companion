//! Decides which tracked file a parsed "path:line" location names (port of
//! `core/location-resolve.js`). Pure: the caller supplies each configured
//! repo's `git ls-files` output, so a match is always a file git tracks inside
//! a configured repo root; a path in pasted text can never point outside them.
//!
//! A location's path may be absolute on the build machine
//! (/home/ci/workspace/sample-app/src/a.ts), a webpack-relative path
//! (src/a.ts) or a Java package path (com/x/Y.java). The longest trailing run
//! of path segments that matches a tracked file wins.

use crate::node_path;
use crate::prereqs::GitRemoteRules;
use crate::uri::decode_uri_component;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const MAX_AMBIGUOUS: usize = 10;

/// A configured repo and the files git tracks in it.
pub struct Repo {
    pub key: String,
    pub root: String,
    files: HashSet<String>,
    by_base: HashMap<String, Vec<String>>,
}

impl Repo {
    pub fn new(key: &str, root: &str, files: Vec<String>) -> Self {
        let mut by_base: HashMap<String, Vec<String>> = HashMap::new();
        for f in &files {
            let base = f.rsplit('/').next().unwrap_or(f);
            by_base.entry(base.to_string()).or_default().push(f.clone());
        }
        Repo { key: key.into(), root: root.into(), files: files.into_iter().collect(), by_base }
    }
}

/// A parsed location; `line` and `column` pass through untouched.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: String,
    pub line: Value,
    pub column: Value,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Resolution {
    Found { repo_key: String, root: String, rel: String, abs: String, line: Value, column: Value },
    NotFound,
    Ambiguous { matches: Vec<(String, String)> },
}

impl Resolution {
    pub fn to_json(&self) -> Value {
        match self {
            Resolution::Found { repo_key, root, rel, abs, line, column } => {
                json!({ "ok": true, "repoKey": repo_key, "root": root, "rel": rel, "abs": abs, "line": line, "column": column })
            }
            Resolution::NotFound => json!({ "ok": false, "reason": "not-found" }),
            Resolution::Ambiguous { matches } => {
                json!({ "ok": false, "reason": "ambiguous", "matches": matches.iter().map(|(k, r)| json!({ "repoKey": k, "rel": r })).collect::<Vec<_>>() })
            }
        }
    }
}

fn safe_rel(rel: &str) -> bool {
    !rel.is_empty() && !rel.starts_with('/') && !rel.split('/').any(|s| s == "..")
}

fn found(repo: &Repo, rel: &str, c: &Candidate) -> Resolution {
    Resolution::Found { repo_key: repo.key.clone(), root: repo.root.clone(), rel: rel.into(), abs: node_path::join(&[&repo.root, rel]), line: c.line.clone(), column: c.column.clone() }
}

/// `hint_repo_key`: the repo the page belongs to, when known.
pub fn resolve_location(candidate: &Candidate, repos: &[Repo], hint_repo_key: Option<&str>) -> Resolution {
    let normalized = node_path::normalize(&candidate.path);
    let segs: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return Resolution::NotFound;
    }

    // 1. An absolute path that is really inside a configured repo (the
    //    developer's own machine, or a stack trace copied from a local run).
    if normalized.starts_with('/') {
        for repo in repos {
            let root = if repo.root.ends_with('/') { repo.root.clone() } else { format!("{}/", repo.root) };
            let Some(rel) = normalized.strip_prefix(&root) else { continue };
            if safe_rel(rel) && repo.files.contains(rel) {
                return found(repo, rel, candidate);
            }
        }
    }

    // 2. Longest trailing run of segments that matches a tracked file.
    let base = segs[segs.len() - 1];
    for k in (1..=segs.len()).rev() {
        let suffix = segs[segs.len() - k..].join("/");
        let mut matches: Vec<(&Repo, &str)> = vec![];
        for repo in repos {
            for rel in repo.by_base.get(base).into_iter().flatten() {
                if safe_rel(rel) && (*rel == suffix || rel.ends_with(&format!("/{suffix}"))) {
                    matches.push((repo, rel));
                }
            }
        }
        if matches.is_empty() {
            continue;
        }
        if matches.len() == 1 {
            return found(matches[0].0, matches[0].1, candidate);
        }
        let hinted: Vec<&(&Repo, &str)> = hint_repo_key.map(|h| matches.iter().filter(|m| m.0.key == h).collect()).unwrap_or_default();
        if hinted.len() == 1 {
            return found(hinted[0].0, hinted[0].1, candidate);
        }
        let pool: Vec<&(&Repo, &str)> = if hinted.len() > 1 { hinted } else { matches.iter().collect() };
        return Resolution::Ambiguous { matches: pool.into_iter().take(MAX_AMBIGUOUS).map(|(r, rel)| (r.key.clone(), rel.to_string())).collect() };
    }
    Resolution::NotFound
}

/// The configured repo key ("PROJECT/repo") a pull-request page belongs to,
/// matched case-insensitively from the page URL, or `None`. Used only to
/// break ties between files with the same name.
pub fn repo_key_from_page_url(page_url: Option<&str>, repo_keys: &[String], git: GitRemoteRules) -> Option<String> {
    let m = git.parse_repo_url(page_url?)?;
    let want = format!("{}/{}", decode_uri_component(&m.project)?, decode_uri_component(&m.repo)?).to_lowercase();
    repo_keys.iter().find(|k| k.to_lowercase() == want).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use GitRemoteRules::Github;

    fn repos() -> Vec<Repo> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        vec![
            Repo::new("ACME/sample-app", "/Users/me/sample-app", s(&["src/app/login.ts", "src/app/index.ts", "src/components/Button.tsx", "tests/login.spec.ts", "src/main/java/com/acme/ci/LoginPage.java"])),
            Repo::new("ACME/sample-service", "/Users/me/sample-service", s(&["src/app/index.ts", "src/billing/invoice.ts"])),
        ]
    }

    fn loc(p: &str) -> Candidate {
        Candidate { path: p.into(), line: json!(10), column: json!(5) }
    }

    fn at(p: &str, line: i64, column: Value) -> Candidate {
        Candidate { path: p.into(), line: json!(line), column }
    }

    #[test]
    fn an_absolute_path_inside_a_configured_repo_resolves_directly() {
        let r = resolve_location(&loc("/Users/me/sample-app/src/app/login.ts"), &repos(), None);
        assert_eq!(
            r.to_json(),
            json!({ "ok": true, "repoKey": "ACME/sample-app", "root": "/Users/me/sample-app", "rel": "src/app/login.ts", "abs": "/Users/me/sample-app/src/app/login.ts", "line": 10, "column": 5 })
        );
    }

    #[test]
    fn a_build_machine_absolute_path_matches_by_its_longest_tracked_suffix() {
        let r = resolve_location(&at("/home/ci/workspace/sample-app/src/app/login.ts", 42, json!(13)), &repos(), None).to_json();
        assert_eq!((r["ok"].clone(), r["abs"].clone(), r["line"].clone()), (json!(true), json!("/Users/me/sample-app/src/app/login.ts"), json!(42)));
    }

    #[test]
    fn webpack_relative_and_java_package_paths_resolve() {
        assert_eq!(resolve_location(&loc("src/components/Button.tsx"), &repos(), None).to_json()["rel"], "src/components/Button.tsx");
        let java = resolve_location(&at("com/acme/ci/LoginPage.java", 88, Value::Null), &repos(), None).to_json();
        assert_eq!(java["rel"], "src/main/java/com/acme/ci/LoginPage.java");
        assert_eq!(java["column"], Value::Null);
    }

    #[test]
    fn a_file_name_found_in_two_repos_is_ambiguous_unless_a_hint_picks_the_repo() {
        let a = resolve_location(&loc("src/app/index.ts"), &repos(), None);
        assert_eq!(a, Resolution::Ambiguous { matches: vec![("ACME/sample-app".into(), "src/app/index.ts".into()), ("ACME/sample-service".into(), "src/app/index.ts".into())] });
        assert_eq!(a.to_json()["reason"], "ambiguous");
        let hinted = resolve_location(&loc("src/app/index.ts"), &repos(), Some("ACME/sample-service")).to_json();
        assert_eq!((hinted["ok"].clone(), hinted["abs"].clone()), (json!(true), json!("/Users/me/sample-service/src/app/index.ts")));
    }

    #[test]
    fn the_longest_matching_suffix_wins_and_a_bare_name_shared_by_two_repos_stays_ambiguous() {
        let r = resolve_location(&loc("/ci/sample-app/src/app/index.ts"), &repos(), None);
        assert!(matches!(r, Resolution::Ambiguous { .. }));
        let r2 = resolve_location(&loc("billing/src/billing/invoice.ts"), &repos(), None).to_json();
        assert_eq!((r2["ok"].clone(), r2["repoKey"].clone()), (json!(true), json!("ACME/sample-service")));
    }

    #[test]
    fn an_unknown_file_an_empty_path_and_a_traversal_attempt_are_not_found() {
        for p in ["src/nope/missing.ts", "/", "../../etc/passwd.py"] {
            assert_eq!(resolve_location(&loc(p), &repos(), None).to_json(), json!({ "ok": false, "reason": "not-found" }), "{p}");
        }
    }

    #[test]
    fn only_tracked_files_can_match_even_when_the_path_is_inside_a_repo_root() {
        assert_eq!(resolve_location(&loc("/Users/me/sample-app/node_modules/x/evil.js"), &repos(), None), Resolution::NotFound);
    }

    #[test]
    fn an_absolute_path_that_only_looks_like_a_repo_root_prefix_does_not_match_it() {
        let repo = vec![Repo::new("P/app", "/r/app", vec!["other/src/a.ts".into(), "src/a.ts".into()])];
        assert!(matches!(resolve_location(&loc("/r/appother/src/a.ts"), &repo, None), Resolution::Ambiguous { .. }));
        assert_eq!(resolve_location(&loc("/r/app/other/src/a.ts"), &repo, None).to_json()["rel"], "other/src/a.ts");
    }

    #[test]
    fn a_repo_root_with_a_trailing_slash_and_a_dotdot_tracked_name_are_handled() {
        let repo = vec![Repo::new("P/app", "/r/app/", vec!["src/a.ts".into(), "../evil/b.ts".into()])];
        assert_eq!(resolve_location(&loc("/r/app/src/a.ts"), &repo, None).to_json()["abs"], "/r/app/src/a.ts");
        assert_eq!(resolve_location(&loc("/x/evil/b.ts"), &repo, None), Resolution::NotFound, "an unsafe tracked name never matches");
    }

    #[test]
    fn more_than_ten_ambiguous_matches_are_cut_and_hints_narrow_to_one_repo() {
        let many: Vec<Repo> = (0..12).map(|i| Repo::new(&format!("P/r{i}"), &format!("/r{i}"), vec!["src/a.ts".into()])).collect();
        match resolve_location(&loc("src/a.ts"), &many, None) {
            Resolution::Ambiguous { matches } => assert_eq!(matches.len(), 10),
            other => panic!("{other:?}"),
        }
        assert_eq!(resolve_location(&loc("src/a.ts"), &many, Some("P/r11")).to_json()["repoKey"], "P/r11");
    }

    #[test]
    fn repo_key_from_page_url_finds_the_configured_repo_of_a_pull_request_page_case_insensitively() {
        let keys = vec!["ACME/sample-app".to_string(), "ACME/sample-service".to_string()];
        let key = |u: Option<&str>| repo_key_from_page_url(u, &keys, Github);
        assert_eq!(key(Some("https://github.com/ACME/sample-app/pull/12/files")).as_deref(), Some("ACME/sample-app"));
        assert_eq!(key(Some("https://github.com/acme/Sample-Service/tree/main")).as_deref(), Some("ACME/sample-service"));
        assert_eq!(key(Some("https://github.com/OTHER/x/tree/main")), None);
        assert_eq!(key(Some("https://jenkins.example/job/x/5/")), None);
        assert_eq!(key(Some("https://github.com/%E0%A4%A/x")), None);
        assert_eq!(key(None), None);
    }
}
