//! One TCP port for everything.
//!
//! Each connection is sorted by its first byte: a Wyoming client starts with a
//! JSON header (`{`), an HTTP client with a method name (`GET`, `POST`, ...).
//!
//! HTTP routes:
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /health` | Status of the parts |
//! | `/llm/...` | llama-server, for example `/llm/v1/chat/completions` |
//! | `POST /mcp` | MCP tools for the conversation agent |
//! | `GET /web/search?q=` | Web search as JSON |
//! | `GET /youtube/search?q=` | One YouTube video to play, as JSON |
//! | `GET /media/search?q=&limit=` | The media index ranking, for tests and other clients |
//! | `GET /media/status` | State and size of the media index |
//! | `GET /jellyfin/search?q=&devices=&user=` | One Jellyfin episode or movie to play, as JSON |

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::jellyfin::Jellyfin;
use crate::llm::proxy::LlmProxy;
use crate::mcp::Tools;
use crate::search::{self, SearchBackend};
use crate::wyoming::{self, Voice};
use crate::youtube::YouTube;

pub struct AppState {
    pub voice: Arc<Voice>,
    pub llm: Option<LlmProxy>,
    pub tools: Tools,
    pub web_search: Option<Arc<dyn SearchBackend>>,
    pub youtube: Option<Arc<YouTube>>,
    pub jellyfin: Option<Arc<Jellyfin>>,
    pub media: Option<Arc<crate::media::MediaIndex>>,
}

type Shared = Arc<AppState>;

fn json_error(status: StatusCode, error: &str, message: impl Into<String>) -> Response {
    (status, axum::Json(json!({"error": error, "message": message.into()}))).into_response()
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/mcp", any(mcp))
        .route("/web/search", get(web_search))
        .route("/youtube/search", get(youtube_search))
        .route("/jellyfin/search", get(jellyfin_search))
        .route("/media/search", get(media_search))
        .route("/media/status", get(media_status))
        .route("/llm", any(llm))
        .route("/llm/{*rest}", any(llm))
        .fallback(|| async { json_error(StatusCode::NOT_FOUND, "not_found", "no such route") })
        .with_state(state)
}

async fn health(State(app): State<Shared>) -> Response {
    let tts = app.voice.tts.healthy().await;
    let llm = match &app.llm {
        None => "off",
        Some(proxy) => proxy.health().await.name(),
    };
    let stt = app.voice.stt.is_some();
    // 503 when a configured part does not answer. The supervisor restarts a
    // hung engine, but the container must say so until that happens. A busy
    // LLM works on a request and is no fault.
    let ok = stt && tts && llm != "unavailable";
    let code = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        axum::Json(json!({
            "status": if ok { "ok" } else { "degraded" },
            "stt": stt,
            "stt_denoise": app.voice.denoise.is_some(),
            "tts": tts,
            "llm": llm,
            "web_search": app.web_search.is_some(),
            "youtube": app.youtube.is_some(),
            "jellyfin": app.jellyfin.is_some(),
            "media_index": app.media.as_ref().map_or(json!("off"), |m| json!(m.state())),
        })),
    )
        .into_response()
}

async fn llm(State(app): State<Shared>, req: Request<Body>) -> Response {
    match &app.llm {
        Some(proxy) => proxy.forward(req).await,
        None => json_error(
            StatusCode::NOT_FOUND,
            "llm_off",
            "no LLM is configured (LLM_HF_REPO or LLM_MODEL_FILENAME)",
        ),
    }
}

async fn mcp(State(app): State<Shared>, method: axum::http::Method, body: Bytes) -> Response {
    match method.as_str() {
        // No server-initiated stream: the spec allows 405 here.
        "GET" => (StatusCode::METHOD_NOT_ALLOWED, [("Allow", "POST, DELETE")]).into_response(),
        "DELETE" => StatusCode::OK.into_response(),
        "POST" => {
            let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(
                        json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}),
                    ),
                )
                    .into_response();
            };
            let batch = message.is_array();
            let messages = match &message {
                Value::Array(list) => list.clone(),
                other => vec![other.clone()],
            };
            let mut replies = Vec::new();
            for m in &messages {
                if let Some(r) = app.tools.handle(m).await {
                    replies.push(r);
                }
            }
            let mut headers = HeaderMap::new();
            if !batch
                && message["method"] == "initialize"
                && let Ok(v) = HeaderValue::from_str(&uuid::Uuid::new_v4().simple().to_string())
            {
                headers.insert("Mcp-Session-Id", v);
            }
            if replies.is_empty() {
                // Only notifications or responses.
                return (StatusCode::ACCEPTED, headers).into_response();
            }
            let body = if batch {
                Value::Array(replies)
            } else {
                replies.remove(0)
            };
            (headers, axum::Json(body)).into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn web_search(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(backend) = &app.web_search else {
        return json_error(
            StatusCode::NOT_FOUND,
            "search_off",
            "web search is off (SEARCH_ENABLED)",
        );
    };
    let Some(query) = q.get("q").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return json_error(StatusCode::BAD_REQUEST, "missing_q", "the parameter q is missing");
    };
    let count = q
        .get("max_results")
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(3)
        .clamp(1, 5);
    let query = search::with_year(query, search::current_year());
    match backend.search(query.clone(), count).await {
        Ok(results) => {
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            axum::Json(json!({
                "query": query,
                "text": search::format_results(&results, count, &today),
                "results": results.results.iter().take(count).map(|h| json!({
                    "title": h.title, "url": h.url, "content": h.content, "published": h.published,
                })).collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Err(err) => json_error(StatusCode::BAD_GATEWAY, "search_failed", format!("{err:#}")),
    }
}

async fn youtube_search(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(yt) = &app.youtube else {
        return json_error(
            StatusCode::NOT_FOUND,
            "youtube_off",
            "YouTube search is off (YOUTUBE_ENABLED)",
        );
    };
    let Some(query) = q.get("q").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return json_error(StatusCode::BAD_REQUEST, "missing_q", "the parameter q is missing");
    };
    match yt.search(query).await {
        Ok(Some(pick)) => {
            tracing::info!(
                "youtube {query:?} -> {:?} {} ({})",
                pick.kind,
                pick.video_id,
                pick.title
            );
            axum::Json(pick).into_response()
        }
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no_result", format!("nothing found for {query}")),
        Err(err) => {
            tracing::warn!("youtube {query:?}: {err:#}");
            json_error(StatusCode::BAD_GATEWAY, "upstream", format!("{err:#}"))
        }
    }
}

async fn media_status(State(app): State<Shared>) -> Response {
    match &app.media {
        Some(m) => axum::Json(m.status()).into_response(),
        None => json_error(
            StatusCode::NOT_FOUND,
            "media_off",
            "the media index is off (MEDIA_INDEX)",
        ),
    }
}

async fn media_search(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(media) = &app.media else {
        return json_error(
            StatusCode::NOT_FOUND,
            "media_off",
            "the media index is off (MEDIA_INDEX)",
        );
    };
    let Some(query) = q.get("q").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return json_error(StatusCode::BAD_REQUEST, "missing_q", "the parameter q is missing");
    };
    if !media.ready() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "the media index is not ready yet",
        );
    }
    let limit = q
        .get("limit")
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(10)
        .clamp(1, 100);
    let (ranked, best) = match (media.rank(query, limit, |_| true).await, media.semantic(query).await) {
        (Ok(r), Ok(b)) => (r, b),
        (Err(err), _) | (_, Err(err)) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "media_failed", format!("{err:#}"));
        }
    };
    axum::Json(json!({
        "query": query,
        "best": best,
        "hits": ranked.iter().take(limit).map(|h| json!({
            "id": h.entry.id, "kind": h.entry.kind, "name": h.entry.name,
            "series": h.entry.series_name, "score": h.score,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn jellyfin_search(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(jf) = &app.jellyfin else {
        return json_error(
            StatusCode::NOT_FOUND,
            "jellyfin_off",
            "Jellyfin search is off (JELLYFIN_URL, JELLYFIN_API_KEY)",
        );
    };
    let Some(query) = q.get("q").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return json_error(StatusCode::BAD_REQUEST, "missing_q", "the parameter q is missing");
    };
    match jf
        .search(
            query,
            q.get("user").map(String::as_str),
            q.get("devices").map(String::as_str),
        )
        .await
    {
        Ok(Some(pick)) => {
            tracing::info!(
                "jellyfin {query:?} -> {:?} {} S{:?}E{:?} at {} s",
                pick.kind,
                pick.title,
                pick.season,
                pick.episode,
                pick.resume_seconds
            );
            axum::Json(pick).into_response()
        }
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no_result", format!("nothing found for {query}")),
        Err(err) => {
            tracing::warn!("jellyfin {query:?}: {err:#}");
            json_error(StatusCode::BAD_GATEWAY, "upstream", format!("{err:#}"))
        }
    }
}

/// A client that sends nothing within this time is dropped.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);

async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    app: Shared,
    router: Router,
    shutdown: CancellationToken,
) {
    let _ = stream.set_nodelay(true);
    let mut first = [0u8; 1];
    match tokio::time::timeout(FIRST_BYTE_TIMEOUT, stream.peek(&mut first)).await {
        Ok(Ok(1)) => {}
        _ => return,
    }
    if first[0] == b'{' {
        tracing::debug!("wyoming connection from {peer}");
        if let Err(err) = wyoming::serve(stream, app.voice.clone()).await {
            tracing::warn!("wyoming connection from {peer} ended: {err:#}");
        }
        return;
    }
    let service = TowerToHyperService::new(router);
    let conn = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service);
    tokio::pin!(conn);
    tokio::select! {
        res = conn.as_mut() => {
            if let Err(err) = res {
                tracing::debug!("http connection from {peer}: {err}");
            }
        }
        () = shutdown.cancelled() => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

/// Accepts connections until `shutdown` fires.
pub async fn run(listener: TcpListener, app: Shared, shutdown: CancellationToken) {
    let router = router(app.clone());
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    tokio::spawn(serve_connection(stream, peer, app.clone(), router.clone(), shutdown.clone()));
                }
                Err(err) => {
                    tracing::warn!("accept failed: {err}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SpeechToText;
    use crate::tts::TextToSpeech;
    use crate::wyoming::protocol::{Event, read_event, write_event};
    use futures_util::future::BoxFuture;
    use tokio::io::BufReader;

    struct FakeStt;
    impl SpeechToText for FakeStt {
        fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String> {
            Ok(format!("{} samples", samples.len()))
        }
    }

    struct FakeTts(bool);
    impl TextToSpeech for FakeTts {
        fn synthesize<'a>(&'a self, text: &'a str, _voice: Option<&'a str>) -> BoxFuture<'a, anyhow::Result<Bytes>> {
            let samples = vec![0i16; text.len() * 1000];
            Box::pin(async move { Ok(Bytes::from(crate::audio::wav_bytes(&samples, 24_000, 1))) })
        }
        fn healthy(&self) -> BoxFuture<'_, bool> {
            Box::pin(async move { self.0 })
        }
    }

    async fn start_with(tts_healthy: bool, llm: Option<LlmProxy>) -> (SocketAddr, CancellationToken) {
        let voice = Arc::new(Voice {
            stt: Some(Arc::new(FakeStt)),
            denoise: None,
            tts: Arc::new(FakeTts(tts_healthy)),
            gpu: crate::gpu::Gpu::new(CancellationToken::new(), true, true),
            info: crate::wyoming::info("stt-model", "alba", &["en".to_owned()]),
            debug_dir: None,
        });
        let state = Arc::new(AppState {
            voice,
            llm,
            tools: Tools::default(),
            web_search: None,
            youtube: None,
            jellyfin: None,
            media: None,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(run(listener, state, shutdown.clone()));
        (addr, shutdown)
    }

    async fn start() -> (SocketAddr, CancellationToken) {
        start_with(true, None).await
    }

    #[tokio::test]
    async fn wyoming_and_http_share_one_port() {
        let (addr, shutdown) = start().await;

        let health: Value = reqwest::get(format!("http://{addr}/health"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["stt"], true);
        assert_eq!(health["llm"], "off");
        let off = reqwest::get(format!("http://{addr}/llm/v1/models")).await.unwrap();
        assert_eq!(off.status(), 404);

        let stream = TcpStream::connect(addr).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        write_event(&mut w, &Event::new("describe", json!({}))).await.unwrap();
        let info = read_event(&mut r).await.unwrap().unwrap();
        assert_eq!(info.kind, "info");
        assert_eq!(info.data["asr"][0]["models"][0]["name"], "stt-model");

        // 0.5 s at 48 kHz stereo in many small chunks becomes 8000 samples at 16 kHz mono.
        let fmt = json!({"rate": 48000, "width": 2, "channels": 2});
        write_event(&mut w, &Event::new("audio-start", fmt.clone()))
            .await
            .unwrap();
        for _ in 0..24 {
            let ev = Event::new("audio-chunk", fmt.clone()).with_payload(Bytes::from(vec![0u8; 4000]));
            write_event(&mut w, &ev).await.unwrap();
        }
        write_event(&mut w, &Event::new("audio-stop", json!({}))).await.unwrap();
        let transcript = read_event(&mut r).await.unwrap().unwrap();
        assert_eq!(transcript.kind, "transcript");
        assert_eq!(transcript.data["text"], "8000 samples");

        write_event(&mut w, &Event::new("synthesize", json!({"text": "**Hi** there"})))
            .await
            .unwrap();
        let mut kinds = Vec::new();
        let mut bytes = 0;
        loop {
            let ev = read_event(&mut r).await.unwrap().unwrap();
            bytes += ev.payload.as_ref().map_or(0, |p| p.len());
            kinds.push(ev.kind.clone());
            if ev.kind == "audio-stop" {
                break;
            }
        }
        assert_eq!(kinds.first().map(String::as_str), Some("audio-start"));
        // speakable("**Hi** there") is "Hi there." (9 chars): 9000 samples of 2 bytes.
        assert_eq!(bytes, 18_000);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn health_is_ok_without_llm() {
        let (addr, shutdown) = start().await;
        let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let health: Value = resp.json().await.unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["tts"], true);
        assert_eq!(health["llm"], "off");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn health_fails_when_an_engine_does_not_answer() {
        // Port 1 has no listener: llama-server is configured but unreachable.
        let llm = LlmProxy::new(1, None).unwrap();
        let (addr, shutdown) = start_with(true, Some(llm)).await;
        let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
        assert_eq!(resp.status(), 503);
        let health: Value = resp.json().await.unwrap();
        assert_eq!(health["status"], "degraded");
        assert_eq!(health["llm"], "unavailable");
        shutdown.cancel();

        let (addr, shutdown) = start_with(false, None).await;
        let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
        assert_eq!(resp.status(), 503);
        let health: Value = resp.json().await.unwrap();
        assert_eq!(health["status"], "degraded");
        assert_eq!(health["tts"], false);
        shutdown.cancel();
    }
}
