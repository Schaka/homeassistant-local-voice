//! The output of the engine child processes.
//!
//! The engines print their model metadata at each start and several lines for
//! each request. ik_llama.cpp also prints three lines for each `/health`
//! request, and the hub sends two of them every 30 s. The hub reads stdout
//! and stderr of each child line by line and gives each line a level:
//!
//! 1. A line that names a problem (error, failure, abort, warning, ...) is a
//!    warning.
//! 2. The timings of a request (`prompt eval time`, `eval time`,
//!    `total time`) are info.
//! 3. All other lines are debug.
//!
//! The debug lines of a run stay in a short tail. When a child exits by
//! itself or hangs, the supervisor logs this tail as warnings, so the log
//! shows the cause without debug output. All lines use the log target
//! `engine`: `RUST_LOG=info,engine=debug` shows the full output.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::task::JoinHandle;

/// The debug lines that the tail keeps for a crash report.
const TAIL_LINES: usize = 30;
/// A longer line is cut in the tail. The debug log gets the full line.
const TAIL_LINE_CHARS: usize = 500;

/// Words of a line that reports a problem, in lower case. The engines use
/// different formats and levels, so the words decide, not a level prefix.
/// `err [` and `warn [` are the level prefixes of ik_llama.cpp.
const PROBLEM_WORDS: &[&str] = &[
    "error",
    "err [",
    "fail",
    "fatal",
    "abort",
    "assert",
    "exception",
    "panic",
    "segmentation fault",
    "core dumped",
    "terminate called",
    "out of memory",
    "device lost",
    "warn",
];

/// Words of the timing lines of a request, in lower case.
const TIMING_WORDS: &[&str] = &["eval time", "total time"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Debug,
    Info,
    Warn,
}

fn level(line: &str) -> Level {
    let lower = line.to_lowercase();
    if PROBLEM_WORDS.iter().any(|word| lower.contains(word)) {
        Level::Warn
    } else if TIMING_WORDS.iter().any(|word| lower.contains(word)) {
        Level::Info
    } else {
        Level::Debug
    }
}

/// The last debug lines of one run of a child.
#[derive(Clone, Default)]
pub struct Tail(Arc<Mutex<VecDeque<String>>>);

impl Tail {
    fn push(&self, line: &str) {
        let line = match line.char_indices().nth(TAIL_LINE_CHARS) {
            Some((cut, _)) => format!("{}…", &line[..cut]),
            None => line.to_owned(),
        };
        let mut lines = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if lines.len() == TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Logs the kept lines as warnings and empties the tail.
    pub fn report(&self, name: &str) {
        let lines: Vec<String> = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        if lines.is_empty() {
            return;
        }
        tracing::warn!(target: "engine", "[{name}] its last {} lines of debug output:", lines.len());
        for line in lines {
            tracing::warn!(target: "engine", "[{name}] | {line}");
        }
    }
}

/// Reads `pipe` line by line until it closes and logs each line. It must
/// read to the end: a child that writes to a full pipe blocks.
pub fn forward(name: String, pipe: impl AsyncRead + Unpin + Send + 'static, tail: Tail) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&buf);
            let line = text.trim_end();
            if line.is_empty() {
                continue;
            }
            match level(line) {
                Level::Warn => tracing::warn!(target: "engine", "[{name}] {line}"),
                Level::Info => tracing::info!(target: "engine", "[{name}] {line}"),
                Level::Debug => {
                    tracing::debug!(target: "engine", "[{name}] {line}");
                    tail.push(line);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_and_load_lines_are_debug() {
        for line in [
            r#"INFO [      log_server_request] request | tid="1" timestamp=1 remote_addr="127.0.0.1" remote_port=36280 status=200 method="GET" path="/health" params={}"#,
            r#"INFO [              slots_idle] all slots are idle | tid="1" timestamp=1"#,
            r#"INFO [     process_single_task] slot data | tid="1" timestamp=1 id_task=0 n_idle_slots=1 n_processing_slots=0"#,
            "llm_load_print_meta: n_expert         = 128",
            "print_info: EOG token             = 156895 '<|role_end|>'",
            "Layer  3:    178.71,     72.00,    250.71      256.00  MiB",
            "======================================= HAVE_FANCY_SIMD is NOT defined",
            "llama_init_from_model: n_ctx         = 65536",
            "...........................................................................",
            "srv  update_slots: all slots are idle",
            "slot launch_slot_: id  0 | task 12 | processing task, is_child = 0",
            "init: chat template, example_format: '<role>SYSTEM</role>You are a helpful assistant",
            "srv          init: Exclude reasoning tokens when selecting slot based on similarity: start: <think>, end: </think>",
        ] {
            assert_eq!(level(line), Level::Debug, "{line}");
        }
    }

    #[test]
    fn timings_are_info() {
        for line in [
            "prompt eval time =     205.60 ms /    20 tokens (   10.28 ms per token,    97.28 tokens per second)",
            "       eval time =     541.18 ms /    32 tokens (   16.91 ms per token,    59.13 tokens per second)",
            "      total time =     746.78 ms /    52 tokens",
            r#"INFO [           print_timings] prompt eval time     =     123.45 ms /   100 tokens | tid="1""#,
        ] {
            assert_eq!(level(line), Level::Info, "{line}");
        }
    }

    #[test]
    fn problems_are_warnings() {
        for line in [
            "llama_model_load: error loading model: tensor 'blk.0.attn_q.weight' data is not within the file bounds",
            "common_init_from_params: failed to load model '/models/llm/m.gguf'",
            "/src/ggml/src/ggml-vulkan.cpp:5000: GGML_ASSERT(ctx->device) failed",
            "terminate called after throwing an instance of 'vk::DeviceLostError'",
            r#"ERR [              send_error] task error | tid="1" id_task=5"#,
            r#"WARN [                    main] something | tid="1""#,
            "srv  operator(): got exception: unknown model",
        ] {
            assert_eq!(level(line), Level::Warn, "{line}");
        }
    }

    #[test]
    fn the_tail_keeps_the_last_lines_and_cuts_long_ones() {
        let tail = Tail::default();
        for i in 0..TAIL_LINES + 5 {
            tail.push(&format!("line {i}"));
        }
        tail.push(&"x".repeat(TAIL_LINE_CHARS + 10));
        let lines = tail.0.lock().unwrap();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "line 6");
        assert_eq!(lines[TAIL_LINES - 1].chars().count(), TAIL_LINE_CHARS + 1);
    }

    #[tokio::test]
    async fn forward_reads_every_line_and_keeps_the_debug_ones() {
        let (mut write, read) = tokio::io::duplex(64);
        let tail = Tail::default();
        let task = forward("test".into(), read, tail.clone());
        tokio::io::AsyncWriteExt::write_all(
            &mut write,
            b"loading\r\n\nerror: boom\nprompt eval time = 1 ms\nlast without newline",
        )
        .await
        .unwrap();
        drop(write);
        task.await.unwrap();
        let lines: Vec<String> = tail.0.lock().unwrap().iter().cloned().collect();
        assert_eq!(lines, ["loading", "last without newline"]);
    }
}
