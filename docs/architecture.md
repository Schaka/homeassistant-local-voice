# Architecture

The container runs one Rust server, `assistant-hub`, on one port (10300). It is the only process that the network can reach. The native engines run as its child processes on 127.0.0.1, or inside its own process.

## Processes

| Part | Where it runs | Port |
|---|---|---|
| `assistant-hub` (Rust, tokio, axum on hyper) | Main process, under `tini` | 10300, all interfaces |
| STT: parakeet.cpp | Inside `assistant-hub`, through the C API of `libparakeet.so` | none |
| Web search: SearXNG | Inside `assistant-hub`, as a Python library through PyO3. SearXNG's own web server never starts. | none |
| TTS: audiocpp_server | Child process. The hub restarts it after an exit. | 127.0.0.1:8100 |
| LLM: llama-server of llama.cpp or of ik_llama.cpp | Child process, one of the two at a time. It starts only with a model setting. | 127.0.0.1:8081 |
| Media index: EmbeddingGemma 2 | Inside `assistant-hub`, through the C API of `libllama.so`, on the CPU. Only with `MEDIA_INDEX=on`. | none |

At start, `assistant-hub` asks `llama-server --list-devices` for the Vulkan GPUs. Without one, STT and TTS run on the CPU. Then it starts the child processes and restarts them 2 s after they exit. On SIGTERM it saves the prompt cache of the LLM engine, stops the children and exits. A stop takes less than a second.

The hub also probes `/health` of both children every 30 s. A child that exits is restarted at once. A child that hangs keeps its port open and never exits, so only a probe can find it. After four failed probes in a row, the hub stops the child and the normal restart starts a fresh engine. A start that has not answered yet is loading its model. As a result, failed probes only count after a grace period. Until the first answer, the probe runs every second, and the log shows the start time (`[llama-server] ready after 7.4 s`).

