//! Wyoming STT and TTS for Home Assistant on the main port.

pub mod protocol;

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use crate::audio::{self, Format};
use crate::gpu::Gpu;
use crate::stt::{self, SpeechToText};
use crate::tts::{self, TextToSpeech};
use protocol::{Event, read_event, write_event};

/// Samples per TTS audio chunk.
const CHUNK_SAMPLES: usize = 4096;

pub struct Voice {
    pub stt: Option<Arc<dyn SpeechToText>>,
    pub tts: Arc<dyn TextToSpeech>,
    pub gpu: Gpu,
    pub info: Value,
    pub debug_dir: Option<std::path::PathBuf>,
}

/// The `info` answer to `describe`. Field for field the same as the Python
/// server sent, so Home Assistant sees the same services (`parakeet`, `audiocpp`).
pub fn info(stt_model_name: &str, tts_voice: &str, tts_languages: &[String]) -> Value {
    json!({
        "asr": [{
            "name": "parakeet",
            "attribution": {"name": "parakeet.cpp", "url": "https://github.com/mudler/parakeet.cpp"},
            "installed": true,
            "description": "parakeet.cpp ASR (ggml, Vulkan)",
            "version": "0.5.0",
            "models": [{
                "name": stt_model_name,
                "attribution": {"name": "NVIDIA NeMo", "url": "https://github.com/NVIDIA-NeMo/NeMo"},
                "installed": true,
                "description": format!("Parakeet (English, {stt_model_name})"),
                "version": "0.5.0",
                "languages": ["en"],
            }],
            "supports_transcript_streaming": false,
            "requires_external_vad": true,
            "prefers_auto_gain_enabled": true,
            "prefers_noise_reduction_enabled": true,
        }],
        "tts": [{
            "name": "audiocpp",
            "attribution": {"name": "audio.cpp", "url": "https://github.com/0xShug0/audio.cpp"},
            "installed": true,
            "description": "audio.cpp PocketTTS (ggml, Vulkan)",
            "version": "0.6.0",
            "voices": [{
                "name": tts_voice,
                "attribution": {"name": "Kyutai", "url": "https://github.com/Kyutai-Labs/pocket-tts"},
                "installed": true,
                "description": tts_voice,
                "version": null,
                "languages": tts_languages,
                "speakers": null,
            }],
            "supports_synthesize_streaming": false,
        }],
        "handle": [],
        "intent": [],
        "wake": [],
        "mic": [],
        "snd": [],
    })
}

fn format_of(data: &serde_json::Map<String, Value>, fallback: Format) -> Format {
    let get = |k: &str| data.get(k).and_then(Value::as_u64);
    Format {
        rate: get("rate").map_or(fallback.rate, |v| v as u32),
        width: get("width").map_or(fallback.width, |v| v as u16),
        channels: get("channels").map_or(fallback.channels, |v| v as u16),
    }
}

impl Voice {
    async fn transcribe(&self, pcm: Vec<i16>) -> Result<String> {
        let Some(stt) = self.stt.clone() else {
            anyhow::bail!("STT is not available (the model failed to load, see the log)");
        };
        let guard = self.gpu.acquire().await?;
        let samples: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            stt.transcribe(&samples)
        })
        .await?;
        match result {
            Ok(text) => {
                self.gpu.success();
                if let Some(dir) = self.debug_dir.clone() {
                    let text = text.clone();
                    tokio::task::spawn_blocking(move || stt::save_debug_recording(&dir, &pcm, &text));
                }
                Ok(text)
            }
            Err(err) => {
                self.gpu.failure(&err);
                Err(err)
            }
        }
    }

    async fn synthesize(&self, text: &str, voice: Option<&str>) -> Result<Bytes> {
        let _guard = self.gpu.acquire().await?;
        match self.tts.synthesize(text, voice).await {
            Ok(wav) => {
                self.gpu.success();
                Ok(wav)
            }
            Err(err) => {
                self.gpu.failure(&err);
                Err(err)
            }
        }
    }
}

