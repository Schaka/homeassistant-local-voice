# Configuration reference

You configure the container with environment variables. Each variable has a default, so the container starts without any of them. `entrypoint.sh` reads them at each start.

## Ports

| Port | Service | Starts |
|---|---|---|
| 10300 | Wyoming server: STT and TTS | Always |
| 8080 | llama-server, OpenAI-compatible API `/v1` | When `LLM_HF_REPO` or `LLM_MODEL_FILENAME` is set |
| 8090 | Web search MCP endpoint `/mcp` | When `SEARCH_ENABLED=on` |

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
| `PARAKEET_DEVICE` | `Vulkan0` | Vulkan device for STT |
| `STT_DEBUG_DIR` | empty | Folder for recordings of every STT request (WAV and transcript, newest 200). Use it to compare models on real requests, then turn it off. |

## Text to speech (TTS)

| Variable | Default | Purpose |
|---|---|---|
| `TTS_DIR` | `/models/pocket-tts` | Folder with the TTS model and `embeddings/` |
| `TTS_MODEL_FILENAME` | `pocket-tts-english-q8_0.gguf` | Model file in `TTS_DIR` |
| `TTS_MODEL_ID` | `pocket-tts` | Model ID inside audio.cpp |
| `TTS_LANGUAGE` | `english` | PocketTTS language: `english`, `german`, `italian`, `portuguese` or `spanish` |
| `TTS_LANGUAGES_BCP` | `en` | Language code that Home Assistant shows, for example `de` |
| `TTS_VOICE_ID` | `alba` | Voice embedding in `TTS_DIR/embeddings/` |
| `TTS_THREADS` | `2` | CPU threads of audio.cpp |
| `VK_DEVICE` | `0` | Vulkan device index for TTS |

## Conversation agent (LLM)

Set `LLM_HF_REPO` or `LLM_MODEL_FILENAME`, not both. With neither, llama-server does not start. [llm.md](llm.md) explains the models.

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HF_REPO` | empty | `<user>/<repo>[:QUANT]`. llama-server downloads the model into `/models/llm/hf` at the first start. |
| `LLM_MODEL_FILENAME` | empty | Model file that is already in `/models/llm` |
| `LLM_FAMILY` | `minicpm5` | `minicpm5` or `ling3`. Selects the sampling values. |
| `LLM_MODEL_NAME` | From the file or repo | Model name that Home Assistant shows |
| `LLM_DEVICE` | `Vulkan0` | Vulkan device. `none` runs the LLM on the CPU. |
| `LLM_GPU_LAYERS` | `auto` | Layers on the GPU. `auto` fits the model to the free memory. |
| `LLM_CTX` | `64k` | Context size: `32k`, `64k`, `128k` or `256k`. `256k` is only for `ling3` (YaRN rope scaling). |
| `LLM_REASONING` | `on` | Thinking mode: `on` or `off` |
| `LLM_REASONING_BUDGET` | `512` | Maximum thinking tokens per reply. `-1` removes the limit. |
| `LLM_API_KEY` | empty | Empty accepts any key. With a value, the server accepts only that key. |
| `LLM_EXTRA_ARGS` | empty | More llama-server arguments, for example `--n-cpu-moe 8`. They come last and override the defaults. |
| `LLM_SLOT_CACHE` | `on` | Keeps the prompt cache across restarts in `/models/llm/slots`. `off` disables it. |
| `LLM_TEMPLATE_FIX` | `on` | For `ling3`: use the patched chat template `templates/ling3.jinja`. After a tool result, the turn starts with a closed thinking block, so the answer never ends up as thinking. `off` uses the template from the GGUF. |
| `LLM_THINKING_GUARD` | `on` | With thinking on: if a reply has no text and no tool call (the answer is stuck in the thinking block), `llm_guard.py` asks once more without thinking. `off` disables it. |
| `HF_TOKEN` | empty | Token for gated Hugging Face repos |

## Web search

| Variable | Default | Purpose |
|---|---|---|
| `SEARCH_ENABLED` | `off` | `on` starts SearXNG and the MCP server |
| `SEARCH_MCP_PORT` | `8090` | Port of the MCP endpoint `/mcp` |
| `SEARCH_LANGUAGE` | `en` | Language of the results, for example `de`. Empty gives all languages. |

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
| `LLM_FAMILY` | `minicpm5` | Default of the variable `LLM_FAMILY` |
| `LLAMACPP_BUILD` | `b11966` | llama.cpp release that the image contains |
| `SEARXNG_COMMIT` | `ff8f5ddcc9f2c58d8b0276ec030f6a3adfa17525` | SearXNG commit that the image contains |

The full STT and TTS URLs start with `https://huggingface.co/`. The TTS URLs are in the repo `audio-cpp/audio.cpp-gguf`.

```bash
docker buildx bake final --set '*.args.STT_MODEL_URL=<url>' --set '*.args.STT_MODEL_FILENAME=<file>'
```
