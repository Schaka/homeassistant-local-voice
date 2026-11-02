//! Keeps llama-server's prompt cache (KV cache) across restarts.
//!
//! Home Assistant sends the same long system prompt with every request. After
//! a restart the first request must prefill all of it again: about 35 s for
//! 4.8k tokens on a CPU. llama-server can write a slot to a file and read it
//! back (`--slot-save-path`) in well under a second.
//!
//! The task restores the file once after each start of llama-server and saves
//! it again every 5 min when it changed. It never restores while a request
//! runs. A slow or failed health check is no restart: under full CPU load,
//! `/health` can answer late, and a restore then would overwrite the current
//! cache with the saved one. Every request would then prefill the full prompt
//! (7k tokens) again. `save_now` runs once at shutdown, before
//! llama-server stops. A save goes to `FILE.tmp` first and then replaces
//! `FILE`, so a kill in the middle never leaves a broken file.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::engine::{ActiveEngine, EngineKind};

const SAVE_EVERY: Duration = Duration::from_secs(300);
/// Smaller slots are side requests (titles), not Assist conversations.
const MIN_TOKENS: i64 = 1024;

#[derive(Clone)]
pub struct SlotCache {
    client: reqwest::Client,
    base: String,
    dir: PathBuf,
    file: String,
    /// The file of ik_llama.cpp, and the engine of the current start. Without
    /// them, every start uses `file`.
    per_engine: Option<(ActiveEngine, String)>,
    /// Start counter of the llama-server child process.
    starts: Arc<AtomicU64>,
    poll: Duration,
    save_every: Duration,
}

/// The files next to a saved slot: `FILE.checkpoints` and `FILE.tokens.json`
/// of ik_llama.cpp, which needs them to reuse the cache of a model with
/// recurrent layers. They carry the name of the slot file.
async fn sidecars(dir: &std::path::Path, file: &str) -> Vec<String> {
    let prefix = format!("{file}.");
    let mut found = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with(&prefix)
            {
                found.push(name.to_owned());
            }
        }
    }
    found
}

/// Renames `from` to `to`, and each `from.SUFFIX` to `to.SUFFIX`. Other files
/// that belong to `to` from an earlier save go away first.
async fn move_with_sidecars(dir: &std::path::Path, from: &str, to: &str) -> Result<()> {
    let from_prefix = format!("{from}.");
    for old in sidecars(dir, to).await {
        // `from` itself starts with `to.` when it is `to.tmp`.
        if old != from && !old.starts_with(&from_prefix) {
            let _ = tokio::fs::remove_file(dir.join(old)).await;
        }
    }
    tokio::fs::rename(dir.join(from), dir.join(to)).await?;
    for name in sidecars(dir, from).await {
        let suffix = &name[from.len()..];
        tokio::fs::rename(dir.join(&name), dir.join(format!("{to}{suffix}"))).await?;
    }
    Ok(())
}

async fn remove_with_sidecars(dir: &std::path::Path, file: &str) {
    let _ = tokio::fs::remove_file(dir.join(file)).await;
    for name in sidecars(dir, file).await {
        let _ = tokio::fs::remove_file(dir.join(name)).await;
    }
}

/// True while the slot runs a request. llama.cpp says `is_processing`,
/// ik_llama.cpp says `state` (0 is idle).
fn is_processing(slot: &Value) -> bool {
    slot["is_processing"]
        .as_bool()
        .or_else(|| slot["state"].as_i64().map(|s| s != 0))
        .unwrap_or(false)
}

/// The tokens in the slot. ik_llama.cpp does not count them, so about four
/// characters of its `prompt` text make one token.
fn prompt_tokens(slot: &Value) -> i64 {
    slot["n_prompt_tokens"]
        .as_i64()
        .or_else(|| {
            slot["prompt"]
                .as_str()
                .map(|p| i64::try_from(p.chars().count() / 4).unwrap_or(i64::MAX))
        })
        .unwrap_or(0)
}

