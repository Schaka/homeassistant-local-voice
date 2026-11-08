//! A fake llama-server for tests. It answers like the real one: the bodies
//! come from the recordings in `tests/fixtures/llm/<engine>/`
//! (`scripts/record-llm-fixtures.sh`). Tests change its behavior (loading,
//! hang, error status, slow stream) and read back what the hub sent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

pub const LLAMACPP_CHAT: &str = include_str!("../../tests/fixtures/llm/llamacpp/chat_tool_call.json");
pub const LLAMACPP_STREAM: &str = include_str!("../../tests/fixtures/llm/llamacpp/chat_stream.sse");
pub const LLAMACPP_MODELS: &str = include_str!("../../tests/fixtures/llm/llamacpp/models.json");
pub const LLAMACPP_SLOTS_IDLE: &str = include_str!("../../tests/fixtures/llm/llamacpp/slots_idle.json");
pub const LLAMACPP_SLOTS_BUSY: &str = include_str!("../../tests/fixtures/llm/llamacpp/slots_busy.json");
pub const IK_SLOTS_IDLE: &str = include_str!("../../tests/fixtures/llm/ik/slots_idle.json");
pub const IK_SLOTS_BUSY: &str = include_str!("../../tests/fixtures/llm/ik/slots_busy.json");
pub const LLAMACPP_SLOT_SAVE: &str = include_str!("../../tests/fixtures/llm/llamacpp/slot_save.json");
pub const LLAMACPP_SLOT_RESTORE: &str = include_str!("../../tests/fixtures/llm/llamacpp/slot_restore.json");

/// How `/health` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthMode {
    Ok,
    /// 503 while the model loads.
    Loading,
    /// No answer for 5 s, like an engine that hangs.
    Hang,
}

/// One request that reached the fake.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// Path and query as sent.
    pub target: String,
    /// Header names in lower case.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

