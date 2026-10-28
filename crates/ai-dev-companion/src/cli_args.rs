//! Argument parsing for the `companion` command (port of `core/cli-args.js`).
//! Pure.

use std::collections::BTreeMap;

pub const COMMANDS: [&str; 15] = [
    "status", "jobs", "history", "similar", "resume", "forget", "open", "inbox", "digest", "precheck", "hooks",
    "worktrees", "open-shell", "doctor", "help",
];
const VALUE_FLAGS: [&str; 4] = ["--feature", "--search", "--days", "--cwd"];

fn boolean_flag(arg: &str) -> Option<&'static str> {
    Some(match arg {
        "--json" => "json",
        "--yes" | "-y" => "yes",
        "--metrics" => "metrics",
        "--all" => "all",
        "--stdin" => "stdin",
        "--brief" => "brief",
        "--prune" => "prune",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flag {
    On,
    Value(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArgs {
    pub command: String,
    pub positional: Vec<String>,
    pub flags: BTreeMap<String, Flag>,
    pub errors: Vec<String>,
}

pub fn parse_args<S: AsRef<str>>(argv: &[S]) -> ParsedArgs {
    let mut out = ParsedArgs { command: "help".into(), positional: vec![], flags: BTreeMap::new(), errors: vec![] };
    let mut args: Vec<&str> = argv.iter().map(AsRef::as_ref).collect();
    if args.is_empty() {
        return out;
    }
    if !args[0].starts_with('-') {
        let command = args.remove(0);
        if !COMMANDS.contains(&command) {
            out.errors.push(format!("Unknown command \"{command}\"."));
            return out;
        }
        out.command = command.into();
    }
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if let Some(name) = boolean_flag(a) {
            out.flags.insert(name.into(), Flag::On);
        } else if a == "--help" || a == "-h" {
            out.command = "help".into();
        } else if VALUE_FLAGS.contains(&a) {
            match args.get(i + 1) {
                Some(value) if !value.starts_with('-') => {
                    out.flags.insert(a[2..].into(), Flag::Value((*value).into()));
                    i += 1;
                }
                _ => out.errors.push(format!("{a} needs a value.")),
            }
        } else if a.starts_with('-') {
            out.errors.push(format!("Unknown option \"{a}\"."));
        } else {
            out.positional.push(a.into());
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(p: &ParsedArgs, k: &str) -> Option<Flag> {
        p.flags.get(k).cloned()
    }

    #[test]
    fn no_arguments_means_help() {
        assert_eq!(parse_args::<&str>(&[]).command, "help");
    }

    #[test]
    fn commands_positionals_and_flags() {
        let p = parse_args(&["history", "PROJ-12", "--json", "--days", "7"]);
        assert_eq!(p.command, "history");
        assert_eq!(p.positional, ["PROJ-12"]);
        assert_eq!(
            p.flags,
            BTreeMap::from([("json".to_string(), Flag::On), ("days".to_string(), Flag::Value("7".into()))])
        );
        assert!(p.errors.is_empty());
        assert_eq!(flag(&parse_args(&["forget", "PROJ-1", "-y"]), "yes"), Some(Flag::On));
        assert_eq!(parse_args(&["open", "src/a.ts:10"]).command, "open");
        assert_eq!(parse_args(&["open", "src/a.ts:10", "extra"]).positional, ["src/a.ts:10", "extra"]);
        assert_eq!(
            flag(&parse_args(&["jobs", "--feature", "analyze-issue"]), "feature"),
            Some(Flag::Value("analyze-issue".into()))
        );
    }

    #[test]
    fn unknown_commands_unknown_options_and_missing_values_are_errors() {
        assert!(parse_args(&["frobnicate"]).errors[0].starts_with("Unknown command"));
        assert!(parse_args(&["jobs", "--wat"]).errors[0].starts_with("Unknown option"));
        assert!(parse_args(&["jobs", "--feature"]).errors[0].ends_with("needs a value."));
        assert!(parse_args(&["jobs", "--feature", "--json"]).errors[0].ends_with("needs a value."));
        assert_eq!(parse_args(&["frobnicate"]).errors, ["Unknown command \"frobnicate\"."]);
    }

    #[test]
    fn help_anywhere_selects_help_and_a_leading_flag_keeps_the_default_command() {
        assert_eq!(parse_args(&["status", "--help"]).command, "help");
        assert_eq!(parse_args(&["-h"]).command, "help");
        let p = parse_args(&["--json"]);
        assert_eq!((p.command.as_str(), flag(&p, "json")), ("help", Some(Flag::On)));
    }

    #[test]
    fn a_value_flag_does_not_swallow_the_next_flag() {
        let p = parse_args(&["jobs", "--feature", "--json"]);
        assert_eq!(flag(&p, "json"), Some(Flag::On));
        assert_eq!(flag(&p, "feature"), None);
    }
}
