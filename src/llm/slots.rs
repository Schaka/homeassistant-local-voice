//! Keeps llama-server's prompt cache (KV cache) across restarts.
//!
//! Home Assistant sends the same long system prompt with every request. After
//! a restart the first request must prefill all of it again: about 35 s for
//! 4.8k tokens on a CPU. llama-server can write a slot to a file and read it
//! back (`--slot-save-path`) in well under a second.
//!
//! The task restores the file each time llama-server comes up and saves it
//! again every 5 min when it changed. `save_now` runs once at shutdown, before
//! llama-server stops. A save goes to `FILE.tmp` first and then replaces
//! `FILE`, so a kill in the middle never leaves a broken file.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const POLL: Duration = Duration::from_secs(5);
const SAVE_EVERY: Duration = Duration::from_secs(300);
/// Smaller slots are side requests (titles), not Assist conversations.
const MIN_TOKENS: i64 = 1024;

#[derive(Clone)]
pub struct SlotCache {
    client: reqwest::Client,
    base: String,
    dir: PathBuf,
    file: String,
}

impl SlotCache {
    pub fn new(port: u16, dir: PathBuf, file: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            base: format!("http://127.0.0.1:{port}"),
            dir,
            file,
        })
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
        self.call("/health", None, Duration::from_secs(3))
            .await
            .is_ok_and(|v| v["status"] == "ok")
    }

    /// The most recently used slot with an Assist-sized prompt, or None while
    /// any request runs.
    async fn pick_slot(&self) -> Result<Option<Value>> {
        let slots = self.call("/slots", None, Duration::from_secs(10)).await?;
        let mut best: Option<Value> = None;
        for slot in slots.as_array().context("/slots is not a list")? {
            if slot["is_processing"].as_bool().unwrap_or(false) {
                return Ok(None);
            }
            if slot["n_prompt_tokens"].as_i64().unwrap_or(0) < MIN_TOKENS {
                continue;
            }
            let task = slot["id_task"].as_i64().unwrap_or(-1);
            if best.as_ref().is_none_or(|b| task > b["id_task"].as_i64().unwrap_or(-1)) {
                best = Some(slot.clone());
            }
        }
        Ok(best)
    }

    async fn save(&self, slot: &Value) -> Result<Option<i64>> {
        let id = slot["id"].as_i64().context("slot without id")?;
        let tmp = format!("{}.tmp", self.file);
        let res = self
            .call(
                &format!("/slots/{id}?action=save"),
                Some(json!({"filename": tmp})),
                Duration::from_secs(120),
            )
            .await?;
        tokio::fs::rename(self.dir.join(&tmp), self.dir.join(&self.file)).await?;
        tracing::info!(
            "prompt cache: saved slot {id}, {} tokens, {:.0} ms",
            res["n_saved"],
            res["timings"]["save_ms"].as_f64().unwrap_or(0.0)
        );
        Ok(slot["id_task"].as_i64())
    }

    async fn restore(&self) {
        let path = self.dir.join(&self.file);
        if !path.is_file() {
            tracing::info!(
                "prompt cache: no saved {} yet, the first request prefills the full prompt",
                self.file
            );
            return;
        }
        match self
            .call(
                "/slots/0?action=restore",
                Some(json!({"filename": self.file})),
                Duration::from_secs(120),
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
                tracing::warn!("prompt cache: restore failed ({err}), removing {}", self.file);
                let _ = tokio::fs::remove_file(&path).await;
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
        let mut up = false;
        let mut last_task: Option<i64> = None;
        let mut next_save = Instant::now() + SAVE_EVERY;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(POLL) => {}
            }
            if !self.healthy().await {
                up = false;
                continue;
            }
            if !up {
                // First start, or llama-server came back after a crash.
                up = true;
                last_task = None;
                self.restore().await;
                next_save = Instant::now() + SAVE_EVERY;
            } else if Instant::now() >= next_save {
                next_save = Instant::now() + SAVE_EVERY;
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
}
