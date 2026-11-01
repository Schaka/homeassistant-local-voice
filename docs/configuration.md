# Configuration reference

You configure the container with environment variables. Each variable has a default, so the container starts without any of them. `assistant-hub` reads them at each start (`src/config.rs`).

## Ports

The server has one port for everything. [architecture.md](architecture.md) lists the routes.

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `10300` | The only port: Wyoming (STT, TTS) and HTTP (`/llm/v1`, `/mcp`, `/web/search`, `/youtube/search`, `/health`) |
| `BIND_ADDRESS` | `0.0.0.0` | Address to listen on |
| `RUST_LOG` | `info` | Log level, for example `debug` or `info,assistant_hub=debug` |

Home Assistant connects to `http://<host>:10300/llm/v1` and `http://<host>:10300/mcp`.

## Volumes

| Container path | Content |
|---|---|
| `/models/stt` | STT model files |
| `/models/pocket-tts` | TTS model and voice embeddings (`TTS_DIR`) |
| `/models/llm` | LLM files, the Hugging Face cache (`hf/`) and the saved prompt cache (`slots/`). Mount it writable. |

The image contains the default STT and TTS models. Mount a folder only to use other models or to keep LLM downloads.

## Speech to text (STT)

| Variable | Default | Purpose |
|---|---|---|
| `STT_MODEL_FILENAME` | `parakeet-ctc-0.6b-q8_0.gguf` | Model file in `/models/stt` |
| `STT_MODEL_NAME` | `parakeet-ctc-0.6b` | Model name that Home Assistant shows |
| `PARAKEET_DEVICE` | empty | Device for STT: `Vulkan0`, `Vulkan1` or `cpu`. Empty selects the first Vulkan GPU, or the CPU if there is none. |
| `STT_DEBUG_DIR` | empty | Folder for recordings of every STT request (WAV and transcript, newest 200). Use it to compare models on real requests, then turn it off. |

## Text to speech (TTS)

| Variable | Default | Purpose |
|---|---|---|
| `TTS_DIR` | `/models/pocket-tts` | Folder with the TTS model and `embeddings/` |
| `TTS_MODEL_FILENAME` | `pocket-tts-english-q8_0.gguf` | Model file in `TTS_DIR` |
| `TTS_MODEL_ID` | `pocket-tts` | Model ID inside audio.cpp |
| `TTS_LANGUAGE` | `english` | PocketTTS language: `english`, `french`, `german`, `italian`, `portuguese` or `spanish` |
| `TTS_LANGUAGES_BCP` | `en` | Language code that Home Assistant shows, for example `de` |
| `TTS_VOICE_ID` | `alba` | Voice embedding in `TTS_DIR/embeddings/` |
| `TTS_BACKEND` | `auto` | `vulkan`, `cpu` or `auto`. `auto` uses Vulkan if the server finds a Vulkan GPU at start, else the CPU. |
| `TTS_THREADS` | empty | CPU threads of audio.cpp. Empty: 2 on a GPU, the number of CPUs (at most 8) on the CPU. |
| `VK_DEVICE` | `0` | Vulkan device index for TTS |

## Conversation agent (LLM)