The hub reads the output of the children and logs each line with a level. Problems are warnings, the timings of an LLM request are info, and the rest is debug. [configuration.md](configuration.md#ports) has the details.

## One port

The server reads the first byte of each new connection and does not consume it:

1. `{` is the start of a Wyoming event (a JSON header line). The connection goes to the Wyoming handler. Wyoming is plain TCP, not HTTP.
2. Any other byte is the start of an HTTP request. The connection goes to the HTTP router.

A client that sends nothing for 30 s is dropped.

## HTTP routes

| Route | Purpose |
|---|---|
| `GET /health` | Status of STT, TTS, LLM, web search and YouTube search. The hub probes STT and TTS directly and the LLM through `/health` of llama-server. The answer is 503 unless `status` is `ok`. The Docker health check uses it (`assistant-hub healthcheck`). |
| `/llm/...` | Forwarded to llama-server without the `/llm` prefix. `/llm/v1/chat/completions` becomes `/v1/chat/completions`. Bodies stream in both directions, so server-sent events arrive token by token. With `LLM_API_KEY`, the server checks the key itself. |
| `POST /mcp` | MCP server (Streamable HTTP, JSON responses) with all tools. A new tool is one more entry in `src/mcp.rs`, not a new endpoint. |
| `GET /web/search?q=...&max_results=3` | Web search as JSON: the query as sent, the text that the MCP tool returns, and the results. |
| `GET /youtube/search?q=...` | One YouTube video to play, as JSON: `kind`, `video_id`, `title`, `channel`, `url`. See [search.md](search.md). |
| `GET /jellyfin/search?q=...` | One episode or movie of the Jellyfin library to play, as JSON. See [search.md](search.md#jellyfin-search). |
| `GET /media/search?q=...`, `GET /media/status` | The media index: its ranking and its state. See [media-index.md](media-index.md). |

## Wyoming

The `info` answer lists the services `parakeet` and `audiocpp`. STT collects the audio of a request as it arrives and converts it once at `audio-stop` to 16 kHz mono. TTS answers with `audio-start`, chunks of 4096 samples and `audio-stop`.

STT and TTS share one GPU lock, so they never run on the GPU at the same time. Three GPU faults in a row ("device lost", HTTP 500 from audiocpp_server, a timeout of 120 s) make the server exit with an error. The restart policy of the container then starts it with a clean GPU state.

## Source

| File | Content |
|---|---|
| `src/main.rs` | Command line, start and stop |
| `src/config.rs` | Environment variables |
| `src/supervisor.rs` | Child processes: start, restart, health probes, the log level of their output (`supervisor/output.rs`) |
| `src/server.rs` | Listener, first-byte check, HTTP routes |
| `src/wyoming/` | Wyoming framing and the STT and TTS handler |
| `src/stt.rs`, `src/tts.rs`, `src/audio.rs`, `src/gpu.rs` | parakeet.cpp binding, audiocpp client and text cleanup, PCM and WAV, GPU detection and GPU lock |
| `src/llm/` | The engine choice (`engine.rs`) and the spec of the child process for each start (`specs.rs`), the arguments for llama.cpp (`llamacpp.rs`) and ik_llama.cpp (`ik.rs`) with the shared sampling values (`mod.rs`), the proxy, the prompt cache, the evaluation (`eval.rs`), and a fake llama-server for tests (`fake.rs`) |
| `src/search.rs`, `src/youtube.rs`, `src/jellyfin.rs`, `src/mcp.rs` | SearXNG through PyO3, YouTube search, Jellyfin search, MCP |
| `src/text.rs` | Matching of spoken names against titles, trigram similarity |
| `build.rs` | Generates the bindings of llama.cpp's C API with bindgen, from the headers of the llama.cpp build in the image |
| `src/media/` | Media index: EmbeddingGemma 2 through `libllama.so` (`embed.rs`), item texts and Jellyfin's change messages (`docs.rs`), vectors on disk (`index.rs`), sync and gate (`mod.rs`), real-library evaluation (`eval.rs`) |
| `src/supervisor.rs` | Child processes. A spec source makes the spec for each start, so the LLM can choose its engine anew at every start. |
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
| `assistant-hub healthcheck` | Calls `/health`. Exit code 0 means the answer is 200 and `status` is `ok`. |
| `assistant-hub llm-command` | Prints the command line of the LLM engine for the current environment as JSON, and prepares its files (chat template, cache folder) |
| `assistant-hub eval llm [--base-url URL] [--repeat N] [--out FILE]` | Asks the Assist questions of `src/llm/assist.json` and scores the answers. See "Tests" below. |
| `assistant-hub eval compare A.json B.json` | Decides whether engine B may replace engine A. Exit code 1 means no. |

All of them take `--addr host:port` (default `127.0.0.1:10300`). In the container: `docker exec <name> assistant-hub probe describe`.

## Tests

`cargo test` needs no model and no GPU. The LLM code has four layers:

1. Unit tests with a fake llama-server (`src/llm/fake.rs`). Its answers come from recordings of the real server in `tests/fixtures/llm/<engine>/`. `scripts/record-llm-fixtures.sh ENGINE BINARY MODEL` writes them. A new engine version that changes an answer shows up as a diff of these files. The tests cover the command line of each model family, the child process spec, the proxy (streaming, key check, errors, a client that leaves), the supervisor (restart, hang, stop with SIGTERM and SIGKILL) and the prompt cache.
2. Contract tests with a real engine (`tests/llm_contract.rs`, ignored by default). They check health, the timings, the event stream, the prompt cache, slots, stopping on SIGTERM and the restart on the same port. With a real model they also check tool calls and the thinking apart from the reply. CI runs them for both engines of the image, with a 1 MB model: `scripts/test-engine-contract.sh` (llama.cpp) and `scripts/test-engine-contract.sh ik` do the same on your machine. A test also makes sure that every option the server passes is in the `--help` of the engine that gets it. The top of the file has the command for another engine or model.
3. The evaluation (`assistant-hub eval llm`). It asks 12 questions several times and scores the tool calls and the answers. Three questions must call no tool, one must not change a list, and one is a known failure that does not count. `scripts/eval-llm.sh REPORT.json` starts the engine with the command line of `assistant-hub llm-command` and writes the report. `eval compare` lets engine B replace engine A only if B answers as well (within 5 points) and is faster (20 % less time per question, or 30 % less time for the first request).
4. The image test: start the image without a GPU and run `assistant-hub probe` inside it.

## Build

The Dockerfile has these stages, all on Debian 13 "trixie":

1. audio.cpp from source at a pinned commit. Debian ships `glslc`, so shaderc is not built from source.
2. parakeet.cpp and llama.cpp as prebuilt Vulkan releases. ik_llama.cpp from source at a pinned commit, as one binary for any CPU with AVX2 and FMA (`scripts/build-ik-llama.sh`).
3. The default STT and TTS models.
4. SearXNG at a pinned commit, with its packages in `/opt/searxng/lib` for Python 3.13.
5. `assistant-hub` with cargo-chef (pinned image tag). Its `build.rs` reads the llama.cpp headers of stage 2: the dependencies build in their own cached layer, so a code change rebuilds only the crate.
6. Runtime: `debian:trixie-slim` with Mesa Vulkan drivers (RADV, ANV), `libpython3.13` (for the embedded SearXNG) and `tini`. There is no Python interpreter binary, no pip and no compiler in the image.

`docker-bake.hcl` holds every pinned version. `docker buildx bake final` locally builds the same image as CI.

## CI

| Workflow | Runs on | Jobs |
|---|---|---|
| `ci.yml` | Every push to a branch, every pull request, manual, and from `bump-pins.yml` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, a release build (all `--locked`), `cargo deny check all` (RustSec advisories, licenses, bans, sources), and the contract tests of llama.cpp and ik_llama.cpp (`engine-contract`) |
| `image.yml` | Push to `main`, tags `v*`, manual | Calls `ci.yml` first. Builds with the registry cache and pushes to ghcr.io with provenance and SBOM attestations. |
| `bump-pins.yml` | Every Monday, manual | For the pinned native components (llama.cpp, ik_llama.cpp, audio.cpp, parakeet.cpp, SearXNG) with a newer upstream version, `scripts/bump-pins.sh` changes the pins. The workflow opens one pull request on the branch `bump/pins` for all outdated components together, because the image contains them together. It starts `ci.yml` on the branch and test-builds the image without a push. A person merges. |

The toolchain version is in `rust-toolchain.toml` and in `docker-bake.hcl` (`RUST_VERSION`). The actions are pinned to commit SHAs. Dependabot proposes updates for crates, actions and base images every week. The Rust toolchain and cargo-chef are bumped by hand.
