//! Small Wyoming and HTTP clients for checks: `assistant-hub probe ...` and
//! `assistant-hub healthcheck`. They need nothing else in the container, and
//! they also work from another host with `--addr`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::audio;
use crate::wyoming::protocol::{Event, read_event, write_event};

const TIMEOUT: Duration = Duration::from_secs(120);

async fn connect(
    addr: &str,
) -> Result<(
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
)> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("cannot connect to {addr}"))?;
    let (r, w) = stream.into_split();
    Ok((BufReader::new(r), w))
}

async fn next(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<Event> {
    tokio::time::timeout(TIMEOUT, read_event(reader))
        .await
        .context("timeout")??
        .context("server closed the connection")
}

pub async fn describe(addr: &str) -> Result<()> {
    let (mut r, mut w) = connect(addr).await?;
    write_event(&mut w, &Event::new("describe", json!({}))).await?;
    let ev = next(&mut r).await?;
    println!("{}", serde_json::to_string_pretty(&ev.data)?);
    Ok(())
}

pub async fn stt(addr: &str, wav: &Path) -> Result<()> {
    let data = std::fs::read(wav)?;
    let (fmt, pcm) = audio::parse_wav(&data)?;
    let (mut r, mut w) = connect(addr).await?;
    let f = json!({"rate": fmt.rate, "width": fmt.width, "channels": fmt.channels});
    write_event(&mut w, &Event::new("transcribe", json!({}))).await?;
    write_event(&mut w, &Event::new("audio-start", f.clone())).await?;
    for chunk in pcm.chunks(32_000) {
        write_event(
            &mut w,
            &Event::new("audio-chunk", f.clone()).with_payload(Bytes::copy_from_slice(chunk)),
        )
        .await?;
    }
    write_event(&mut w, &Event::new("audio-stop", json!({}))).await?;
    let start = std::time::Instant::now();
    loop {
        let ev = next(&mut r).await?;
        if ev.kind == "transcript" {
            println!("{}", ev.data.get("text").and_then(|t| t.as_str()).unwrap_or(""));
            eprintln!("({:.2} s)", start.elapsed().as_secs_f64());
            return Ok(());
        }
    }
}

pub async fn tts(addr: &str, text: &str, out: &Path) -> Result<()> {
    let (mut r, mut w) = connect(addr).await?;
    write_event(&mut w, &Event::new("synthesize", json!({"text": text}))).await?;
    let start = std::time::Instant::now();
    let mut pcm: Vec<i16> = Vec::new();
    let mut rate = 0u32;
    let mut channels = 1u16;
    loop {
        let ev = next(&mut r).await?;
        match ev.kind.as_str() {
            "audio-chunk" => {
                rate = ev.data.get("rate").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                channels = ev.data.get("channels").and_then(|v| v.as_u64()).unwrap_or(1) as u16;
                if ev.data.get("width").and_then(|v| v.as_u64()) != Some(2) {
                    bail!("only 16-bit audio is supported here");
                }
                if let Some(p) = ev.payload {
                    pcm.extend(p.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)));
                }
            }
            "audio-stop" => break,
            _ => {}
        }
    }
    std::fs::write(out, audio::wav_bytes(&pcm, rate, channels))?;
    eprintln!(
        "saved {} ({} Hz, {:.1} s of audio in {:.2} s)",
        out.display(),
        rate,
        pcm.len() as f64 / f64::from(rate.max(1)) / f64::from(channels),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The system prompt of `probe llm`: two devices plus `devices` filler entities.
pub fn llm_system_prompt(devices: usize) -> String {
    let mut prompt = String::from(
        "You are a voice assistant for a smart home. Answer in one short sentence. \
         Devices: light.kitchen 'Kitchen Light' (area: Kitchen) = off; \
         light.bedroom 'Bedroom Light' (area: Bedroom) = on.",
    );
    for i in 0..devices {
        prompt.push_str(&format!(
            "\nsensor.room_{i}_temperature 'Room {i} Temperature' (area: Room {i}) = {}.5 °C;\
             unit_of_measurement=°C;device_class=temperature",
            18 + i % 7
        ));
    }
    prompt
}

/// One chat completion with a HassTurnOn tool, the way Home Assistant's
/// llama.cpp integration sends it. Prints the reply, the tool calls and the
/// prompt and generation speed of llama-server.
pub async fn llm(addr: &str, prompt: &str, devices: usize, key: Option<&str>) -> Result<()> {
    let base = format!("http://{addr}/llm/v1");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(600)).build()?;
    let auth = |r: reqwest::RequestBuilder| match key {
        Some(k) => r.bearer_auth(k),
        None => r,
    };
    let models: serde_json::Value = auth(client.get(format!("{base}/models")))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let model = models["data"][0]["id"]
        .as_str()
        .context("no model in /llm/v1/models")?
        .to_owned();
    println!("model: {model}");
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": llm_system_prompt(devices)},
            {"role": "user", "content": prompt},
        ],
        "tools": [{"type": "function", "function": {
            "name": "HassTurnOn",
            "description": "Turns on/opens a device or entity",
            "parameters": {"type": "object", "properties": {
                "name": {"type": "string"},
                "area": {"type": "string"},
                "domain": {"type": "array", "items": {"type": "string"}},
            }},
        }}],
    });
    let start = std::time::Instant::now();
    let out: serde_json::Value = auth(client.post(format!("{base}/chat/completions")).json(&body))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let msg = &out["choices"][0]["message"];
    if let Some(r) = msg["reasoning_content"].as_str().filter(|r| !r.is_empty()) {
        println!("reasoning: {} chars (not spoken)", r.chars().count());
    }
    println!("content: {}", msg["content"].as_str().unwrap_or(""));
    for call in msg["tool_calls"].as_array().into_iter().flatten() {
        println!(
            "tool_call: {} {}",
            call["function"]["name"], call["function"]["arguments"]
        );
    }
    let t = &out["timings"];
    println!(
        "wall {:.2} s, prompt {} tokens at {:.1} tokens/s, generation {:.1} tokens/s",
        start.elapsed().as_secs_f64(),
        t["prompt_n"],
        t["prompt_per_second"].as_f64().unwrap_or(0.0),
        t["predicted_per_second"].as_f64().unwrap_or(0.0)
    );
    Ok(())
}