Set `LLM_HF_REPO` or `LLM_MODEL_FILENAME`, not both. With neither, llama-server does not start. [llm.md](llm.md) explains the models.

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HF_REPO` | empty | `<user>/<repo>[:QUANT]`. llama-server downloads the model into `/models/llm/hf` at the first start. |
| `LLM_MODEL_FILENAME` | empty | Model file that is already in `/models/llm` |
| `LLM_FAMILY` | empty | `ling3`, `minicpm5`, `lfm3`, `qwen35` or `granite4`. Selects the sampling values and, for `ling3`, the patched chat template. Empty detects the family from the model name. |
| `LLM_MODEL_NAME` | From the file or repo | Model name that Home Assistant shows |
| `LLM_DEVICE` | `none` | `none` runs the LLM on the CPU, which was faster than an Intel iGPU in the tests. `Vulkan0` uses the first GPU. |
| `LLM_GPU_LAYERS` | `auto` | Layers on the GPU. `auto` fits the model to the free memory. |
| `LLM_CTX` | `64k` | Context size: `32k`, `64k`, `128k` or `256k`. `256k` is only for `ling3` (YaRN rope scaling) and `lfm3`. |
| `LLM_REASONING` | `on` | Thinking mode: `on` or `off` |
| `LLM_REASONING_BUDGET` | Per family: `128` for `lfm3`, `256` for `minicpm5`, `512` for the others | Maximum thinking tokens per reply. `-1` removes the limit. The defaults are measured, see [llm.md](llm.md#speed-settings-per-family). |
| `LLM_API_KEY` | empty | Empty accepts any key. With a value, `/llm/...` accepts only `Authorization: Bearer <key>`. llama-server itself listens on 127.0.0.1 without a key. |
| `LLM_EXTRA_ARGS` | empty | More llama-server arguments, for example `--n-cpu-moe 8`. They come last and override the defaults, also the family defaults (sampling, thinking budget). On a GPU (`LLM_DEVICE=Vulkan0`), set `-fa off` (flash attention off). This is a strong recommendation: with flash attention on, an Intel iGPU prefilled Ling-3.0-tiny 3.7 times slower (79 against 294 tokens/s). For Granite and MiniCPM5, it made no difference. See [igpu-backends.md](igpu-backends.md). |
| `LLM_SLOT_CACHE` | `on` | Keeps the prompt cache across restarts in `/models/llm/slots`. `off` disables it. |
| `LLM_TEMPLATE_FIX` | `on` | For `ling3`, `lfm3` and `minicpm5`: patch the chat template from the model file so that no new thinking starts after a tool result. Ling otherwise often returns empty answers, and LFM3 and MiniCPM5 need about twice as long. `off` uses the template unchanged. See [llm.md](llm.md#the-tool-result-fix-of-the-chat-templates). |
| `HF_TOKEN` | empty | Token for gated Hugging Face repos |

## Web search

| Variable | Default | Purpose |
|---|---|---|
| `SEARCH_ENABLED` | `off` | `on` loads SearXNG (in process) and offers the MCP tool `web_search` |
| `SEARCH_LANGUAGE` | `en` | Language of the results, for example `de`. Empty gives all languages. |

## YouTube search

| Variable | Default | Purpose |
|---|---|---|
| `YOUTUBE_ENABLED` | `on` | `off` turns `/youtube/search` off |
| `YOUTUBE_LANGUAGE` | `en` | Language of titles (`hl`) |
| `YOUTUBE_REGION` | `DE` | Region of the results (`gl`) |
| `YOUTUBE_CLIENT_VERSION` | `2.20261030.00.00` | Web client version that the server reports to YouTube |

## Jellyfin search

Both `JELLYFIN_URL` and `JELLYFIN_API_KEY` turn on `/jellyfin/search`. [search.md](search.md#jellyfin-search) explains it.

| Variable | Default | Purpose |
|---|---|---|
| `JELLYFIN_URL` | empty | Address of the Jellyfin server, for example `http://jellyfin.local:8096` |
| `JELLYFIN_API_KEY` | empty | API key from Jellyfin (Dashboard > API Keys) |
| `JELLYFIN_USER` | empty, required with the two above | The default user for the next episode and the resume position. A user with an open session on the TV box (`&devices=`) or `&user=` in the request wins ([search.md](search.md#jellyfin-search)). |

## Media index

`MEDIA_INDEX=on` adds semantic and typo-tolerant search to `/jellyfin/search`. [media-index.md](media-index.md) explains it and lists all its variables: `MEDIA_INDEX_DIR`, `MEDIA_INDEX_EPISODES`, `EMBED_MODEL`, `EMBED_MODEL_URL`, `EMBED_DIMS`, `EMBED_THREADS`, `MEDIA_MIN_SCORE`, `MEDIA_MIN_MARGIN` and `LLAMA_LIB`.

## Build arguments

The Dockerfile bakes models and pins versions with these arguments. `docker-bake.hcl` and the CI workflow pass them on.

| Argument | Default | Purpose |
|---|---|---|
| `STT_MODEL_URL` | `.../mudler/parakeet-cpp-gguf/resolve/main/ctc-0.6b-q8_0.gguf` | STT model to download |
| `STT_MODEL_FILENAME` | `parakeet-ctc-0.6b-q8_0.gguf` | File name of the baked STT model |
| `STT_MODEL_NAME` | `parakeet-ctc-0.6b` | Default of the variable `STT_MODEL_NAME` |
| `TTS_MODEL_URL` | `.../PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf` | TTS model to download |
| `TTS_MODEL_FILENAME` | `pocket-tts-english-q8_0.gguf` | File name of the baked TTS model |
| `TTS_VOICE_URL` | `.../PocketTTS-GGUF/english/embeddings/alba.safetensors` | Voice embedding to download |
| `TTS_VOICE_FILENAME` | `alba.safetensors` | File name of the baked voice |
| `TTS_LANGUAGE`, `TTS_MODEL_ID`, `TTS_VOICE_ID` | `english`, `pocket-tts`, `alba` | Defaults of the matching variables |
| `LLM_MODEL_URL` | empty | LLM file to bake into the image. Empty bakes none. |
| `LLM_MODEL_FILENAME` | empty | File name of the baked LLM |
| `LLM_FAMILY` | empty | Default of the variable `LLM_FAMILY` |
| `LLAMACPP_BUILD` | `b11966` | llama.cpp release that the image contains |
| `SEARXNG_COMMIT` | `ff8f5ddcc9f2c58d8b0276ec030f6a3adfa17525` | SearXNG commit that the image contains |
| `AUDIOCPP_COMMIT` | `bd02977fc1fd7a03087f271b2d62cd2ea1bfc0aa` | audio.cpp commit that the image builds |
| `PARAKEET_VERSION` | `v0.5.0` | parakeet.cpp release that the image contains |
| `RUST_VERSION` | `1.99.1` | Rust toolchain of the build. Keep it the same as in `rust-toolchain.toml`. |

The full STT and TTS URLs start with `https://huggingface.co/`. The TTS URLs are in the repo `audio-cpp/audio.cpp-gguf`.

```bash
docker buildx bake final --set '*.args.STT_MODEL_URL=<url>' --set '*.args.STT_MODEL_FILENAME=<file>'
```
