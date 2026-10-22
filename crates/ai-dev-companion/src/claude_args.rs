//! Pure argv/policy construction for headless `claude` runs (port of
//! `core/claude-args.js`). Nothing here touches the filesystem or spawns a
//! process, so the exact flag order and semantics (verified empirically
//! against the CLI) can be tested without one.
//!
//! One deliberate change from the Node build: the guard hooks used to run
//! `<node> '<guard script>' '<arg>'`. They now run the companion binary itself
//! with a `guard` subcommand (`'<binary>' guard edit '<root>'`, see
//! `guard::cli`), so the only thing a guard needs on the machine is the one
//! executable, and there are no script paths to pass around.

use crate::guard::bash_policy::{has_shell_metacharacters, prefixes_from_bash_rules};
use crate::js_text::{js_stringify, js_trim};
use crate::node_path;
use serde_json::{Map, Value, json};

/// Named tool grants a feature asks for by name instead of hand-listing tools
/// on every run. `mcp_guard` means the caller also wires in the MCP guard as a
/// PreToolUse hook, so even an MCP server Claude discovers via `~/.claude.json`
/// can't run a mutating tool under this policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub name: &'static str,
    pub tools: &'static [&'static str],
    pub mcp_guard: bool,
    /// Read/Glob/Grep are confined to the edit root by the read guard.
    pub read_guard: bool,
    /// Bash is narrowed to the `Bash(<prefix>:*)` grants by the bash guard.
    pub bash_guard: bool,
    /// The Bash grant has no legitimate reason to write anywhere, including
    /// its own cwd: the sandbox denies writes there.
    pub deny_cwd_write: bool,
}

impl Policy {
    /// Analysis / read-only jobs: no Edit/Write/Bash(*) at all, and MCP calls
    /// are filtered down to read-only verbs by the guard hook.
    pub const READ_ONLY: Policy = Policy {
        name: "readOnly",
        tools: &["Read", "Glob", "Grep", "WebFetch", "WebSearch", "Bash(git log:*)", "Bash(git blame:*)", "Bash(git show:*)"],
        mcp_guard: true,
        read_guard: false,
        bash_guard: false,
        deny_cwd_write: true,
    };
    /// Background runs a watcher starts with no click: readOnly minus
    /// WebFetch/WebSearch. They work from ticket and PR text anyone can write,
    /// so they get no way to send anything out.
    pub const READ_ONLY_BACKGROUND: Policy = Policy {
        name: "readOnlyBackground",
        tools: &["Read", "Glob", "Grep", "Bash(git log:*)", "Bash(git blame:*)", "Bash(git show:*)"],
        mcp_guard: true,
        read_guard: false,
        bash_guard: false,
        deny_cwd_write: true,
    };
    /// resolve-conflict: Claude edits files inside a disposable worktree that's
    /// never pushed until a human approves the diff. The read guard confines
    /// Read/Glob/Grep to the same root as the edit guard: the PR being worked
    /// on is untrusted input that may ask Claude to go reading elsewhere.
    pub const WORKTREE_WRITE: Policy = Policy {
        name: "worktreeWrite",
        tools: &["Read", "Edit", "Write", "Bash", "Glob", "Grep"],
        mcp_guard: false,
        read_guard: true,
        bash_guard: false,
        deny_cwd_write: false,
    };
    /// address-review-comments: the same worktree confinement, but Bash is
    /// narrowed to `git diff`/`git status` plus the user's configured check
    /// commands. The `Bash(...)` entries only PRE-APPROVE under
    /// bypassPermissions (live-verified: `touch x && echo MADE` ran with no
    /// denial), so the bash guard hook is what ENFORCES the allowlist.
    pub const WORKTREE_WRITE_NARROW_BASH: Policy = Policy {
        name: "worktreeWriteNarrowBash",
        tools: &["Read", "Edit", "Write", "Glob", "Grep", "Bash(git diff:*)", "Bash(git status:*)"],
        mcp_guard: false,
        read_guard: true,
        bash_guard: true,
        deny_cwd_write: false,
    };
    /// A Claude call with nothing to do but answer in text.
    pub const NO_TOOLS: Policy = Policy {
        name: "noTools",
        tools: &[],
        mcp_guard: false,
        read_guard: false,
        bash_guard: false,
        deny_cwd_write: false,
    };

    pub const ALL: [Policy; 5] =
        [Self::READ_ONLY, Self::READ_ONLY_BACKGROUND, Self::WORKTREE_WRITE, Self::WORKTREE_WRITE_NARROW_BASH, Self::NO_TOOLS];

    pub fn by_name(name: &str) -> Option<Policy> {
        Self::ALL.into_iter().find(|p| p.name == name)
    }
}

/// `Bash(git log:*)` -> `Bash`; bare names pass through. `--tools` only accepts
/// built-in tool names, not permission patterns.
fn base_tool_name(tool: &str) -> &str {
    match tool.find('(') {
        Some(i) if i > 0 && tool[..i].bytes().all(|b| b.is_ascii_alphabetic()) => &tool[..i],
        _ => tool,
    }
}

/// Single-quote shell-escapes `value` for embedding in the guard hook's
/// `command` string, which Claude hands to the user's shell to execute.
/// Single quotes disable ALL shell interpretation (`$VAR`, `$(...)`, globbing,
/// backticks) except a literal single quote, which closes the quoting, emits
/// an escaped quote and reopens it: `'` -> `'\''`. Double-quoting is NOT
/// enough: it still expands `$...`, which an install path containing a `$`, a
/// space or an apostrophe must survive.
pub fn shell_quote_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The common places a developer's own credentials live, relative to `$HOME`:
/// denied to EVERY Claude run (as `Read(...)` rules and as sandbox `denyRead`
/// paths), not just the worktree ones: an analysis job reads untrusted
/// PR/issue text too, and none of these is ever something a job needs.
/// `.bitbucket-ai-companion` is the companion's own legacy state dir.
const SECRET_HOME_DIRS: [&str; 6] = [".ssh", ".aws", ".gnupg", ".config/gh", ".kube", ".bitbucket-ai-companion"];
const SECRET_HOME_FILES: [&str; 4] = [".netrc", ".npmrc", ".docker/config.json", ".cursor/mcp.json"];

