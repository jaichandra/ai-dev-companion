# Rust port checklist

Generated from the Node source tree. `[x]` = ported with its tests passing.

Order: guards/policies, API clients and normalizers, job runner, history store, features, HTTP server, MCP, CLI/setup/doctor.

Cross-cutting decisions:

- Guards that Claude's hooks launch (`bash-guard`, `edit-guard`, `read-guard`, `mcp-guard`) become subcommands of the one binary (`<binary> guard <name> ...`); the Claude argument builder must emit that instead of `<node> '<script>'`.
- Packs register at compile time: the distribution crate depends on `ai-dev-companion` and passes its pack(s) to the framework's `main`.
- Same HTTP API on `127.0.0.1` so the Chrome extension is unchanged.

| Done | Node module | src lines | test lines | Rust |
|---|---|---|---|---|
| [x] | `features/analyze-issue/plan.js` | 990 | 641 | `crates/ai-dev-companion/src/features/analyze_issue/plan.rs` |
| [x] | `features/resolve-conflict/index.ts` | 966 | 0 | `crates/ai-dev-companion/src/features/resolve_conflict/index.rs` |
| [x] | `core/settings.js` | 825 | 827 | `crates/ai-dev-companion/src/settings.rs` |
| [x] | `features/address-review-comments/index.ts` | 766 | 0 | `crates/ai-dev-companion/src/features/address_review_comments/index.rs` |
| [x] | `features/analyze-issue/index.ts` | 747 | 0 | `crates/ai-dev-companion/src/features/analyze_issue/index.rs` |
| [x] | `features/ticket-to-pr/index.ts` | 672 | 0 | `crates/ai-dev-companion/src/features/ticket_to_pr/index.rs` |
| [x] | `core/mcp.ts` | 601 | 0 | `crates/ai-dev-companion/src/mcp.rs` |
| [x] | `core/jobs.ts` | 522 | 0 | `crates/ai-dev-companion/src/jobs.rs` |
| [x] | `features/address-review-comments/plan.js` | 477 | 528 | `crates/ai-dev-companion/src/features/address_review_comments/plan.rs` |
| [x] | `core/mcp-tools.js` | 469 | 366 | `crates/ai-dev-companion/src/mcp_tools.rs` |
| [x] | `core/updater.js` | 461 | 226 | `crates/ai-dev-companion/src/updater.rs` |
| [x] | `core/prereqs.js` | 455 | 349 | `crates/ai-dev-companion/src/prereqs.rs` |
| [x] | `features/review-in-editor/index.ts` | 448 | 0 | `crates/ai-dev-companion/src/features/review_in_editor/index.rs` |
| [x] | `core/bitbucket-normalize.js` | 448 | 510 | `crates/ai-dev-companion/src/bitbucket_normalize.rs` |
| [x] | `core/claude-args.js` | 443 | 744 | `crates/ai-dev-companion/src/claude_args.rs` |
| [x] | `core/cli-run.js` | 406 | 245 | `crates/ai-dev-companion/src/cli_run.rs` |
| [x] | `core/bitbucket.ts` | 404 | 0 | `crates/ai-dev-companion/src/bitbucket.rs` |
| [x] | `core/history-db.js` | 393 | 138 | `crates/ai-dev-companion/src/history_db.rs` |
| [x] | `features/ticket-workspace/index.ts` | 365 | 0 | `crates/ai-dev-companion/src/features/ticket_workspace/index.rs` |
| [x] | `core/job-files.js` | 359 | 404 | `crates/ai-dev-companion/src/job_files.rs` |
| [x] | `features/review-in-editor/plan.js` | 341 | 453 | `crates/ai-dev-companion/src/features/review_in_editor/plan.rs` |
| [x] | `core/watcher-runner.ts` | 338 | 373 | `crates/ai-dev-companion/src/watcher_runner.rs` |
| [x] | `core/review-session.js` | 330 | 239 | `crates/ai-dev-companion/src/review_session.rs` |
| [x] | `core/scheduler.ts` | 321 | 330 | `crates/ai-dev-companion/src/scheduler.rs` |
| [x] | `features/ticket-to-pr/plan.js` | 320 | 246 | `crates/ai-dev-companion/src/features/ticket_to_pr/plan.rs` |
| [x] | `core/claude.ts` | 317 | 0 | `crates/ai-dev-companion/src/claude.rs` |
| [x] | `core/history-record.js` | 294 | 273 | `crates/ai-dev-companion/src/history_record.rs` |
| [x] | `core/ticket-git.js` | 290 | 203 | `crates/ai-dev-companion/src/ticket_git.rs` |
| [x] | `core/terminal.ts` | 275 | 0 | `crates/ai-dev-companion/src/terminal.rs` |
| [x] | `core/packs.js` | 272 | 124 | `crates/ai-dev-companion/src/packs.rs` |
| [x] | `core/providers.ts` | 271 | 113 | `crates/ai-dev-companion/src/providers.rs` |
| [x] | `core/github-normalize.js` | 269 | 191 | `crates/ai-dev-companion/src/github_normalize.rs` |
| [x] | `core/schedule.js` | 262 | 223 | `crates/ai-dev-companion/src/schedule.rs` |
| [x] | `core/launchd.js` | 257 | 108 | `crates/ai-dev-companion/src/launchd.rs` |
| [x] | `core/notifications.js` | 255 | 264 | `crates/ai-dev-companion/src/notifications.rs` |
| [x] | `core/atlassian.ts` | 254 | 0 | `crates/ai-dev-companion/src/atlassian.rs` |
| [x] | `core/mcp-registration.js` | 252 | 408 | `crates/ai-dev-companion/src/mcp_registration.rs` |
| [x] | `core/worktree.ts` | 248 | 0 | `crates/ai-dev-companion/src/worktree.rs` |
| [x] | `features/ticket-workspace/plan.js` | 246 | 181 | `crates/ai-dev-companion/src/features/ticket_workspace/plan.rs` |
| [x] | `core/watchers.js` | 243 | 203 | `crates/ai-dev-companion/src/watchers.rs` |
| [x] | `core/similar.js` | 242 | 182 | `crates/ai-dev-companion/src/similar.rs` |
| [x] | `core/edit-path-policy.js` | 241 | 304 | `crates/ai-dev-companion/src/guard/path_policy.rs` |
| [x] | `core/credential-store.js` | 237 | 254 | `crates/ai-dev-companion/src/credential_store.rs` |
| [x] | `core/settings-background.js` | 233 | 89 | `crates/ai-dev-companion/src/settings_background.rs` |
| [x] | `core/mcp-stdio.js` | 232 | 199 | `crates/ai-dev-companion/src/mcp_stdio.rs` |
| [x] | `core/jenkins.ts` | 216 | 0 | `crates/ai-dev-companion/src/jenkins.rs` |
| [x] | `core/extension-manifest.js` | 215 | 145 | `crates/ai-dev-companion/src/extension_manifest.rs` |
| [x] | `core/diagnose-prompt.js` | 214 | 144 | `crates/ai-dev-companion/src/diagnose_prompt.rs` |
| [x] | `core/github.ts` | 210 | 320 | `crates/ai-dev-companion/src/github.rs` |
| [x] | `core/paths.js` | 207 | 195 | `crates/ai-dev-companion/src/paths.rs` |
| [x] | `features/diagnose-build/index.ts` | 197 | 0 | `crates/ai-dev-companion/src/features/diagnose_build/index.rs` |
| [x] | `core/history-schema.js` | 197 | 42 | `crates/ai-dev-companion/src/history_schema.rs` |
| [x] | `features/digest/index.ts` | 188 | 0 | `crates/ai-dev-companion/src/features/digest/index.rs` |
| [x] | `core/cli-format.js` | 188 | 71 | `crates/ai-dev-companion/src/cli_format.rs` |
| [x] | `features/ticket-to-pr/review-mover.ts` | 186 | 146 | `crates/ai-dev-companion/src/features/ticket_to_pr/review_mover.rs` |
| [x] | `core/repo-setup.ts` | 185 | 0 | `crates/ai-dev-companion/src/repo_setup.rs` |
| [x] | `core/editor.ts` | 181 | 0 | `crates/ai-dev-companion/src/editor.rs` |
| [x] | `core/feature-registry.js` | 178 | 188 | `crates/ai-dev-companion/src/feature_registry.rs` |
| [x] | `features/digest/plan.js` | 162 | 155 | `crates/ai-dev-companion/src/features/digest/plan.rs` |
| [x] | `core/integrity-baselines.js` | 161 | 246 | `crates/ai-dev-companion/src/integrity_baselines.rs` |
| [x] | `core/bitbucket-endpoints.js` | 159 | 0 | `crates/ai-dev-companion/src/bitbucket_endpoints.rs` |
| [x] | `core/open-location.ts` | 155 | 60 | `crates/ai-dev-companion/src/open_location.rs` |
| [x] | `core/bitbucket-contract.js` | 154 | 197 | `crates/ai-dev-companion/src/bitbucket_contract.rs` |
| [x] | `core/workspace-resolver.js` | 153 | 145 | `crates/ai-dev-companion/src/workspace_resolver.rs` |
| [x] | `core/background-wiring.ts` | 152 | 376 | `crates/ai-dev-companion/src/background_wiring.rs` |
| [x] | `core/github-endpoints.js` | 150 | 72 | `crates/ai-dev-companion/src/github_endpoints.rs` |
| [x] | `core/work-units.js` | 143 | 92 | `crates/ai-dev-companion/src/work_units.rs` |
| [x] | `features/summarize-comments/plan.js` | 143 | 87 | `crates/ai-dev-companion/src/features/summarize_comments/plan.rs` |
| [x] | `core/exec.ts` | 142 | 0 | `crates/ai-dev-companion/src/exec.rs` |
| [x] | `core/scope-key.js` | 140 | 179 | `crates/ai-dev-companion/src/scope_key.rs` |
| [x] | `core/review-worktree.js` | 140 | 115 | `crates/ai-dev-companion/src/review_worktree.rs` |
| [x] | `features/summarize-comments/index.ts` | 137 | 0 | `crates/ai-dev-companion/src/features/summarize_comments/index.rs` |
| [x] | `core/worktree-report.js` | 136 | 131 | `crates/ai-dev-companion/src/worktree_report.rs` |
| [x] | `core/git-hook.js` | 130 | 143 | `crates/ai-dev-companion/src/git_hook.rs` |
| [x] | `core/data-folder.js` | 130 | 72 | `crates/ai-dev-companion/src/data_folder.rs` |
| [x] | `core/auth-context.ts` | 129 | 0 | `crates/ai-dev-companion/src/auth_context.rs` |
| [x] | `core/risk-facts.js` | 129 | 123 | `crates/ai-dev-companion/src/risk_facts.rs` |
| [x] | `features/create-jira-subtasks/index.ts` | 128 | 0 | `crates/ai-dev-companion/src/features/create_jira_subtasks/index.rs` |
| [x] | `core/token-cache.js` | 128 | 187 | `crates/ai-dev-companion/src/token_cache.rs` |
| [x] | `features/resolve-conflict/plan.js` | 124 | 148 | `crates/ai-dev-companion/src/features/resolve_conflict/plan.rs` |
| [x] | `core/jenkins-testreport.js` | 114 | 0 | `crates/ai-dev-companion/src/jenkins_testreport.rs` |
| [x] | `core/analysis-checkout.js` | 114 | 121 | `crates/ai-dev-companion/src/analysis_checkout.rs` |
| [x] | `core/location-resolve.js` | 114 | 107 | `crates/ai-dev-companion/src/location_resolve.rs` |
| [x] | `core/service-control.js` | 110 | 0 | `crates/ai-dev-companion/src/service_control.rs` |
| [x] | `core/history.ts` | 110 | 0 | `crates/ai-dev-companion/src/history_db.rs` |
| [x] | `core/claude-stream.js` | 110 | 75 | `crates/ai-dev-companion/src/claude_stream.rs` |
| [x] | `core/terminal-focus.js` | 110 | 68 | `crates/ai-dev-companion/src/terminal_focus.rs` |
| [x] | `core/similar-search.js` | 109 | 162 | `crates/ai-dev-companion/src/similar_search.rs` |
| [x] | `core/jira.ts` | 107 | 0 | `crates/ai-dev-companion/src/jira.rs` |
| [x] | `core/edit-guard.js` | 107 | 120 | `crates/ai-dev-companion/src/guard/edit_guard.rs` |
| [x] | `core/push-risk.js` | 107 | 102 | `crates/ai-dev-companion/src/push_risk.rs` |
| [x] | `core/targets.js` | 95 | 42 | `crates/ai-dev-companion/src/targets.rs` |
| [x] | `core/credentials.ts` | 93 | 0 | `crates/ai-dev-companion/src/credentials.rs` |
| [x] | `core/external-tokens.js` | 92 | 66 | `crates/ai-dev-companion/src/external_tokens.rs` |
| [x] | `core/mcp-guard.js` | 91 | 75 | `crates/ai-dev-companion/src/guard/mcp_guard.rs` |
| [x] | `core/jira-workflow.js` | 91 | 57 | `crates/ai-dev-companion/src/jira_workflow.rs` |
| [x] | `core/location-parse.js` | 90 | 68 | `crates/ai-dev-companion/src/location_parse.rs` |
| [x] | `core/plugin-info.js` | 89 | 0 | `crates/ai-dev-companion/src/plugin_info.rs` |
| [x] | `core/repo-choice.ts` | 85 | 0 | `crates/ai-dev-companion/src/repo_choice.rs` |
| [x] | `core/pr-status.js` | 85 | 131 | `crates/ai-dev-companion/src/pr_status.rs` |
| [x] | `core/resume.js` | 84 | 73 | `crates/ai-dev-companion/src/resume.rs` |
| [ ] | `core/with-git-provider.js` | 82 | 0 |  |
| [x] | `features/analyze-issue/feature.js` | 81 | 0 | `crates/ai-dev-companion/src/features/analyze_issue/feature.rs` |
| [x] | `core/environment-base.js` | 80 | 0 | `crates/ai-dev-companion/src/environment_base.rs` |
| [x] | `core/reviewed-push.ts` | 79 | 0 | `crates/ai-dev-companion/src/reviewed_push.rs` |
| [x] | `core/update-runner.js` | 77 | 0 | `crates/ai-dev-companion/src/update_runner.rs` |
| [x] | `core/embed-task.js` | 75 | 149 | `crates/ai-dev-companion/src/embed_task.rs` |
| [x] | `core/mcp-tool-classifier.js` | 75 | 84 | `crates/ai-dev-companion/src/guard/mcp_classifier.rs` |
| [x] | `core/read-guard.js` | 74 | 76 | `crates/ai-dev-companion/src/guard/read_guard.rs` |
| [x] | `core/bash-command-policy.js` | 73 | 121 | `crates/ai-dev-companion/src/guard/bash_policy.rs` |
| [x] | `core/bash-guard.js` | 72 | 56 | `crates/ai-dev-companion/src/guard/bash_guard.rs` |
| [x] | `core/background-routes.js` | 70 | 127 | `crates/ai-dev-companion/src/background_routes.rs` |
| [x] | `core/worktree-integrity.js` | 69 | 69 | `crates/ai-dev-companion/src/worktree_integrity.rs` |
| [x] | `core/session-vault.js` | 65 | 209 | `crates/ai-dev-companion/src/session_vault.rs` |
| [x] | `features/create-jira-subtasks/plan.js` | 62 | 70 | `crates/ai-dev-companion/src/features/create_jira_subtasks/plan.rs` |
| [x] | `core/resume-session.ts` | 62 | 49 | `crates/ai-dev-companion/src/resume_session.rs` |
| [x] | `core/mcp-client.js` | 60 | 55 | `crates/ai-dev-companion/src/mcp_client.rs` |
| [x] | `core/site-urls.js` | 60 | 44 | `crates/ai-dev-companion/src/site_urls.rs` |
| [x] | `core/setup-helpers.js` | 53 | 0 | `crates/ai-dev-companion/src/setup_helpers.rs` |
| [x] | `core/cursor-agent.ts` | 51 | 0 | `crates/ai-dev-companion/src/cursor_agent.rs` |
| [x] | `core/mcp-auth.js` | 51 | 83 | `crates/ai-dev-companion/src/mcp_auth.rs` |
| [x] | `core/session-cwd.js` | 51 | 60 | `crates/ai-dev-companion/src/session_cwd.rs` |
| [x] | `core/mcp-memory-tools.js` | 47 | 77 | `crates/ai-dev-companion/src/mcp_memory_tools.rs` |
| [x] | `features/review-in-editor/feature.js` | 46 | 0 | `crates/ai-dev-companion/src/features/review_in_editor/feature.rs` |
| [x] | `core/review-memory.js` | 46 | 33 | `crates/ai-dev-companion/src/review_memory.rs` |
| [x] | `core/feature-actions.ts` | 45 | 54 | `crates/ai-dev-companion/src/feature_actions.rs` |
| [x] | `features/address-review-comments/feature.js` | 44 | 0 | `crates/ai-dev-companion/src/features/address_review_comments/feature.rs` |
| [x] | `core/main-clone-choice.ts` | 41 | 0 | `crates/ai-dev-companion/src/main_clone_choice.rs` |
| [x] | `features/ticket-workspace/feature.js` | 39 | 0 | `crates/ai-dev-companion/src/features/ticket_workspace/feature.rs` |
| [x] | `core/mcp-background-tools.js` | 39 | 63 | `crates/ai-dev-companion/src/mcp_background_tools.rs` |
| [x] | `features/resolve-conflict/feature.js` | 38 | 0 | `crates/ai-dev-companion/src/features/resolve_conflict/feature.rs` |
| [x] | `core/worktree-registry.js` | 38 | 34 | `crates/ai-dev-companion/src/worktree_registry.rs` |
| [x] | `core/work-unit-record.js` | 36 | 43 | `crates/ai-dev-companion/src/work_unit_record.rs` |
| [x] | `core/cli-args.js` | 35 | 30 | `crates/ai-dev-companion/src/cli_args.rs` |
| [x] | `features/digest/feature.js` | 34 | 0 | `crates/ai-dev-companion/src/features/digest/feature.rs` |
| [x] | `core/process-group.js` | 34 | 32 | `crates/ai-dev-companion/src/process_group.rs` |
| [x] | `features/create-jira-subtasks/feature.js` | 33 | 0 | `crates/ai-dev-companion/src/features/create_jira_subtasks/feature.rs` |
| [x] | `core/line-splitter.js` | 33 | 24 | `crates/ai-dev-companion/src/line_splitter.rs` |
| [x] | `core/editor-url.js` | 30 | 29 | `crates/ai-dev-companion/src/editor_url.rs` |
| [x] | `features/resolve-conflict/prompt.ts` | 27 | 0 | `crates/ai-dev-companion/src/features/resolve_conflict/prompt.rs` |
| [x] | `features/summarize-comments/feature.js` | 25 | 0 | `crates/ai-dev-companion/src/features/summarize_comments/feature.rs` |
| [x] | `core/link-guard.js` | 25 | 29 | `crates/ai-dev-companion/src/guard/link_guard.rs` |
| [x] | `features/ticket-to-pr/feature.js` | 23 | 0 | `crates/ai-dev-companion/src/features/ticket_to_pr/feature.rs` |
| [x] | `core/route-policy.js` | 23 | 48 | `crates/ai-dev-companion/src/route_policy.rs` |
| [x] | `core/worktree-prune.js` | 22 | 0 | `crates/ai-dev-companion/src/worktree_prune.rs` |
| [x] | `features/diagnose-build/feature.js` | 21 | 0 | `crates/ai-dev-companion/src/features/diagnose_build/feature.rs` |
| [x] | `core/reviewed-push-policy.js` | 21 | 0 | `crates/ai-dev-companion/src/reviewed_push_policy.rs` |
| [x] | `core/diff.ts` | 20 | 0 | `crates/ai-dev-companion/src/diff.rs` |
| [x] | `core/cli-link.js` | 20 | 33 | `crates/ai-dev-companion/src/cli_link.rs` |
| [x] | `core/cursor-deeplink.js` | 20 | 17 | `crates/ai-dev-companion/src/cursor_deeplink.rs` |
| [x] | `packs/builtin.js` | 19 | 0 | `crates/ai-dev-companion/src/packs/builtin.rs` |
| [x] | `core/host-guard.js` | 19 | 43 | `crates/ai-dev-companion/src/host_guard.rs` |
| [x] | `core/permission-mode.js` | 15 | 17 | `crates/ai-dev-companion/src/permission_mode.rs` |
| [x] | `core/app-slug.js` | 11 | 0 | `crates/ai-dev-companion/src/app_identity.rs` |
| [x] | `core/product-name.js` | 9 | 0 | `crates/ai-dev-companion/src/app_identity.rs` |

