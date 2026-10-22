//! The real machine and network side of `doctor` (`DoctorIo`, `BitbucketProbe`) and the entry a CLI
//! hook calls (`run_doctor`): the Rust form of what `doctor.js`'s `main()` wired from the global
//! modules. `doctor.rs` holds the checks and takes this as an injected trait.

use crate::atlassian::AuthContext;
use crate::bitbucket::{bitbucket_site, Bitbucket};
use crate::config::ConfigStore;
use crate::doctor::{self, BitbucketProbe, DoctorCtx, DoctorIo, DoctorPaths, LlmView, Platform, RiskView};
use crate::external_tokens::ExternalTokens;
use crate::feature_registry::FeatureRegistry;
use crate::http::{HttpTransport, ReqwestTransport};
use crate::launchd;
use crate::llm::{LlmBackend, LlmContext, NoLlm};
use crate::packs::PackRegistry;
use crate::prereqs::{self, Check};
use crate::providers::Deps;
use crate::risk_facts::{RiskFactsClient, RiskFactsResult};
use crate::service_control::{check_health, Health};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `git` with `args` in `cwd` without ever prompting; its exit code, or `None` when it
/// couldn't start or didn't finish in `timeout` (it is killed then).
fn git_status(args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Option<i32> {
    let mut cmd = Command::new("git");
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

pub struct SystemDoctorIo<T: HttpTransport + 'static> {
    /// For the `/features` health probe.
    pub transport: T,
    pub llm: Arc<dyn LlmBackend>,
    pub risk: RiskFactsClient<T>,
    pub home: PathBuf,
}

#[async_trait]
impl<T: HttpTransport + 'static> DoctorIo for SystemDoctorIo<T> {
    fn claude_cli_ok(&self) -> bool {
        prereqs::check_claude_cli().ok
    }

    fn run_check(&self, name: &str) -> Option<Check> {
        prereqs::run_check_by_name(name)
    }

    fn check_repo_path(&self, path: &str) -> Check {
        prereqs::check_repo_path(path)
    }

    fn git_hook_configured(&self, repo_path: &str) -> bool {
        let Ok(args) = crate::git_hook::status_args(repo_path) else { return false };
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        git_status(&argv, None, LS_REMOTE_TIMEOUT) == Some(0)
    }

    fn git_ls_remote_status(&self, repo_path: &str) -> Option<i32> {
        git_status(&["ls-remote", "--exit-code", "--heads", "origin"], Some(Path::new(repo_path)), LS_REMOTE_TIMEOUT)
    }

    async fn llm(&self, _config: &Value) -> Result<Option<LlmView>, String> {
        self.llm.doctor_view().await
    }

    async fn risk_facts(&self, url: &str) -> RiskView {
        match self.risk.get(url).await {
            RiskFactsResult::Ok { facts, stale, .. } => RiskView::Ok { tests: facts.tests.len(), generated_at: facts.generated_at.clone(), stale },
            RiskFactsResult::Err { reason } => RiskView::Err(reason),
        }
    }

    async fn health(&self, port: u16, secret: &str) -> Health {
        check_health(&self.transport, port, secret).await
    }

    fn launchd_installed(&self) -> bool {
        launchd::is_installed(&self.home)
    }

    fn launchd_status(&self) -> launchd::Status {
        launchd::service_status()
    }

    fn launchd_log_paths(&self) -> (PathBuf, PathBuf) {
        launchd::log_paths(&crate::paths::state_dir(&self.home))
    }
}

/// Live, read-only access to the configured Bitbucket over the service's own client.
pub struct SystemBitbucketProbe<T: HttpTransport + 'static> {
    pub deps: Deps<T>,
}

impl<T: HttpTransport + 'static> SystemBitbucketProbe<T> {
    fn bitbucket(&self) -> Bitbucket<'_, T> {
        Bitbucket { client: &self.deps.client, site: bitbucket_site(&(self.deps.config)(), &self.deps.tokens) }
    }
}

