//! Small Wyoming and HTTP clients for checks: `assistant-hub probe ...` and
//! `assistant-hub healthcheck`. They need nothing else in the container.

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

/// GET /health over plain TCP. Exit code 0 when the server answers 200.
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
    Ok(())
}