/// Serves one Wyoming connection until the peer closes it or an engine fails.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(stream: S, voice: Arc<Voice>) -> Result<()> {
    let (r, w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);
    let mut writer = BufWriter::new(w);
    let mut format = Format::STT;
    // Raw audio as received, one segment per format. It is converted once at
    // audio-stop, so resampling has no seams at chunk borders.
    let mut audio: Vec<(Format, Vec<u8>)> = Vec::new();

    while let Some(event) = read_event(&mut reader).await? {
        tracing::debug!("wyoming event: {}", event.kind);
        match event.kind.as_str() {
            "describe" => {
                write_event(&mut writer, &Event::new("info", voice.info.clone())).await?;
            }
            // English-only model, the language is ignored.
            "transcribe" => {}
            "audio-start" => {
                format = format_of(&event.data, Format::STT);
                tracing::info!(
                    "audio-start: {} Hz/{}-bit/{}ch",
                    format.rate,
                    format.width * 8,
                    format.channels
                );
                audio.clear();
            }
            "audio-chunk" => {
                let chunk_format = format_of(&event.data, format);
                if let Some(p) = &event.payload {
                    match audio.last_mut() {
                        Some((f, buf)) if *f == chunk_format => buf.extend_from_slice(p),
                        _ => audio.push((chunk_format, p.to_vec())),
                    }
                }
            }
            "audio-stop" => {
                let pcm: Vec<i16> = std::mem::take(&mut audio)
                    .into_iter()
                    .flat_map(|(f, raw)| audio::to_mono_i16(&raw, f, stt::RATE))
                    .collect();
                let text = if pcm.is_empty() {
                    tracing::warn!("STT: audio-stop with no audio received");
                    String::new()
                } else {
                    let bytes = pcm.len() * 2;
                    match voice.transcribe(pcm).await {
                        Ok(text) if text.is_empty() => {
                            tracing::warn!("STT: empty transcript for {bytes} bytes");
                            text
                        }
                        Ok(text) => {
                            tracing::info!("STT: {text}");
                            text
                        }
                        Err(err) => {
                            tracing::error!("STT failed: {err:#}");
                            return Err(err);
                        }
                    }
                };
                // Always answer with a transcript, also an empty one. Home
                // Assistant waits for it; without it the satellite stays in
                // "listening".
                write_event(&mut writer, &Event::new("transcript", json!({"text": text}))).await?;
            }
            "synthesize" => {
                let text = tts::speakable(event.data.get("text").and_then(Value::as_str).unwrap_or(""));
                let voice_name = event.data.get("voice").and_then(|v| {
                    v.get("name")
                        .and_then(Value::as_str)
                        .or_else(|| v.get("language").and_then(Value::as_str))
                });
                if text.is_empty() {
                    tracing::warn!("empty synthesis text");
                    write_event(&mut writer, &Event::new("audio-stop", json!({"timestamp": null}))).await?;
                } else {
                    let wav = match voice.synthesize(&text, voice_name).await {
                        Ok(wav) => wav,
                        Err(err) => {
                            tracing::error!("TTS failed: {err:#}");
                            return Err(err);
                        }
                    };
                    let (fmt, pcm) = audio::parse_wav(&wav)?;
                    tracing::info!(
                        "TTS: {} chars -> {} Hz/{}-bit/{}ch (voice={voice_name:?})",
                        text.chars().count(),
                        fmt.rate,
                        fmt.width * 8,
                        fmt.channels
                    );
                    let audio_data = json!({"rate": fmt.rate, "width": fmt.width, "channels": fmt.channels});
                    let mut start = audio_data.clone();
                    start["timestamp"] = Value::Null;
                    write_event(&mut writer, &Event::new("audio-start", start)).await?;
                    let chunk_bytes = CHUNK_SAMPLES * usize::from(fmt.width) * usize::from(fmt.channels);
                    for chunk in pcm.chunks(chunk_bytes.max(1)) {
                        let mut data = audio_data.clone();
                        data["timestamp"] = Value::Null;
                        let ev = Event::new("audio-chunk", data).with_payload(Bytes::copy_from_slice(chunk));
                        write_event(&mut writer, &ev).await?;
                    }
                    write_event(&mut writer, &Event::new("audio-stop", json!({"timestamp": null}))).await?;
                }
            }
            other => tracing::debug!("wyoming: ignoring {other}"),
        }
        writer.flush().await?;
    }
    Ok(())
}
