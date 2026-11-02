//! What assistant-hub needs from a real LLM engine, tested on the real binary.
//!
//! The unit tests use a fake engine that replays recordings. These tests start
//! a real `llama-server` (mainline llama.cpp or ik_llama.cpp) and check that it
//! still answers the way the recordings say. Run them after each engine bump,
//! and for each engine that the image can start:
//!
//! ```text
//! LLM_CONTRACT_ENGINE=llamacpp LLM_CONTRACT_BIN=/opt/llama.cpp/llama-server \
//!   LLM_CONTRACT_MODEL=/models/llm/Ling-3.0-tiny-Q4_K_M.gguf \
//!   cargo test --test llm_contract -- --ignored --test-threads=1
//! ```
//!
//! `LLM_CONTRACT_ENGINE` is `llamacpp` or `ik`. The tests run in two tiers:
//!
//! 1. With `LLM_CONTRACT_TINY=1` and a tiny model (`stories260K.gguf`, 1 MB),
//!    only the HTTP behavior is tested: health, models, the event stream, the
//!    timings, the prompt cache, slots, saving and restoring, stopping and the
//!    restart on the same port. CI runs this tier for every engine of the image.
//! 2. Without it, the model needs a thinking template and tool calls
//!    (Ling-3.0-tiny is the reference). The tests then also check the tool call
//!    and the thinking apart from the reply.
//!
//! The tests share one port at a time, so run them with one thread.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The time that the hub gives a child to stop after SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(5);

struct Engine {
    child: Child,
    port: u16,
    kind: String,
    slots: tempfile::TempDir,
    http: reqwest::Client,
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} (see the top of tests/llm_contract.rs)"))
}

