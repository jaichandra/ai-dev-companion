# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

`ai-dev-companion`: the generic framework behind a Chrome extension plus a local "companion service" (Rust) that add one-click AI actions to git-host, issue-tracker and CI pages. It has no knowledge of any one organization: a **distribution** (a separate repo) supplies an environment profile and optional packs. The distribution crate depends on this one and builds the binary. See `README.md`. The previous Node implementation is on the `nodejs` branch.

## Commands

- `scripts/check.sh` (clippy with `-D warnings`, then `cargo test`) is the gate before every commit. `cargo test -p ai-dev-companion <filter>` runs a subset.
- A fresh clone needs `git clone --recurse-submodules` (or `git submodule update --init` after a plain clone) to fill `mods/run-pane/`. The crate builds and its tests pass without it; only a distribution install that copies `mods/` needs it (the binary starts sessions without the mod when the folder is empty).
- The library has no real `main`: `src/main.rs` only runs the `guard` subcommand, so the guards can be tested as the child processes Claude's hooks start. A distribution crate has the real binary (`startup::run_service`, `companion_cli::run_cli`).
- Tests use fakes for everything outside the process (`FakeClaude`, fake `HttpTransport`, injected `Git`, tempdir homes). Never let a test auto-detect an editor or launch a terminal/`osascript`/`launchctl`.

## Architecture

- `chrome-extension/` (JavaScript): one file per feature in `features/`, each calling `PaiRegistry.register(entry)`. Its `manifest.json` is generated from the profile and packs by `extension_manifest.rs`.
- `crates/ai-dev-companion/src/`: `server.rs` (axum routes, host guard, shared secret), `startup.rs` (builds and runs the service), `mcp.rs` (`POST /mcp`), `packs.rs` + `packs/builtin.rs` (the pack contract and the generic features), `features/<id>/{plan,index,feature}.rs` (pure planning, the job runner, the declaration), `providers.rs` (`GitHost`, `IssueTracker`, `CiServer` and their Bitbucket, GitHub, Jira and Jenkins implementations), `claude*.rs` and `guard/` (the Claude runner and its guards), `jobs.rs`/`job_files.rs`, `history_*.rs`, `scheduler.rs`/`watchers.rs`, `settings*.rs`, `doctor.rs`, `setup.rs`, `companion_cli.rs`, `updater.rs`.
- Git-host differences live in `prereqs.rs` (`GitRemoteRules`: origin parsing, clone URLs, PR page paths, PR keys).
- `mods/run-pane/`: a git submodule, the Claude Code mod in its own repo (`github.com/jaichandra/run-pane`; its README, tests and marketplace file live there; `claude plugin validate|test mods/run-pane`): the Run pane (`/dev`, `run_output` tool) and `/shell`, which the mod opens itself (iTerm, else Terminal.app, through `osascript`), so it needs no companion binary and works in any Claude Code install. A distribution can lay an optional `hooks/run-pane.json` (`suggestions`, `openFor`) over the mod's folder; the mod has none of its own. Change the mod in its own repo, push it, then bump the gitlink here (`git -C mods/run-pane pull`, `git add mods/run-pane`); push the mod before the commit that bumps it, or a clone cannot fetch that commit. Before pushing this repo, check that the gitlink (`git submodule status`) is the tip of the mod's remote `main` (`git ls-remote https://github.com/jaichandra/run-pane.git main` prints the same SHA): if the remote lacks that commit, every cargo operation in a distribution that depends on this repo (`update`, `build`, `metadata`, `install`, `release publish`) fails with `failed to fetch submodule`. Keep the mod's repo public, because cargo fetches it without credentials. Mods run in the engine's own sandboxed environment, so `$` is only passed to top-level functions of the module.
- `plugin/`: the Claude Code plugin template (MCP bridge and session-start hook run the installed binary's `mcp-stdio` and `inbox --brief`; skills).

## Design decisions to respect

- Conflict resolution merges the target branch in (never rebases) so the push is a plain, non-force `git push`. Approve & Push refuses if the worktree fingerprint differs from what the panel rendered.
- Claude jobs that edit reuse an existing clean linked worktree of the branch if there is one (`core/worktree.ts`'s `acquireWorktree`; rejecting the job restores it, approving leaves it, and it is never deleted), and otherwise run in a disposable worktree. They never run in the main clone (Claude's sandbox may write the worktree's git dir, which for the clone is the whole `.git`) or in a checkout with uncommitted work or unpushed commits. Review PR only reads, so it may use the clone. Analysis is read-only.
- Keep organization-specific names (hosts, job names, internal tools) out of this repo; they belong in a distribution's profile or pack.

## Docs

`docs/DESIGN.md` (~2,600 lines) is the design: read the relevant section before changing a subsystem. It is written against the placeholder profile; keep organization-specific facts (hosts, internal tools, instance versions that only one deployment has) in the distribution's own docs.

## Tool output

Keep output small: run tests quiet and show failures only; use ranged reads for big files.
