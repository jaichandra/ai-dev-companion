//! The features that ship with the framework (port of `packs/builtin.js`), and the way a
//! distribution's `main` builds each of them from one set of shared services. A distribution that
//! wants a different set (or its own features) builds other packs and names them in its profile.

use crate::claude::ClaudeRunner;
use crate::config::ConfigFn;
use crate::editor::Editors;
use crate::features::address_review_comments::index::{create_address_review_comments_feature, AddressDeps, HostReviewApi};
use crate::features::analyze_issue::index::{create_analyze_issue_feature, AnalyzeIssueDeps, GitCheckouts, ProviderIssues, SaveComponent};
use crate::features::create_jira_subtasks::index::{create_jira_subtasks_feature, TrackerSubtasks};
use crate::features::declare::SetupContext;
use crate::features::diagnose_build::index::{create_diagnose_build_feature, CiBuilds, DiagnoseBuildDeps, FactsSource};
use crate::features::digest::index::{create_digest_feature, ProviderDigestIo};
use crate::features::resolve_conflict::index::{create_resolve_conflict_feature, ResolveConflictDeps};
use crate::features::review_in_editor::index::{create_review_in_editor_feature, ReviewDeps};
use crate::features::summarize_comments::index::{create_summarize_comments_feature, TrackerComments};
use crate::features::ticket_to_pr::index::{create_ticket_to_pr_feature, ProviderApi, RealStartFixIo, TicketToPrDeps};
use crate::features::ticket_to_pr::review_mover::create_review_mover;
use crate::features::ticket_workspace::index::{create_ticket_workspace_feature, SystemWorkspaceIo, TicketWorkspaceDeps, WorkspaceHistory};
use crate::features::{address_review_comments, analyze_issue, create_jira_subtasks, diagnose_build, digest, resolve_conflict, review_in_editor, summarize_comments, ticket_to_pr, ticket_workspace};
use crate::git_cli::SystemGit;
use crate::history_db::HistoryDb;
use crate::integrity_baselines::BaselineStore;
use crate::jobs::{Feature, FindSimilar, JobStore};
use crate::packs::{define_pack, Diagnose, Pack, ServerHint};
use crate::prereqs::GitRemoteRules;
use crate::providers::Providers;
use crate::repo_choice::RepoChoices;
use crate::terminal::Terminal;
use crate::work_units::WorkUnits;
use std::path::PathBuf;
use std::sync::Arc;

/// "PROJECT", "repo" -> the local clone, or why there is none (`Config::repo_path` over the home).
pub type RepoResolver = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// Everything the builtin features are built from. A distribution `main` fills it once and every
/// factory takes what it needs; nothing is a global.
#[derive(Clone)]
pub struct FeatureServices {
    /// The real HTTP stack the providers were built from (the sign-in chain, the token lookup and the
    /// transport): a distribution's own clients, such as its own Jenkins build lists, are built from
    /// it. `None` when the providers were replaced (tests).
    pub http_deps: Option<crate::providers::Deps<crate::http::ReqwestTransport>>,
    pub store: Arc<JobStore>,
    pub providers: Providers,
    pub claude: Arc<dyn ClaudeRunner>,
    pub config: ConfigFn,
    pub home: PathBuf,
    /// The folder that holds config.json: saved analyses and summaries live under it.
    pub cache_root: PathBuf,
    pub rules: GitRemoteRules,
    pub work_units: Arc<WorkUnits>,
    pub choices: Arc<RepoChoices>,
    pub baselines: Arc<BaselineStore>,
    pub repo_path: RepoResolver,
    /// The local history, or `None` when it is off.
    pub history: Option<Arc<HistoryDb>>,
    pub find_similar: Option<Arc<dyn FindSimilar>>,
    pub save_component: SaveComponent,
    /// The shared risk-facts feed (diagnose-build's flaky lines).
    pub facts: Arc<dyn FactsSource>,
    /// The pack's richer diagnose command, asked at run time.
    pub richer_diagnose: Arc<dyn Fn() -> Option<Diagnose> + Send + Sync>,
    /// The packs' MCP deny list and server hints (what a read-only analysis may not call / should prefer).
    pub deny_list: Vec<String>,
    pub server_hints: Vec<ServerHint>,
    /// The user's read-only MCP servers (`analyze_issue::index::mcp_servers_from(~/.claude.json)`).
    pub mcp_servers: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    pub terminal: Terminal,
    pub editors: Editors,
    /// Where review-in-editor's status files and per-worktree records live.
    pub sessions_dir: PathBuf,
    /// Where the remembered reviews live.
    pub memory_dir: PathBuf,
    /// The profile's Claude Code permission mode for a feature id (`permission_mode_for_current`).
    pub permission_mode: Arc<dyn Fn(&str) -> String + Send + Sync>,
    pub osascript: String,
    pub cursor_command: String,
}