/// Where the files that must stay private live.
pub struct ProtectedDirs<'a> {
    pub state_dir: &'a str,
    pub companion_dir: &'a str,
    pub home: &'a str,
}

/// Deny rules for `--disallowedTools`, in Claude's `//<absolute path>` syntax.
/// Deliberately narrow: credentials.key, credentials.enc, config.json,
/// `.claude.json` and history.db (and its -wal/-shm) are protected
/// individually, and Edit is denied across the whole companion tree (Claude
/// should never rewrite its own code), but the state dir itself is NOT
/// blanket-denied: resolve-conflict's disposable worktrees live under it and
/// Claude needs to Read/Edit/Write inside those.
pub fn protected_path_rules(d: &ProtectedDirs<'_>) -> Vec<String> {
    // The absolute paths already start with "/", so one more makes `//abs`.
    let rule = |kind: &str, base: &str, rest: &[&str]| {
        let mut parts = vec![base];
        parts.extend_from_slice(rest);
        format!("{kind}(/{})", node_path::join(&parts))
    };
    let mut rules = vec![
        rule("Read", d.state_dir, &["credentials.key"]),
        rule("Read", d.state_dir, &["history.db"]),
        rule("Read", d.state_dir, &["history.db-wal"]),
        rule("Read", d.state_dir, &["history.db-shm"]),
        rule("Read", d.companion_dir, &["credentials.enc"]),
        rule("Read", d.companion_dir, &["config.json"]),
        rule("Edit", d.companion_dir, &["**"]),
        rule("Read", d.home, &[".claude.json"]),
    ];
    rules.extend(SECRET_HOME_DIRS.iter().map(|dir| rule("Read", d.home, &[dir, "**"])));
    rules.extend(SECRET_HOME_FILES.iter().map(|f| rule("Read", d.home, &[f])));
    rules
}

/// The same protected files as plain absolute paths: the form the Claude Code
/// sandbox's `filesystem.denyRead` expects (unlike the `//abs` form
/// `--disallowedTools` rules use).
pub fn protected_sandbox_paths(d: &ProtectedDirs<'_>) -> Vec<String> {
    let mut paths = vec![
        node_path::join(&[d.state_dir, "credentials.key"]),
        node_path::join(&[d.state_dir, "history.db"]),
        node_path::join(&[d.state_dir, "history.db-wal"]),
        node_path::join(&[d.state_dir, "history.db-shm"]),
        node_path::join(&[d.companion_dir, "credentials.enc"]),
        node_path::join(&[d.companion_dir, "config.json"]),
        node_path::join(&[d.home, ".claude.json"]),
    ];
    paths.extend(SECRET_HOME_DIRS.iter().map(|dir| node_path::join(&[d.home, dir])));
    paths.extend(SECRET_HOME_FILES.iter().map(|f| node_path::join(&[d.home, f])));
    paths
}

fn grants_bash(tool: &str) -> bool {
    tool == "Bash" || tool.starts_with("Bash(")
}

/// A policy needs the Bash sandbox exactly when it grants any form of Bash.
/// Read/Edit `--disallowedTools` rules never governed Bash: this is what
/// closes that gap.
pub fn is_sandboxed_policy(policy: &Policy) -> bool {
    policy.tools.iter().any(|t| grants_bash(t))
}

/// The `sandbox` object for `--settings`. `denyRead` is always present;
/// `allowWrite`/`denyWrite` are omitted entirely when empty rather than sent as
/// `[]`, so a run that doesn't need either doesn't have to reason about what an
/// empty array means to the sandbox.
pub fn sandbox_settings(deny_read: &[String], allow_write: &[String], deny_write: &[String]) -> Value {
    let mut filesystem = Map::new();
    filesystem.insert("denyRead".into(), json!(deny_read));
    if !allow_write.is_empty() {
        filesystem.insert("allowWrite".into(), json!(allow_write));
    }
    if !deny_write.is_empty() {
        filesystem.insert("denyWrite".into(), json!(deny_write));
    }
    json!({
        "enabled": true,
        // A command the sandbox can't mediate must fail loudly, not silently
        // run unsandboxed: that would defeat the entire point.
        "allowUnsandboxedCommands": false,
        // If the sandbox mechanism itself can't start (missing bwrap/socat on
        // Linux, say), the run must fail rather than fall back to running
        // Bash unsandboxed.
        "failIfUnavailable": true,
        "filesystem": Value::Object(filesystem),
    })
}

#[derive(Debug, Clone)]
pub struct ClaudeArgsOptions<'a> {
    pub prompt: String,
    /// No `--model` unless one is given: Claude Code then uses its own default.
    pub model: Option<String>,
    pub policy: &'a Policy,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub extra_allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub protected_rules: Vec<String>,
    pub sandbox_deny_read: Vec<String>,
    pub sandbox_allow_write: Vec<String>,
    pub sandbox_deny_write: Vec<String>,
    /// The one directory the edit and read guards confine Claude to.
    pub edit_root: Option<String>,
    /// The companion binary the guard hooks run (`<binary> guard ...`).
    pub guard_binary: String,
    pub output_format: String,
}

impl<'a> ClaudeArgsOptions<'a> {
    pub fn new(prompt: &str, policy: &'a Policy) -> Self {
        ClaudeArgsOptions {
            prompt: prompt.to_string(),
            model: None,
            policy,
            session_id: None,
            cwd: None,
            extra_allowed_tools: vec![],
            disallowed_tools: vec![],
            protected_rules: vec![],
            sandbox_deny_read: vec![],
            sandbox_allow_write: vec![],
            sandbox_deny_write: vec![],
            edit_root: None,
            guard_binary: String::new(),
            output_format: "json".into(),
        }
    }
}