impl SlotCache {
    pub fn new(port: u16, dir: PathBuf, file: String, starts: Arc<AtomicU64>, poll: Duration) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            base: format!("http://127.0.0.1:{port}"),
            dir,
            file,
            per_engine: None,
            starts,
            poll,
            save_every: SAVE_EVERY,
        })
    }

    /// The engines save different formats, so each reads only its own file.
    pub fn per_engine(mut self, active: ActiveEngine, ik_file: String) -> Self {
        self.per_engine = Some((active, ik_file));
        self
    }

    /// The file of the engine that runs now.
    fn file(&self) -> String {
        match &self.per_engine {
            Some((active, ik_file)) if active.get() == EngineKind::Ik => ik_file.clone(),
            _ => self.file.clone(),
        }
    }

    #[cfg(test)]
    fn with_save_every(mut self, every: Duration) -> Self {
        self.save_every = every;
        self
    }

    async fn call(&self, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let req = match body {
            Some(b) => self.client.post(url).json(&b),
            None => self.client.get(url),
        };
        let resp = req.timeout(timeout).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("{status}: {}", text.chars().take(200).collect::<String>());
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn healthy(&self) -> bool {
        self.call("/health", None, Duration::from_secs(10))
            .await
            .is_ok_and(|v| v["status"] == "ok")
    }

    /// The most recently used slot with an Assist-sized prompt, or None while
    /// any request runs.
    async fn pick_slot(&self) -> Result<Option<Value>> {
        let slots = self.call("/slots", None, Duration::from_secs(10)).await?;
        let mut best: Option<Value> = None;
        for slot in slots.as_array().context("/slots is not a list")? {
            if is_processing(slot) {
                return Ok(None);
            }
            if prompt_tokens(slot) < MIN_TOKENS {
                continue;
            }
            let task = slot["id_task"].as_i64().unwrap_or(-1);
            if best.as_ref().is_none_or(|b| task > b["id_task"].as_i64().unwrap_or(-1)) {
                best = Some(slot.clone());
            }
        }
        Ok(best)
    }

    /// True while any slot processes a request.
    async fn busy(&self) -> Result<bool> {
        let slots = self.call("/slots", None, Duration::from_secs(10)).await?;
        Ok(slots
            .as_array()
            .context("/slots is not a list")?
            .iter()
            .any(is_processing))
    }

    async fn save(&self, slot: &Value) -> Result<Option<i64>> {
        let id = slot["id"].as_i64().context("slot without id")?;
        let file = self.file();
        let tmp = format!("{file}.tmp");
        let res = self
            .call(
                &format!("/slots/{id}?action=save"),
                Some(json!({"filename": tmp})),
                Duration::from_secs(120),
            )
            .await?;
        move_with_sidecars(&self.dir, &tmp, &file).await?;
        tracing::info!(
            "prompt cache: saved slot {id}, {} tokens, {:.0} ms",
            res["n_saved"],
            res["timings"]["save_ms"].as_f64().unwrap_or(0.0)
        );
        Ok(slot["id_task"].as_i64())
    }

    async fn restore(&self) {
        let file = self.file();
        let path = self.dir.join(&file);
        if !path.is_file() {
            tracing::info!("prompt cache: no saved {file} yet, the first request prefills the full prompt");
            return;
        }
        match self
            .call(
                "/slots/0?action=restore",
                Some(json!({"filename": file})),
                // A good file restores in well under a second. ik_llama.cpp
                // never answers a file that it cannot read.
                Duration::from_secs(30),
            )
            .await
        {
            Ok(res) => tracing::info!(
                "prompt cache: restored {} tokens into slot 0, {:.0} ms",
                res["n_restored"],
                res["timings"]["restore_ms"].as_f64().unwrap_or(0.0)
            ),
            Err(err) => {
                // Wrong model or context size: the file is useless, start fresh.
                tracing::warn!("prompt cache: restore failed ({err}), removing {file}");
                remove_with_sidecars(&self.dir, &file).await;
            }
        }
    }

    /// Saves once if llama-server is up. Used at shutdown.
    pub async fn save_now(&self) {
        if !self.healthy().await {
            return;
        }
        match self.pick_slot().await {
            Ok(Some(slot)) => {
                if let Err(err) = self.save(&slot).await {
                    tracing::warn!("prompt cache: save failed: {err}");
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("prompt cache: save failed: {err}"),
        }
    }

    pub async fn run(self, cancel: CancellationToken) {
        // Start number whose cache was restored. 0 = none yet.
        let mut restored_start = 0u64;
        let mut last_task: Option<i64> = None;
        let mut next_save = Instant::now() + self.save_every;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(self.poll) => {}
            }
            let start = self.starts.load(Ordering::SeqCst);
            if start != restored_start {
                // llama-server started (again). Wait until it answers.
                if !self.healthy().await {
                    continue;
                }
                match self.busy().await {
                    Ok(false) => self.restore().await,
                    Ok(true) => tracing::info!("prompt cache: a request already runs, no restore"),
                    Err(_) => continue,
                }
                restored_start = start;
                last_task = None;
                next_save = Instant::now() + self.save_every;
                continue;
            }
            if start == 0 || Instant::now() < next_save {
                continue;
            }
            next_save = Instant::now() + self.save_every;
            match self.pick_slot().await {
                Ok(Some(slot)) if slot["id_task"].as_i64() != last_task => match self.save(&slot).await {
                    Ok(task) => last_task = task,
                    Err(err) => tracing::warn!("prompt cache: save failed: {err}"),
                },
                Ok(_) => {}
                Err(err) => tracing::warn!("prompt cache: save failed: {err}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{Fake, HealthMode, IK_SLOTS_BUSY, IK_SLOTS_IDLE, LLAMACPP_SLOTS_BUSY, LLAMACPP_SLOTS_IDLE};

    /// The recorded idle slots with the prompt size of slot 0 set to `tokens`.
    fn idle_slots(tokens: i64, task: i64) -> String {
        let mut v: Value = serde_json::from_str(LLAMACPP_SLOTS_IDLE).unwrap();
        v[0]["n_prompt_tokens"] = json!(tokens);
        v[0]["id_task"] = json!(task);
        v.to_string()
    }

    struct Rig {
        fake: Fake,
        cache: SlotCache,
        dir: tempfile::TempDir,
        starts: Arc<AtomicU64>,
    }

    async fn rig() -> Rig {
        let fake = Fake::default();
        let dir = tempfile::tempdir().unwrap();
        fake.set_save_dir(dir.path().to_path_buf());
        let port = fake.spawn().await;
        let starts = Arc::new(AtomicU64::new(0));
        let cache = SlotCache::new(
            port,
            dir.path().into(),
            "m.slot".into(),
            starts.clone(),
            Duration::from_millis(20),
        )
        .unwrap();
        Rig {
            fake,
            cache,
            dir,
            starts,
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(700)).await;
    }

    #[tokio::test]
    async fn a_small_prompt_is_a_side_request_and_is_not_saved() {
        let r = rig().await;
        // The recording has a prompt of 318 tokens: below an Assist prompt.
        assert!(r.cache.pick_slot().await.unwrap().is_none());
        r.cache.save_now().await;
        assert!(r.fake.seen_with("/slots/").is_empty());
    }

    #[tokio::test]
    async fn the_assist_sized_slot_is_chosen_and_nothing_is_chosen_while_a_request_runs() {
        let r = rig().await;
        r.fake.set_slots(&idle_slots(5000, 7));
        let slot = r.cache.pick_slot().await.unwrap().expect("an Assist slot");
        assert_eq!(slot["id"], 0);
        assert_eq!(slot["id_task"], 7);
        r.fake.set_slots(LLAMACPP_SLOTS_BUSY);
        assert!(r.cache.pick_slot().await.unwrap().is_none());
        assert!(r.cache.busy().await.unwrap());
        r.fake.set_slots(LLAMACPP_SLOTS_IDLE);
        assert!(!r.cache.busy().await.unwrap());
    }

    #[tokio::test]
    async fn the_newest_of_several_big_slots_wins() {
        let r = rig().await;
        let mut v: Value = serde_json::from_str(LLAMACPP_SLOTS_IDLE).unwrap();
        for (i, task) in [(0, 10), (1, 30), (2, 20)] {
            v[i]["n_prompt_tokens"] = json!(4000);
            v[i]["id_task"] = json!(task);
        }
        r.fake.set_slots(&v.to_string());
        assert_eq!(r.cache.pick_slot().await.unwrap().unwrap()["id"], 1);
    }

    #[tokio::test]
    async fn save_now_writes_the_file_through_a_temporary_name() {
        let r = rig().await;
        r.fake.set_slots(&idle_slots(5000, 7));
        r.cache.save_now().await;
        let calls = r.fake.seen_with("/slots/0?action=save");
        assert_eq!(calls.len(), 1);
        let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
        assert_eq!(body["filename"], "m.slot.tmp");
        assert!(r.dir.path().join("m.slot").is_file(), "the saved file is in place");
        assert!(!r.dir.path().join("m.slot.tmp").exists(), "no temporary file is left");
    }

    #[tokio::test]
    async fn save_now_does_nothing_while_the_server_does_not_answer_ok() {
        let r = rig().await;
        r.fake.set_slots(&idle_slots(5000, 7));
        r.fake.set_health(HealthMode::Loading);
        r.cache.save_now().await;
        assert!(r.fake.seen_with("/slots/").is_empty());
    }

    #[tokio::test]
    async fn a_start_restores_the_saved_file_once() {
        let r = rig().await;
        std::fs::write(r.dir.path().join("m.slot"), b"x").unwrap();
        let cancel = CancellationToken::new();
        tokio::spawn(r.cache.clone().run(cancel.clone()));
        settle().await;
        // The hub has not seen a start of the server yet.
        assert!(r.fake.seen_with("/slots/0?action=restore").is_empty());
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        let calls = r.fake.seen_with("/slots/0?action=restore");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&calls[0].body).unwrap()["filename"],
            "m.slot"
        );
        settle().await;
        assert_eq!(r.fake.seen_with("/slots/0?action=restore").len(), 1, "once per start");
        // A real restart restores again.
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        assert_eq!(r.fake.seen_with("/slots/0?action=restore").len(), 2);
        cancel.cancel();
    }

    #[tokio::test]
    async fn no_restore_without_a_saved_file() {
        let r = rig().await;
        let cancel = CancellationToken::new();
        tokio::spawn(r.cache.clone().run(cancel.clone()));
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        assert!(r.fake.seen_with("/slots/0?action=restore").is_empty());
        cancel.cancel();
    }

    #[tokio::test]
    async fn no_restore_while_a_request_runs_and_none_later_for_the_same_start() {
        let r = rig().await;
        std::fs::write(r.dir.path().join("m.slot"), b"x").unwrap();
        r.fake.set_slots(LLAMACPP_SLOTS_BUSY);
        let cancel = CancellationToken::new();
        tokio::spawn(r.cache.clone().run(cancel.clone()));
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        assert!(r.fake.seen_with("/slots/0?action=restore").is_empty());
        // The request ends. A restore now would replace the live cache.
        r.fake.set_slots(LLAMACPP_SLOTS_IDLE);
        settle().await;
        assert!(r.fake.seen_with("/slots/0?action=restore").is_empty());
        cancel.cancel();
    }

    #[tokio::test]
    async fn a_failed_restore_removes_the_file() {
        let r = rig().await;
        let file = r.dir.path().join("m.slot");
        std::fs::write(&file, b"x").unwrap();
        r.fake.set_restore_status(400);
        let cancel = CancellationToken::new();
        tokio::spawn(r.cache.clone().run(cancel.clone()));
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        assert_eq!(r.fake.seen_with("/slots/0?action=restore").len(), 1);
        assert!(!file.exists(), "a file that cannot be restored is removed");
        cancel.cancel();
    }

    #[tokio::test]
    async fn failed_health_checks_do_not_restore_again() {
        let r = rig().await;
        std::fs::write(r.dir.path().join("m.slot"), b"x").unwrap();
        let cancel = CancellationToken::new();
        tokio::spawn(r.cache.clone().run(cancel.clone()));
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        // The server is busy and answers 503 for a while, then recovers.
        r.fake.set_health(HealthMode::Loading);
        settle().await;
        r.fake.set_health(HealthMode::Ok);
        settle().await;
        assert_eq!(r.fake.seen_with("/slots/0?action=restore").len(), 1);
        cancel.cancel();
    }

    #[tokio::test]
    async fn the_periodic_save_runs_only_when_the_conversation_changed() {
        let r = rig().await;
        r.fake.set_slots(&idle_slots(5000, 7));
        let cache = r.cache.clone().with_save_every(Duration::from_millis(100));
        r.starts.fetch_add(1, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        tokio::spawn(cache.run(cancel.clone()));
        tokio::time::sleep(Duration::from_millis(900)).await;
        let saves = |f: &Fake| f.seen_with("/slots/0?action=save").len();
        assert_eq!(saves(&r.fake), 1, "the same task is saved once");
        r.fake.set_slots(&idle_slots(5200, 8));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(saves(&r.fake), 2, "a new task is saved again");
        cancel.cancel();
    }

    // ---- ik_llama.cpp ----

    /// The recorded idle slot of ik_llama.cpp with a prompt of `chars` characters.
    fn ik_slots(chars: usize, task: i64) -> String {
        let mut v: Value = serde_json::from_str(IK_SLOTS_IDLE).unwrap();
        v[0]["prompt"] = json!("x".repeat(chars));
        v[0]["id_task"] = json!(task);
        v.to_string()
    }

    #[test]
    fn both_engines_say_in_their_own_words_that_a_slot_is_busy() {
        let busy = |s: &str| is_processing(&serde_json::from_str::<Value>(s).unwrap()[0]);
        assert!(!busy(LLAMACPP_SLOTS_IDLE));
        assert!(!busy(IK_SLOTS_IDLE));
        assert!(busy(IK_SLOTS_BUSY));
        assert!(
            serde_json::from_str::<Value>(LLAMACPP_SLOTS_BUSY)
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(is_processing)
        );
        assert!(!is_processing(&json!({})), "no field: idle");
    }

    #[test]
    fn the_tokens_of_a_slot_are_counted_or_estimated() {
        assert_eq!(prompt_tokens(&json!({"n_prompt_tokens": 318})), 318);
        assert_eq!(prompt_tokens(&json!({"prompt": "x".repeat(4000)})), 1000);
        assert_eq!(
            prompt_tokens(&json!({"prompt": "é".repeat(400)})),
            100,
            "characters, not bytes"
        );
        assert_eq!(prompt_tokens(&json!({})), 0);
    }

    #[tokio::test]
    async fn the_slot_of_ik_is_picked_and_a_busy_one_blocks() {
        let r = rig().await;
        r.fake.set_slots(IK_SLOTS_IDLE);
        assert!(
            r.cache.pick_slot().await.unwrap().is_none(),
            "a short prompt is a side request"
        );
        r.fake.set_slots(&ik_slots(20_000, 9));
        let slot = r.cache.pick_slot().await.unwrap().expect("an Assist slot");
        assert_eq!((slot["id"].as_i64(), slot["id_task"].as_i64()), (Some(0), Some(9)));
        r.fake.set_slots(IK_SLOTS_BUSY);
        assert!(r.cache.pick_slot().await.unwrap().is_none());
        assert!(r.cache.busy().await.unwrap());
    }

    #[tokio::test]
    async fn each_engine_saves_and_restores_its_own_file() {
        let r = rig().await;
        let active = ActiveEngine::new(EngineKind::LlamaCpp);
        let cache = r.cache.clone().per_engine(active.clone(), "m-ik.slot".into());
        r.fake.set_slots(&idle_slots(5000, 7));
        cache.save_now().await;
        assert!(r.dir.path().join("m.slot").is_file());
        assert!(!r.dir.path().join("m-ik.slot").exists());

        active.set(EngineKind::Ik);
        r.fake.set_slots(&ik_slots(20_000, 8));
        cache.save_now().await;
        assert!(r.dir.path().join("m-ik.slot").is_file());

        // A start of ik_llama.cpp restores its file and leaves the other alone.
        let cancel = CancellationToken::new();
        tokio::spawn(cache.run(cancel.clone()));
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        let calls = r.fake.seen_with("/slots/0?action=restore");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&calls[0].body).unwrap()["filename"],
            "m-ik.slot"
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn a_start_on_another_engine_does_not_read_the_file_of_the_first_one() {
        let r = rig().await;
        std::fs::write(r.dir.path().join("m.slot"), b"x").unwrap();
        let active = ActiveEngine::new(EngineKind::Ik);
        let cache = r.cache.clone().per_engine(active, "m-ik.slot".into());
        let cancel = CancellationToken::new();
        tokio::spawn(cache.run(cancel.clone()));
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        assert!(r.fake.seen_with("/slots/0?action=restore").is_empty());
        assert!(r.dir.path().join("m.slot").is_file(), "the other file stays");
        cancel.cancel();
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_str().unwrap().to_owned())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn the_files_next_to_a_saved_slot_follow_its_name() {
        let r = rig().await;
        r.fake.set_sidecars(true);
        r.fake.set_slots(&idle_slots(5000, 7));
        r.cache.save_now().await;
        assert_eq!(
            names(r.dir.path()),
            ["m.slot", "m.slot.checkpoints", "m.slot.tokens.json"],
            "no file keeps the temporary name"
        );
        // A second save replaces them.
        r.fake.set_slots(&idle_slots(5000, 8));
        r.cache.save_now().await;
        assert_eq!(names(r.dir.path()).len(), 3);
    }

    #[tokio::test]
    async fn a_failed_restore_removes_the_files_next_to_the_slot_too() {
        let r = rig().await;
        for f in ["m.slot", "m.slot.checkpoints", "m.slot.tokens.json", "other.slot"] {
            std::fs::write(r.dir.path().join(f), b"x").unwrap();
        }
        r.fake.set_restore_status(400);
        let cancel = CancellationToken::new();
        tokio::spawn(r.cache.clone().run(cancel.clone()));
        r.starts.fetch_add(1, Ordering::SeqCst);
        settle().await;
        assert_eq!(names(r.dir.path()), ["other.slot"], "files of other slots stay");
        cancel.cancel();
    }
}