/// How a feature is built from the shared services.
pub type FeatureFactory = Arc<dyn Fn(&FeatureServices) -> Arc<dyn Feature> + Send + Sync>;

fn factory(f: impl Fn(&FeatureServices) -> Arc<dyn Feature> + Send + Sync + 'static) -> FeatureFactory {
    Arc::new(f)
}

fn resolve_conflict_factory() -> FeatureFactory {
    factory(|s| {
        create_resolve_conflict_feature(ResolveConflictDeps {
            store: s.store.clone(),
            claude: s.claude.clone(),
            repo_choices: s.choices.clone(),
            baselines: s.baselines.clone(),
            work_units: s.work_units.clone(),
            repo_path: s.repo_path.clone(),
            home: s.home.clone(),
            rules: s.rules,
        })
    })
}

fn create_jira_subtasks_factory() -> FeatureFactory {
    factory(|s| create_jira_subtasks_feature(s.store.clone(), Arc::new(TrackerSubtasks(s.providers.issues.clone()))))
}

fn review_in_editor_factory() -> FeatureFactory {
    factory(|s| {
        create_review_in_editor_feature(ReviewDeps {
            store: s.store.clone(),
            git: SystemGit,
            config: s.config.clone(),
            repo_path: s.repo_path.clone(),
            home: s.home.clone(),
            sessions_dir: s.sessions_dir.clone(),
            memory_dir: s.memory_dir.clone(),
            work_units: s.work_units.clone(),
            rules: s.rules,
            editors: s.editors.clone(),
            permission_mode: (s.permission_mode)("review-in-editor"),
            osascript: s.osascript.clone(),
        })
    })
}

fn diagnose_build_factory() -> FeatureFactory {
    factory(|s| {
        create_diagnose_build_feature(DiagnoseBuildDeps {
            store: s.store.clone(),
            ci: Arc::new(CiBuilds(s.providers.ci.clone())),
            claude: s.claude.clone(),
            facts: s.facts.clone(),
            config: s.config.clone(),
            home: s.home.clone(),
            richer: s.richer_diagnose.clone(),
            deny_list: s.deny_list.clone(),
            mcp_servers: s.mcp_servers.clone(),
        })
    })
}

fn analyze_issue_factory() -> FeatureFactory {
    factory(|s| {
        create_analyze_issue_feature(AnalyzeIssueDeps {
            store: s.store.clone(),
            choices: s.choices.clone(),
            api: Arc::new(ProviderIssues(s.providers.clone())),
            claude: s.claude.clone(),
            checkouts: Arc::new(GitCheckouts),
            config: s.config.clone(),
            save_component: s.save_component.clone(),
            find_similar: s.find_similar.clone(),
            home: s.home.clone(),
            cache_root: s.cache_root.clone(),
            rules: s.rules,
            deny_list: s.deny_list.clone(),
            server_hints: s.server_hints.clone(),
            mcp_servers: s.mcp_servers.clone(),
        })
    })
}

fn ticket_workspace_factory() -> FeatureFactory {
    factory(|s| {
        let history: Option<Arc<dyn WorkspaceHistory>> = s.history.clone().map(|h| h as Arc<dyn WorkspaceHistory>);
        create_ticket_workspace_feature(TicketWorkspaceDeps {
            store: s.store.clone(),
            config: s.config.clone(),
            home: s.home.clone(),
            rules: s.rules,
            history,
            io: Arc::new(SystemWorkspaceIo { providers: s.providers.clone(), home: s.home.clone() }),
            editors: s.editors.clone(),
            terminal: s.terminal.clone(),
        })
    })
}

fn ticket_to_pr_factory() -> FeatureFactory {
    factory(|s| {
        let api = ProviderApi::new(&s.providers);
        let git = s.providers.git.clone();
        let mover = create_review_mover(s.store.clone(), s.config.clone(), Arc::new(api.clone()), Arc::new(move || git.base_url().map_err(|e| e.to_string())), s.rules);
        let history: Option<Arc<dyn WorkspaceHistory>> = s.history.clone().map(|h| h as Arc<dyn WorkspaceHistory>);
        create_ticket_to_pr_feature(TicketToPrDeps {
            store: s.store.clone(),
            config: s.config.clone(),
            git: SystemGit,
            home: s.home.clone(),
            api: Arc::new(api.clone()),
            io: Arc::new(RealStartFixIo { api, terminal: s.terminal.clone(), choices: s.choices.clone(), analysis_root: s.cache_root.clone(), home: s.home.clone() }),
            mover,
            choices: s.choices.clone(),
            claude: s.claude.clone(),
            work_units: s.work_units.clone(),
            history,
            terminal: s.terminal.clone(),
            editors: s.editors.clone(),
            rules: s.rules,
            permission_mode: (s.permission_mode)("ticket-to-pr"),
        })
    })
}