/// `'<binary>' guard <name> ['<arg>']`, for a hook's `command`.
fn hook_command(binary: &str, guard: &str, arg: Option<&str>) -> String {
    let mut c = format!("{} guard {guard}", shell_quote_single(binary));
    if let Some(arg) = arg {
        c.push(' ');
        c.push_str(&shell_quote_single(arg));
    }
    c
}

fn hook(matcher: &str, command: String) -> Value {
    json!({ "matcher": matcher, "hooks": [{ "type": "command", "command": command }] })
}

/// Builds the `claude` CLI argv, with the exact flag order and semantics the CLI
/// was verified against. The MCP guard hook (readOnly), the edit guard (via
/// `edit_root`), the read guard and the bash guard (per policy flag) and the
/// Bash sandbox (any policy that grants Bash) are all merged into ONE
/// `--settings` object: Claude Code only accepts a single `--settings` flag per
/// invocation. The hooks share one `PreToolUse` array; Claude Code runs every
/// entry whose matcher fits the call.
///
/// Fails closed: a policy that needs a guard it can't wire (no edit root for
/// the read guard, no guard binary for any hook) is refused rather than run
/// with Claude's reads, writes or Bash unconfined.
pub fn build_claude_args(o: &ClaudeArgsOptions<'_>) -> Result<Vec<String>, String> {
    let policy = o.policy;
    let mut allowed: Vec<String> = policy.tools.iter().map(|t| t.to_string()).collect();
    allowed.extend(o.extra_allowed_tools.iter().cloned());
    let mut availability: Vec<&str> = Vec::new();
    for t in policy.tools {
        let b = base_tool_name(t);
        if !availability.contains(&b) {
            availability.push(b);
        }
    }

    let mut args: Vec<String> = vec!["-p".into(), o.prompt.clone()];
    if let Some(m) = o.model.as_ref().filter(|m| !m.is_empty()) {
        args.extend(["--model".into(), m.clone()]);
    }
    args.extend([
        "--permission-mode".into(),
        "bypassPermissions".into(),
        "--allowedTools".into(),
        allowed.join(" "),
        "--output-format".into(),
        o.output_format.clone(),
        "--tools".into(),
        availability.join(","),
    ]);
    // The CLI requires --verbose alongside stream-json.
    if o.output_format == "stream-json" {
        args.push("--verbose".into());
    }
    if let Some(id) = o.session_id.as_ref().filter(|s| !s.is_empty()) {
        args.extend(["--session-id".into(), id.clone()]);
    }
    let disallowed: Vec<&str> = o.disallowed_tools.iter().chain(&o.protected_rules).map(String::as_str).collect();
    if !disallowed.is_empty() {
        args.extend(["--disallowedTools".into(), disallowed.join(" ")]);
    }

    let edit_root = o.edit_root.as_deref().filter(|r| !r.is_empty());
    let needs_binary = policy.mcp_guard || edit_root.is_some() || policy.read_guard || policy.bash_guard;
    if needs_binary && o.guard_binary.is_empty() {
        return Err(if policy.bash_guard {
            "this policy requires the bash guard, but no guard binary was given".into()
        } else if policy.read_guard {
            "this policy requires the read guard, but no guard binary was given".into()
        } else {
            "this policy requires a guard hook, but no guard binary was given".into()
        });
    }

    let mut hooks: Vec<Value> = Vec::new();
    if policy.mcp_guard {
        hooks.push(hook("mcp__.*", hook_command(&o.guard_binary, "mcp", None)));
    }
    if let Some(root) = edit_root {
        // Fail-closed filesystem confinement for Edit/Write/MultiEdit/NotebookEdit.
        hooks.push(hook("Edit|Write|MultiEdit|NotebookEdit", hook_command(&o.guard_binary, "edit", Some(root))));
    }
    if policy.read_guard {
        // A worktree policy with no root would silently leave Claude's reads
        // unconfined, so refuse instead.
        let Some(root) = edit_root else {
            return Err("this policy requires the read guard, but no editRoot was given".into());
        };
        hooks.push(hook("Read|Glob|Grep", hook_command(&o.guard_binary, "read", Some(root))));
    }
    if policy.bash_guard {
        // The allowlist is derived from the very `Bash(<prefix>:*)` rules
        // --allowedTools pre-approves, so the two can't disagree.
        let prefixes = prefixes_from_bash_rules(&allowed);
        let json_prefixes = serde_json::to_string(&prefixes).unwrap_or_default();
        hooks.push(hook("Bash", hook_command(&o.guard_binary, "bash", Some(&json_prefixes))));
    }

    let mut settings = Map::new();
    if !hooks.is_empty() {
        settings.insert("hooks".into(), json!({ "PreToolUse": hooks }));
    }
    if is_sandboxed_policy(policy) {
        // deny_cwd_write means "this policy's Bash grant has no legitimate
        // reason to write, including into its own cwd": added here rather than
        // by every caller, so a policy that wants it always gets it.
        let mut deny_write = o.sandbox_deny_write.clone();
        if let Some(cwd) = o.cwd.as_ref().filter(|c| policy.deny_cwd_write && !c.is_empty()) {
            deny_write.push(cwd.clone());
        }
        settings.insert("sandbox".into(), sandbox_settings(&o.sandbox_deny_read, &o.sandbox_allow_write, &deny_write));
    }
    if !settings.is_empty() {
        args.extend(["--settings".into(), Value::Object(settings).to_string()]);
    }
    Ok(args)
}