#[async_trait]
impl<T: HttpTransport + 'static> BitbucketProbe for SystemBitbucketProbe<T> {
    fn base_url(&self) -> String {
        self.bitbucket().site.base_url.clone()
    }

    fn token_name(&self) -> String {
        self.bitbucket().site.config_key.clone()
    }

    async fn application_properties_raw(&self, auth: &AuthContext) -> Result<Value, String> {
        self.bitbucket().fetch_application_properties_raw(auth).await.map_err(|e| e.to_string())
    }

    async fn pull_request_raw(&self, auth: &AuthContext, project: &str, repo: &str, id: i64) -> Result<Value, String> {
        self.bitbucket().fetch_pull_request_raw(auth, project, repo, id).await.map_err(|e| e.to_string())
    }

    async fn activity_pages(&self, auth: &AuthContext, project: &str, repo: &str, id: i64) -> Result<(Vec<Value>, bool), String> {
        let p = self.bitbucket().fetch_all_activity_pages(auth, project, repo, id).await.map_err(|e| e.to_string())?;
        Ok((p.pages, p.truncated))
    }
}

/// Where a doctor run looks.
pub struct DoctorRun {
    pub home: PathBuf,
    pub config_path: PathBuf,
    /// The installed `chrome-extension` folder (its manifest is checked against config + packs).
    pub extension_dir: PathBuf,
    /// The command line after `doctor` (`--bitbucket-contract [PR url]`).
    pub args: Vec<String>,
}

/// The PR url after `--bitbucket-contract`, unless the next argument is another flag.
pub fn contract_pr_url(args: &[String]) -> Option<Option<&str>> {
    let i = args.iter().position(|a| a == "--bitbucket-contract")?;
    Some(args.get(i + 1).map(String::as_str).filter(|a| !a.starts_with("--")))
}

