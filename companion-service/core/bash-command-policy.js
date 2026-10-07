// Pure decision logic for core/bash-guard.js's PreToolUse hook — the
// enforcement half of core/claude-args.js's worktreeWriteNarrowBash policy.
// `--allowedTools` patterns like `Bash(git diff:*)` only PRE-APPROVE
// matching calls; under `--permission-mode bypassPermissions` they do not
// restrict anything (live-verified: `touch probe-made.txt && echo MADE` ran
// with zero denials under that policy). This module is what actually says
// no.
//
// Deliberately stricter than a shell parser: a command is allowed only if
// it is one plain invocation of an allowed prefix — nothing that could
// chain, pipe, substitute, redirect or continue a line, so an allowed
// prefix can never be extended into a second command.
//
// Plain JS (no build step) — core/bash-guard.js requires this directly and
// runs standalone with plain `node`, same as core/edit-path-policy.js.

// ; & | ` $ ( ) < > and CR/LF (which also covers a backslash-newline line
// continuation). A backslash on its own is refused too: it has no use in
// the commands this policy allows and only exists to change how the shell
// reads what follows it.
const SHELL_METACHARACTERS = /[;&|`$()<>\\\r\n]/;

// `git diff` is granted so Claude can look at its own changes, but these
// flags make it something else: `--no-index` compares ANY two paths on
// disk (a read outside the worktree the read guard can't see, since it's
// Bash), `--output` writes the diff to a file of Claude's choosing, and
// `--ext-diff` runs whatever external diff program is configured.
const REFUSED_GIT_DIFF_OPTIONS = ["--no-index", "--output", "--ext-diff"];

/** True if a `git diff` invocation carries one of REFUSED_GIT_DIFF_OPTIONS
 * anywhere in its arguments — in `--opt`, `--opt=value` or quoted form, or
 * as an abbreviation (git accepts any unambiguous prefix of a long
 * option, so `--outp=x` is `--output=x`). A bare `--` (end of options) is
 * not a refused prefix. Over-refusing an odd spelling is fine; a missed
 * one isn't. */
function gitDiffCarriesRefusedOption(cmd) {
  const tokens = cmd.split(/\s+/);
  if (tokens[0] !== "git" || tokens[1] !== "diff") return false;
  return tokens.slice(2).some((raw) => {
    const name = raw.replace(/['"]/g, "").split("=")[0];
    return name.length > 2 && REFUSED_GIT_DIFF_OPTIONS.some((opt) => opt.startsWith(name));
  });
}

/** True only when `command`, trimmed, is exactly one of `allowedPrefixes`
 * or one of them followed by a space and arguments, with no shell
 * metacharacters anywhere and — for `git diff` — none of
 * REFUSED_GIT_DIFF_OPTIONS. Anything malformed denies. */
function isBashCommandAllowed(command, allowedPrefixes) {
  if (typeof command !== "string" || !Array.isArray(allowedPrefixes)) return false;
  const cmd = command.trim();
  if (!cmd || SHELL_METACHARACTERS.test(cmd)) return false;
  if (gitDiffCarriesRefusedOption(cmd)) return false;
  return allowedPrefixes.some((p) => {
    if (typeof p !== "string") return false;
    const prefix = p.trim();
    return prefix.length > 0 && (cmd === prefix || cmd.startsWith(`${prefix} `));
  });
}

/** `Bash(<prefix>:*)` allow rules -> their prefixes (other entries are
 * ignored). Lets core/claude-args.js hand the guard exactly the commands
 * its `--allowedTools` pre-approves, from one list. */
function prefixesFromBashRules(rules) {
  const out = [];
  for (const rule of rules) {
    const m = typeof rule === "string" ? rule.match(/^Bash\((.+):\*\)$/) : null;
    if (m) out.push(m[1]);
  }
  return out;
}

module.exports = { SHELL_METACHARACTERS, isBashCommandAllowed, prefixesFromBashRules };