/// `addressReviewComments.checkCommands` from config.json, trimmed and
/// validated: the one list the prompt, the `--allowedTools` rules and the bash
/// guard all use. Each entry becomes part of a permission-rule string, so
/// anything that could close the rule early or widen it is refused outright
/// rather than escaped: `(`/`)` delimit the rule, `*` is its wildcard, and a
/// newline would split one rule into two. Anything the bash guard would refuse
/// at run time (chaining, pipes, `$`, redirection, backslashes) is refused here
/// too, so a configured check can never be one Claude isn't allowed to run.
/// The error carries the offending index so the user knows which entry to fix.
pub fn normalize_check_commands(check_commands: Option<&Value>) -> Result<Vec<String>, String> {
    let list = match check_commands {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Array(a)) => a,
        Some(_) => {
            return Err("addressReviewComments.checkCommands in config.json must be an array of command strings, e.g. [\"npm test\", \"npx tsc --noEmit\"].".into());
        }
    };
    list.iter()
        .enumerate()
        .map(|(i, raw)| {
            let cmd = raw.as_str().map_or("", js_trim);
            if cmd.is_empty() || cmd.contains('*') || has_shell_metacharacters(cmd) {
                return Err(format!(
                    "addressReviewComments.checkCommands[{i}] in config.json ({}) must be one plain non-empty command with no ; & | ` $ ( ) < > \\ * or line breaks \u{2014} fix or remove it, then retry.",
                    js_stringify(Some(raw))
                ));
            }
            Ok(cmd.to_string())
        })
        .collect()
}

