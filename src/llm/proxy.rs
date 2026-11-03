//! `/llm/...` on the main port, forwarded to llama-server on 127.0.0.1.
//!
//! Request and response bodies stream through, so server-sent events from
//! `/llm/v1/chat/completions` reach the client token by token.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::json;

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
            .build()?;
        Ok(Self {
            client,
            backend: format!("http://127.0.0.1:{port}"),
            api_key,
            in_flight: Arc::default(),
        })
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
        let url = format!("{}{}", self.backend, backend_path(&parts.uri));
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut out = self.client.request(method, &url);
        for (name, value) in &parts.headers {
            if keep_header(name) && name != header::AUTHORIZATION {
                out = out.header(name.as_str(), value.as_bytes());
            }
        }
        if parts.method != Method::GET && parts.method != Method::HEAD {
            out = out.body(reqwest::Body::wrap_stream(body.into_data_stream()));
        }
        let resp = match out.send().await {
            Ok(resp) => resp,
            Err(err) if err.is_connect() => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "llama-server is not ready yet",
                    "unavailable",
                );
            }
            Err(err) => {
                tracing::warn!("LLM proxy: {err}");
                return error(StatusCode::BAD_GATEWAY, &err.to_string(), "upstream_error");
            }
        };
        let mut builder = Response::builder().status(resp.status().as_u16());
        for (name, value) in resp.headers() {
            if !HOP_HEADERS.contains(&name.as_str()) {
                builder = builder.header(name.as_str(), value.as_bytes());
            }
        }
        // The count ends with the body stream, after the last token.
        let body = resp.bytes_stream().map(move |chunk| {
            let _counted = &in_flight;
            chunk
        });
        builder
            .body(Body::from_stream(body))
            .unwrap_or_else(|err| error(StatusCode::BAD_GATEWAY, &err.to_string(), "upstream_error"))
    }
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
}