/// GET /health over plain TCP. Exit code 0 only when the server answers 200
/// *and* reports `"status": "ok"`.
pub async fn healthcheck(addr: &str) -> Result<()> {
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await??;
    stream
        .write_all(format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
        .await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf)).await??;
    if !buf.starts_with(b"HTTP/1.1 200") {
        bail!(
            "unhealthy: {}",
            String::from_utf8_lossy(&buf).lines().next().unwrap_or("")
        );
    }
    let text = String::from_utf8_lossy(&buf);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    let status = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["status"].as_str().map(str::to_owned))
        .unwrap_or_default();
    if status != "ok" {
        bail!("unhealthy: {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Answers one request with `response`, then closes the connection.
    async fn fake_health(response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            stream.write_all(response).await.unwrap();
        });
        addr.to_string()
    }

    #[tokio::test]
    async fn healthcheck_accepts_only_ok() {
        let ok = fake_health(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{\"status\":\"ok\"}").await;
        assert!(healthcheck(&ok).await.is_ok());

        let degraded = fake_health(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{\"status\":\"degraded\"}").await;
        assert!(healthcheck(&degraded).await.is_err());

        let error = fake_health(b"HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\n\r\n{}").await;
        assert!(healthcheck(&error).await.is_err());
    }

    #[test]
    fn filler_devices_grow_the_prompt() {
        let short = llm_system_prompt(0);
        let long = llm_system_prompt(100);
        assert!(short.contains("light.kitchen"));
        assert_eq!(long.lines().count(), 101);
        assert!(long.contains("sensor.room_99_temperature"));
    }
}
