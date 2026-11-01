# Architecture

The container runs one Rust server, `assistant-hub`, on one port (10300). It is the only process that the network can reach. The native engines run as its child processes on 127.0.0.1, or inside its own process.

## Processes

| Part | Where it runs | Port |
|---|---|---|
| `assistant-hub` (Rust, tokio, axum on hyper) | Main process, under `tini` | 10300, all interfaces |
| STT: parakeet.cpp | Inside `assistant-hub`, through the C API of `libparakeet.so` | none |
| Web search: SearXNG | Inside `assistant-hub`, as a Python library through PyO3. SearXNG's own web server never starts. | none |
| TTS: audiocpp_server | Child process. The hub restarts it after an exit. | 127.0.0.1:8100 |
| LLM: llama-server | Child process. It starts only with a model setting. | 127.0.0.1:8081 |
| Media index: EmbeddingGemma 2 | Inside `assistant-hub`, through the C API of `libllama.so`, on the CPU. Only with `MEDIA_INDEX=on`. | none |

At start, `assistant-hub` asks `llama-server --list-devices` for the Vulkan GPUs. Without one, STT and TTS run on the CPU. Then it starts the child processes and restarts them 2 s after they exit. On SIGTERM it saves the prompt cache of llama-server, stops the children and exits. A stop takes less than a second.

## One port

The server reads the first byte of each new connection and does not consume it:

1. `{` is the start of a Wyoming event (a JSON header line). The connection goes to the Wyoming handler. Wyoming is plain TCP, not HTTP.
2. Any other byte is the start of an HTTP request. The connection goes to the HTTP router.

A client that sends nothing for 30 s is dropped.

## HTTP routes

| Route | Purpose |
|---|---|
| `GET /health` | Status of STT, TTS, LLM, web search and YouTube search. The Docker health check uses it (`assistant-hub healthcheck`). |
| `/llm/...` | Forwarded to llama-server without the `/llm` prefix. `/llm/v1/chat/completions` becomes `/v1/chat/completions`. Bodies stream in both directions, so server-sent events arrive token by token. With `LLM_API_KEY`, the server checks the key itself. |
| `POST /mcp` | MCP server (Streamable HTTP, JSON responses) with all tools. A new tool is one more entry in `src/mcp.rs`, not a new endpoint. |
| `GET /web/search?q=...&max_results=3` | Web search as JSON: the query as sent, the text that the MCP tool returns, and the results. |
| `GET /youtube/search?q=...` | One YouTube video to play, as JSON: `kind`, `video_id`, `title`, `channel`, `url`. See [search.md](search.md). |
| `GET /jellyfin/search?q=...` | One episode or movie of the Jellyfin library to play, as JSON. See [search.md](search.md#jellyfin-search). |
| `GET /media/search?q=...`, `GET /media/status` | The media index: its ranking and its state. See [media-index.md](media-index.md). |

## Wyoming

The `info` answer is the same as the one of the old Python server. So Home Assistant sees the same services (`parakeet` and `audiocpp`). STT collects the audio of a request as it arrives and converts it once at `audio-stop` to 16 kHz mono. TTS answers with `audio-start`, chunks of 4096 samples and `audio-stop`.

STT and TTS share one GPU lock, so they never run on the GPU at the same time. Three GPU faults in a row ("device lost", HTTP 500 from audiocpp_server) make the server exit with an error. The restart policy of the container then starts it with a clean GPU state.

## Source

| File | Content |
|---|---|
| `src/main.rs` | Command line, start and stop |
| `src/config.rs` | Environment variables |
| `src/server.rs` | Listener, first-byte check, HTTP routes |
| `src/wyoming/` | Wyoming framing and the STT and TTS handler |
| `src/stt.rs`, `src/tts.rs`, `src/audio.rs`, `src/gpu.rs` | parakeet.cpp binding, audiocpp client and text cleanup, PCM and WAV, GPU detection and GPU lock |
| `src/llm/` | llama-server arguments per model family, the proxy, the prompt cache |
| `src/search.rs`, `src/youtube.rs`, `src/jellyfin.rs`, `src/mcp.rs` | SearXNG through PyO3, YouTube search, Jellyfin search, MCP |
| `src/text.rs` | Matching of spoken names against titles, trigram similarity |
| `build.rs` | Generates the bindings of llama.cpp's C API with bindgen, from the headers of the llama.cpp build in the image |
| `src/media/` | Media index: EmbeddingGemma 2 through `libllama.so` (`embed.rs`), item texts and Jellyfin's change messages (`docs.rs`), vectors on disk (`index.rs`), sync and gate (`mod.rs`), real-library evaluation (`eval.rs`) |
| `src/supervisor.rs` | Child processes |
| `src/gguf.rs` | Check for truncated model files before the engines load them |
| `src/probe.rs` | `assistant-hub probe describe|stt|tts|llm` and `assistant-hub healthcheck` |

## Commands

| Command | Purpose |
|---|---|
| `assistant-hub serve` | The server (default command of the image) |
| `assistant-hub probe describe` | Print the Wyoming info of a running server |
| `assistant-hub probe stt FILE.wav` | Transcribe a WAV file |
| `assistant-hub probe tts "TEXT" OUT.wav` | Synthesize a WAV file |
| `assistant-hub probe llm "TEXT" [--devices 100]` | One chat request with a Home Assistant tool to `/llm/v1`: reply, tool calls, prompt and generation speed |
| `assistant-hub verify-gguf FILE...` | Check model files |
| `assistant-hub healthcheck` | Checks `/health`. Exit code 0 means healthy. |

All of them take `--addr host:port` (default `127.0.0.1:10300`). In the container: `docker exec <name> assistant-hub probe describe`.

## Build

The Dockerfile has these stages, all on Debian 13 "trixie":

1. audio.cpp from source at a pinned commit. Debian ships `glslc`, so shaderc is not built from source.
2. parakeet.cpp and llama.cpp as prebuilt Vulkan releases.
3. The default STT and TTS models.
4. SearXNG at a pinned commit, with its packages in `/opt/searxng/lib` for Python 3.13.
5. `assistant-hub` with cargo-chef (pinned image tag). Its `build.rs` reads the llama.cpp headers of stage 2: the dependencies build in their own cached layer, so a code change rebuilds only the crate.
6. Runtime: `debian:trixie-slim` with Mesa Vulkan drivers (RADV, ANV), `libpython3.13` (for the embedded SearXNG) and `tini`. There is no Python interpreter binary, no pip and no compiler in the image.

`docker-bake.hcl` holds every pinned version. `docker buildx bake final` locally builds the same image as CI.

## CI

| Workflow | Runs on | Jobs |
|---|---|---|
| `ci.yml` | Every push to a branch, every pull request | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, a release build (all `--locked`), and `cargo deny check all` (RustSec advisories, licenses, bans, sources) |
| `image.yml` | Push to `main`, tags `v*`, manual | Calls `ci.yml` first. Builds with the registry cache and pushes to ghcr.io with provenance and SBOM attestations. |

The toolchain version is in `rust-toolchain.toml` and in `docker-bake.hcl` (`RUST_VERSION`). The actions are pinned to commit SHAs. Dependabot proposes updates for crates, actions and base images every week.
