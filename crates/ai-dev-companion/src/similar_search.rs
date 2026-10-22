//! `similar(item, k)`: the past tickets and PRs in this user's local history
//! most like a given one, from two rankings merged by reciprocal rank fusion
//! (port of `core/similar-search.js`): the FTS5 words it shares (always) and
//! the cosine of its embedding (only when vectors exist and a query vector can
//! be had: the item's own stored one, or one embedded through the LLM
//! when that is ready). With no LLM it is words only. A ticket and its
//! analysis count as one result. Used by analyze-issue's prompt, the MCP tool
//! `find_similar` and `companion similar`.

use crate::history_db::HistoryDb;
use crate::history_schema::normalize_key;
use crate::prereqs::GitRemoteRules;
use crate::route_policy::{choose_tier, Tier, TierRequest};
use crate::similar::{self, embed_text, group_key_of};
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;

const CANDIDATES: i64 = 20;

/// The slice of the LLM that embedding needs.
pub trait Embedder: Send + Sync {
    /// The configured embedding model; an error when the configuration is unusable.
    fn embedding_model(&self) -> Result<String, String>;
    /// Whether the LLM is ready to be asked.
    fn available(&self) -> impl Future<Output = bool> + Send;
    /// The detection state (`ready`, `no-key`, ...).
    fn detect_state(&self) -> impl Future<Output = String> + Send;
    fn embed(&self, texts: &[String]) -> impl Future<Output = Result<Vec<Vec<f64>>, String>> + Send;
    /// The short phrase recorded when the history-embedding task did nothing because this isn't ready.
    fn skip_reason(&self) -> String {
        "no llm".into()
    }
}

pub struct SimilarSearch<'a, E: Embedder> {
    pub history: Option<&'a HistoryDb>,
    pub llm: Option<&'a E>,
    pub config: Value,
    pub embed_timeout: Duration,
    pub git: GitRemoteRules,
}

impl<'a, E: Embedder> SimilarSearch<'a, E> {
    pub fn new(history: Option<&'a HistoryDb>, llm: Option<&'a E>, config: Value, git: GitRemoteRules) -> Self {
        Self { history, llm, config, embed_timeout: Duration::from_millis(8000), git }
    }

    /// A query vector: the item's stored one, else the text embedded on-prem; `None` when neither.
    async fn query_vector(&self, history: &HistoryDb, item_id: Option<i64>, text: &str, model: Option<&str>) -> Option<Vec<f32>> {
        // No stored vectors to compare with: don't send text to the LLM for nothing.
        let model = model.filter(|m| !m.is_empty())?;
        if !history.has_vectors(model).ok()? {
            return None;
        }
        if let Some(id) = item_id {
            if let Some(stored) = history.vector_for(id, model).ok().flatten() {
                return Some(stored);
            }
        }
        let embed_input = embed_text(&json!({ "title": text }))?;
        let llm = self.llm?;
        if !llm.available().await {
            return None;
        }
        let route = choose_tier(TierRequest { task: Some("embed"), bytes: Some(embed_input.len() as f64), needs_tools: false }, true);
        if route.tier != Tier::Onprem {
            return None;
        }
        // Words alone still work; a slow or failing LLM never blocks the caller.
        let vectors = tokio::time::timeout(self.embed_timeout, llm.embed(&[embed_input])).await.ok()?.ok()?;
        similar::decode_vector(&similar::encode_vector(vectors.first()?).ok()?)
    }

    fn hydrate(&self, history: &HistoryDb, key: &str, score: f64, via: &[&str]) -> Result<Value, String> {
        let main = history.get_item(key)?;
        let analysis = if key.starts_with("jira:") { history.get_item(&format!("analysis:{key}"))? } else { None };
        let kind = main.as_ref().map_or(if key.starts_with("jira:") { "ticket" } else { "pr" }.to_string(), |m| m["item"]["kind"].as_str().unwrap_or("").to_string());
        let updated = |d: &Option<Value>| d.as_ref().and_then(|d| d["item"]["updatedAt"].as_i64()).unwrap_or(0);
        let newest = updated(&main).max(updated(&analysis));
        Ok(json!({
            "key": key,
            "kind": kind,
            "title": main.as_ref().map_or(Value::Null, |m| m["item"]["title"].clone()),
            "analysis": analysis.as_ref().map_or(Value::Null, |a| a["item"]["excerpt"].clone()),
            "repo": analysis.as_ref().map_or(Value::Null, |a| a["item"]["repo"].clone()),
            "updatedAt": if newest == 0 { Value::Null } else { json!(newest) },
            "score": score,
            "via": via,
        }))
    }