/// Runs doctor against this machine and prints the report; the process exit code (1 when anything
/// needs fixing). The caller has initialised the environment profile.
pub async fn run_doctor<F: Sync>(packs: &PackRegistry<F>, run: &DoctorRun) -> i32 {
    let env = crate::environment::current();
    let registry = FeatureRegistry::from_packs(packs);
    let config = ConfigStore::load(&run.config_path, Some(&registry)).ok().map(|c| c.snapshot());
    let state_dir = crate::paths::state_dir(&run.home);
    let companion_dir = run.config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| run.home.clone());
    let paths = DoctorPaths {
        companion_dir: companion_dir.clone(),
        config_path: run.config_path.clone(),
        credentials_path: companion_dir.join("credentials.enc"),
        credentials_key_path: state_dir.join("credentials.key"),
        state_dir: state_dir.clone(),
        home: run.home.clone(),
        extension_config_path: run.extension_dir.join("companion-config.js"),
        claude_config_dir_env: std::env::var("CLAUDE_CONFIG_DIR").ok(),
        path_env: std::env::var("PATH").ok(),
    };

    let live_config = Arc::new(std::sync::RwLock::new(config.clone().unwrap_or_else(|| json!({}))));
    let llm_config = live_config.clone();
    let creds = Arc::new(crate::credentials::Credentials::open(paths.credentials_path.clone(), paths.credentials_key_path.clone()));
    let llm_creds = creds.clone();
    let transport = match ReqwestTransport::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("\nDoctor itself failed: {e}");
            return 1;
        }
    };
    let risk_t = match ReqwestTransport::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("\nDoctor itself failed: {e}");
            return 1;
        }
    };
    let llm: Arc<dyn LlmBackend> = match packs.llm_factory() {
        Some(build) => match build(&LlmContext { config: Arc::new(move || llm_config.read().unwrap_or_else(|p| p.into_inner()).clone()), get_token: Arc::new(move |name| llm_creds.get_token(name)), os_user: std::env::var("USER").unwrap_or_default() }) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("\nDoctor itself failed: {e}");
                return 1;
            }
        },
        None => Arc::new(NoLlm),
    };
    let io = SystemDoctorIo { transport, llm, risk: RiskFactsClient::new(risk_t), home: run.home.clone() };

    let external = ExternalTokens::new(run.home.clone(), packs.external_token_sources().into_iter().cloned().collect());
    let external_sources = move |urls: &Map<String, Value>| external.sources_for(urls);
    let manifest_problems = {
        let (dir, env) = (run.extension_dir.clone(), env);
        move |c: &Value| crate::extension_manifest::check_manifest(&dir, c, env, packs)
    };
    let ctx = DoctorCtx {
        paths,
        platform: Platform::current(),
        io: &io,
        registry: &registry,
        product_name: env.branding_text("productName").unwrap_or("AI Dev Companion").to_string(),
        pack_checks: packs.doctor_checks().into_iter().cloned().collect(),
        external_label: packs.external_token_label().to_string(),
        external_sources: &external_sources,
        sites: env.sites.iter().map(|s| (s.id.clone(), s.base_url.clone())).collect(),
        manifest_problems: &manifest_problems,
        protected_rule_count: crate::claude_args::protected_path_rules(&crate::claude_args::ProtectedDirs {
            state_dir: &state_dir.to_string_lossy(),
            companion_dir: &companion_dir.to_string_lossy(),
            home: &run.home.to_string_lossy(),
        })
        .len(),
        now_ms: &|| crate::job_files::now_ms() as i64,
    };

    let contract = contract_pr_url(&run.args);
    let mut report = match (contract, config) {
        (Some(pr_url), Some(config)) => {
            let mut r = doctor::Report::new();
            let tokens = Arc::new(crate::site_tokens::SiteTokens::new(Arc::new(crate::startup::CredentialTokens(creds.clone())), ExternalTokens::new(run.home.clone(), vec![])));
            let hub = Arc::new(crate::auth_context::AuthHub::new(Arc::new(crate::startup::CredentialTokens(creds))));
            let client = match ReqwestTransport::new() {
                Ok(t) => Arc::new(crate::atlassian::Client::new(t, hub)),
                Err(e) => {
                    eprintln!("\nDoctor itself failed: {e}");
                    return 1;
                }
            };
            let cfg = config.clone();
            let probe = SystemBitbucketProbe { deps: Deps { client, tokens, config: Arc::new(move || cfg.clone()) } };
            doctor::run_bitbucket_contract_check(&mut r, &ctx, &probe, pr_url).await;
            r
        }
        (Some(_), None) => {
            let mut r = doctor::Report::new();
            r.report("FAIL", &format!("{} is missing or invalid \u{2014} setup hasn't been completed.", ctx.paths.config_path.display()), Some("companion setup"));
            r
        }
        (None, config) => doctor::run(&ctx, config).await,
    };
    let code = doctor::finish(&mut report, contract.is_some());
    for line in &report.lines {
        println!("{line}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::FakeTransport;
    use crate::http::HttpResponse;

    fn io(reply: Value) -> SystemDoctorIo<FakeTransport> {
        let t = |reply: Value| FakeTransport::new(move |_| Ok(HttpResponse { status: 200, headers: vec![], body: reply.to_string().into_bytes() }));
        SystemDoctorIo { transport: t(reply.clone()), llm: Arc::new(NoLlm), risk: RiskFactsClient::new(t(reply)), home: "/nowhere".into() }
    }

    #[test]
    fn the_contract_flag_takes_an_optional_pr_url_that_is_not_another_flag() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(contract_pr_url(&a(&[])), None);
        assert_eq!(contract_pr_url(&a(&["--bitbucket-contract"])), Some(None));
        assert_eq!(contract_pr_url(&a(&["--bitbucket-contract", "https://bb/pr/1"])), Some(Some("https://bb/pr/1")));
        assert_eq!(contract_pr_url(&a(&["--bitbucket-contract", "--other"])), Some(None));
    }

    #[test]
    fn git_helpers_run_real_git_with_a_timeout_and_say_none_when_it_cannot() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(git_status(&["init", "-q"], Some(d.path()), Duration::from_secs(10)), Some(0));
        // An origin that doesn't exist: git runs and exits non-zero.
        let io = io(json!({}));
        let repo = d.path().to_string_lossy().into_owned();
        assert!(io.git_ls_remote_status(&repo).is_some_and(|c| c != 0));
        assert!(!io.git_hook_configured(&repo));
        assert_eq!(git_status(&["status"], Some(Path::new("/definitely/not/a/dir")), Duration::from_secs(1)), None);
    }

    #[tokio::test]
    async fn there_is_no_llm_view_without_a_backend_and_the_risk_view_reads_the_feed() {
        let io = io(json!({ "schema": "risk-facts/v1", "generatedAt": "2026-01-01T00:00:00Z", "tests": [], "files": [] }));
        assert_eq!(io.llm(&json!({})).await.unwrap(), None);
        match io.risk_facts("https://feed.example/facts.json").await {
            RiskView::Ok { tests, stale, .. } => assert_eq!((tests, stale), (0, false)),
            RiskView::Err(e) => panic!("{e}"),
        }
        assert!(matches!(io.risk_facts("not a url").await, RiskView::Err(_)));
    }

    #[tokio::test]
    async fn health_probes_the_features_route_through_the_transport() {
        let io = io(json!({ "features": ["digest"] }));
        let h = io.health(7777, "secret").await;
        assert_eq!(h.state, crate::service_control::HealthState::Ok);
    }
}
