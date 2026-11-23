//! `/llm/...` on the main port, forwarded to llama-server on 127.0.0.1.
//!
//! Request and response bodies stream through, so server-sent events from
//! `/llm/v1/chat/completions` reach the client token by token.
//!
//! One exception, for chat requests (`retry_empty`): Ling sometimes writes its
//! tool call as text into its thinking text. llama-server then returns a reply
//! with no text and no tool call, and Home Assistant answers "Unable to get
//! response". The proxy detects such a reply and repeats the request once with
//! `chat_template_kwargs.enable_thinking=false`, where the model calls the tool
//! directly. A good reply passes through token by token. Only the events from
//! the `finish_reason` on wait until the proxy knows that the reply is not empty.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::defer::{self, Deferral, Prepared, ToolCall};

/// The largest chat request that the proxy buffers for a possible repeat.
const MAX_CHAT_BODY: usize = 16 * 1024 * 1024;

const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "host",
    "content-length",
];

#[derive(Clone)]
pub struct LlmProxy {
    client: reqwest::Client,
    backend: String,
    api_key: Option<String>,
    /// Requests that are forwarded and not yet answered to the end.
    in_flight: Arc<AtomicUsize>,
    /// Repeat an empty chat reply once with thinking off.
    retry_empty: bool,
    /// Move the area line of Home Assistant to the last user message.
    area_in_user: bool,
    /// Leave the matching tools out of the prompt, see `defer.rs`.
    defer: Option<Deferral>,
}

/// The state of llama-server, from its own `/health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmHealth {
    Ok,
    /// No answer in time while a request runs. ik_llama.cpp answers `/health`
    /// only between two batches of a long prompt.
    Busy,
    /// Loading, not running, or no answer without a request.
    Unavailable,
}

impl LlmHealth {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Counts one forwarded request until it is dropped.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn error(status: StatusCode, message: &str, kind: &str) -> Response {
    (status, axum::Json(json!({"error": {"message": message, "type": kind}}))).into_response()
}

fn keep_header(name: &HeaderName) -> bool {
    !HOP_HEADERS.contains(&name.as_str())
}

/// The path below `/llm`: `/llm/v1/models` becomes `/v1/models`.
pub fn backend_path(uri: &Uri) -> String {
    let path = uri.path().strip_prefix("/llm").unwrap_or(uri.path());
    let path = if path.is_empty() { "/" } else { path };
    match uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    }
}

impl LlmProxy {
    pub fn new(port: u16, api_key: Option<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            // Pass bodies through as they are, also compressed ones.
            .no_gzip()
            // No idle connections to llama-server. When llama-server stops, it
            // closes each open connection itself, and that socket holds its
            // port in TIME_WAIT for 60 s. ik_llama.cpp cannot bind the port
            // then (`specs.rs`). A new connection on 127.0.0.1 costs well
            // under a millisecond.
            .pool_max_idle_per_host(0)
            .build()?;
        Ok(Self {
            client,
            backend: format!("http://127.0.0.1:{port}"),
            api_key,
            in_flight: Arc::default(),
            retry_empty: false,
            area_in_user: false,
            defer: None,
        })
    }

    /// Leaves the tools that match `patterns` out of the prompt until the
    /// model asks for them (`defer.rs`). An empty list turns it off.
    pub fn with_deferred_tools(mut self, patterns: Vec<String>) -> Self {
        self.defer = (!patterns.is_empty()).then(|| Deferral::new(patterns));
        self
    }

    /// Turns the move of the area line on or off, see `area_to_user_message`.
    pub fn with_area_in_user(mut self, on: bool) -> Self {
        self.area_in_user = on;
        self
    }

    /// Turns the repeat of empty chat replies on or off.
    pub fn with_retry_empty(mut self, on: bool) -> Self {
        self.retry_empty = on;
        self
    }

