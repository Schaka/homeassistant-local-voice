//! `/llm/...` on the main port, forwarded to llama-server on 127.0.0.1.
//!
//! Request and response bodies stream through, so server-sent events from
//! `/llm/v1/chat/completions` reach the client token by token.

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
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
        })
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
        builder
            .body(Body::from_stream(resp.bytes_stream()))
            .unwrap_or_else(|err| error(StatusCode::BAD_GATEWAY, &err.to_string(), "upstream_error"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