struct Inner {
    health: Mutex<HealthMode>,
    seen: Mutex<Vec<Seen>>,
    slots: Mutex<String>,
    chat_status: Mutex<u16>,
    stream_gap: Mutex<Duration>,
    save_dir: Mutex<Option<PathBuf>>,
    restore_status: Mutex<u16>,
    streams_started: AtomicUsize,
    streams_cut: AtomicUsize,
    chat_script: Mutex<Vec<String>>,
    stream_script: Mutex<Vec<String>>,
    chat_calls: AtomicUsize,
    sidecars: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Fake(Arc<Inner>);

impl Default for Fake {
    fn default() -> Self {
        Self(Arc::new(Inner {
            health: Mutex::new(HealthMode::Ok),
            seen: Mutex::default(),
            slots: Mutex::new(LLAMACPP_SLOTS_IDLE.to_owned()),
            chat_status: Mutex::new(200),
            stream_gap: Mutex::new(Duration::from_millis(5)),
            save_dir: Mutex::default(),
            restore_status: Mutex::new(200),
            streams_started: AtomicUsize::new(0),
            streams_cut: AtomicUsize::new(0),
            chat_script: Mutex::default(),
            stream_script: Mutex::default(),
            chat_calls: AtomicUsize::new(0),
            sidecars: std::sync::atomic::AtomicBool::new(false),
        }))
    }
}

/// Counts a stream that the client cancels before its last event.
struct StreamGuard {
    inner: Arc<Inner>,
    finished: std::cell::Cell<bool>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if !self.finished.get() {
            self.inner.streams_cut.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Fake {
    pub fn set_health(&self, mode: HealthMode) {
        *self.0.health.lock().unwrap() = mode;
    }

    /// The JSON answers of non-streamed chat requests, in order. The last one
    /// repeats. Without a script the recording answers.
    pub fn set_chat_script(&self, replies: Vec<String>) {
        *self.0.chat_script.lock().unwrap() = replies;
        self.0.chat_calls.store(0, Ordering::SeqCst);
    }

    /// The server-sent event bodies of streamed chat requests, in order. The
    /// last one repeats. Without a script the recording answers.
    pub fn set_stream_script(&self, bodies: Vec<String>) {
        *self.0.stream_script.lock().unwrap() = bodies;
        self.0.chat_calls.store(0, Ordering::SeqCst);
    }

    /// Slot saves also write `FILE.checkpoints` and `FILE.tokens.json`, like
    /// ik_llama.cpp.
    pub fn set_sidecars(&self, on: bool) {
        self.0.sidecars.store(on, Ordering::SeqCst);
    }

    pub fn set_slots(&self, json: &str) {
        *self.0.slots.lock().unwrap() = json.to_owned();
    }

    pub fn set_chat_status(&self, status: u16) {
        *self.0.chat_status.lock().unwrap() = status;
    }

    /// The pause between two events of a streamed answer.
    pub fn set_stream_gap(&self, gap: Duration) {
        *self.0.stream_gap.lock().unwrap() = gap;
    }

    /// Slot saves write a file here, like `--slot-save-path`.
    pub fn set_save_dir(&self, dir: PathBuf) {
        *self.0.save_dir.lock().unwrap() = Some(dir);
    }

    /// The status of a restore answer. 0 breaks the connection in the middle
    /// of the answer, like an engine that stops.
    pub fn set_restore_status(&self, status: u16) {
        *self.0.restore_status.lock().unwrap() = status;
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.0.seen.lock().unwrap().clone()
    }

    /// The requests whose target starts with `prefix`.
    pub fn seen_with(&self, prefix: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.target.starts_with(prefix))
            .collect()
    }

    pub fn streams_started(&self) -> usize {
        self.0.streams_started.load(Ordering::SeqCst)
    }

    pub fn streams_cut(&self) -> usize {
        self.0.streams_cut.load(Ordering::SeqCst)
    }

    /// Starts the fake on a free port of 127.0.0.1 and returns the port.
    pub async fn spawn(&self) -> u16 {
        let fake = self.clone();
        let app = Router::new().fallback(move |req: Request<Body>| {
            let fake = fake.clone();
            async move { fake.handle(req).await }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    async fn handle(&self, req: Request<Body>) -> Response {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap_or_default()
            .to_vec();
        let target = parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string);
        self.0.seen.lock().unwrap().push(Seen {
            method: parts.method.to_string(),
            target,
            headers: parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                .collect(),
            body: body.clone(),
        });
        let path = parts.uri.path().to_owned();
        let query = parts.uri.query().unwrap_or("").to_owned();
        match (parts.method.as_str(), path.as_str()) {
            ("GET", "/health") => self.health().await,
            ("GET", "/v1/models") => json(200, LLAMACPP_MODELS),
            ("GET", "/slots") => json(200, &self.0.slots.lock().unwrap().clone()),
            ("POST", "/v1/chat/completions") => self.chat(&body),
            ("POST", "/echo") => json(200, &format!("{{\"bytes\":{}}}", body.len())),
            ("POST", p) if p.starts_with("/slots/") => self.slot_action(&query, &body),
            _ => json(
                404,
                r#"{"error":{"message":"not found","type":"not_found_error","code":404}}"#,
            ),
        }
    }

    async fn health(&self) -> Response {
        let mode = *self.0.health.lock().unwrap();
        match mode {
            HealthMode::Ok => json(200, r#"{"status":"ok"}"#),
            HealthMode::Loading => json(
                503,
                r#"{"error":{"message":"Loading model","type":"unavailable_error","code":503}}"#,
            ),
            HealthMode::Hang => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                json(200, r#"{"status":"ok"}"#)
            }
        }
    }

    fn chat(&self, body: &[u8]) -> Response {
        let status = *self.0.chat_status.lock().unwrap();
        if status != 200 {
            return json(
                status,
                r#"{"error":{"message":"fake failure","type":"server_error","code":500}}"#,
            );
        }
        let streaming = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v["stream"].as_bool())
            .unwrap_or(false);
        if !streaming {
            let script = self.0.chat_script.lock().unwrap().clone();
            if script.is_empty() {
                return json(200, LLAMACPP_CHAT);
            }
            let i = self.0.chat_calls.fetch_add(1, Ordering::SeqCst);
            return json(200, &script[i.min(script.len() - 1)]);
        }
        // The `data:` lines of the recording, one event each.
        let script = self.0.stream_script.lock().unwrap().clone();
        let recording = if script.is_empty() {
            LLAMACPP_STREAM.to_owned()
        } else {
            let i = self.0.chat_calls.fetch_add(1, Ordering::SeqCst);
            script[i.min(script.len() - 1)].clone()
        };
        let events: Vec<String> = recording
            .lines()
            .filter(|l| l.starts_with("data: "))
            .map(|l| format!("{l}\n\n"))
            .collect();
        let gap = *self.0.stream_gap.lock().unwrap();
        self.0.streams_started.fetch_add(1, Ordering::SeqCst);
        let guard = StreamGuard {
            inner: self.0.clone(),
            finished: std::cell::Cell::new(false),
        };
        let stream = futures_util::stream::unfold((0usize, events, guard, gap), |(i, events, guard, gap)| async move {
            if i >= events.len() {
                guard.finished.set(true);
                return None;
            }
            if i > 0 {
                tokio::time::sleep(gap).await;
            }
            let chunk = Ok::<_, std::convert::Infallible>(bytes::Bytes::from(events[i].clone()));
            Some((chunk, (i + 1, events, guard, gap)))
        });
        Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    }

    /// `POST /slots/{id}?action=save|restore` with `{"filename": ...}`.
    fn slot_action(&self, query: &str, body: &[u8]) -> Response {
        let filename = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v["filename"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let dir = self.0.save_dir.lock().unwrap().clone();
        if query.contains("action=save") {
            if let Some(dir) = dir {
                let _ = std::fs::write(dir.join(&filename), b"slot");
                if self.0.sidecars.load(Ordering::SeqCst) {
                    let _ = std::fs::write(dir.join(format!("{filename}.checkpoints")), b"checkpoints");
                    let _ = std::fs::write(dir.join(format!("{filename}.tokens.json")), b"[]");
                }
            }
            json(200, LLAMACPP_SLOT_SAVE)
        } else if query.contains("action=restore") {
            let status = *self.0.restore_status.lock().unwrap();
            if status == 200 {
                json(200, LLAMACPP_SLOT_RESTORE)
            } else if status == 0 {
                let broken = futures_util::stream::once(async {
                    Err::<bytes::Bytes, _>(std::io::Error::other("engine stopped"))
                });
                Response::builder()
                    .status(200)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from_stream(broken))
                    .unwrap()
            } else {
                json(
                    status,
                    r#"{"error":{"message":"Unable to restore slot","type":"invalid_request_error","code":400}}"#,
                )
            }
        } else {
            json(
                400,
                r#"{"error":{"message":"unknown action","type":"invalid_request_error","code":400}}"#,
            )
        }
    }
}

fn json(status: u16, body: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body.to_owned(),
    )
        .into_response()
}

/// A port on which nothing listens.
pub fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