    /// Probes llama-server's own `/health`. A 2xx answer means the model is
    /// loaded. 503 means that it loads the model.
    pub async fn health(&self) -> LlmHealth {
        let probe = self
            .client
            .get(format!("{}/health", self.backend))
            .timeout(Duration::from_secs(3))
            .send()
            .await;
        match probe {
            Ok(resp) if resp.status().is_success() => LlmHealth::Ok,
            Err(err) if err.is_timeout() && self.in_flight.load(Ordering::SeqCst) > 0 => LlmHealth::Busy,
            _ => LlmHealth::Unavailable,
        }
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(key) = &self.api_key else {
            return true;
        };
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|given| given.trim() == key)
    }

    pub async fn forward(&self, req: Request<Body>) -> Response {
        let (parts, body) = req.into_parts();
        if !self.authorized(&parts.headers) {
            return error(StatusCode::UNAUTHORIZED, "Invalid API Key", "authentication_error");
        }
        let in_flight = InFlight::new(&self.in_flight);
        let path = backend_path(&parts.uri);
        let url = format!("{}{}", self.backend, path);
        if (self.retry_empty || self.area_in_user || self.defer.is_some())
            && parts.method == Method::POST
            && is_chat_path(&path)
        {
            return self.forward_chat(&parts.headers, url, body, in_flight).await;
        }
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut out = self.outgoing(method, &url, &parts.headers);
        if parts.method != Method::GET && parts.method != Method::HEAD {
            out = out.body(reqwest::Body::wrap_stream(body.into_data_stream()));
        }
        match out.send().await {
            Ok(resp) => relay(resp, in_flight),
            Err(err) => send_error(&err),
        }
    }

    /// A request to llama-server with the headers of the client.
    fn outgoing(&self, method: reqwest::Method, url: &str, headers: &HeaderMap) -> reqwest::RequestBuilder {
        let mut out = self.client.request(method, url);
        for (name, value) in headers {
            if keep_header(name) && name != header::AUTHORIZATION {
                out = out.header(name.as_str(), value.as_bytes());
            }
        }
        out
    }

    /// A chat request. A reply that has no text and no tool call is repeated
    /// once with thinking off, see the module comment. With deferred tools,
    /// the proxy answers the search calls of the model itself (`defer.rs`).
    async fn forward_chat(&self, headers: &HeaderMap, url: String, body: Body, in_flight: InFlight) -> Response {
        let Ok(bytes) = axum::body::to_bytes(body, MAX_CHAT_BODY).await else {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body is too large",
                "invalid_request_error",
            );
        };
        // The prompt of the model is the same for every satellite, so that one
        // cached prefix serves all of them.
        let bytes = if self.area_in_user {
            area_to_user_message(&bytes).unwrap_or(bytes)
        } else {
            bytes
        };
        let defer = self.defer.as_ref().and_then(|d| d.prepare(&bytes));
        let bytes = defer.as_ref().and_then(Prepared::bytes).unwrap_or(bytes);
        let retry = self.retry_empty && without_thinking(&bytes).is_some();
        let resp = match self
            .outgoing(reqwest::Method::POST, &url, headers)
            .body(bytes.clone())
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => return send_error(&err),
        };
        if !resp.status().is_success() || (!retry && defer.is_none()) {
            return relay(resp, in_flight);
        }
        let chat = Chat {
            proxy: self.clone(),
            url,
            headers: headers.clone(),
            body: bytes,
            retry,
            defer,
        };
        if is_event_stream(&resp) {
            chat.stream(resp, in_flight)
        } else {
            chat.json(resp, in_flight).await
        }
    }
}

/// One chat request that the proxy may send more than once: again without
/// thinking after an empty reply, and with a search step after a search call.
struct Chat {
    proxy: LlmProxy,
    url: String,
    headers: HeaderMap,
    /// The body of the last request to llama-server.
    body: Bytes,
    /// Repeat an empty reply once without thinking.
    retry: bool,
    /// The rewritten request with deferred tools.
    defer: Option<Prepared>,
}

impl Chat {
    /// Sends one more request. `None` if llama-server does not accept it.
    async fn send(&self, body: Bytes) -> Option<reqwest::Response> {
        let resp = self
            .proxy
            .outgoing(reqwest::Method::POST, &self.url, &self.headers)
            .body(body)
            .send()
            .await;
        match resp {
            Ok(resp) if resp.status().is_success() => Some(resp),
            Ok(resp) => {
                tracing::warn!("LLM proxy: the repeated request got status {}", resp.status());
                None
            }
            Err(err) => {
                tracing::warn!("LLM proxy: the repeated request failed: {err}");
                None
            }
        }
    }

    /// Adds the answer to the search calls of the model to the request.
    /// `None` if the reply has other calls, or the search limit is reached.
    fn search_step(&mut self, calls: &[ToolCall], searches: &mut usize) -> Option<(Bytes, Vec<Value>)> {
        let prepared = self.defer.as_mut()?;
        if *searches >= defer::MAX_SEARCHES || !defer::only_searches(calls) {
            return None;
        }
        *searches += 1;
        let step = prepared.answer(calls);
        prepared.with_step(&step);
        let body = prepared.bytes()?;
        self.body = body.clone();
        Some((body, step))
    }

    /// Keeps the search steps of this reply for the next requests.
    fn remember(&self, calls: &[ToolCall], steps: &[Value]) {
        if let Some(d) = &self.proxy.defer {
            d.remember(calls, steps);
        }
    }

