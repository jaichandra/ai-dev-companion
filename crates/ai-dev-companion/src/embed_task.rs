//! The scheduler's `history.embed` task (port of `core/embed-task.js`): embeds
//! history items that have no vector yet with the configured embedding
//! model, so similar items can be found by meaning, not only by shared words.
//! It runs only when the LLM is ready (otherwise it records why it skipped and
//! similar-item search uses the words alone), never sends text anywhere but the
//! LLM's on-prem model (the route policy sends "embed" on-prem, and the backend
//! refuses a model outside its allow-list), masks secrets and clips every text, and does at
//! most 48 items a run.

use crate::history_db::{HistoryDb, SimilarItem};
use crate::route_policy::{choose_tier, Tier, TierRequest};
use crate::similar::{self, embed_text};
use crate::similar_search::Embedder;
use serde_json::{json, Map, Value};

pub struct EmbedTask<'a, E: Embedder> {
    pub history: Option<&'a HistoryDb>,
    pub llm: &'a E,
    pub config: Value,
    pub now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl<'a, E: Embedder> EmbedTask<'a, E> {
    pub fn new(history: Option<&'a HistoryDb>, llm: &'a E, config: Value) -> Self {
        Self { history, llm, config, now: Box::new(|| crate::job_files::now_ms() as i64) }
    }

    /// An item that can't be embedded (no text, or the model's answer for it
    /// can't be stored) gets a one-number placeholder vector, which the
    /// nearest-neighbour search never matches (it compares equal sizes only).
    /// That takes it out of the pending list until it changes, so it can't
    /// starve the items behind it run after run.
    fn skip(&self, history: &HistoryDb, model: &str, item: &SimilarItem, skipped: &mut u64) {
        *skipped += 1;
        // Still pending if even this fails; the other items are not held up by it.
        let _ = history.save_vector(item.id, model, &[1.0], item.updated_at, Some((self.now)()));
    }

    pub async fn run(&self) -> Value {
        let obj = |entries: Vec<(&str, Value)>| Value::Object(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<Map<_, _>>());
        if !similar::similar_enabled(&self.config) {
            return json!({ "skipped": "similar items are off", "embedded": 0 });
        }
        let Some(history) = self.history else { return json!({ "skipped": "local history is off", "embedded": 0 }) };
        let state = self.llm.detect_state().await;
        if state != "ready" {
            return json!({ "skipped": self.llm.skip_reason(), "llm": state, "embedded": 0 });
        }
        let model = match self.llm.embedding_model() {
            Ok(m) => m,
            Err(error) => return obj(vec![("outcome", json!("failed")), ("error", json!(error)), ("embedded", json!(0))]),
        };
        // One more than the run's share, to know whether there is more waiting.
        let found = match history.pending_embeddings(&model, Some(&similar::VECTOR_KINDS), Some(similar::EMBED_PER_RUN as i64 + 1)) {
            Ok(f) => f,
            Err(error) => return obj(vec![("outcome", json!("failed")), ("error", json!(error)), ("embedded", json!(0)), ("model", json!(model))]),
        };
        let more = found.len() > similar::EMBED_PER_RUN;
        let mut skipped = 0u64;
        let mut work: Vec<(SimilarItem, String)> = vec![];
        for item in found.into_iter().take(similar::EMBED_PER_RUN) {
            match embed_text(&json!({ "title": item.title, "excerpt": item.excerpt })) {
                Some(text) => work.push((item, text)),
                None => self.skip(history, &model, &item, &mut skipped),
            }
        }
        let tail = |skipped: u64| if skipped > 0 { Some(("skippedItems", json!(skipped))) } else { None };
        let with_tail = |mut entries: Vec<(&'static str, Value)>, skipped: u64| {
            entries.extend(tail(skipped));
            obj(entries)
        };
        if work.is_empty() {
            return with_tail(vec![("embedded", json!(0)), ("model", json!(model)), ("tier", json!("onprem"))], skipped);
        }
        let bytes = work.iter().map(|(_, t)| t.len()).max().unwrap_or(0);
        let route = choose_tier(TierRequest { task: Some("embed"), bytes: Some(bytes as f64), needs_tools: false }, true);
        // Embedding never falls back to Claude: if the rules say no, it waits.
        if route.tier != Tier::Onprem {
            return json!({ "skipped": route.reason, "embedded": 0 });
        }
        let mut embedded = 0u64;
        for batch in work.chunks(similar::EMBED_BATCH) {
            let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
            let vectors = match self.llm.embed(&texts).await {
                Ok(v) => v,
                Err(error) => {
                    return with_tail(vec![("outcome", json!("failed")), ("error", json!(error)), ("embedded", json!(embedded)), ("model", json!(model)), ("tier", json!("onprem"))], skipped);
                }
            };
            for (j, (item, _)) in batch.iter().enumerate() {
                let saved = vectors.get(j).ok_or_else(|| "missing vector".to_string()).and_then(|v| history.save_vector(item.id, &model, v, item.updated_at, Some((self.now)())));
                match saved {
                    Ok(()) => embedded += 1,
                    Err(_) => self.skip(history, &model, item, &mut skipped),
                }
            }
        }
        with_tail(vec![("embedded", json!(embedded)), ("model", json!(model)), ("tier", json!("onprem")), ("more", json!(more))], skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_db::UpsertItem;
    use crate::prereqs::GitRemoteRules::BitbucketDc;
    use crate::similar_search::testing::FakeLlm;

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

    fn ticket(h: &HistoryDb, key: &str, title: &str, at: i64) {
        h.upsert_item(&UpsertItem { kind: "ticket", key, title: Some(title), at: Some(at), ..Default::default() }).unwrap();
    }

    fn task<'a>(s: &'a Store, llm: &'a FakeLlm) -> EmbedTask<'a, FakeLlm> {
        let mut t = EmbedTask::new(Some(&s.h), llm, json!({}));
        t.now = Box::new(|| 99);
        t
    }

    fn pending(s: &Store) -> usize {
        s.h.pending_embeddings(MODEL, None, None).unwrap().len()
    }

    #[tokio::test]
    async fn with_no_llm_ready_the_task_records_why_it_skipped_and_sends_nothing() {
        let s = open();
        ticket(&s.h, "PROJ-1", "Login fails", 1);
        let llm = FakeLlm { state: "no-key", ..FakeLlm::new() };
        assert_eq!(task(&s, &llm).run().await, json!({ "skipped": "no llm", "llm": "no-key", "embedded": 0 }));
        assert!(llm.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn turned_off_or_without_a_history_it_does_nothing() {
        let s = open();
        let llm = FakeLlm::new();
        let off = EmbedTask::new(Some(&s.h), &llm, json!({ "similar": { "enabled": false } }));
        assert_eq!(off.run().await["skipped"], "similar items are off");
        assert_eq!(EmbedTask::new(None, &llm, json!({})).run().await["skipped"], "local history is off");
        assert!(llm.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn embeds_the_pending_items_in_batches_redacted_and_clipped_and_stores_their_vectors() {
        let s = open();
        for i in 1..=20 {
            ticket(&s.h, &format!("PROJ-{i}"), &format!("Ticket {i} token=abc123"), i);
        }
        s.h.upsert_item(&UpsertItem { kind: "analysis", key: "analysis:jira:PROJ-1", title: Some("Analysis of PROJ-1"), excerpt: Some(&"x".repeat(9000)), at: Some(30), ..Default::default() }).unwrap();
        s.h.upsert_item(&UpsertItem { kind: "job", key: "job:1", title: Some("not embedded"), at: Some(40), ..Default::default() }).unwrap();
        let llm = FakeLlm::new();
        let t = task(&s, &llm);
        assert_eq!(t.run().await, json!({ "embedded": 21, "model": MODEL, "tier": "onprem", "more": false }));
        let calls = llm.calls.lock().unwrap().clone();
        assert_eq!(calls.iter().map(Vec::len).collect::<Vec<_>>(), [16, 5]);
        let sent: Vec<&String> = calls.iter().flatten().collect();
        assert!(sent.iter().all(|t| crate::js_text::utf16_len(t) <= similar::EMBED_TEXT_MAX));
        assert!(sent.iter().any(|t| t.contains("token=***")) && !sent.iter().any(|t| t.contains("abc123")), "secrets are masked");
        assert_eq!(s.h.vector_stats().unwrap()["vectors"], 21);
        assert_eq!(pending(&s), 0);
        assert_eq!(t.run().await, json!({ "embedded": 0, "model": MODEL, "tier": "onprem" }), "nothing new, no call");
        assert_eq!(llm.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn at_most_48_items_a_run_a_failed_call_reports_what_it_did_so_far() {
        let s = open();
        for i in 1..=60 {
            ticket(&s.h, &format!("PROJ-{i}"), &format!("Ticket {i}"), i);
        }
        let first = task(&s, &FakeLlm::new()).run().await;
        assert_eq!((first["embedded"].clone(), first["more"].clone()), (json!(48), json!(true)));
        let failing = FakeLlm { fail: true, ..FakeLlm::new() };
        let failed = task(&s, &failing).run().await;
        assert_eq!((failed["outcome"].as_str(), failed["embedded"].clone()), (Some("failed"), json!(0)));
        assert!(failed["error"].as_str().unwrap().contains("HTTP 500"));
    }

    #[tokio::test]
    async fn exactly_48_pending_is_not_more_49_is() {
        let s = open();
        for i in 1..=48 {
            ticket(&s.h, &format!("PROJ-{i}"), &format!("Ticket {i}"), i);
        }
        assert_eq!(task(&s, &FakeLlm::new()).run().await["more"], false);
        for i in 101..=149 {
            ticket(&s.h, &format!("PROJ-{i}"), &format!("Ticket {i}"), i);
        }
        let r = task(&s, &FakeLlm::new()).run().await;
        assert_eq!((r["embedded"].clone(), r["more"].clone()), (json!(48), json!(true)));
    }

    #[tokio::test]
    async fn a_model_the_backend_refuses_fails_the_run_and_stores_nothing() {
        // A backend that refuses at embed time, as an allowed-models check does.
        struct Refusing;
        impl Embedder for Refusing {
            fn embedding_model(&self) -> Result<String, String> {
                Ok(MODEL.into())
            }
            async fn available(&self) -> bool {
                true
            }
            async fn detect_state(&self) -> String {
                "ready".into()
            }
            async fn embed(&self, _t: &[String]) -> Result<Vec<Vec<f64>>, String> {
                Err("The model \"evil-model\" is not allowed.".into())
            }
        }
        let s = open();
        ticket(&s.h, "PROJ-1", "Login fails", 1);
        let r = EmbedTask::new(Some(&s.h), &Refusing, json!({})).run().await;
        assert_eq!(r["outcome"], "failed");
        assert!(r["error"].as_str().unwrap().contains("is not allowed"));
        assert_eq!(s.h.vector_stats().unwrap()["vectors"], 0);
    }

    #[tokio::test]
    async fn one_unsavable_vector_does_not_wedge_the_queue_it_is_skipped_and_the_rest_are_stored() {
        // The 1st vector is all zeros, the 2nd not finite, the rest fine.
        struct Odd(std::sync::Mutex<usize>);
        impl Embedder for Odd {
            fn embedding_model(&self) -> Result<String, String> {
                Ok(MODEL.into())
            }
            async fn available(&self) -> bool {
                true
            }
            async fn detect_state(&self) -> String {
                "ready".into()
            }
            async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
                *self.0.lock().unwrap() += 1;
                Ok(texts.iter().enumerate().map(|(i, _)| match i {
                    0 => vec![0.0, 0.0, 0.0],
                    1 => vec![f64::NAN, 1.0],
                    _ => vec![1.0, 2.0, 3.0],
                }).collect())
            }
        }
        let s = open();
        for i in 1..=5 {
            ticket(&s.h, &format!("PROJ-{i}"), &format!("Ticket {i}"), i);
        }
        let llm = Odd(Default::default());
        let t = EmbedTask::new(Some(&s.h), &llm, json!({}));
        let r = t.run().await;
        assert_eq!((r["embedded"].clone(), r["skippedItems"].clone()), (json!(3), json!(2)));
        assert!(r.get("outcome").is_none());
        assert_eq!(pending(&s), 0, "the bad ones are not first in line next time");
        assert_eq!(t.run().await["embedded"], 0);
        assert_eq!(*llm.0.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn items_with_nothing_to_embed_are_marked_done_so_they_cannot_starve_the_others() {
        let s = open();
        ticket(&s.h, "PROJ-1", "\u{200b}\u{200b}", 5);
        ticket(&s.h, "PROJ-2", "Real ticket", 1);
        let llm = FakeLlm::new();
        let r = task(&s, &llm).run().await;
        assert_eq!((r["embedded"].clone(), r["skippedItems"].clone()), (json!(1), json!(1)));
        assert_eq!(pending(&s), 0);
        assert_eq!(llm.calls.lock().unwrap().iter().flatten().count(), 1);
    }

    #[tokio::test]
    async fn a_failed_second_batch_keeps_the_first_batchs_vectors() {
        // Fails on the second call.
        struct Flaky(std::sync::Mutex<usize>);
        impl Embedder for Flaky {
            fn embedding_model(&self) -> Result<String, String> {
                Ok(MODEL.into())
            }
            async fn available(&self) -> bool {
                true
            }
            async fn detect_state(&self) -> String {
                "ready".into()
            }
            async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
                let mut n = self.0.lock().unwrap();
                *n += 1;
                if *n == 2 { Err("boom".into()) } else { Ok(texts.iter().map(|_| vec![1.0, 2.0, 3.0]).collect()) }
            }
        }
        let s = open();
        for i in 1..=20 {
            ticket(&s.h, &format!("PROJ-{i}"), &format!("Ticket {i}"), i);
        }
        let r = EmbedTask::new(Some(&s.h), &Flaky(Default::default()), json!({})).run().await;
        assert_eq!((r["outcome"].as_str(), r["embedded"].clone()), (Some("failed"), json!(16)));
        assert_eq!(s.h.vector_stats().unwrap()["vectors"], 16);
        assert_eq!(pending(&s), 4);
    }
}
