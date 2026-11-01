//! Wyoming event framing.
//!
//! An event is one JSON header line (`type`, optional `data`, `data_length`,
//! `payload_length`), then `data_length` bytes of JSON data, then
//! `payload_length` bytes of binary payload (audio). It runs on plain TCP, not
//! HTTP.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Wyoming library version that the events claim.
pub const VERSION: &str = "1.10.2";
const MAX_HEADER: usize = 64 * 1024;
const MAX_DATA: usize = 1024 * 1024;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: String,
    pub data: Map<String, Value>,
    pub payload: Option<Bytes>,
}

impl Event {
    pub fn new(kind: &str, data: Value) -> Self {
        Self {
            kind: kind.to_owned(),
            data: match data {
                Value::Object(m) => m,
                _ => Map::new(),
            },
            payload: None,
        }
    }

    pub fn with_payload(mut self, payload: Bytes) -> Self {
        self.payload = Some(payload);
        self
    }
}

fn length(header: &Value, key: &str, max: usize) -> Result<usize> {
    match header.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => {
            let n = v.as_u64().with_context(|| format!("bad {key}"))? as usize;
            if n > max {
                bail!("{key} {n} is larger than {max}");
            }
            Ok(n)
        }
    }
}

/// Reads one event. `None` means the peer closed the connection.
pub async fn read_event<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Event>> {
    let mut line = Vec::new();
    let n = (&mut *reader)
        .take(MAX_HEADER as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_HEADER {
        bail!("Wyoming header line too long");
    }
    let header: Value = serde_json::from_slice(&line).context("bad Wyoming header")?;
    let kind = header["type"]
        .as_str()
        .context("Wyoming header without type")?
        .to_owned();
    let mut data = match header.get("data") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let data_len = length(&header, "data_length", MAX_DATA)?;
    if data_len > 0 {
        let mut buf = vec![0; data_len];
        reader.read_exact(&mut buf).await?;
        if let Value::Object(extra) = serde_json::from_slice(&buf).context("bad Wyoming data")? {
            data.extend(extra);
        }
    }
    let payload_len = length(&header, "payload_length", MAX_PAYLOAD)?;
    let payload = if payload_len > 0 {
        let mut buf = vec![0; payload_len];
        reader.read_exact(&mut buf).await?;
        Some(Bytes::from(buf))
    } else {
        None
    };
    Ok(Some(Event { kind, data, payload }))
}

pub async fn write_event<W: AsyncWrite + Unpin>(writer: &mut W, event: &Event) -> Result<()> {
    let mut header = json!({"type": event.kind, "version": VERSION});
    let data = if event.data.is_empty() {
        None
    } else {
        Some(serde_json::to_vec(&event.data)?)
    };
    if let Some(d) = &data {
        header["data_length"] = json!(d.len());
    }
    if let Some(p) = &event.payload {
        header["payload_length"] = json!(p.len());
    }
    let mut out = serde_json::to_vec(&header)?;
    out.push(b'\n');
    if let Some(d) = data {
        out.extend_from_slice(&d);
    }
    writer.write_all(&out).await?;
    if let Some(p) = &event.payload {
        writer.write_all(p).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn roundtrip_with_payload() {
        let ev = Event::new("audio-chunk", json!({"rate": 16000, "width": 2, "channels": 1}))
            .with_payload(Bytes::from_static(&[1, 2, 3, 4]));
        let mut buf = Vec::new();
        write_event(&mut buf, &ev).await.unwrap();
        let mut r = BufReader::new(&buf[..]);
        assert_eq!(read_event(&mut r).await.unwrap().unwrap(), ev);
        assert!(read_event(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reads_python_framing() {
        // Exactly what the Python wyoming 1.10.2 library writes.
        let raw: &[u8] = b"{\"type\": \"audio-chunk\", \"version\": \"1.10.2\", \"data_length\": 61, \"payload_length\": 2}\n{\"rate\": 24000, \"width\": 2, \"channels\": 1, \"timestamp\": null}\x01\x02{\"type\": \"describe\"}\n{\"type\": \"synthesize\", \"data\": {\"text\": \"hi\"}}\n";
        let mut r = BufReader::new(raw);
        let a = read_event(&mut r).await.unwrap().unwrap();
        assert_eq!(a.kind, "audio-chunk");
        assert_eq!(a.data["rate"], 24000);
        assert_eq!(a.payload.as_deref(), Some(&[1u8, 2][..]));
        assert_eq!(read_event(&mut r).await.unwrap().unwrap().kind, "describe");
        assert_eq!(read_event(&mut r).await.unwrap().unwrap().data["text"], "hi");
    }
}