    /// Streams the events to the client. The events from the first one with a
    /// `finish_reason` on wait until the end, because a client stops at that
    /// event. With deferred tools, the events from the first tool call on
    /// wait too, so that a search call never reaches the client.
    fn stream(self, resp: reqwest::Response, in_flight: InFlight) -> Response {
        let (status, headers) = (resp.status(), resp.headers().clone());
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let _counted = in_flight;
            self.run_stream(resp, tx).await;
        });
        let body = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
        response_from(status, &headers, Body::from_stream(body))
    }

    async fn run_stream(mut self, mut resp: reqwest::Response, tx: Sender) {
        let hold_calls = self.defer.is_some();
        let mut searches = 0;
        let mut steps: Vec<Value> = Vec::new();
        let mut last = loop {
            let pumped = pump(resp, &tx, hold_calls).await;
            if pumped.end != End::Done {
                return;
            }
            let Some((body, step)) = self.search_step(&pumped.calls, &mut searches) else {
                break pumped;
            };
            steps.extend(step);
            match self.send(body).await.filter(is_event_stream) {
                Some(next) => resp = next,
                None => break pumped,
            }
        };
        if self.retry && last.finished && !last.output {
            tracing::warn!("LLM reply has no text and no tool call, repeating the request without thinking");
            if let Some(body) = without_thinking(&self.body)
                && let Some(resp) = self.send(body).await.filter(is_event_stream)
            {
                let second = pump(resp, &tx, hold_calls).await;
                if second.end != End::Done {
                    return;
                }
                if second.output {
                    last = second;
                } else {
                    tracing::warn!("the repeated LLM reply is empty too");
                }
            }
        }
        if !steps.is_empty() {
            self.remember(&last.calls, &steps);
        }
        release(last.held, &tx).await;
    }

    /// A reply that is not streamed.
    async fn json(mut self, mut resp: reqwest::Response, in_flight: InFlight) -> Response {
        let _counted = in_flight;
        let mut searches = 0;
        let mut steps: Vec<Value> = Vec::new();
        let (mut status, mut headers, mut body) = loop {
            let (status, headers) = (resp.status(), resp.headers().clone());
            let body = match resp.bytes().await {
                Ok(body) => body,
                Err(err) => return send_error(&err),
            };
            let Some((next, step)) = self.search_step(&reply_calls(&body), &mut searches) else {
                break (status, headers, body);
            };
            steps.extend(step);
            match self.send(next).await {
                Some(next) => resp = next,
                None => break (status, headers, body),
            }
        };
        if self.retry && reply_is_empty(&body) {
            tracing::warn!("LLM reply has no text and no tool call, repeating the request without thinking");
            if let Some(retry) = without_thinking(&self.body)
                && let Some(resp) = self.send(retry).await
            {
                let (s, h) = (resp.status(), resp.headers().clone());
                if let Ok(second) = resp.bytes().await {
                    if reply_is_empty(&second) {
                        tracing::warn!("the repeated LLM reply is empty too");
                    } else {
                        (status, headers, body) = (s, h, second);
                    }
                }
            }
        }
        if !steps.is_empty() {
            self.remember(&reply_calls(&body), &steps);
        }
        response_from(status, &headers, Body::from(body))
    }
}

type Sender = mpsc::Sender<Result<Bytes, std::io::Error>>;

/// What `pump` did with one response.
#[derive(Debug, Default, PartialEq, Eq)]
enum End {
    /// The upstream stream ended.
    #[default]
    Done,
    /// The client closed its connection.
    ClientGone,
    /// The upstream stream broke. The client got the error.
    UpstreamError,
}

#[derive(Default)]
struct Pumped {
    /// Some event had text or a tool call.
    output: bool,
    /// Some event had a `finish_reason`.
    finished: bool,
    /// Hold the events from the first tool call on, not only from the
    /// `finish_reason` on.
    hold_calls: bool,
    /// Some event had a tool call.
    called: bool,
    /// The tool calls of the reply, put together from their deltas.
    calls: Vec<ToolCall>,
    /// The events that wait until the end.
    held: Vec<Bytes>,
    end: End,
}

impl Pumped {
    /// Sends one event to the client, or holds it. `false` if the client left.
    async fn handle(&mut self, event: Vec<u8>, tx: &Sender) -> bool {
        let info = classify(&event);
        self.output |= info.text || info.tool_call;
        self.finished |= info.finish;
        self.called |= info.tool_call;
        for delta in info.calls {
            add_call_delta(&mut self.calls, delta);
        }
        let event = Bytes::from(event);
        if self.finished || (self.hold_calls && self.called) {
            self.held.push(event);
            true
        } else {
            tx.send(Ok(event)).await.is_ok()
        }
    }
}

/// Reads the events of one streamed response and sends them on, except for
/// the held ones.
async fn pump(resp: reqwest::Response, tx: &Sender, hold_calls: bool) -> Pumped {
    let mut out = Pumped {
        hold_calls,
        ..Pumped::default()
    };
    let mut upstream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let item = tokio::select! {
            () = tx.closed() => {
                out.end = End::ClientGone;
                return out;
            }
            item = upstream.next() => item,
        };
        match item {
            Some(Ok(chunk)) => {
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                    let event: Vec<u8> = buf.drain(..pos + 2).collect();
                    if !out.handle(event, tx).await {
                        out.end = End::ClientGone;
                        return out;
                    }
                }
            }
            Some(Err(err)) => {
                let _ = tx.send(Err(std::io::Error::other(err))).await;
                out.end = End::UpstreamError;
                return out;
            }
            None => break,
        }
    }
    if !buf.is_empty() && !out.handle(buf, tx).await {
        out.end = End::ClientGone;
    }
    out
}

async fn release(held: Vec<Bytes>, tx: &Sender) {
    for event in held {
        if tx.send(Ok(event)).await.is_err() {
            return;
        }
    }
}

/// What one server-sent event carries.
#[derive(Debug, Default, PartialEq, Eq)]
struct EventInfo {
    /// Text for the user. Thinking is not text.
    text: bool,
    tool_call: bool,
    finish: bool,
    /// The tool call deltas of the event.
    calls: Vec<CallDelta>,
}

/// One part of a streamed tool call. The first part of a call has its ID and
/// name, the next parts add to its arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct CallDelta {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn add_call_delta(calls: &mut Vec<ToolCall>, delta: CallDelta) {
    while calls.len() <= delta.index {
        calls.push(ToolCall::default());
    }
    let call = &mut calls[delta.index];
    if let Some(id) = delta.id {
        call.id = id;
    }
    if let Some(name) = delta.name {
        call.name.push_str(&name);
    }
    call.arguments.push_str(&delta.arguments);
}

