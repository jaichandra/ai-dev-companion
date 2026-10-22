//! Bookkeeping for the work-unit registry (`work_units`) that features call as
//! they work on a branch (port of `core/work-unit-record.js`). Never fails:
//! losing a record must not fail a job.

use crate::prereqs::GitRemoteRules;
use crate::work_units::{UnitFields, WorkUnits};

pub struct Unit<'a> {
    pub repo_path: &'a str,
    pub branch: &'a str,
    pub dir: Option<&'a str>,
    pub issue_key: Option<&'a str>,
    pub pr_key: Option<&'a str>,
}

impl<'a> Unit<'a> {
    fn fields(&self) -> UnitFields<'a> {
        UnitFields { dir: self.dir, issue_key: self.issue_key, pr_key: self.pr_key }
    }
}

/// A Claude session started for the unit.
pub struct Session<'a> {
    pub id: &'a str,
    pub cwd: Option<&'a str>,
    pub feature: Option<&'a str>,
}

fn key_of(unit: &serde_json::Value) -> Option<String> {
    unit["key"].as_str().map(str::to_string)
}

/// Notes that a feature is working on `branch` of `repo_path`. Returns the unit key, or `None`.
pub fn record_work_unit(registry: &WorkUnits, unit: &Unit<'_>) -> Option<String> {
    registry.upsert(unit.repo_path, unit.branch, &unit.fields()).ok().as_ref().and_then(key_of)
}

/// Notes a Claude session started for the unit; the unit is made if it isn't there yet.
pub fn record_work_unit_session(registry: &WorkUnits, unit: &Unit<'_>, session: &Session<'_>) -> Option<String> {
    let key = key_of(&registry.upsert(unit.repo_path, unit.branch, &unit.fields()).ok()?)?;
    registry.add_session(&key, session.id, session.cwd, session.feature).ok()?;
    Some(key)
}

/// The stored PR key for the profile's git host (`bitbucket:PROJ/repo#4`, `github:owner/repo#4`).
pub fn pr_key_for(rules: GitRemoteRules, project: &str, repo: &str, pr_id: u64) -> String {
    rules.pr_key(project, repo, &pr_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (tempfile::TempDir, WorkUnits) {
        let d = tempfile::tempdir().unwrap();
        let r = WorkUnits::open(d.path().join("units.json"));
        (d, r)
    }
    fn unit<'a>() -> Unit<'a> {
        Unit { repo_path: "/r", branch: "b", dir: Some("/w"), issue_key: None, pr_key: None }
    }

    #[test]
    fn record_work_unit_upserts_and_returns_the_key() {
        let (_d, r) = registry();
        let key = record_work_unit(&r, &unit()).unwrap();
        assert_eq!(r.get(&key).unwrap()["dir"], "/w");
    }

    #[test]
    fn a_session_makes_the_unit_then_is_added_to_it() {
        let (_d, r) = registry();
        let key = record_work_unit_session(&r, &Unit { dir: None, ..unit() }, &Session { id: "s-1", cwd: Some("/w"), feature: Some("digest") }).unwrap();
        assert_eq!(r.get(&key).unwrap()["sessions"][0]["id"], "s-1");
    }

    #[test]
    fn bookkeeping_never_fails_even_when_the_registry_cannot_be_written() {
        let r = WorkUnits::open("/proc/nope/units.json".into());
        assert_eq!(record_work_unit(&r, &unit()), None);
        assert_eq!(record_work_unit_session(&r, &unit(), &Session { id: "s", cwd: None, feature: None }), None);
        assert_eq!(record_work_unit(&r, &Unit { repo_path: "", branch: "", ..unit() }), None, "an unusable key is dropped quietly");
    }

    #[test]
    fn the_pr_key_follows_the_profiles_git_host() {
        assert_eq!(pr_key_for(GitRemoteRules::BitbucketDc, "proj", "App", 4), "bitbucket:PROJ/app#4");
        assert_eq!(pr_key_for(GitRemoteRules::Github, "Owner", "App", 4), "github:owner/app#4");
    }
}