fn summarize_comments_factory() -> FeatureFactory {
    factory(|s| create_summarize_comments_feature(s.store.clone(), Arc::new(TrackerComments(s.providers.issues.clone())), s.claude.clone(), s.config.clone(), s.cache_root.clone()))
}

fn digest_factory() -> FeatureFactory {
    factory(|s| create_digest_feature(s.config.clone(), s.store.clone(), Arc::new(ProviderDigestIo { providers: s.providers.clone(), jobs: s.store.clone() })))
}

fn address_review_comments_factory() -> FeatureFactory {
    factory(|s| {
        create_address_review_comments_feature(AddressDeps {
            store: s.store.clone(),
            git: SystemGit,
            api: Arc::new(HostReviewApi(s.providers.git.clone())),
            claude: s.claude.clone(),
            config: s.config.clone(),
            repo_path: s.repo_path.clone(),
            home: s.home.clone(),
            choices: s.choices.clone(),
            baselines: s.baselines.clone(),
            work_units: s.work_units.clone(),
            rules: s.rules,
            cursor_command: s.cursor_command.clone(),
        })
    })
}

/// The builtin pack: the generic features, in the order the Node pack lists them. `setup` carries
/// what the features' setup prompts need (a pack's borrowed-token detector and its label).
pub fn pack(setup: &SetupContext) -> Result<Pack<FeatureFactory>, String> {
    define_pack(Pack::new(
        "builtin",
        vec![
            resolve_conflict::feature::feature_spec(resolve_conflict_factory(), setup),
            create_jira_subtasks::feature::feature_spec(create_jira_subtasks_factory(), setup),
            review_in_editor::feature::feature_spec(review_in_editor_factory(), setup),
            diagnose_build::feature::feature_spec(diagnose_build_factory(), setup),
            analyze_issue::feature::feature_spec(analyze_issue_factory(), setup),
            ticket_workspace::feature::feature_spec(ticket_workspace_factory(), setup),
            ticket_to_pr::feature::feature_spec(ticket_to_pr_factory(), setup),
            summarize_comments::feature::feature_spec(summarize_comments_factory(), setup),
            digest::feature::feature_spec(digest_factory(), setup),
            address_review_comments::feature::feature_spec(address_review_comments_factory(), setup),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment;
    use crate::extension_manifest::{build_manifest, feature_scripts, render};
    use crate::packs::PackRegistry;
    use serde_json::{json, Value};
    use std::path::Path;

    fn registry() -> PackRegistry<FeatureFactory> {
        PackRegistry::new(vec![pack(&SetupContext::default()).unwrap()]).unwrap()
    }

    fn extension_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("chrome-extension")
    }

    const IDS: [&str; 10] = ["resolve-conflict", "create-jira-subtasks", "review-in-editor", "diagnose-build", "analyze-issue", "ticket-workspace", "ticket-to-pr", "summarize-comments", "digest", "address-review-comments"];

    #[test]
    fn the_builtin_pack_lists_the_ten_features_in_the_node_order() {
        let r = registry();
        assert_eq!(r.features().map(|f| f.descriptor.id.as_str()).collect::<Vec<_>>(), IDS);
        assert_eq!(r.packs()[0].id, "builtin");
    }

    #[test]
    fn the_menu_order_matches_the_node_declarations() {
        let r = registry();
        let order = |id: &str| r.feature_spec(id).unwrap().extension.as_ref().unwrap().order.unwrap();
        let got: Vec<(&str, f64)> = IDS.iter().map(|id| (*id, order(id))).collect();
        assert_eq!(
            got,
            [("resolve-conflict", 10.0), ("create-jira-subtasks", 30.0), ("review-in-editor", 50.0), ("diagnose-build", 40.0), ("analyze-issue", 70.0), ("ticket-workspace", 80.0), ("ticket-to-pr", 90.0), ("summarize-comments", 100.0), ("digest", 110.0), ("address-review-comments", 20.0)]
        );
    }

    #[test]
    fn flags_scope_keys_and_tools_follow_the_node_declarations() {
        let r = registry();
        assert_eq!(r.not_persisted_ids(), ["review-in-editor", "ticket-workspace", "digest"]);
        assert_eq!(r.ids_with_history_flag(|h| h.read_only), ["analyze-issue", "ticket-workspace", "digest"]);
        assert_eq!(r.ids_with_history_flag(|h| h.event_only), ["ticket-workspace", "digest"]);
        assert_eq!(r.ids_with_history_flag(|h| h.milestone), ["ticket-to-pr"]);
        let tools: Vec<&str> = r.mcp_tool_defs().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tools, ["start_resolve_conflict", "get_issue_analysis", "analyze_issue", "ticket_workspace", "get_digest", "start_address_review_comments"]);
        let kinds = |name: &str| r.mcp_tool_defs().iter().find(|t| t.name == name).unwrap().def["kind"].clone();
        assert_eq!((kinds("analyze_issue"), kinds("get_digest"), kinds("start_resolve_conflict")), (json!("start"), json!("read"), json!("pending-start")));
        let pr = json!({ "project": "A", "repo": "b", "prId": 4 });
        let kind_rules = |_: crate::packs::ScopeKeyKind, p: &Value| Some(format!("{}/{}#{}", p["project"].as_str()?, p["repo"].as_str()?, p["prId"]));
        assert!(r.scope_key_for("resolve-conflict", &pr, &kind_rules).as_deref() == Some("A/b#4"));
        assert!(r.scope_key_for("digest", &pr, &kind_rules).is_none());
    }

    #[test]
    fn every_script_a_pack_feature_names_exists_and_registers_that_feature_once() {
        let dir = extension_dir();
        for spec in registry().features() {
            let id = &spec.descriptor.id;
            let script = &spec.extension.as_ref().unwrap().script;
            let source = std::fs::read_to_string(dir.join(script)).unwrap_or_else(|_| panic!("{script} is missing"));
            assert_eq!(source.matches("PaiRegistry.register({").count(), 1, "{script} must register exactly once");
            assert!(source.lines().any(|l| l.starts_with(&format!("  id: \"{id}\","))), "{script} doesn't register \"{id}\"");
        }
    }

    #[test]
    fn the_committed_manifest_equals_the_one_generated_for_the_builtin_pack() {
        let on_disk = std::fs::read_to_string(extension_dir().join("manifest.json")).unwrap();
        let committed: Value = serde_json::from_str(&on_disk).unwrap();
        let env = environment::placeholder();
        assert_eq!(build_manifest(&committed, &json!({}), &env, &registry()), committed);
        // Formatted the way the generator writes it, so setup never dirties the tree.
        assert_eq!(render(&committed), on_disk);
        assert_eq!(feature_scripts(&registry()).len(), 10);
    }

    #[test]
    fn a_setup_prompt_asks_what_node_asked_and_merges_what_node_merged() {
        struct P(Vec<String>, Vec<String>);
        impl crate::site_urls::Prompter for P {
            fn ask(&mut self, q: &str, d: &str) -> String {
                self.0.push(q.to_string());
                d.to_string()
            }
            fn log(&mut self, l: &str) {
                self.1.push(l.to_string());
            }
        }
        let setup = SetupContext { token_detector: Some(Arc::new(|_, _| Some("~/.tool/.jira.env".into()))), token_tool: "some-tool".into() };
        let r = PackRegistry::new(vec![pack(&setup).unwrap()]).unwrap();
        let prompt = |id: &str, existing: Value| {
            let mut p = P(vec![], vec![]);
            let out = (r.feature_spec(id).unwrap().descriptor.prompt_setup.as_ref().unwrap())(&mut p, &existing);
            (out, p.0, p.1)
        };
        // A borrowed token: no token question, the note names the source.
        let (out, asked, logs) = prompt("create-jira-subtasks", json!({ "jira": { "apiToken": "keep" } }));
        assert!(asked.is_empty());
        assert_eq!(out["jira"]["apiToken"], "keep");
        assert!(logs[0].contains("using the credentials from some-tool (~/.tool/.jira.env)"), "{logs:?}");
        // Prompts with nothing to ask give an empty fragment.
        assert_eq!(prompt("digest", json!({})).0, json!({}));
        assert_eq!(prompt("diagnose-build", json!({})).0, json!({}));
        assert!(r.feature_spec("resolve-conflict").unwrap().descriptor.prompt_setup.is_none());
        // The git host's base URL is kept and filled in.
        let (out, _, _) = prompt("address-review-comments", json!({}));
        let git = environment::current().sites.iter().find(|s| s.kind == "git").unwrap().id.clone();
        assert!(out[&git]["baseUrl"].as_str().is_some_and(|u| u.starts_with("http")));
        // The saved summarize model is kept.
        let (out, _, _) = prompt("summarize-comments", json!({ "summarizeComments": { "model": "m" } }));
        assert_eq!(out["summarizeComments"]["model"], "m");
    }

    #[test]
    fn diagnose_build_is_on_by_default_and_the_rest_leave_it_unset() {
        let r = registry();
        for spec in r.features() {
            let want = (spec.descriptor.id == "diagnose-build").then_some(true);
            assert_eq!(spec.descriptor.enabled_by_default, want, "{}", spec.descriptor.id);
        }
    }
}