/// The tool calls of a complete chat reply (not streamed).
fn reply_calls(body: &[u8]) -> Vec<ToolCall> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    v["choices"][0]["message"]["tool_calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .map(|c| ToolCall {
                    id: c["id"].as_str().unwrap_or_default().to_owned(),
                    name: c["function"]["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: c["function"]["arguments"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn classify(event: &[u8]) -> EventInfo {
    let mut info = EventInfo::default();
    for line in event.split(|b| *b == b'\n') {
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii()) else {
            continue;
        };
        let Some(choice) = v["choices"].get(0) else {
            continue;
        };
        let delta = &choice["delta"];
        info.text |= delta["content"].as_str().is_some_and(|s| !s.trim().is_empty());
        info.tool_call |= delta["tool_calls"].as_array().is_some_and(|a| !a.is_empty());
        info.finish |= !choice["finish_reason"].is_null();
        for c in delta["tool_calls"].as_array().into_iter().flatten() {
            info.calls.push(CallDelta {
                index: c["index"].as_u64().and_then(|i| usize::try_from(i).ok()).unwrap_or(0),
                id: c["id"].as_str().map(str::to_owned),
                name: c["function"]["name"].as_str().map(str::to_owned),
                arguments: c["function"]["arguments"].as_str().unwrap_or_default().to_owned(),
            });
        }
    }
    info
}

/// `true` for a complete chat reply (not streamed) without text and tool call.
fn reply_is_empty(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(choice) = v["choices"].get(0) else {
        return false;
    };
    let message = &choice["message"];
    message["content"].as_str().is_none_or(|s| s.trim().is_empty())
        && message["tool_calls"].as_array().is_none_or(Vec::is_empty)
}

/// The request body with `chat_template_kwargs.enable_thinking` off. `None`
/// if the body is no JSON object, or thinking is off already.
fn without_thinking(body: &[u8]) -> Option<Bytes> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let kwargs = v
        .as_object_mut()?
        .entry("chat_template_kwargs")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    if kwargs.get("enable_thinking") == Some(&Value::Bool(false)) {
        return None;
    }
    kwargs.insert("enable_thinking".into(), Value::Bool(false));
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

/// Home Assistant writes "You are in area X (floor Y) and all generic
/// commands ... should target this area." into the system message, before the
/// tools. That line differs for each satellite, so the cached prefix (about
/// 7,000 tokens, 17 to 30 s of prefill on the CPU) would hold for one satellite
/// only. This moves the line to the front of the last user message. The system
/// message and the tools are then the same for every satellite. `None` if the
/// body has no such line, so the request goes on unchanged.
fn area_to_user_message(body: &[u8]) -> Option<Bytes> {
    const MARK: &str = "You are in area ";
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let messages = v.get_mut("messages")?.as_array_mut()?;
    // Cut the line out of the system message.
    let mut line = None;
    for m in messages.iter_mut().filter(|m| m["role"] == "system") {
        let Some(text) = m["content"].as_str() else {
            continue;
        };
        let Some(start) = text.find(MARK).filter(|i| *i == 0 || text.as_bytes()[i - 1] == b'\n') else {
            continue;
        };
        let end = text[start..].find('\n').map_or(text.len(), |n| start + n + 1);
        line = Some(text[start..end].trim_end().to_owned());
        m["content"] = Value::String(format!("{}{}", &text[..start], &text[end..]));
        break;
    }
    let line = line?;
    // Put it in front of the last user message. Without a user message, the request stays as sent.
    let user = messages.iter_mut().rev().find(|m| m["role"] == "user")?;
    let text = user["content"].as_str()?.to_owned();
    user["content"] = Value::String(format!("({line})\n{text}"));
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

fn is_chat_path(path: &str) -> bool {
    path.split('?').next() == Some("/v1/chat/completions")
}

fn is_event_stream(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

fn send_error(err: &reqwest::Error) -> Response {
    if err.is_connect() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "llama-server is not ready yet",
            "unavailable",
        );
    }
    tracing::warn!("LLM proxy: {err}");
    error(StatusCode::BAD_GATEWAY, &err.to_string(), "upstream_error")
}