    /// `{enabled, mode: "off" | "text" | "vector+text", items}`. With a key,
    /// its own history text is the query (unless `text` is given) and it is
    /// left out of the results. Errors when there is nothing to compare: no
    /// history, or a key with nothing recorded and no text.
    pub async fn find(&self, key: Option<&str>, text: Option<&str>, k: Option<f64>) -> Result<Value, String> {
        if !similar::similar_enabled(&self.config) {
            return Ok(json!({ "enabled": false, "mode": "off", "items": [] }));
        }
        let history = self.history.ok_or("Local history is off on this copy of the companion (it only records on the installed copy).")?;
        let self_key = key.and_then(|k| normalize_key(Some(k), self.git));
        let exclude = self_key.as_deref().map(|k| group_key_of(k).unwrap_or_else(|| k.to_string()));
        let mut query_text = text.unwrap_or("").to_string();
        let mut item_id = None;
        if let Some(sk) = &self_key {
            if let Some(detail) = history.get_item(sk)? {
                item_id = detail["item"]["id"].as_i64();
                if crate::js_text::js_trim(&query_text).is_empty() {
                    let analysis = if sk.starts_with("jira:") { history.get_item(&format!("analysis:{sk}"))? } else { None };
                    let parts = [detail["item"]["title"].as_str(), detail["item"]["excerpt"].as_str(), analysis.as_ref().and_then(|a| a["item"]["excerpt"].as_str())];
                    query_text = parts.into_iter().flatten().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
                }
            }
        }
        if crate::js_text::js_trim(&query_text).is_empty() {
            return Err(if self_key.is_some() { "Nothing is recorded under that key." } else { "Give a key or some text to compare." }.into());
        }
        let text_hits = history.search_any(&query_text, Some(&similar::VECTOR_KINDS), Some(CANDIDATES))?;
        // Any trouble on the vector side leaves the words-only answer.
        let mut vector = None;
        let mut vector_hits = vec![];
        if let Ok(model) = self.llm.map(Embedder::embedding_model).transpose() {
            vector = self.query_vector(history, item_id, &query_text, model.as_deref()).await;
            if let (Some(v), Some(m)) = (&vector, &model) {
                vector_hits = history.nearest(m, v, Some(&similar::VECTOR_KINDS), Some(CANDIDATES), similar::MIN_COSINE).unwrap_or_default();
            } else {
                vector = None;
            }
        }
        let grouped = |hits: &[crate::history_db::SimilarItem]| hits.iter().filter_map(|h| group_key_of(&h.key)).collect::<Vec<_>>();
        let (v_keys, t_keys) = (grouped(&vector_hits), grouped(&text_hits));
        let (v_refs, t_refs): (Vec<&str>, Vec<&str>) = (v_keys.iter().map(String::as_str).collect(), t_keys.iter().map(String::as_str).collect());
        let ranked = similar::combine_scores(&v_refs, &t_refs, Some(similar::clamp_k(k) as f64), exclude.as_deref());
        let items = ranked.iter().map(|r| self.hydrate(history, &r.key, r.score, &r.via)).collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "enabled": true, "mode": if vector.is_some() { "vector+text" } else { "text" }, "items": items }))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A canned embedder that records what it was asked to embed.
    pub struct FakeLlm {
        pub ready: bool,
        pub state: &'static str,
        pub fail: bool,
        pub hang: bool,
        pub settings_error: bool,
        pub model: &'static str,
        pub vector: Vec<f64>,
        pub calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeLlm {
        pub fn new() -> Self {
            Self { ready: true, state: "ready", fail: false, hang: false, settings_error: false, model: "embedding-model", vector: vec![1.0, 0.0, 0.0], calls: Mutex::new(vec![]) }
        }
    }

    impl Embedder for FakeLlm {
        fn embedding_model(&self) -> Result<String, String> {
            if self.settings_error { Err("config broke".into()) } else { Ok(self.model.into()) }
        }
        async fn available(&self) -> bool {
            self.ready
        }
        async fn detect_state(&self) -> String {
            self.state.into()
        }
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
            self.calls.lock().unwrap().push(texts.to_vec());
            if self.hang {
                std::future::pending::<()>().await;
            }
            if self.fail {
                return Err("The LLM answered HTTP 500. Bearer sk-secret".into());
            }
            Ok(texts.iter().map(|_| self.vector.clone()).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeLlm;
    use super::*;
    use GitRemoteRules::BitbucketDc;

    const MODEL: &str = "embedding-model";

    struct Store {
        _dir: tempfile::TempDir,
        h: HistoryDb,
    }

    fn open() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let h = HistoryDb::open(&dir.path().join("history.db"), BitbucketDc).unwrap();
        Store { _dir: dir, h }
    }

    fn up(h: &HistoryDb, kind: &str, key: &str, title: &str, at: i64) -> i64 {
        h.upsert_item(&crate::history_db::UpsertItem { kind, key, title: Some(title), at: Some(at), ..Default::default() }).unwrap()
    }

    /// Three past tickets: two about SSO login, one about billing.
    fn seed(h: &HistoryDb) -> (i64, i64, i64) {
        let t1 = up(h, "ticket", "PROJ-1", "Login fails for SSO users", 10);
        h.upsert_item(&crate::history_db::UpsertItem { kind: "analysis", key: "analysis:jira:PROJ-1", repo: Some("ACME/sample-app"), title: Some("Analysis of PROJ-1"), excerpt: Some("The SSO token refresh races the redirect"), at: Some(11), ..Default::default() }).unwrap();
        let t2 = up(h, "ticket", "PROJ-2", "Sign-in loop after the session expires", 20);
        let t3 = up(h, "ticket", "PROJ-3", "Billing export has the wrong totals", 30);
        up(h, "job", "job:x", "SSO login job", 40);
        (t1, t2, t3)
    }

    fn search<'a>(s: &'a Store, llm: Option<&'a FakeLlm>) -> SimilarSearch<'a, FakeLlm> {
        SimilarSearch::new(Some(&s.h), llm, json!({}), BitbucketDc)
    }

    fn item_keys(r: &Value) -> Vec<&str> {
        r["items"].as_array().unwrap().iter().map(|i| i["key"].as_str().unwrap()).collect()
    }

    #[tokio::test]
    async fn without_the_llm_it_ranks_by_shared_words_groups_a_ticket_with_its_analysis_and_leaves_out_jobs() {
        let s = open();
        seed(&s.h);
        let llm = FakeLlm { ready: false, ..FakeLlm::new() };
        let r = search(&s, Some(&llm)).find(None, Some("SSO users see a login error"), None).await.unwrap();
        assert_eq!(r["mode"], "text");
        assert_eq!(item_keys(&r), ["jira:PROJ-1"]);
        let first = &r["items"][0];
        assert_eq!(
            *first,
            json!({ "key": "jira:PROJ-1", "kind": "ticket", "title": "Login fails for SSO users", "analysis": "The SSO token refresh races the redirect", "repo": "ACME/sample-app", "updatedAt": 11, "score": first["score"], "via": ["text"] })
        );
    }

    #[tokio::test]
    async fn with_vectors_it_also_finds_tickets_that_share_no_words_and_leaves_the_ticket_itself_out() {
        let s = open();
        let (t1, t2, t3) = seed(&s.h);
        s.h.save_vector(t1, MODEL, &[1.0, 0.0, 0.0], 10, None).unwrap();
        s.h.save_vector(t2, MODEL, &[0.9, 0.2, 0.0], 20, None).unwrap();
        s.h.save_vector(t3, MODEL, &[0.0, 0.0, 1.0], 30, None).unwrap();
        let llm = FakeLlm::new();
        let sr = search(&s, Some(&llm));
        let r = sr.find(Some("PROJ-1"), None, None).await.unwrap();
        assert_eq!(r["mode"], "vector+text");
        assert_eq!(item_keys(&r), ["jira:PROJ-2"], "PROJ-1 itself is excluded; PROJ-3 is too far");
        assert_eq!(r["items"][0]["via"], json!(["vector"]));
        assert_eq!(llm.calls.lock().unwrap().len(), 0, "the stored vector is the query; nothing is sent");
        let fresh = sr.find(None, Some("Users bounce back to the sign-in page"), None).await.unwrap();
        assert_eq!(llm.calls.lock().unwrap().len(), 1, "new text is embedded once");
        assert_eq!(item_keys(&fresh), ["jira:PROJ-1", "jira:PROJ-2"]);
    }

    #[tokio::test]
    async fn a_failing_llm_falls_back_to_words_and_k_caps_the_list() {
        let s = open();
        let (t1, _, _) = seed(&s.h);
        s.h.save_vector(t1, MODEL, &[1.0, 0.0, 0.0], 10, None).unwrap();
        let llm = FakeLlm { fail: true, ..FakeLlm::new() };
        let r = search(&s, Some(&llm)).find(None, Some("SSO login session billing totals"), Some(2.0)).await.unwrap();
        assert_eq!(llm.calls.lock().unwrap().len(), 1, "the LLM was tried");
        assert_eq!(r["mode"], "text");
        assert_eq!(r["items"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn off_no_history_or_nothing_to_compare() {
        let s = open();
        seed(&s.h);
        let llm = FakeLlm::new();
        let off = SimilarSearch::new(Some(&s.h), Some(&llm), json!({ "similar": { "enabled": false } }), BitbucketDc);
        assert_eq!(off.find(Some("PROJ-1"), None, None).await.unwrap(), json!({ "enabled": false, "mode": "off", "items": [] }));
        let none = SimilarSearch::<FakeLlm>::new(None, None, json!({}), BitbucketDc);
        assert!(none.find(None, Some("x"), None).await.unwrap_err().contains("history is off"));
        let down = FakeLlm { ready: false, ..FakeLlm::new() };
        let sr = search(&s, Some(&down));
        assert!(sr.find(Some("PROJ-99"), None, None).await.unwrap_err().contains("Nothing is recorded"));
        assert!(sr.find(None, None, None).await.unwrap_err().contains("Give a key or some text"));
        assert_eq!(item_keys(&sr.find(Some("PROJ-99"), Some("billing totals wrong"), None).await.unwrap())[0], "jira:PROJ-3", "text works for a ticket not yet recorded");
    }

    #[tokio::test]
    async fn with_no_vectors_stored_nothing_is_sent_to_the_llm_and_the_mode_is_text() {
        let s = open();
        seed(&s.h);
        let llm = FakeLlm::new();
        let r = search(&s, Some(&llm)).find(None, Some("SSO login"), None).await.unwrap();
        assert_eq!(r["mode"], "text");
        assert_eq!(llm.calls.lock().unwrap().len(), 0);
        let t3 = s.h.get_item("PROJ-3").unwrap().unwrap()["item"]["id"].as_i64().unwrap();
        s.h.save_vector(t3, MODEL, &[1.0], 30, None).unwrap();
        search(&s, Some(&llm)).find(None, Some("SSO login"), None).await.unwrap();
        assert_eq!(llm.calls.lock().unwrap().len(), 0, "a placeholder vector isn't a real one");
    }

    #[tokio::test]
    async fn a_query_is_masked_before_it_is_embedded_and_an_unavailable_llm_is_not_called() {
        let s = open();
        let (t1, _, _) = seed(&s.h);
        s.h.save_vector(t1, MODEL, &[1.0, 0.0, 0.0], 10, None).unwrap();
        let llm = FakeLlm::new();
        search(&s, Some(&llm)).find(None, Some("SSO login token=abc123secret\u{7} fails"), None).await.unwrap();
        {
            let calls = llm.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert!(!calls[0][0].contains("abc123secret") && !calls[0][0].contains('\u{7}'), "{}", calls[0][0]);
        }
        let down = FakeLlm { ready: false, ..FakeLlm::new() };
        let r = search(&s, Some(&down)).find(None, Some("SSO login fails"), None).await.unwrap();
        assert_eq!(down.calls.lock().unwrap().len(), 0);
        assert_eq!(r["mode"], "text");
    }

    #[tokio::test]
    async fn a_slow_embedding_times_out_and_a_missing_or_broken_llm_leaves_the_words() {
        let s = open();
        let (t1, _, _) = seed(&s.h);
        s.h.save_vector(t1, MODEL, &[1.0, 0.0, 0.0], 10, None).unwrap();
        let slow = FakeLlm { hang: true, ..FakeLlm::new() };
        let mut sr = search(&s, Some(&slow));
        sr.embed_timeout = Duration::from_millis(30);
        let started = std::time::Instant::now();
        assert_eq!(sr.find(None, Some("SSO login fails"), None).await.unwrap()["mode"], "text");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(search(&s, None).find(None, Some("SSO login"), None).await.unwrap()["mode"], "text");
        let broken = FakeLlm { settings_error: true, ..FakeLlm::new() };
        assert_eq!(search(&s, Some(&broken)).find(None, Some("SSO login"), None).await.unwrap()["mode"], "text");
    }

    #[tokio::test]
    async fn a_pr_result_is_hydrated_as_a_pr_and_a_large_k_is_capped_at_20() {
        let s = open();
        up(&s.h, "pr", "bitbucket:CI/sample-app#7", "Fix the login redirect", 5);
        for i in 1..=25 {
            up(&s.h, "ticket", &format!("PROJ-{i}"), &format!("Login redirect problem {i}"), i);
        }
        let llm = FakeLlm { ready: false, ..FakeLlm::new() };
        let sr = search(&s, Some(&llm));
        assert_eq!(sr.find(None, Some("login redirect"), Some(100.0)).await.unwrap()["items"].as_array().unwrap().len(), 20);
        let r = sr.find(None, Some("Fix the login redirect"), Some(20.0)).await.unwrap();
        let pr = r["items"].as_array().unwrap().iter().find(|i| i["key"] == "bitbucket:CI/sample-app#7").unwrap();
        assert_eq!((pr["kind"].as_str(), pr["analysis"].clone()), (Some("pr"), Value::Null));
    }
}
