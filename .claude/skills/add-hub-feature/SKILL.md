---
name: add-hub-feature
description: Use when the user asks for a new feature of the voice server assistant-hub, such as a route, an MCP tool, a search, a background service, a configuration variable or a native inference engine, and when you edit src/server.rs, src/mcp.rs, src/config.rs or add a module under src/. Covers the design rules, where the code goes, concurrency, native libraries, configuration, tests (unit, fake servers, live and evaluation tests), documentation and the checks before a push. Not for version bumps, see bump-native-engine.
---

# Add a feature to assistant-hub

Read `AGENTS.md` first. The media index (`src/media/`) is the most complete example of every step below.

## 1. Design check

Answer these before you write code:

1. Does it fit one port and one process? A new HTTP API is a route in `src/server.rs`. A new LLM tool is an entry in `src/mcp.rs`. No sidecar server, no second container.
2. Does upstream code exist? Embed it (a library through FFI or PyO3, or a child process as for llama-server) instead of a rewrite. Do not copy upstream files into the repository: take them from the pinned upstream version in the Dockerfile.
3. What does it cost on a 4th-generation Core i5 without a GPU (`docs/hardware.md`)? Measure RAM, CPU time per request and the time for any one-time work. Anything that costs more than a few MB or loads a model must be optional and off by default.
4. Which failure is worse: no answer or a wrong answer? For anything that acts (plays, switches), a wrong answer is worse. Plan a gate that can say "nothing found".

## 2. Where the code goes

| Part | Place |
|---|---|
| The feature | A module `src/<feature>.rs`, or `src/<feature>/mod.rs` with submodules for a bigger one. Start with a module comment: what it does, its rules, and why. |
| Configuration | A struct and a `*_from_env` function in `src/config.rs`, an `Option<...>` field in `Config`. Every variable gets a default and is validated with a clear error. |
| State | A field in `AppState` (`src/server.rs`), built in `run` in `src/main.rs`. Background tasks get the `shutdown` token. |
| Routes | `src/server.rs`: a handler, the route in `router`, a line in the module comment and an entry in `/health`. Errors use `json_error` with a short code. |
| Shared helpers | `src/text.rs` for name matching, `src/audio.rs` for PCM. |

## 3. Concurrency rules

1. Never block the tokio runtime. Short blocking work goes to `spawn_blocking`. Long or CPU-heavy background work runs on its own thread with a lower priority (`setpriority`, nice 10), as `media::Worker` does. Do not renice threads of the blocking pool: STT also runs there.
2. GPU work holds the lock in `src/gpu.rs`. New engines run on the CPU unless they replace STT or TTS.
3. Background tasks must stop when the `shutdown` token fires and must survive a lost connection (retry with a delay, then a full resync).

## 4. Native libraries

1. Open them at runtime with `libloading` and `RTLD_LOCAL` (the default). `libparakeet.so` carries its own ggml with the same symbol names as llama.cpp's ggml. Global loading would mix them.
2. Generate bindings with bindgen in `build.rs` from the headers of the pinned build. The headers come from the same upstream tag as the library (see the llamacpp stage in the Dockerfile and `scripts/fetch-llama-headers.sh`). Never commit generated bindings or upstream headers.
3. Every `unsafe` block gets a `// SAFETY:` comment. Raw pointers live behind a `Mutex`, and `unsafe impl Send/Sync` explains why it is sound.
4. Compare the results with the upstream tool once (for the embedder: the vectors must equal those of llama-server).

## 5. Data on disk

1. Write to `*.tmp` and rename, so a crash never leaves a broken file.
2. Store a fingerprint (model, sizes, format version) and rebuild when it does not match.
3. Keep data in a mounted folder (`/models/llm/...`) and document the path.
4. Use a stable hash (FNV-1a in `src/media/index.rs`), never `DefaultHasher`, for data that outlives the process.

## 6. Outside services

1. A `reqwest::Client` per service with timeouts.
2. Prefer change events (WebSocket) plus a periodic full comparison over polling only.
3. Tolerate odd data: optional fields get `#[serde(default)]`. Jellyfin sends episodes without a name, and one such item must not break a whole list.

## 7. Tests

1. Unit tests for every pure function: parsing, matching, scoring, gates, file formats.
2. Fake servers with axum for outside services (see `src/media/tests.rs`: paging, ID filters, a WebSocket with tokio-tungstenite). Do not pause tokio time in tests that do network I/O: HTTP timeouts would fire. Make delays adjustable instead.
3. A fake engine where a model would run (the hashed bag of words in `src/media/tests.rs`).
4. Ignored tests for real systems, configured with environment variables: `#[ignore = "needs ..."]`, run with `cargo test --release <name> -- --ignored --nocapture`.
5. For anything with a score or a threshold, an evaluation test on real data (`src/media/eval.rs`): labeled positives, typos, descriptions and negatives (things that must find nothing). Tune thresholds only from its numbers, and write the numbers into the documentation.

## 8. Documentation

1. A page in `docs/` for a bigger feature: what it does, how it decides, resources, configuration, routes, tests, why this design.
2. `docs/configuration.md` (every variable), `docs/architecture.md` (processes, routes, source files), the feature table in `README.md`, `docs/hardware.md` if it needs RAM or CPU.
3. A tested example in `examples/home-assistant/` if Home Assistant uses it.
4. Plain English, as in `AGENTS.md`.
5. State the current state only, in documentation, comments and messages (rule 9 of `AGENTS.md`). When the feature replaces behavior, rewrite the old sentences. Do not describe the change.

## 9. Before the push

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo deny check all
```

Then build the image or wait for CI, and test it without a GPU: `podman run --cpus=4 ghcr.io/schaka/homeassistant-local-voice:latest`, then `assistant-hub probe tts|stt|llm` inside it and the new routes from outside.