fn response_from(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap, body: Body) -> Response {
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in headers {
        if !HOP_HEADERS.contains(&name.as_str()) {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
    }
    builder
        .body(body)
        .unwrap_or_else(|err| error(StatusCode::BAD_GATEWAY, &err.to_string(), "upstream_error"))
}

/// Passes the response through as it comes.
fn relay(resp: reqwest::Response, in_flight: InFlight) -> Response {
    let (status, headers) = (resp.status(), resp.headers().clone());
    // The count ends with the body stream, after the last token.
    let body = resp.bytes_stream().map(move |chunk| {
        let _counted = &in_flight;
        chunk
    });
    response_from(status, &headers, Body::from_stream(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{Fake, HealthMode, closed_port};
    use std::time::Instant;

    #[test]
    fn paths() {
        let p = |s: &str| backend_path(&s.parse::<Uri>().unwrap());
        assert_eq!(p("/llm/v1/chat/completions"), "/v1/chat/completions");
        assert_eq!(p("/llm/slots?fail_on_no_slot=1"), "/slots?fail_on_no_slot=1");
        assert_eq!(p("/llm"), "/");
    }

    #[test]
    fn api_key() {
        let proxy = LlmProxy::new(1, Some("secret".into())).unwrap();
        let mut h = HeaderMap::new();
        assert!(!proxy.authorized(&h));
        h.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(proxy.authorized(&h));
        h.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!proxy.authorized(&h));
        assert!(LlmProxy::new(1, None).unwrap().authorized(&HeaderMap::new()));
    }

    fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    fn chat(stream: bool) -> Request<Body> {
        request(
            "POST",
            "/llm/v1/chat/completions",
            Body::from(format!(
                r#"{{"messages":[{{"role":"user","content":"hi"}}],"stream":{stream}}}"#
            )),
        )
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn proxy_to_fake(fake: &Fake, key: Option<&str>) -> LlmProxy {
        let port = fake.spawn().await;
        LlmProxy::new(port, key.map(str::to_owned)).unwrap()
    }

    #[tokio::test]
    async fn a_json_answer_passes_through_unchanged() {
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, None).await;
        let resp = proxy.forward(chat(false)).await;
        assert_eq!(resp.status(), 200);
        assert!(
            resp.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "HassTurnOn"
        );
        // The backend path has no /llm prefix, the body arrived as sent.
        let seen = fake.seen_with("/v1/chat/completions");
        assert_eq!(seen.len(), 1);
        assert!(String::from_utf8_lossy(&seen[0].body).contains(r#""stream":false"#));
    }

    #[tokio::test]
    async fn events_arrive_one_by_one_and_not_in_one_block() {
        let fake = Fake::default();
        fake.set_stream_gap(Duration::from_millis(100));
        let proxy = proxy_to_fake(&fake, None).await;
        let start = Instant::now();
        let resp = proxy.forward(chat(true)).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let mut stream = resp.into_body().into_data_stream();
        let mut arrivals = Vec::new();
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            arrivals.push(start.elapsed());
            text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
        assert!(arrivals.len() >= 5, "got {} chunks", arrivals.len());
        // The events are 100 ms apart. The first one must come long before the last.
        let last = *arrivals.last().unwrap();
        assert!(
            arrivals[0] + Duration::from_millis(400) < last,
            "first {:?}, last {last:?}",
            arrivals[0]
        );
        assert!(text.contains("\"reasoning_content\""));
        assert!(text.trim_end().ends_with("data: [DONE]"));
    }

    #[tokio::test]
    async fn a_client_that_leaves_cancels_the_backend_stream() {
        let fake = Fake::default();
        fake.set_stream_gap(Duration::from_millis(100));
        let proxy = proxy_to_fake(&fake, None).await;
        let resp = proxy.forward(chat(true)).await;
        let mut stream = resp.into_body().into_data_stream();
        assert!(stream.next().await.is_some());
        drop(stream);
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.streams_cut() == 0 {
            assert!(
                Instant::now() < deadline,
                "the backend kept generating for a gone client"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(fake.streams_started(), 1);
    }

    #[tokio::test]
    async fn the_api_key_is_checked_and_not_sent_to_the_backend() {
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, Some("secret")).await;
        let denied = proxy.forward(chat(false)).await;
        assert_eq!(denied.status(), 401);
        let v: serde_json::Value = serde_json::from_str(&body_text(denied).await).unwrap();
        assert_eq!(v["error"]["type"], "authentication_error");
        assert!(fake.seen().is_empty(), "a denied request must not reach the backend");

        let mut ok = chat(false);
        ok.headers_mut()
            .insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(proxy.forward(ok).await.status(), 200);
        let seen = fake.seen_with("/v1/chat/completions");
        assert!(!seen[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn headers_and_query_strings_are_forwarded_and_hop_headers_are_not() {
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, None).await;
        let mut req = request("GET", "/llm/slots?fail_on_no_slot=1", Body::empty());
        req.headers_mut().insert("x-request-id", "abc".parse().unwrap());
        req.headers_mut().insert("connection", "keep-alive".parse().unwrap());
        req.headers_mut().insert("keep-alive", "timeout=5".parse().unwrap());
        let resp = proxy.forward(req).await;
        assert_eq!(resp.status(), 200);
        let seen = fake.seen_with("/slots");
        assert_eq!(seen[0].target, "/slots?fail_on_no_slot=1");
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].headers.get("x-request-id").map(String::as_str), Some("abc"));
        assert!(!seen[0].headers.contains_key("keep-alive"));
        assert!(seen[0].body.is_empty());
    }

    #[tokio::test]
    async fn an_error_status_of_the_backend_reaches_the_client() {
        let fake = Fake::default();
        fake.set_chat_status(500);
        let proxy = proxy_to_fake(&fake, None).await;
        let resp = proxy.forward(chat(false)).await;
        assert_eq!(resp.status(), 500);
        let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert_eq!(v["error"]["message"], "fake failure");
    }

    #[tokio::test]
    async fn a_large_request_body_arrives_complete() {
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, None).await;
        let big = vec![b'a'; 8 * 1024 * 1024];
        let resp = proxy.forward(request("POST", "/llm/echo", Body::from(big))).await;
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert_eq!(v["bytes"], 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn a_backend_that_is_not_listening_gives_503() {
        let proxy = LlmProxy::new(closed_port(), None).unwrap();
        let resp = proxy.forward(chat(false)).await;
        assert_eq!(resp.status(), 503);
        let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert_eq!(v["error"]["type"], "unavailable");
        assert_eq!(proxy.health().await, LlmHealth::Unavailable);
    }

    #[tokio::test]
    async fn health_follows_the_backend() {
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, None).await;
        assert_eq!(proxy.health().await, LlmHealth::Ok);
        fake.set_health(HealthMode::Loading);
        assert_eq!(
            proxy.health().await,
            LlmHealth::Unavailable,
            "503 while the model loads"
        );
        fake.set_health(HealthMode::Ok);
        assert_eq!(proxy.health().await, LlmHealth::Ok);
    }

    #[tokio::test]
    async fn a_hanging_backend_is_not_healthy_after_three_seconds() {
        let fake = Fake::default();
        fake.set_health(HealthMode::Hang);
        let proxy = proxy_to_fake(&fake, None).await;
        let start = Instant::now();
        assert_eq!(proxy.health().await, LlmHealth::Unavailable);
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(3) && took < Duration::from_secs(8),
            "the probe gave up after {took:?}"
        );
    }

    #[tokio::test]
    async fn a_late_health_answer_during_a_request_is_busy_and_the_count_ends_with_the_body() {
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, None).await;
        let resp = proxy.forward(chat(true)).await;
        assert_eq!(resp.status(), 200);
        fake.set_health(HealthMode::Hang);
        assert_eq!(proxy.health().await, LlmHealth::Busy, "a request still streams");
        let _ = body_text(resp).await;
        assert_eq!(proxy.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(proxy.health().await, LlmHealth::Unavailable);
    }

    /// One streamed reply as the `data:` lines of llama-server.
    fn sse(deltas: &[&str], finish: &str) -> String {
        let mut out = String::new();
        for d in deltas {
            out.push_str(&format!(
                "data: {{\"choices\":[{{\"finish_reason\":null,\"index\":0,\"delta\":{d}}}]}}\n\n"
            ));
        }
        out.push_str(&format!(
            "data: {{\"choices\":[{{\"finish_reason\":\"{finish}\",\"index\":0,\"delta\":{{}}}}]}}\n\ndata: [DONE]\n\n"
        ));
        out
    }

    const THINKING_ONLY: &[&str] = &[
        r#"{"role":"assistant","content":null}"#,
        r#"{"reasoning_content":"<tool_call>x"}"#,
    ];
    const TOOL_CALL: &[&str] = &[
        r#"{"role":"assistant","content":null}"#,
        r#"{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"T","arguments":"{}"}}]}"#,
    ];

    async fn stream_text(proxy: &LlmProxy) -> String {
        let resp = proxy.forward(chat(true)).await;
        assert_eq!(resp.status(), 200);
        body_text(resp).await
    }

    #[tokio::test]
    async fn an_empty_streamed_reply_is_repeated_without_thinking() {
        let fake = Fake::default();
        fake.set_stream_script(vec![sse(THINKING_ONLY, "stop"), sse(TOOL_CALL, "tool_calls")]);
        let proxy = proxy_to_fake(&fake, None).await.with_retry_empty(true);
        let text = stream_text(&proxy).await;
        assert!(text.contains("\"tool_calls\""), "{text}");
        assert!(text.trim_end().ends_with("data: [DONE]"));
        assert_eq!(text.matches("[DONE]").count(), 1, "{text}");
        let seen = fake.seen_with("/v1/chat/completions");
        assert_eq!(seen.len(), 2);
        assert!(String::from_utf8_lossy(&seen[1].body).contains(r#""enable_thinking":false"#));
        assert!(!String::from_utf8_lossy(&seen[0].body).contains("enable_thinking"));
    }

    #[tokio::test]
    async fn a_good_streamed_reply_is_sent_once() {
        let fake = Fake::default();
        fake.set_stream_script(vec![sse(TOOL_CALL, "tool_calls")]);
        let proxy = proxy_to_fake(&fake, None).await.with_retry_empty(true);
        let text = stream_text(&proxy).await;
        assert!(text.contains("\"tool_calls\""));
        assert_eq!(fake.seen_with("/v1/chat/completions").len(), 1);
    }

    #[tokio::test]
    async fn a_second_empty_reply_ends_the_stream_properly() {
        let fake = Fake::default();
        fake.set_stream_script(vec![sse(THINKING_ONLY, "stop")]);
        let proxy = proxy_to_fake(&fake, None).await.with_retry_empty(true);
        let text = stream_text(&proxy).await;
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert_eq!(text.matches("[DONE]").count(), 1, "{text}");
        assert_eq!(fake.seen_with("/v1/chat/completions").len(), 2);
    }

    /// A Home Assistant request with one tool that `LLM_DEFER_TOOLS=on` defers.
    fn deferred_chat(stream: bool, history: &str) -> Request<Body> {
        let body = format!(
            r#"{{"messages":[{{"role":"system","content":"Intro."}},{{"role":"user","content":"set a timer"}}{history}],
              "tools":[{{"type":"function","function":{{"name":"intent__HassTurnOn","description":"Turns on a device","parameters":{{}}}}}},
                       {{"type":"function","function":{{"name":"intent__HassStartTimer","description":"Starts a new timer","parameters":{{}}}}}}],
              "stream":{stream}}}"#
        );
        request("POST", "/llm/v1/chat/completions", Body::from(body))
    }

    fn deferring(proxy: LlmProxy) -> LlmProxy {
        proxy
            .with_retry_empty(true)
            .with_deferred_tools(defer::DEFAULT_PATTERNS.iter().map(|p| (*p).to_owned()).collect())
    }

    const SEARCH_CALL: &[&str] = &[
        r#"{"role":"assistant","content":null}"#,
        r#"{"reasoning_content":"I need a timer tool."}"#,
        r#"{"tool_calls":[{"index":0,"id":"s1","type":"function","function":{"name":"search_tools","arguments":""}}]}"#,
        r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"query\":\"timer\"}"}}]}"#,
    ];
    const TIMER_CALL: &[&str] = &[
        r#"{"role":"assistant","content":null}"#,
        r#"{"tool_calls":[{"index":0,"id":"t1","type":"function","function":{"name":"intent__HassStartTimer","arguments":"{\"minutes\":5}"}}]}"#,
    ];

    fn json_reply(id: &str, name: &str, arguments: &str) -> String {
        serde_json::json!({"choices": [{"finish_reason": "tool_calls", "index": 0, "message": {"role": "assistant",
            "content": null, "tool_calls": [{"type": "function", "id": id,
            "function": {"name": name, "arguments": arguments}}]}}]})
        .to_string()
    }

    fn seen_json(fake: &Fake) -> Vec<Value> {
        fake.seen_with("/v1/chat/completions")
            .iter()
            .map(|s| serde_json::from_slice(&s.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn the_proxy_answers_a_streamed_search_and_the_client_sees_only_the_final_call() {
        let fake = Fake::default();
        fake.set_stream_script(vec![sse(SEARCH_CALL, "tool_calls"), sse(TIMER_CALL, "tool_calls")]);
        let proxy = deferring(proxy_to_fake(&fake, None).await);
        let text = body_text(proxy.forward(deferred_chat(true, "")).await).await;
        assert!(!text.contains("search_tools"), "{text}");
        assert!(text.contains("intent__HassStartTimer"), "{text}");
        assert!(
            text.contains("I need a timer tool."),
            "the thinking before the call streams on: {text}"
        );
        assert_eq!(text.matches("[DONE]").count(), 1, "{text}");
        assert_eq!(text.matches(r#""finish_reason":"tool_calls""#).count(), 1, "{text}");

        let seen = seen_json(&fake);
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0]["chat_template_kwargs"]["deferred_tools"],
            json!(["intent__HassStartTimer"])
        );
        assert_eq!(seen[0]["tools"][2]["function"]["name"], "search_tools");
        let step = seen[1]["messages"].as_array().unwrap();
        assert_eq!(step.len(), 4);
        assert_eq!(
            step[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"query":"timer"}"#
        );
        let result = step[3]["content"].as_str().unwrap();
        assert!(
            result.starts_with("Found 1 tool.\n<additional_tools>\n{\"name\": \"intent__HassStartTimer\""),
            "{result}"
        );

        // The next request of Home Assistant gets the search step back.
        fake.set_stream_script(vec![sse(TOOL_CALL, "tool_calls")]);
        let history = r#",{"role":"assistant","content":"","tool_calls":[{"id":"t1","type":"function","function":{"name":"intent__HassStartTimer","arguments":"{\"minutes\":5}"}}]},{"role":"tool","tool_call_id":"t1","content":"ok"}"#;
        body_text(proxy.forward(deferred_chat(true, history)).await).await;
        let seen = seen_json(&fake);
        let roles: Vec<&str> = seen[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant", "tool"]);
        assert_eq!(seen[2]["messages"][2]["tool_calls"][0]["id"], "s1");
    }

    #[tokio::test]
    async fn the_proxy_answers_a_search_in_a_reply_that_is_not_streamed() {
        let fake = Fake::default();
        fake.set_chat_script(vec![
            json_reply("s1", "search_tools", r#"{"query":"timer"}"#),
            json_reply("t1", "intent__HassStartTimer", "{}"),
        ]);
        let proxy = deferring(proxy_to_fake(&fake, None).await);
        let v: Value = serde_json::from_str(&body_text(proxy.forward(deferred_chat(false, "")).await).await).unwrap();
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "intent__HassStartTimer"
        );
        assert_eq!(seen_json(&fake).len(), 2);
    }

    #[tokio::test]
    async fn the_searches_of_one_reply_have_a_limit() {
        let fake = Fake::default();
        fake.set_stream_script(vec![sse(SEARCH_CALL, "tool_calls")]);
        let proxy = deferring(proxy_to_fake(&fake, None).await);
        let text = body_text(proxy.forward(deferred_chat(true, "")).await).await;
        assert_eq!(seen_json(&fake).len(), defer::MAX_SEARCHES + 1);
        assert!(text.contains("search_tools"), "the last reply goes on as it is: {text}");
        assert_eq!(text.matches("[DONE]").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn a_request_without_deferred_tools_streams_as_before() {
        let fake = Fake::default();
        fake.set_stream_script(vec![sse(TOOL_CALL, "tool_calls")]);
        let proxy = deferring(proxy_to_fake(&fake, None).await);
        let text = stream_text(&proxy).await;
        assert!(text.contains("\"tool_calls\""));
        let seen = seen_json(&fake);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].get("chat_template_kwargs").is_none());
    }

    #[tokio::test]
    async fn an_empty_reply_passes_when_the_repeat_is_off() {
        let fake = Fake::default();
        fake.set_stream_script(vec![sse(THINKING_ONLY, "stop")]);
        let proxy = proxy_to_fake(&fake, None).await;
        stream_text(&proxy).await;
        assert_eq!(fake.seen_with("/v1/chat/completions").len(), 1);
    }

    #[tokio::test]
    async fn an_empty_json_reply_is_repeated_without_thinking() {
        let empty = r#"{"choices":[{"finish_reason":"stop","index":0,"message":{"role":"assistant","content":"","reasoning_content":"x"}}]}"#;
        let fake = Fake::default();
        fake.set_chat_script(vec![empty.to_owned(), crate::llm::fake::LLAMACPP_CHAT.to_owned()]);
        let proxy = proxy_to_fake(&fake, None).await.with_retry_empty(true);
        let resp = proxy.forward(chat(false)).await;
        let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(fake.seen_with("/v1/chat/completions").len(), 2);
    }

    #[test]
    fn helpers() {
        assert!(reply_is_empty(
            br#"{"choices":[{"message":{"content":" ","tool_calls":[]}}]}"#
        ));
        assert!(!reply_is_empty(br#"{"choices":[{"message":{"content":"hi"}}]}"#));
        assert!(!reply_is_empty(b"not json"));
        let b = without_thinking(br#"{"a":1}"#).unwrap();
        assert!(String::from_utf8_lossy(&b).contains(r#""enable_thinking":false"#));
        assert!(without_thinking(br#"{"chat_template_kwargs":{"enable_thinking":false}}"#).is_none());
        assert!(is_chat_path("/v1/chat/completions"));
        assert!(!is_chat_path("/v1/models"));
    }

    #[test]
    fn the_area_line_moves_to_the_last_user_message() {
        let body = br#"{"messages":[
            {"role":"system","content":"Intro\nWhen controlling Home Assistant always call the intent tools.\nYou are in area Kitchen (floor Downstairs) and all generic commands should target this area.\n\nFollow these instructions."},
            {"role":"user","content":"first"},
            {"role":"assistant","content":"ok"},
            {"role":"user","content":"turn on the lights"}]}"#;
        let out: Value = serde_json::from_slice(&area_to_user_message(body).unwrap()).unwrap();
        let system = out["messages"][0]["content"].as_str().unwrap();
        assert!(!system.contains("You are in area"), "{system}");
        assert!(system.contains("intent tools.\n\nFollow these"), "{system}");
        assert_eq!(out["messages"][1]["content"], "first");
        let last = out["messages"][3]["content"].as_str().unwrap();
        assert!(
            last.starts_with("(You are in area Kitchen (floor Downstairs)"),
            "{last}"
        );
        assert!(last.ends_with("\nturn on the lights"), "{last}");
        // The system messages of two satellites are the same afterwards.
        let other = br#"{"messages":[{"role":"system","content":"Intro\nWhen controlling Home Assistant always call the intent tools.\nYou are in area Bedroom (floor Upstairs) and all generic commands should target this area.\n\nFollow these instructions."},{"role":"user","content":"x"}]}"#;
        let other: Value = serde_json::from_slice(&area_to_user_message(other).unwrap()).unwrap();
        assert_eq!(other["messages"][0]["content"], out["messages"][0]["content"]);
        // No line, no user message, no JSON: unchanged.
        assert!(
            area_to_user_message(
                br#"{"messages":[{"role":"system","content":"Intro"},{"role":"user","content":"x"}]}"#
            )
            .is_none()
        );
        assert!(
            area_to_user_message(br#"{"messages":[{"role":"system","content":"You are in area A (floor B) x."}]}"#)
                .is_none()
        );
        assert!(area_to_user_message(b"nope").is_none());
    }

    #[tokio::test]
    async fn the_proxy_sends_the_area_line_in_the_user_message() {
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, None).await.with_area_in_user(true);
        let body = r#"{"stream":false,"messages":[{"role":"system","content":"A\nYou are in area Hallway (floor Downstairs) and x.\nB"},{"role":"user","content":"hi"}]}"#;
        let resp = proxy
            .forward(request("POST", "/llm/v1/chat/completions", Body::from(body)))
            .await;
        assert_eq!(resp.status(), 200);
        let seen = fake.seen_with("/v1/chat/completions");
        let sent: Value = serde_json::from_slice(&seen[0].body).unwrap();
        assert_eq!(sent["messages"][0]["content"], "A\nB");
        assert!(
            sent["messages"][1]["content"]
                .as_str()
                .unwrap()
                .starts_with("(You are in area Hallway")
        );
        // Off: the body arrives as sent.
        let fake = Fake::default();
        let proxy = proxy_to_fake(&fake, None).await;
        proxy
            .forward(request("POST", "/llm/v1/chat/completions", Body::from(body)))
            .await;
        assert!(
            String::from_utf8_lossy(&fake.seen_with("/v1/chat/completions")[0].body)
                .contains("A\\nYou are in area Hallway")
        );
    }
}