/// The normalized list as one `Bash(<cmd>:*)` allow rule each, for the narrow
/// Bash policy's extra allowed tools (and so its bash guard).
pub fn check_command_rules(check_commands: Option<&Value>) -> Result<Vec<String>, String> {
    Ok(normalize_check_commands(check_commands)?.into_iter().map(|c| format!("Bash({c}:*)")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn opts<'a>(policy: &'a Policy) -> ClaudeArgsOptions<'a> {
        let mut o = ClaudeArgsOptions::new("p", policy);
        o.model = Some("m".into());
        o
    }

    /// The edit root + guard binary every worktree-policy (read guard) build needs.
    fn wt<'a>(policy: &'a Policy) -> ClaudeArgsOptions<'a> {
        ClaudeArgsOptions { edit_root: Some("/wt".into()), guard_binary: "/usr/bin/companion".into(), ..opts(policy) }
    }

    fn settings_of(args: &[String]) -> Value {
        let i = args.iter().position(|a| a == "--settings").expect("--settings");
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1, "only one --settings flag");
        serde_json::from_str(&args[i + 1]).unwrap()
    }

    fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
        &args[args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag}")) + 1]
    }

    #[test]
    fn policies_have_the_exact_shapes() {
        let p = Policy::READ_ONLY;
        assert_eq!(p.tools, ["Read", "Glob", "Grep", "WebFetch", "WebSearch", "Bash(git log:*)", "Bash(git blame:*)", "Bash(git show:*)"]);
        assert_eq!((p.mcp_guard, p.deny_cwd_write, p.read_guard, p.bash_guard), (true, true, false, false));
        let b = Policy::READ_ONLY_BACKGROUND;
        assert_eq!(b.tools, ["Read", "Glob", "Grep", "Bash(git log:*)", "Bash(git blame:*)", "Bash(git show:*)"]);
        assert_eq!((b.mcp_guard, b.deny_cwd_write), (true, true));
        let w = Policy::WORKTREE_WRITE;
        assert_eq!(w.tools, ["Read", "Edit", "Write", "Bash", "Glob", "Grep"]);
        assert_eq!((w.mcp_guard, w.read_guard, w.bash_guard, w.deny_cwd_write), (false, true, false, false));
        let n = Policy::WORKTREE_WRITE_NARROW_BASH;
        assert_eq!(n.tools, ["Read", "Edit", "Write", "Glob", "Grep", "Bash(git diff:*)", "Bash(git status:*)"]);
        assert_eq!((n.mcp_guard, n.read_guard, n.bash_guard), (false, true, true));
        assert_eq!(Policy::NO_TOOLS.tools.len(), 0);
        assert!(!std::hint::black_box(Policy::NO_TOOLS).mcp_guard);
        assert_eq!(Policy::by_name("worktreeWrite"), Some(Policy::WORKTREE_WRITE));
        assert_eq!(Policy::by_name("nope"), None);
        // Background is readOnly without the web tools.
        let no_web: Vec<&str> = Policy::READ_ONLY.tools.iter().copied().filter(|t| !t.starts_with("Web")).collect();
        assert_eq!(b.tools, no_web.as_slice());
        assert!(is_sandboxed_policy(&b));
    }

    const DIRS: ProtectedDirs<'static> = ProtectedDirs { state_dir: "/home/me/.ai-dev-companion", companion_dir: "/opt/ai-dev-companion/companion-service", home: "/home/me" };

    #[test]
    fn protected_path_rules_are_the_exact_deny_rules_not_a_blanket_state_dir_deny() {
        let rules = protected_path_rules(&DIRS);
        assert_eq!(
            rules,
            [
                "Read(//home/me/.ai-dev-companion/credentials.key)",
                "Read(//home/me/.ai-dev-companion/history.db)",
                "Read(//home/me/.ai-dev-companion/history.db-wal)",
                "Read(//home/me/.ai-dev-companion/history.db-shm)",
                "Read(//opt/ai-dev-companion/companion-service/credentials.enc)",
                "Read(//opt/ai-dev-companion/companion-service/config.json)",
                "Edit(//opt/ai-dev-companion/companion-service/**)",
                "Read(//home/me/.claude.json)",
                "Read(//home/me/.ssh/**)",
                "Read(//home/me/.aws/**)",
                "Read(//home/me/.gnupg/**)",
                "Read(//home/me/.config/gh/**)",
                "Read(//home/me/.kube/**)",
                "Read(//home/me/.bitbucket-ai-companion/**)",
                "Read(//home/me/.netrc)",
                "Read(//home/me/.npmrc)",
                "Read(//home/me/.docker/config.json)",
                "Read(//home/me/.cursor/mcp.json)",
            ]
        );
        // resolve-conflict worktrees live under the state dir: a blanket deny would block them.
        assert!(!rules.iter().any(|r| r == "Read(//home/me/.ai-dev-companion/**)"));
    }

    #[test]
    fn protected_sandbox_paths_are_plain_absolute_paths() {
        assert_eq!(
            protected_sandbox_paths(&DIRS),
            [
                "/home/me/.ai-dev-companion/credentials.key",
                "/home/me/.ai-dev-companion/history.db",
                "/home/me/.ai-dev-companion/history.db-wal",
                "/home/me/.ai-dev-companion/history.db-shm",
                "/opt/ai-dev-companion/companion-service/credentials.enc",
                "/opt/ai-dev-companion/companion-service/config.json",
                "/home/me/.claude.json",
                "/home/me/.ssh",
                "/home/me/.aws",
                "/home/me/.gnupg",
                "/home/me/.config/gh",
                "/home/me/.kube",
                "/home/me/.bitbucket-ai-companion",
                "/home/me/.netrc",
                "/home/me/.npmrc",
                "/home/me/.docker/config.json",
                "/home/me/.cursor/mcp.json",
            ]
        );
    }

    #[test]
    fn protected_secret_locations_are_denied_to_every_run() {
        let d = ProtectedDirs { state_dir: "/h/.ai-dev-companion", companion_dir: "/c", home: "/h" };
        let (rules, sandbox) = (protected_path_rules(&d), protected_sandbox_paths(&d));
        for dir in [".ssh", ".aws", ".gnupg", ".config/gh", ".kube", ".bitbucket-ai-companion"] {
            assert!(rules.contains(&format!("Read(//h/{dir}/**)")), "{dir}");
            assert!(sandbox.contains(&format!("/h/{dir}")), "{dir}");
        }
        for file in [".netrc", ".npmrc", ".docker/config.json", ".cursor/mcp.json"] {
            assert!(rules.contains(&format!("Read(//h/{file})")), "{file}");
            assert!(sandbox.contains(&format!("/h/{file}")), "{file}");
        }
    }

    #[test]
    fn sandboxed_exactly_when_bash_is_granted() {
        assert!(is_sandboxed_policy(&Policy::READ_ONLY));
        assert!(is_sandboxed_policy(&Policy::WORKTREE_WRITE));
        assert!(is_sandboxed_policy(&Policy::WORKTREE_WRITE_NARROW_BASH));
        assert!(!is_sandboxed_policy(&Policy::NO_TOOLS));
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn sandbox_settings_deny_read_always_present_allow_and_deny_write_only_when_non_empty() {
        assert_eq!(
            sandbox_settings(&s(&["/a"]), &[], &[]),
            json!({ "enabled": true, "allowUnsandboxedCommands": false, "failIfUnavailable": true, "filesystem": { "denyRead": ["/a"] } })
        );
        assert_eq!(sandbox_settings(&[], &[], &[])["filesystem"], json!({ "denyRead": [] }));
        assert_eq!(
            sandbox_settings(&s(&["/a"]), &s(&["/b"]), &s(&["/c"]))["filesystem"],
            json!({ "denyRead": ["/a"], "allowWrite": ["/b"], "denyWrite": ["/c"] })
        );
        // The keys come out in the order the sandbox docs list them.
        assert_eq!(sandbox_settings(&[], &s(&["/b"]), &s(&["/c"])).to_string(), r#"{"enabled":true,"allowUnsandboxedCommands":false,"failIfUnavailable":true,"filesystem":{"denyRead":[],"allowWrite":["/b"],"denyWrite":["/c"]}}"#);
    }

    #[test]
    fn read_only_gets_the_base_argv_and_one_settings_with_the_mcp_hook_and_the_sandbox() {
        let o = ClaudeArgsOptions {
            prompt: "do the thing".into(),
            model: None,
            guard_binary: "/usr/bin/companion".into(),
            sandbox_deny_read: s(&["/home/me/.claude.json"]),
            sandbox_deny_write: s(&["/repo/session-dir"]),
            ..ClaudeArgsOptions::new("x", &Policy::READ_ONLY)
        };
        let args = build_claude_args(&o).unwrap();
        let base: Vec<String> = [
            "-p", "do the thing", "--permission-mode", "bypassPermissions", "--allowedTools",
            "Read Glob Grep WebFetch WebSearch Bash(git log:*) Bash(git blame:*) Bash(git show:*)",
            "--output-format", "json", "--tools", "Read,Glob,Grep,WebFetch,WebSearch,Bash",
        ]
        .iter()
        .map(|a| a.to_string())
        .collect();
        assert_eq!(&args[..base.len()], &base[..]);
        assert!(!args.contains(&"--session-id".to_string()) && !args.contains(&"--disallowedTools".to_string()) && !args.contains(&"--model".to_string()));
        let settings = settings_of(&args);
        assert_eq!(
            settings,
            json!({
                "hooks": { "PreToolUse": [{ "matcher": "mcp__.*", "hooks": [{ "type": "command", "command": "'/usr/bin/companion' guard mcp" }] }] },
                "sandbox": { "enabled": true, "allowUnsandboxedCommands": false, "failIfUnavailable": true,
                             "filesystem": { "denyRead": ["/home/me/.claude.json"], "denyWrite": ["/repo/session-dir"] } },
            })
        );
        assert_eq!(args.len(), args.iter().position(|a| a == "--settings").unwrap() + 2);
    }

    #[test]
    fn cwd_is_denied_for_write_automatically_only_when_the_policy_asks_for_it() {
        let ro = ClaudeArgsOptions { cwd: Some("/tmp/analysis-session".into()), guard_binary: "/b".into(), sandbox_deny_write: s(&["/other/explicit/path"]), ..opts(&Policy::READ_ONLY) };
        let settings = settings_of(&build_claude_args(&ro).unwrap());
        assert_eq!(settings["sandbox"]["filesystem"]["denyWrite"], json!(["/other/explicit/path", "/tmp/analysis-session"]));
        let ww = ClaudeArgsOptions { cwd: Some("/tmp/some-worktree".into()), ..wt(&Policy::WORKTREE_WRITE) };
        let settings = settings_of(&build_claude_args(&ww).unwrap());
        assert!(settings["sandbox"]["filesystem"].get("denyWrite").is_none());
    }

    #[test]
    fn worktree_write_has_no_mcp_hook_but_the_sandbox_with_the_allowed_write_root() {
        let o = ClaudeArgsOptions { sandbox_deny_read: s(&["/home/me/.claude.json"]), sandbox_allow_write: s(&["/repo/.git/worktrees/abc123"]), ..wt(&Policy::WORKTREE_WRITE) };
        let args = build_claude_args(&o).unwrap();
        assert_eq!(after(&args, "--tools"), "Read,Edit,Write,Bash,Glob,Grep");
        let settings = settings_of(&args);
        assert_eq!(
            settings["sandbox"],
            json!({ "enabled": true, "allowUnsandboxedCommands": false, "failIfUnavailable": true,
                    "filesystem": { "denyRead": ["/home/me/.claude.json"], "allowWrite": ["/repo/.git/worktrees/abc123"] } })
        );
        assert!(!settings["hooks"]["PreToolUse"].as_array().unwrap().iter().any(|h| h["matcher"] == "mcp__.*"));
    }

    #[test]
    fn worktree_write_settings_hold_the_edit_guard_the_read_guard_with_the_same_root_and_the_sandbox() {
        let o = ClaudeArgsOptions { edit_root: Some("/home/me/.ai-dev-companion/worktrees/repo/abc123".into()), sandbox_allow_write: s(&["/repo/.git/worktrees/abc123"]), ..wt(&Policy::WORKTREE_WRITE) };
        assert_eq!(
            settings_of(&build_claude_args(&o).unwrap()),
            json!({
                "hooks": { "PreToolUse": [
                    { "matcher": "Edit|Write|MultiEdit|NotebookEdit", "hooks": [{ "type": "command", "command": "'/usr/bin/companion' guard edit '/home/me/.ai-dev-companion/worktrees/repo/abc123'" }] },
                    { "matcher": "Read|Glob|Grep", "hooks": [{ "type": "command", "command": "'/usr/bin/companion' guard read '/home/me/.ai-dev-companion/worktrees/repo/abc123'" }] },
                ] },
                "sandbox": { "enabled": true, "allowUnsandboxedCommands": false, "failIfUnavailable": true,
                             "filesystem": { "denyRead": [], "allowWrite": ["/repo/.git/worktrees/abc123"] } },
            })
        );
    }

    #[test]
    fn the_mcp_hook_and_the_edit_root_hook_share_one_pre_tool_use_array_mcp_first() {
        let o = ClaudeArgsOptions { edit_root: Some("/r".into()), guard_binary: "/b".into(), ..opts(&Policy::READ_ONLY) };
        let settings = settings_of(&build_claude_args(&o).unwrap());
        let matchers: Vec<&str> = settings["hooks"]["PreToolUse"].as_array().unwrap().iter().map(|h| h["matcher"].as_str().unwrap()).collect();
        assert_eq!(matchers, ["mcp__.*", "Edit|Write|MultiEdit|NotebookEdit"]);
    }

    #[test]
    fn no_tools_is_not_sandboxed_has_no_settings_and_empty_tool_strings() {
        let o = ClaudeArgsOptions { model: Some("claude-haiku-4-5-20251001".into()), sandbox_deny_read: s(&["/home/me/.claude.json"]), ..ClaudeArgsOptions::new("pick a repo", &Policy::NO_TOOLS) };
        assert_eq!(
            build_claude_args(&o).unwrap(),
            ["-p", "pick a repo", "--model", "claude-haiku-4-5-20251001", "--permission-mode", "bypassPermissions", "--allowedTools", "", "--output-format", "json", "--tools", ""]
        );
    }

    #[test]
    fn a_path_with_a_space_a_dollar_and_a_quote_survives_a_real_shell() {
        let tricky = "/opt/weird bin/$HOME/node's-copy";
        let o = ClaudeArgsOptions { guard_binary: tricky.into(), ..opts(&Policy::READ_ONLY) };
        let settings = settings_of(&build_claude_args(&o).unwrap());
        let command = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
        // `f` echoes its positional args back, "|||"-delimited; none contain that.
        let script = format!("f() {{ printf '%s|||%s|||%s' \"$1\" \"$2\" \"$3\"; }}; f {command}");
        let out = Command::new("/bin/sh").arg("-c").arg(script).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{tricky}|||guard|||mcp"));
        assert_eq!(shell_quote_single("it's"), r"'it'\''s'");
        assert_eq!(shell_quote_single(""), "''");
    }

    #[test]
    fn session_id_model_and_output_format_flags() {
        let o = ClaudeArgsOptions { session_id: Some("11111111-1111-1111-1111-111111111111".into()), ..opts(&Policy::NO_TOOLS) };
        assert_eq!(after(&build_claude_args(&o).unwrap(), "--session-id"), "11111111-1111-1111-1111-111111111111");
        assert_eq!(after(&build_claude_args(&ClaudeArgsOptions { output_format: "text".into(), ..opts(&Policy::NO_TOOLS) }).unwrap(), "--output-format"), "text");
        let stream = build_claude_args(&ClaudeArgsOptions { output_format: "stream-json".into(), ..opts(&Policy::NO_TOOLS) }).unwrap();
        assert_eq!(after(&stream, "--output-format"), "stream-json");
        assert!(stream.contains(&"--verbose".to_string()));
        assert!(!build_claude_args(&opts(&Policy::NO_TOOLS)).unwrap().contains(&"--verbose".to_string()));
        assert_eq!(after(&build_claude_args(&opts(&Policy::NO_TOOLS)).unwrap(), "--model"), "m");
        for model in [None, Some(String::new())] {
            assert!(!build_claude_args(&ClaudeArgsOptions { model, ..opts(&Policy::NO_TOOLS) }).unwrap().contains(&"--model".to_string()));
        }
    }

    #[test]
    fn disallowed_tools_and_protected_rules_join_into_one_flag_omitted_when_both_are_empty() {
        let o = ClaudeArgsOptions { disallowed_tools: s(&["mcp__acme-jenkins-dii__stop_build"]), protected_rules: s(&["Read(//home/me/.ai-dev-companion/credentials.key)"]), ..wt(&Policy::WORKTREE_WRITE) };
        assert_eq!(after(&build_claude_args(&o).unwrap(), "--disallowedTools"), "mcp__acme-jenkins-dii__stop_build Read(//home/me/.ai-dev-companion/credentials.key)");
        assert!(!build_claude_args(&wt(&Policy::WORKTREE_WRITE)).unwrap().contains(&"--disallowedTools".to_string()));
    }

    #[test]
    fn extra_allowed_tools_extend_allowed_tools_but_not_tools() {
        let o = ClaudeArgsOptions { extra_allowed_tools: s(&["mcp__wiki"]), guard_binary: "/b".into(), ..opts(&Policy::READ_ONLY) };
        let args = build_claude_args(&o).unwrap();
        assert_eq!(after(&args, "--allowedTools"), format!("{} mcp__wiki", Policy::READ_ONLY.tools.join(" ")));
        assert!(!after(&args, "--tools").contains("mcp__wiki"));
    }

    #[test]
    fn narrow_bash_differs_from_worktree_write_only_in_its_bash_grant() {
        fn non_bash<'a>(t: &[&'a str]) -> Vec<&'a str> {
            t.iter().copied().filter(|x| !grants_bash(x)).collect()
        }
        assert_eq!(non_bash(Policy::WORKTREE_WRITE_NARROW_BASH.tools), non_bash(Policy::WORKTREE_WRITE.tools));
        assert_eq!(Policy::WORKTREE_WRITE_NARROW_BASH.mcp_guard, Policy::WORKTREE_WRITE.mcp_guard);
        let (narrow, plain) = std::hint::black_box((Policy::WORKTREE_WRITE_NARROW_BASH, Policy::WORKTREE_WRITE));
        assert!(!narrow.deny_cwd_write && narrow.bash_guard && !plain.bash_guard);
    }

    #[test]
    fn check_command_rules_turn_each_command_into_a_bash_rule_trimmed() {
        assert_eq!(check_command_rules(Some(&json!(["npm test", "  npx tsc --noEmit  "]))).unwrap(), ["Bash(npm test:*)", "Bash(npx tsc --noEmit:*)"]);
        assert!(check_command_rules(Some(&json!([]))).unwrap().is_empty());
        assert!(check_command_rules(None).unwrap().is_empty());
        assert!(check_command_rules(Some(&json!(null))).unwrap().is_empty());
        assert_eq!(normalize_check_commands(Some(&json!(["  npm test ", "npx tsc --noEmit"]))).unwrap(), ["npm test", "npx tsc --noEmit"]);
    }

    #[test]
    fn check_command_rules_reject_entries_that_could_widen_the_rule_naming_the_bad_entry() {
        let bad_entries = [
            json!(""), json!("   "), json!("npm test) Bash(rm"), json!("npm test\nrm -rf /"), json!("npm\rtest"), json!("npm *"), json!("a(b"),
            // Anything the bash guard would refuse at run time is refused here too.
            json!("npm test && npm run lint"), json!("npm test; rm x"), json!("npm test | tee log"), json!("npm test > log"), json!("npm test < in"),
            json!("echo $HOME"), json!("echo `id`"), json!("npm test \\"), json!(42), json!(null),
        ];
        for bad in bad_entries {
            let err = check_command_rules(Some(&json!(["npm test", bad.clone()]))).unwrap_err();
            assert!(err.contains("addressReviewComments.checkCommands[1]") && err.contains("config.json"), "{bad}: {err}");
            assert!(err.contains(&js_stringify(Some(&bad))), "the entry is quoted: {err}");
        }
        assert!(check_command_rules(Some(&json!("npm test"))).unwrap_err().contains("must be an array"));
        assert!(normalize_check_commands(Some(&json!(["npm test && x"]))).unwrap_err().contains("checkCommands[0]"));
        assert!(check_command_rules(Some(&json!([""]))).unwrap_err().ends_with("\u{2014} fix or remove it, then retry."));
    }

    #[test]
    fn narrow_bash_argv_pre_approves_only_git_diff_status_and_the_checks_and_one_settings_holds_all_guards() {
        let o = ClaudeArgsOptions {
            cwd: Some("/wt".into()),
            extra_allowed_tools: check_command_rules(Some(&json!(["npm test"]))).unwrap(),
            sandbox_deny_read: s(&["/home/me/.claude.json"]),
            sandbox_allow_write: s(&["/repo/.git/worktrees/job"]),
            sandbox_deny_write: s(&["/wt/.git", "/repo/.git/worktrees/job/commondir"]),
            guard_binary: "/usr/bin/node-free-companion".into(),
            ..wt(&Policy::WORKTREE_WRITE_NARROW_BASH)
        };
        let args = build_claude_args(&o).unwrap();
        assert_eq!(after(&args, "--allowedTools"), "Read Edit Write Glob Grep Bash(git diff:*) Bash(git status:*) Bash(npm test:*)");
        assert_eq!(after(&args, "--tools"), "Read,Edit,Write,Glob,Grep,Bash");
        let settings = settings_of(&args);
        assert_eq!(
            settings["sandbox"],
            json!({ "enabled": true, "allowUnsandboxedCommands": false, "failIfUnavailable": true,
                    "filesystem": { "denyRead": ["/home/me/.claude.json"], "allowWrite": ["/repo/.git/worktrees/job"], "denyWrite": ["/wt/.git", "/repo/.git/worktrees/job/commondir"] } })
        );
        // --allowedTools only PRE-APPROVES under bypassPermissions; the bash guard hook refuses everything else.
        assert_eq!(
            settings["hooks"]["PreToolUse"],
            json!([
                { "matcher": "Edit|Write|MultiEdit|NotebookEdit", "hooks": [{ "type": "command", "command": "'/usr/bin/node-free-companion' guard edit '/wt'" }] },
                { "matcher": "Read|Glob|Grep", "hooks": [{ "type": "command", "command": "'/usr/bin/node-free-companion' guard read '/wt'" }] },
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": r#"'/usr/bin/node-free-companion' guard bash '["git diff","git status","npm test"]'"# }] },
            ])
        );
    }

    #[test]
    fn the_bash_guard_command_survives_a_real_shell_with_a_check_command_containing_a_quote() {
        let o = ClaudeArgsOptions { extra_allowed_tools: check_command_rules(Some(&json!(["npm run it's"]))).unwrap(), ..wt(&Policy::WORKTREE_WRITE_NARROW_BASH) };
        let settings = settings_of(&build_claude_args(&o).unwrap());
        let command = settings["hooks"]["PreToolUse"].as_array().unwrap().iter().find(|h| h["matcher"] == "Bash").unwrap()["hooks"][0]["command"].as_str().unwrap().to_string();
        let script = format!("f() {{ printf '%s' \"$4\"; }}; f {command}");
        let out = Command::new("/bin/sh").arg("-c").arg(script).output().unwrap();
        let prefixes: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(prefixes, ["git diff", "git status", "npm run it's"]);
    }

    #[test]
    fn a_policy_that_needs_a_guard_it_cannot_wire_is_refused_fail_closed() {
        // The read guard needs an edit root.
        let no_root = ClaudeArgsOptions { guard_binary: "/b".into(), ..opts(&Policy::WORKTREE_WRITE) };
        assert!(build_claude_args(&no_root).unwrap_err().contains("read guard"));
        let empty_root = ClaudeArgsOptions { edit_root: Some(String::new()), guard_binary: "/b".into(), ..opts(&Policy::WORKTREE_WRITE) };
        assert!(build_claude_args(&empty_root).unwrap_err().contains("read guard"));
        // Every guard needs the binary.
        for policy in [&Policy::WORKTREE_WRITE_NARROW_BASH, &Policy::WORKTREE_WRITE, &Policy::READ_ONLY] {
            let o = ClaudeArgsOptions { edit_root: Some("/wt".into()), ..opts(policy) };
            assert!(build_claude_args(&o).unwrap_err().contains("no guard binary was given"), "{}", policy.name);
        }
        assert!(build_claude_args(&ClaudeArgsOptions { edit_root: Some("/wt".into()), ..opts(&Policy::WORKTREE_WRITE_NARROW_BASH) }).unwrap_err().contains("bash guard"));
        // A policy with no guards needs no binary.
        assert!(build_claude_args(&opts(&Policy::NO_TOOLS)).is_ok());
    }

    #[test]
    fn worktree_write_adds_no_bash_hook_and_narrow_bash_adds_all_three_in_order() {
        let ww = settings_of(&build_claude_args(&wt(&Policy::WORKTREE_WRITE)).unwrap());
        assert!(!ww["hooks"]["PreToolUse"].as_array().unwrap().iter().any(|h| h["matcher"] == "Bash"));
        let nb = settings_of(&build_claude_args(&wt(&Policy::WORKTREE_WRITE_NARROW_BASH)).unwrap());
        let matchers: Vec<&str> = nb["hooks"]["PreToolUse"].as_array().unwrap().iter().map(|h| h["matcher"].as_str().unwrap()).collect();
        assert_eq!(matchers, ["Edit|Write|MultiEdit|NotebookEdit", "Read|Glob|Grep", "Bash"]);
        assert_eq!(nb["sandbox"]["enabled"], true);
    }

    #[test]
    fn base_tool_names_strip_the_permission_pattern() {
        assert_eq!(base_tool_name("Bash(git log:*)"), "Bash");
        assert_eq!(base_tool_name("Read"), "Read");
        assert_eq!(base_tool_name("mcp__x__y"), "mcp__x__y");
        assert_eq!(base_tool_name("(x)"), "(x)");
    }

    /// The Node implementation's argv for four representative runs, byte for
    /// byte, with the guard hook commands normalized (those intentionally
    /// changed from `node script` to `binary guard`): the flag order, the
    /// `--settings` key order and the sandbox shape must not have moved.
    #[test]
    fn argv_matches_what_the_node_implementation_built_apart_from_the_hook_commands() {
        let golden: Value = serde_json::from_str(include_str!("../tests/fixtures/claude-args/golden.json")).unwrap();
        let normalize = |mut args: Vec<String>| {
            if let Some(i) = args.iter().position(|a| a == "--settings") {
                let mut j: Value = serde_json::from_str(&args[i + 1]).unwrap();
                if let Some(hooks) = j.pointer_mut("/hooks/PreToolUse").and_then(Value::as_array_mut) {
                    for h in hooks {
                        h["hooks"][0]["command"] = json!("<cmd>");
                    }
                }
                args[i + 1] = j.to_string();
            }
            json!(args)
        };
        let bin = |o: ClaudeArgsOptions<'static>| ClaudeArgsOptions { guard_binary: "E".into(), ..o };
        let cases = [
            (
                "readOnly",
                bin(ClaudeArgsOptions {
                    model: Some("m".into()),
                    prompt: "p q".into(),
                    cwd: Some("/c".into()),
                    sandbox_deny_read: s(&["/r"]),
                    sandbox_deny_write: s(&["/w"]),
                    extra_allowed_tools: s(&["mcp__wiki"]),
                    disallowed_tools: s(&["d1"]),
                    protected_rules: s(&["Read(//x)"]),
                    session_id: Some("sid".into()),
                    output_format: "stream-json".into(),
                    ..ClaudeArgsOptions::new("p q", &Policy::READ_ONLY)
                }),
            ),
            ("worktreeWrite", bin(ClaudeArgsOptions { edit_root: Some("/wt".into()), sandbox_allow_write: s(&["/a"]), ..ClaudeArgsOptions::new("p", &Policy::WORKTREE_WRITE) })),
            (
                "worktreeWriteNarrowBash",
                bin(ClaudeArgsOptions {
                    edit_root: Some("/wt".into()),
                    extra_allowed_tools: check_command_rules(Some(&json!(["npm test"]))).unwrap(),
                    sandbox_deny_write: s(&["/x"]),
                    ..ClaudeArgsOptions::new("p", &Policy::WORKTREE_WRITE_NARROW_BASH)
                }),
            ),
            ("noTools", bin(ClaudeArgsOptions::new("p", &Policy::NO_TOOLS))),
        ];
        for (name, o) in cases {
            assert_eq!(normalize(build_claude_args(&o).unwrap()), golden[name], "{name}");
        }
    }
}