/// True for the tiny model: it has no thinking and no tool calls.
fn tiny() -> bool {
    std::env::var("LLM_CONTRACT_TINY").is_ok_and(|v| v == "1")
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn args(kind: &str, model: &Path, port: u16, slots: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "-m",
        &model.display().to_string(),
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "-c",
        "8192",
        "--slot-save-path",
        &slots.display().to_string(),
        "--temp",
        "0.1",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if tiny() {
        a.extend(["--chat-template", "chatml", "-n", "32"].map(str::to_owned));
    } else {
        a.extend(["--jinja", "--reasoning", "on", "--reasoning-budget", "256", "-n", "256"].map(str::to_owned));
    }
    if kind == "llamacpp" {
        a.extend(["--device", "none", "-ngl", "0", "--no-ui"].map(str::to_owned));
    }
    a
}

impl Engine {
    async fn start_on(port: u16) -> Self {
        let kind = env("LLM_CONTRACT_ENGINE");
        assert!(
            kind == "llamacpp" || kind == "ik",
            "LLM_CONTRACT_ENGINE is llamacpp or ik"
        );
        let slots = tempfile::tempdir().unwrap();
        let child = Command::new(env("LLM_CONTRACT_BIN"))
            .args(args(
                &kind,
                &PathBuf::from(env("LLM_CONTRACT_MODEL")),
                port,
                slots.path(),
            ))
            // Some engines write a log file to the working directory.
            .current_dir(slots.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the engine");
        let mut e = Self {
            child,
            port,
            kind,
            slots,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
        };
        e.wait_ready(Duration::from_secs(180)).await;
        e
    }

    async fn start() -> Self {
        Self::start_on(free_port()).await
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn wait_ready(&mut self, limit: Duration) {
        let end = Instant::now() + limit;
        loop {
            if let Ok(r) = self
                .http
                .get(self.url("/health"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                && r.status() == 200
            {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("the engine exited with {status} before it was ready");
            }
            assert!(Instant::now() < end, "the engine was not ready after {limit:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn chat(&self, stream: bool) -> reqwest::Response {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "You are a voice assistant for a smart home. Answer in one short sentence. Devices: light.kitchen (Kitchen Light) = off; light.bedroom (Bedroom Light) = on."},
                {"role": "user", "content": "Turn on the kitchen light."}],
            "temperature": 0.1, "stream": stream,
        });
        if tiny() {
            body["max_tokens"] = json!(24);
        } else {
            body["tools"] = json!([{"type": "function", "function": {
                "name": "HassTurnOn", "description": "Turns on a device",
                "parameters": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}}]);
        }
        self.http
            .post(self.url("/v1/chat/completions"))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// Sends SIGTERM and waits for the exit. Returns the time it took, or None
    /// if the engine had to be killed after `limit`.
    fn stop(&mut self, limit: Duration) -> Option<Duration> {
        let begin = Instant::now();
        if self.child.try_wait().unwrap().is_some() {
            return Some(Duration::ZERO);
        }
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        while begin.elapsed() < limit {
            if self.child.try_wait().unwrap().is_some() {
                return Some(begin.elapsed());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.stop(Duration::from_secs(15));
    }
}

#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn health_and_models() {
    let e = Engine::start().await;
    let h: Value = e.http.get(e.url("/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(h["status"], "ok");
    let m: Value = e
        .http
        .get(e.url("/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(m["data"][0]["id"].is_string(), "{m}");
}

/// The evaluation reads these fields of every answer.
#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn the_timings_and_the_usage_that_the_evaluation_reads() {
    let e = Engine::start().await;
    let v: Value = e.chat(false).await.json().await.unwrap();
    let message = &v["choices"][0]["message"];
    assert!(
        message["content"].is_string() || message["tool_calls"].is_array(),
        "{v}"
    );
    for key in ["prompt_n", "prompt_ms", "predicted_n", "predicted_ms"] {
        assert!(
            v["timings"][key].is_number(),
            "timings.{key} is missing: {}",
            v["timings"]
        );
    }
    assert!(v["usage"]["prompt_tokens_details"]["cached_tokens"].is_number());
    assert!(v["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn a_tool_call_with_the_thinking_apart() {
    if tiny() {
        eprintln!("skipped: the tiny model has no tool calls");
        return;
    }
    let e = Engine::start().await;
    let v: Value = e.chat(false).await.json().await.unwrap();
    let choice = &v["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls", "{v}");
    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "HassTurnOn");
    let arguments: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert!(arguments["name"].as_str().unwrap().contains("kitchen"), "{arguments}");
    assert!(
        choice["message"]["reasoning_content"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the thinking goes to reasoning_content, so TTS does not speak it"
    );
    assert_eq!(choice["message"]["content"].as_str().unwrap_or(""), "");
}

#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn a_streamed_answer_ends_with_done() {
    let e = Engine::start().await;
    let text = e.chat(true).await.text().await.unwrap();
    let events: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("data: ")).collect();
    assert_eq!(events.last(), Some(&"[DONE]"), "{text}");
    let chunks: Vec<Value> = events
        .iter()
        .filter(|e| **e != "[DONE]")
        .map(|e| serde_json::from_str(e).unwrap())
        .collect();
    assert!(!chunks.is_empty());
    assert!(
        chunks.iter().any(|c| c["choices"][0]["finish_reason"].is_string()),
        "the last chunk names the reason"
    );
    if !tiny() {
        assert!(
            chunks
                .iter()
                .any(|c| c["choices"][0]["delta"]["reasoning_content"].is_string()),
            "the thinking streams apart from the reply"
        );
        assert!(chunks.iter().any(|c| c["choices"][0]["finish_reason"] == "tool_calls"));
    }
}

#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn the_second_identical_request_reuses_the_prompt_cache() {
    let e = Engine::start().await;
    let first: Value = e.chat(false).await.json().await.unwrap();
    let second: Value = e.chat(false).await.json().await.unwrap();
    let (a, b) = (
        first["timings"]["prompt_n"].as_u64().unwrap(),
        second["timings"]["prompt_n"].as_u64().unwrap(),
    );
    assert!(b < a / 2, "the engine processed {b} of {a} prompt tokens again");
}

#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn slots_show_whether_a_request_runs() {
    let e = Engine::start().await;
    let _ = e.chat(false).await.text().await;
    let slots: Value = e.http.get(e.url("/slots")).send().await.unwrap().json().await.unwrap();
    let first = &slots[0];
    assert!(first["id"].is_number() && first["id_task"].is_number(), "{first}");
    // mainline: is_processing and n_prompt_tokens. ik: state (0 is idle).
    match e.kind.as_str() {
        "llamacpp" => {
            assert_eq!(first["is_processing"], false);
            assert!(first["n_prompt_tokens"].is_number());
        }
        _ => assert_eq!(first["state"], 0),
    }
}

#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn a_slot_is_saved_and_restored_by_file_name() {
    let e = Engine::start().await;
    let _ = e.chat(false).await.text().await;
    let slots: Value = e.http.get(e.url("/slots")).send().await.unwrap().json().await.unwrap();
    let slot = slots
        .as_array()
        .unwrap()
        .iter()
        .max_by_key(|s| s["id_task"].as_i64().unwrap_or(-1))
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let saved: Value = e
        .http
        .post(e.url(&format!("/slots/{slot}?action=save")))
        .json(&json!({"filename": "contract.slot"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(saved["n_saved"].as_i64().unwrap() > 0, "{saved}");
    assert!(e.slots.path().join("contract.slot").is_file());
    let restored: Value = e
        .http
        .post(e.url(&format!("/slots/{slot}?action=restore")))
        .json(&json!({"filename": "contract.slot"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(restored["n_restored"], saved["n_saved"], "{restored}");
}

/// mainline answers a restore of a missing file with 400 at once. ik gives no
/// answer at all. The hub only restores a file that exists, but a damaged file
/// must not block it for long.
#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn restoring_a_missing_file_gives_an_error_or_no_answer_but_never_success() {
    let e = Engine::start().await;
    let r = e
        .http
        .post(e.url("/slots/0?action=restore"))
        .json(&json!({"filename": "missing.slot"}))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    match (e.kind.as_str(), r) {
        ("llamacpp", Ok(r)) => assert!(r.status().is_client_error(), "{}", r.status()),
        ("ik", Err(err)) => assert!(err.is_timeout(), "{err}"),
        (kind, other) => panic!("{kind}: {other:?}"),
    }
    // The engine still works.
    assert_eq!(e.chat(false).await.status(), 200);
}

/// The hub stops a child with SIGTERM and kills it after 5 s. An engine that
/// needs longer makes every stop and every restart slower.
#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn sigterm_stops_the_engine_within_the_grace_time_of_the_hub() {
    let mut e = Engine::start().await;
    // The client keeps its connections open, like the hub's proxy.
    for _ in 0..3 {
        let _ = e.chat(false).await.text().await;
    }
    let took = e.stop(STOP_GRACE);
    assert!(
        took.is_some(),
        "the engine did not stop within {STOP_GRACE:?} of SIGTERM"
    );
}

/// The hub restarts a stopped engine 2 s later on the same port. A port in
/// TIME_WAIT must not stop the new process from listening.
#[tokio::test]
#[ignore = "needs a real engine and a model, see the top of the file"]
async fn a_restarted_engine_listens_on_the_same_port_after_two_seconds() {
    let port = free_port();
    let mut first = Engine::start_on(port).await;
    for _ in 0..3 {
        let _ = first.chat(false).await.text().await;
    }
    first.stop(Duration::from_secs(15));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let second = Engine::start_on(port).await;
    assert_eq!(second.chat(false).await.status(), 200);
}