Partially ported: `core/prereqs.js` (`is_safe_repo_segment` only), `core/js_text` is new (JS-compatible trim/whitespace).

| [x] | `environment.js` | 56 | 35 | `crates/ai-dev-companion/src/environment.rs` |

Not yet in this table: `server.ts`, `config.ts`, `setup.js`, `doctor.js`, `service.js`, `bin/`, `provision-review-worktrees.js` (top-level entry points).


## Deferred checks

- `github-normalize.test.js` has two tests that call `features/address-review-comments/plan.js` (`pullRequestRepoError`, `collectOpenComments`). Add them to `github_normalize.rs` when that plan module is ported.
- Fixtures now live in `crates/ai-dev-companion/tests/fixtures/` (copied from `companion-service/core/fixtures/`), with `golden*.json` files generated from the Node implementation as the behavioral reference.
- `url-parser-twins.test.js` (extension JS parser vs service parser) lives on as the Rust test `diagnose_prompt::tests::twin_parsers_agree_on_one_table`, which runs the extension's `jenkinsBuildFromUrl` through `node` (skipped when node isn't installed).
- `claude-args`: the guard hooks now run `'<binary>' guard <name> [arg]` (see `guard::cli`) instead of `'<node>' '<script>' [arg]`, so `guardScriptPath`/`editGuardScriptPath`/`readGuardScriptPath`/`bashGuardScriptPath`/`execPath` are replaced by one `guard_binary`. `companionServiceDir()` is not ported (it resolved the Node install layout); `protected_path_rules` takes the companion dir as an argument. A golden file generated from the Node implementation pins the argv, flag order and `--settings` shape.
- `mcp-background-tools.test.js`: its first two tests now live in `mcp_tools`'s tests.
- `background-routes`: `used_event` is injected; the tests use a stand-in. Wire the real `history-record` `usedEvent` when that module lands. The routes return `(status, JSON)`; the web layer (axum) adapts them.
- `service-control`: `buildServer`/`COMPANION_SERVICE_DIR` are dropped (nothing to compile); `host-guard` is `check_host`, called first by the web layer.
- `summarize-comments` cache: the Node `companionServiceDir()` default is gone; callers pass the directory that holds `config.json` (the Rust service resolves it once, in `paths`, when that is ported).
- `environment`: a distribution's `main` calls `environment::init(...)` first; everything else reads `environment::current()` (the framework placeholder when nothing was initialised). `environment.test.js`'s last test (consumers see the profile's values: `updater.DEFAULT_SOURCE`, `targets.DEFAULT_PROJECTS`) is added as those modules are ported.
- `packs`: generic over the feature-factory type `F` (the job layer defines the real one). The Node tests that load the builtin pack (factories load, scope keys per builtin feature, every feature's extension script exists) return when the builtin pack is built in Rust. `packs/builtin.js` is that pack. `definePack`'s "must be a function / boolean" checks are enforced by the types instead.
- `prereqs`: `checkNodeVersion`/`nodeMeetsMinimum` are dropped (no Node). The git provider's naming rules are an explicit `GitRemoteRules` value (the Node tests flipped a global with `with-git-provider.js`); `git_remote_rules()` reads the active profile. `golden.json` (from the Node implementation) pins every origin/URL/config parse.
- `credential-store`: byte-compatible with the Node build. A fixture written by Node (`tests/fixtures/credentials/`, test-only throwaway key and fake tokens) must decrypt here, and a test has Node read a file written here. `credentials.ts` (the wiring that picks the real paths and builds the token cache) comes with the server.

- llm: the client lives in the distribution, behind `llm::LlmBackend`.
- history: `history_record::History` is the trait the sqlite store implements; `used_event` still has to be passed into `BackgroundRoutes` at server composition. `job_files` takes the not-persisted ids from `PackRegistry::not_persisted_ids`.

- history-db: `rusqlite` (bundled SQLite with FTS5). `HistoryDb` implements `history_record::History`; its `forget`/`prune` return `(items, events)` tuples (the JSON shape is built by the route). `save_vector` takes `item_updated_at: i64` (the Node NaN check can't happen). The "broken store" sub-case of similar-search's test is not ported (the store is a concrete type). `similar_search::Embedder` is implemented over the distribution's `llm::LlmBackend`.

- launchd: the job runs `<binary> serve` (no Node path, no `NODE_USE_SYSTEM_CA`); log files go to a directory the caller names. `extension_manifest` leaves out `defaultExtensionDir` (the distribution knows where its extension folder is) and the Node-only "extension scripts compile / register their feature" tests (they need the builtin pack and a JS engine); the "committed manifest equals generated" check returns once the builtin pack is ported.

- ticket-to-pr plan: `chooseStartFixRepo` now lives in `features/ticket_to_pr/plan.rs` on top of the analyze-issue plan. Not ported from analyze-issue/plan.js: `MUTATING_MCP_TOOLS` (a pack query, `disallowed_mcp_tools`), `companionServiceDir` and the default cache root (callers pass the folder that holds config.json), `index.ts`'s source tripwire test.

| [x] | `doctor.js` | 825 | 173 | `crates/ai-dev-companion/src/doctor.rs` |
| [x] | `setup.js` (wizard steps; process-side steps live in the distribution main) | 817 | 0 | `crates/ai-dev-companion/src/setup.rs` |
| [x] | `server.ts` (HTTP layer; startup half still to do in main) | 1595 | 0 | `crates/ai-dev-companion/src/server.rs` |
| [x] | `service.js` | 0 | 0 | `crates/ai-dev-companion/src/service.rs` |
| [x] | `bin/companion.js` | 153 | 0 | `crates/ai-dev-companion/src/companion_cli.rs` |
| [x] | `provision-review-worktrees.js` | 0 | 0 | `crates/ai-dev-companion/src/provision_review_worktrees.rs` |
