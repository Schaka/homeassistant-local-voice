# homeassistant-local-voice

A self-hosted voice assistant backend for Home Assistant in one container and on one port. One Rust server (`assistant-hub`) answers all requests. It serves Wyoming STT and TTS, the conversation agent under `/llm`, MCP tools on `/mcp`, and web and YouTube searches. It uses Vulkan on almost any GPU. Without a GPU, it runs on the CPU, also on 10-year-old office PCs ([docs/hardware.md](docs/hardware.md)).

## Features

| Feature | Engine | Default |
|---|---|---|
| Speech to text (STT) | [parakeet.cpp](https://github.com/mudler/parakeet.cpp) with NVIDIA Parakeet CTC 0.6B | On |
| Text to speech (TTS) | [audio.cpp](https://github.com/0xShug0/audio.cpp) with PocketTTS-100M | On |
| Conversation agent (LLM) | [llama.cpp](https://github.com/ggml-org/llama.cpp) llama-server, or [ik_llama.cpp](https://github.com/ikawrakow/ik_llama.cpp) on the CPU for the models it is verified for, with Ling-3.0-tiny, MiniCPM5-2B, LFM3-1.7B and others | Off. A model setting starts it. |
| Web search tool for the agent | [SearXNG](https://github.com/searxng/searxng), embedded as a library (no SearXNG server) | Off |
| YouTube search (`/youtube/search`) | YouTube's own web API (InnerTube), no key | On |
| Jellyfin search (`/jellyfin/search`) | Your Jellyfin server: the next episode of a series or a movie | Off. `JELLYFIN_URL` and `JELLYFIN_API_KEY` turn it on. |
| Media index for Jellyfin search | [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2) in process: descriptions, typos, misheard titles, single episodes | Off. `MEDIA_INDEX=on` turns it on. |

The image contains its own Mesa Vulkan drivers (RADV for AMD, ANV for Intel). You do not install drivers, ROCm or CUDA on the host. Without a GPU, STT and TTS run on the CPU backend of ggml.

## How it fits together

```
Home Assistant                              assistant-hub, port 10300
 ├─ Wyoming Protocol integration ──▶  Wyoming      STT (parakeet.cpp, in process)
 │                                                 TTS (audiocpp_server, child process)
 ├─ llama.cpp integration ─────────▶  /llm/v1      llama-server (child process, optional)
 ├─ Model Context Protocol ────────▶  /mcp         tools: web_search (optional)
 └─ REST commands, scripts ────────▶  /web/search, /youtube/search, /jellyfin/search, /health
```

The server sorts each connection by its first byte: a Wyoming client starts with a JSON header (`{`), an HTTP client with a method name. The native engines listen on 127.0.0.1 only. [docs/architecture.md](docs/architecture.md) explains the parts.

## Quick start

1. Make sure that the CPU has AVX2 (`grep -c avx2 /proc/cpuinfo` is not 0). Start the container. If the host has a GPU, pass through its render node:

   ```bash
   docker run -d --name assistant-hub \
     --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
     -p 10300:10300 --restart unless-stopped \
     ghcr.io/schaka/homeassistant-local-voice:latest
   ```

2. In Home Assistant, go to Settings > Devices & services > Add Integration > Wyoming Protocol. Enter the host of the container and the port `10300`. You get two services: `parakeet` (STT) and `audiocpp` (TTS).
3. Go to Settings > Voice assistants > Add assistant. Select `parakeet` for speech to text and `audiocpp` for text to speech.
4. Select a conversation agent. You can use a cloud model or the local LLM of this image, see [docs/llm.md](docs/llm.md).

Without a GPU, leave out the `--device` and `--group-add` lines. Speech then runs on the CPU: about 0.2 s for STT and 0.7 s for TTS per sentence on 4 modern cores.

[docs/home-assistant.md](docs/home-assistant.md) continues with the agent, tools, sentence triggers and automations. [examples/](examples/) has tested Home Assistant files. AI agents start with [AGENTS.md](AGENTS.md).

## Memory targets

| What runs | Memory |
|---|---|
| STT and TTS with the default models | About 1.7 GB of GPU memory (or RAM on an iGPU), so a 2 GB GPU is enough. On the CPU: about 1.4 GB of RAM. |
| Plus Ling-3.0-tiny Q4_K_M at 64k context, on the CPU | About 5 GB of RAM |
| Plus Granite 4.0 H Tiny Q4_K_M at 64k context, on the CPU | About 4.5 GB of RAM (estimate: the file is 4.23 GB) |

[docs/llm.md](docs/llm.md) has the full table for each model and quant. [docs/hardware.md](docs/hardware.md) recommends models for each kind of hardware, from a 4th-generation Core i5 to a GPU server.

## Documentation

| Document | Content |
|---|---|
| [docs/hardware.md](docs/hardware.md) | Minimum requirements, measured speeds, models for old office PCs, mini PCs and GPUs |
| [docs/home-assistant.md](docs/home-assistant.md) | Home Assistant setup step by step: speech, agent, tools, sentence triggers, YouTube, ideas for automations |
| [examples/](examples/) | Tested Home Assistant files: prompt, tool scripts, automations |
| [AGENTS.md](AGENTS.md) | Instructions for AI agents: work on the code, or set up a home through the Home Assistant API |
| [docs/android-tv-control.md](docs/android-tv-control.md) | Control an Android TV box without root: ADB, Android TV Remote, start a named app, wake and standby |
| [docs/media-index.md](docs/media-index.md) | The media index: search stages, the gate, sync with Jellyfin, resources, evaluation |
| [docs/architecture.md](docs/architecture.md) | The server, its routes, the child processes, the build and CI |
| [docs/voice.md](docs/voice.md) | STT and TTS: models, other languages and voices, GPU notes, test clients |
| [docs/llm.md](docs/llm.md) | The conversation agent: cloud models, the local LLM, supported models, Vulkan or CPU, prompt cache |
| [docs/model-comparison.md](docs/model-comparison.md) | Which small models work as a voice agent on a CPU, and which do not (9 models measured) |
| [docs/igpu-backends.md](docs/igpu-backends.md) | Intel iGPU: Vulkan, SYCL and OpenVINO compared, flash attention, power on the UHD 730 |
| [docs/search.md](docs/search.md) | Web search (MCP tool and `/web/search`) and YouTube search |
| [docs/configuration.md](docs/configuration.md) | All environment variables, build arguments and ports |
| [docs/home-llm-prompt.md](docs/home-llm-prompt.md) | A tested system prompt for the Local LLM (HomeLLM) add-on |
| [docs/hardware-notes.md](docs/hardware-notes.md) | Old AMD GPUs (GCN 1.0, 2 GB): measured speed and why the server protects the GPU |

## Attributions

- [parakeet.cpp](https://github.com/mudler/parakeet.cpp): ggml Parakeet ASR (Apache-2.0). Models from [NVIDIA NeMo](https://github.com/NVIDIA-NeMo/NeMo).
- [audio.cpp](https://github.com/0xShug0/audio.cpp): ggml audio engine. PocketTTS by [Kyutai](https://github.com/Kyutai-Labs/pocket-tts) (Apache-2.0).
- [llama.cpp](https://github.com/ggml-org/llama.cpp): llama-server (MIT).
- [ik_llama.cpp](https://github.com/ikawrakow/ik_llama.cpp): llama-server with faster CPU code (MIT).
- [SearXNG](https://github.com/searxng/searxng): metasearch engine (AGPL-3.0).
- [Wyoming protocol](https://github.com/rhasspy/wyoming) (MIT): implemented in Rust here.
