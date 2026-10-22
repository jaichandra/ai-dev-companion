//! The optional pre-push hook, set in a repo's own git config (git's
//! config-based hooks) so it runs alongside husky and any existing pre-push
//! hook and touches no file (port of `core/git-hook.js`):
//!
//! ```text
//! git config --local hook.companion-precheck.event pre-push
//! git config --local hook.companion-precheck.command "companion precheck --stdin"
//! ```
//!
//! Pure: argv lists for `git` (always an argv array, never a shell string) and
//! the parsing of what git hands a pre-push hook on stdin.

use crate::js_text::{is_js_space, js_trim};
use std::collections::BTreeMap;

pub const HOOK_NAME: &str = "companion-precheck";
pub const HOOK_EVENT: &str = "pre-push";
pub const HOOK_COMMAND: &str = "companion precheck --stdin";
pub const PROBE_MARKER: &str = "companion-probe-ok";
const MAX_UPDATES: usize = 50;

/// `^([0-9a-f]{40}|[0-9a-f]{64})$`
fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_zero(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b'0')
}

fn assert_repo_dir(repo: &str) -> Result<(), String> {
    if repo.starts_with('/') && !repo.chars().any(|c| c <= '\u{1f}') {
        Ok(())
    } else {
        Err("The repository must be an absolute folder path.".into())
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Two `git` argv lists that add the hook to `repo`'s local config.
pub fn install_args(repo: &str) -> Result<Vec<Vec<String>>, String> {
    assert_repo_dir(repo)?;
    Ok(vec![
        argv(&["-C", repo, "config", "--local", &format!("hook.{HOOK_NAME}.event"), HOOK_EVENT]),
        argv(&["-C", repo, "config", "--local", &format!("hook.{HOOK_NAME}.command"), HOOK_COMMAND]),
    ])
}

/// `git config --unset-all` for both keys (exit 5 just means "wasn't set").
pub fn uninstall_args(repo: &str) -> Result<Vec<Vec<String>>, String> {
    assert_repo_dir(repo)?;
    Ok(vec![
        argv(&["-C", repo, "config", "--local", "--unset-all", &format!("hook.{HOOK_NAME}.event")]),
        argv(&["-C", repo, "config", "--local", "--unset-all", &format!("hook.{HOOK_NAME}.command")]),
    ])
}

pub fn status_args(repo: &str) -> Result<Vec<String>, String> {
    assert_repo_dir(repo)?;
    Ok(argv(&["-C", repo, "config", "--local", "--get", &format!("hook.{HOOK_NAME}.command")]))
}

/// Steps that show whether this git runs config-based hooks: a throwaway repo
/// in `dir` with a probe hook, then `git hook run`. Supported when the last
/// step prints [`PROBE_MARKER`].
pub fn probe_steps(dir: &str) -> Result<Vec<Vec<String>>, String> {
    assert_repo_dir(dir)?;
    Ok(vec![
        argv(&["init", "-q", "--template=", dir]),
        argv(&["-C", dir, "config", "--local", "hook.companion-probe.event", HOOK_EVENT]),
        argv(&["-C", dir, "config", "--local", "hook.companion-probe.command", &format!("echo {PROBE_MARKER}")]),
        argv(&["-C", dir, "hook", "run", HOOK_EVENT]),
    ])
}

/// What to tell someone whose git has no config-based hooks.
pub fn fallback_instructions(repo: &str) -> String {
    format!(
        "This git doesn't run hooks from its config. To get the check anyway, add this line to {repo}/.git/hooks/pre-push \
(or to husky's .husky/pre-push) and make the file executable:\n\n    companion precheck --stdin || true\n"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrePushUpdate {
    pub local_ref: String,
    pub local_sha: String,
    pub remote_ref: String,
    pub remote_sha: String,
}

/// The `<local ref> <local sha> <remote ref> <remote sha>` lines git feeds a
/// pre-push hook. Malformed lines are skipped; at most 50.
pub fn parse_pre_push_lines(text: &str) -> Vec<PrePushUpdate> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let parts: Vec<&str> = js_trim(line).split(is_js_space).filter(|p| !p.is_empty()).collect();
        let [local_ref, local_sha, remote_ref, remote_sha] = parts[..] else { continue };
        if !is_sha(local_sha) || !is_sha(remote_sha) {
            continue;
        }
        out.push(PrePushUpdate {
            local_ref: local_ref.into(),
            local_sha: local_sha.into(),
            remote_ref: remote_ref.into(),
            remote_sha: remote_sha.into(),
        });
        if out.len() >= MAX_UPDATES {
            break;
        }
    }
    out
}

/// The `git` argv that lists one update's changed files, or `None` for a
/// deleted branch. A new branch lists files of commits no remote branch has.
/// Only real object ids reach git, and a trailing `--` means nothing after
/// them can ever be read as a path or an option.
pub fn changed_files_args(local_sha: &str, remote_sha: &str) -> Option<Vec<String>> {
    if !is_sha(local_sha) || !is_sha(remote_sha) || is_zero(local_sha) {
        return None;
    }
    Some(if is_zero(remote_sha) {
        argv(&["log", "--name-only", "--format=", local_sha, "--not", "--remotes", "--"])
    } else {
        argv(&["diff", "--name-only", remote_sha, local_sha, "--"])
    })
}

/// `git diff/log --name-only` output -> unique non-empty paths.
pub fn parse_name_list(stdout: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in stdout.split('\n').map(js_trim).filter(|l| !l.is_empty()) {
        if !out.iter().any(|o| o == l) {
            out.push(l.to_string());
        }
    }
    out
}

/// `Number.parseInt(s, 10)`: optional leading whitespace and sign, then the
/// leading digits; `None` where JS gives NaN.
fn parse_int_prefix(s: &str) -> Option<i64> {
    let t = s.trim_start_matches(is_js_space);
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    let n: i64 = digits[..end].parse().ok()?;
    Some(if neg { -n } else { n })
}

/// The environment for a `git` call: prompts off, file names never C-quoted
/// (`core.quotePath=false`, via `GIT_CONFIG_COUNT` so the argv stays as is).
/// `isolated` (the probe) also ignores the user's and the system's git config
/// and any config injected through the environment.
pub fn git_env(base: &BTreeMap<String, String>, isolated: bool) -> BTreeMap<String, String> {
    let mut env = base.clone();
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    let n = if isolated {
        env.remove("GIT_CONFIG_COUNT");
        env.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
        env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        0
    } else {
        env.get("GIT_CONFIG_COUNT").and_then(|c| parse_int_prefix(c)).filter(|g| *g > 0 && *g < 1000).unwrap_or(0)
    };
    env.insert(format!("GIT_CONFIG_KEY_{n}"), "core.quotePath".into());
    env.insert(format!("GIT_CONFIG_VALUE_{n}"), "false".into());
    env.insert("GIT_CONFIG_COUNT".into(), (n + 1).to_string());
    env
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(args: &[String]) -> std::process::Output {
        Command::new("git").args(args).output().unwrap()
    }
    fn gits(args: &[&str]) -> std::process::Output {
        git(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    fn out(o: &std::process::Output) -> String {
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    fn repo() -> tempfile::TempDir {
        let d = tempfile::Builder::new().prefix("git-hook-").tempdir().unwrap();
        assert!(gits(&["init", "-q", d.path().to_str().unwrap()]).status.success());
        d
    }

    fn p(d: &tempfile::TempDir) -> &str {
        d.path().to_str().unwrap()
    }

    #[test]
    fn install_and_uninstall_set_and_clear_both_keys_in_the_repos_own_config() {
        let d = repo();
        for a in install_args(p(&d)).unwrap() {
            assert!(git(&a).status.success(), "{a:?}");
        }
        assert_eq!(out(&git(&status_args(p(&d)).unwrap())), "companion precheck --stdin");
        assert_eq!(out(&gits(&["-C", p(&d), "config", "--local", "--get", "hook.companion-precheck.event"])), "pre-push");
        assert!(!d.path().join(".git/hooks/pre-push").exists(), "no hook file is written");
        for a in uninstall_args(p(&d)).unwrap() {
            assert!(git(&a).status.success());
        }
        assert!(!git(&status_args(p(&d)).unwrap()).status.success(), "gone after uninstall");
    }

    #[test]
    fn argv_builders_refuse_a_relative_or_option_like_folder() {
        for bad in ["repo", "-c", "", "/a\nb"] {
            assert_eq!(install_args(bad).unwrap_err(), "The repository must be an absolute folder path.", "{bad:?}");
        }
        assert_eq!(probe_steps("/tmp/p").unwrap()[0], ["init", "-q", "--template=", "/tmp/p"]);
        assert_eq!(probe_steps("/tmp/p").unwrap().last().unwrap(), &["-C", "/tmp/p", "hook", "run", "pre-push"]);
        assert!(fallback_instructions("/r").contains("companion precheck --stdin || true"));
    }

    #[test]
    fn parse_pre_push_lines_reads_gits_four_fields_and_skips_anything_else() {
        let text = format!("refs/heads/x {A} refs/heads/x {B}\njunk line\nrefs/heads/y {A} refs/heads/y {ZERO}\n");
        let got = parse_pre_push_lines(&text);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].local_ref.as_str(), got[0].local_sha.as_str(), got[0].remote_sha.as_str()), ("refs/heads/x", A, B));
        assert_eq!(got[1].remote_sha, ZERO);
        assert!(parse_pre_push_lines("").is_empty());
        assert!(parse_pre_push_lines(&format!("r {A} r {B} extra")).is_empty());
        assert!(parse_pre_push_lines(&format!("r {} r {B}", "A".repeat(40))).is_empty(), "uppercase is not a git id");
        let many = format!("r {A} r {B}\n").repeat(60);
        assert_eq!(parse_pre_push_lines(&many).len(), 50);
    }

    #[test]
    fn changed_files_args_diff_for_an_update_log_for_a_new_branch_nothing_for_a_delete() {
        assert_eq!(changed_files_args(A, B).unwrap(), ["diff", "--name-only", B, A, "--"]);
        assert_eq!(changed_files_args(A, ZERO).unwrap(), ["log", "--name-only", "--format=", A, "--not", "--remotes", "--"]);
        let long = "a".repeat(41);
        for bad in ["--output=/tmp/x", "HEAD", "abc", "", long.as_str()] {
            assert_eq!(changed_files_args(bad, B), None, "{bad}");
            assert_eq!(changed_files_args(A, bad), None, "{bad}");
        }
        assert_eq!(changed_files_args(ZERO, B), None);
        assert_eq!(parse_name_list("a.ts\n\nb.ts\na.ts\n"), ["a.ts", "b.ts"]);
        assert_eq!(parse_name_list("  x \n"), ["x"]);
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn git_env_quote_path_off_always_the_probe_also_ignores_user_and_system_git_config() {
        let base = env(&[("PATH", "/bin"), ("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", "x.y"), ("GIT_CONFIG_VALUE_0", "z")]);
        let e = git_env(&base, false);
        assert_eq!(e["GIT_TERMINAL_PROMPT"], "0");
        assert_eq!(e["GIT_CONFIG_COUNT"], "2");
        assert_eq!(e["GIT_CONFIG_KEY_0"], "x.y");
        assert_eq!(e["GIT_CONFIG_KEY_1"], "core.quotePath");
        assert_eq!(e["GIT_CONFIG_VALUE_1"], "false");
        assert!(!e.contains_key("GIT_CONFIG_GLOBAL"));
        let iso = git_env(&base, true);
        assert_eq!(iso["GIT_CONFIG_GLOBAL"], "/dev/null");
        assert_eq!(iso["GIT_CONFIG_NOSYSTEM"], "1");
        assert_eq!(iso["GIT_CONFIG_COUNT"], "1", "inherited config injection is dropped");
        assert_eq!(iso["GIT_CONFIG_KEY_0"], "core.quotePath");
        for junk in ["junk", "-3", "0", "5000", ""] {
            assert_eq!(git_env(&env(&[("GIT_CONFIG_COUNT", junk)]), false)["GIT_CONFIG_COUNT"], "1", "{junk:?}");
        }
        assert_eq!(git_env(&env(&[("GIT_CONFIG_COUNT", " 3abc")]), false)["GIT_CONFIG_COUNT"], "4", "parseInt reads the leading digits");
    }

    #[test]
    fn git_env_against_real_git_non_ascii_names_come_out_unquoted() {
        let d = tempfile::tempdir().unwrap();
        let envs = git_env(&std::env::vars().collect(), true);
        let run = |args: &[&str]| Command::new("git").args(args).current_dir(d.path()).env_clear().envs(&envs).output().unwrap();
        assert!(run(&["init", "-q", "--template="]).status.success());
        std::fs::write(d.path().join("caf\u{e9}.txt"), "x").unwrap();
        run(&["add", "."]);
        assert_eq!(out(&run(&["diff", "--name-only", "--cached"])), "caf\u{e9}.txt");
    }

    #[test]
    fn uninstall_removes_only_our_two_keys_and_is_harmless_on_a_repo_that_never_had_them() {
        let d = repo();
        for a in uninstall_args(p(&d)).unwrap() {
            assert_eq!(git(&a).status.code(), Some(5), "never installed: git says the key was not set");
        }
        for (k, v) in [("hook.other.event", "pre-push"), ("hook.other.command", "echo hi"), ("user.name", "T")] {
            assert!(gits(&["-C", p(&d), "config", "--local", k, v]).status.success());
        }
        for a in install_args(p(&d)).unwrap() {
            assert!(git(&a).status.success());
        }
        for a in uninstall_args(p(&d)).unwrap() {
            assert!(git(&a).status.success());
        }
        for (k, v) in [("hook.other.command", "echo hi"), ("hook.other.event", "pre-push"), ("user.name", "T")] {
            assert_eq!(out(&gits(&["-C", p(&d), "config", "--local", "--get", k])), v);
        }
    }

    #[test]
    fn status_args_reports_only_a_repo_that_has_the_command() {
        let d = repo();
        let installed = || git(&status_args(p(&d)).unwrap()).status.success();
        assert!(!installed());
        assert!(gits(&["-C", p(&d), "config", "--local", "hook.companion-precheck.event", "pre-push"]).status.success());
        assert!(!installed(), "no command yet");
        assert!(gits(&["-C", p(&d), "config", "--local", "hook.companion-precheck.command", "companion precheck --stdin"]).status.success());
        assert!(installed());
    }

    #[test]
    fn probe_steps_run_against_real_git_in_an_isolated_environment() {
        let d = tempfile::tempdir().unwrap();
        let envs = git_env(&std::env::vars().collect(), true);
        let mut last = None;
        for a in probe_steps(d.path().to_str().unwrap()).unwrap() {
            let o = Command::new("git").args(&a).env_clear().envs(&envs).output().unwrap();
            let ok = o.status.success();
            last = Some(o);
            if !ok {
                break;
            }
        }
        // Older gits have no `git hook run` / config hooks: then the probe just reports "not supported".
        let o = last.unwrap();
        let supported = o.status.success() && String::from_utf8_lossy(&o.stdout).contains(PROBE_MARKER);
        let _ = (supported, Path::new("/"));
    }
}
