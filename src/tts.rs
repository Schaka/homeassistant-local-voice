//! Text to speech with audio.cpp (PocketTTS) through audiocpp_server, which
//! runs as a child process on 127.0.0.1.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Result, bail};
use bytes::Bytes;
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::json;

use crate::config::TtsConfig;
use crate::supervisor::ChildSpec;

pub trait TextToSpeech: Send + Sync {
    /// Returns a WAV file.
    fn synthesize<'a>(&'a self, text: &'a str, voice: Option<&'a str>) -> BoxFuture<'a, Result<Bytes>>;
    fn healthy(&self) -> BoxFuture<'_, bool>;
}

pub struct AudioCpp {
    client: reqwest::Client,
    base: String,
    model_id: String,
    default_voice: String,
}

impl AudioCpp {
    pub fn new(cfg: &TtsConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            base: format!("http://127.0.0.1:{}", cfg.internal_port),
            model_id: cfg.model_id.clone(),
            default_voice: cfg.voice_id.clone(),
        })
    }
}

impl TextToSpeech for AudioCpp {
    fn synthesize<'a>(&'a self, text: &'a str, voice: Option<&'a str>) -> BoxFuture<'a, Result<Bytes>> {
        Box::pin(async move {
            let resp = self
                .client
                .post(format!("{}/v1/audio/speech", self.base))
                .json(&json!({
                    "model": self.model_id,
                    "input": text,
                    "voice": voice.unwrap_or(&self.default_voice),
                }))
                .timeout(Duration::from_secs(120))
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                // "http error 500" is one of the GPU fault signs (gpu.rs).
                bail!(
                    "HTTP error {}: {}",
                    status.as_u16(),
                    body.chars().take(200).collect::<String>()
                );
            }
            Ok(resp.bytes().await?)
        })
    }

    fn healthy(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            self.client
                .get(format!("{}/health", self.base))
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
        })
    }
}

/// The audiocpp_server child process and its configuration file.
pub fn child_spec(cfg: &TtsConfig) -> Result<ChildSpec> {
    let config = json!({
        "host": "127.0.0.1",
        "port": cfg.internal_port,
        "backend": cfg.backend.name(),
        "device": cfg.vk_device,
        "threads": cfg.threads.unwrap_or(2),
        "lazy_load": true,
        "models": [{
            "id": cfg.model_id,
            "family": "pocket_tts",
            "path": cfg.dir,
            "task": "tts",
            "mode": "offline",
            "load_options": {"language": cfg.language},
            "session_options": {"language": cfg.language},
            "default_voice_preset": {"voice_id": cfg.voice_id},
        }],
    });
    let path = std::env::temp_dir().join("audiocpp-server.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    Ok(ChildSpec {
        name: "audiocpp".into(),
        program: cfg.server_bin.clone(),
        args: vec!["--config".into(), path.display().to_string()],
        env: vec![],
        env_remove: vec![],
        starts: Default::default(),
    })
}

static CODE_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```.*?```").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]*\)").unwrap());
static EMPHASIS: LazyLock<[Regex; 4]> =
    LazyLock::new(|| [r"\*\*(.+?)\*\*", r"__(.+?)__", r"\*(.+?)\*", r"`(.+?)`"].map(|p| Regex::new(p).unwrap()));
static LINE_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(#{1,6}\s+|[-*+•]\s+|\d+[.)]\s+|>\s*)").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Turns markdown from the conversation agent into plain spoken sentences.
///
/// Small local models ignore "no markdown" in the prompt now and then. The TTS
/// model reads "**" and "-" aloud or stumbles on them, so this removes the
/// markup and joins list items and lines into sentences.
pub fn speakable(text: &str) -> String {
    let mut text = CODE_BLOCK.replace_all(text, " ").into_owned();
    text = LINK.replace_all(&text, "$1").into_owned();
    for re in EMPHASIS.iter() {
        text = re.replace_all(&text, "$1").into_owned();
    }
    let mut sentences = Vec::new();
    for line in text.lines() {
        let line = LINE_PREFIX.replace(line, "");
        let line = line.trim_matches(|c| " *_`#|".contains(c));
        if line.is_empty() {
            continue;
        }
        let mut line = line.to_owned();
        if !line.ends_with(['.', '!', '?', ':', ';', ',']) {
            line.push('.');
        }
        sentences.push(line);
    }
    let text = sentences.join(" ").replace("°C", " degrees").replace('°', " degrees");
    SPACES.replace_all(&text, " ").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_sentences() {
        assert_eq!(
            speakable("**Today** it is 12°C.\n- rain in the *evening*\n- see [DWD](https://dwd.de)"),
            "Today it is 12 degrees. rain in the evening. see DWD."
        );
        assert_eq!(speakable("# Weather\n1. Sunny\n2) Warm!"), "Weather. Sunny. Warm!");
        assert_eq!(speakable("Use `code`:\n```\nx = 1\n```\nDone"), "Use code: Done.");
        assert_eq!(speakable("  \n "), "");
    }
}
