# homeassistant-local-voice

A self-hosted voice assistant backend for Home Assistant in one container. It runs speech to text and text to speech on one Wyoming port, and can also run the conversation agent and a web search tool. It uses Vulkan on almost any GPU. Without a GPU, it uses the CPU.

## Features

| Feature | Engine | Default |
|---|---|---|
| Speech to text (STT) | [parakeet.cpp](https://github.com/mudler/parakeet.cpp) with NVIDIA Parakeet CTC 0.6B | On |
| Text to speech (TTS) | [audio.cpp](https://github.com/0xShug0/audio.cpp) with PocketTTS-100M | On |
| Conversation agent (LLM) | [llama.cpp](https://github.com/ggml-org/llama.cpp) llama-server with Ling-3.0-tiny or Granite 4.0 | Off. A model setting starts it. |
| Web search tool for the agent | [SearXNG](https://github.com/searxng/searxng) and a small MCP server | Off |

The image contains its own Mesa Vulkan drivers (RADV for AMD, ANV for Intel, llvmpipe for the CPU). You do not install drivers, ROCm or CUDA on the host.

## How it fits together

```
Home Assistant
 ├─ Wyoming Protocol integration ──▶ :10300  STT + TTS          (always)
 ├─ llama.cpp integration ─────────▶ :8080   llama-server /v1   (optional)
 └─ Model Context Protocol ────────▶ :8090   web search /mcp    (optional)
```

## Quick start

1. Start the container. Pass through the GPU render node:

   ```bash
   docker run -d --name wyoming-voice \
     --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
     -p 10300:10300 --restart unless-stopped \
     ghcr.io/schaka/homeassistant-local-voice:latest
   ```

2. In Home Assistant, go to Settings > Devices & services > Add Integration > Wyoming Protocol. Enter the host of the container and the port `10300`. You get two services: `parakeet` (STT) and `audiocpp` (TTS).
3. Go to Settings > Voice assistants > Add assistant. Select `parakeet` for speech to text and `audiocpp` for text to speech.
4. Select a conversation agent. You can use a cloud model or the local LLM of this image, see [docs/llm.md](docs/llm.md).

If the host has no GPU, the container uses the CPU. Speech is then slower but works.

## Memory targets

| What runs | Memory |
|---|---|
| STT and TTS with the default models | About 1.7 GB of GPU memory (or RAM on an iGPU), so a 2 GB GPU is enough |
| Plus Ling-3.0-tiny Q4_K_M at 64k context, on the CPU | About 5 GB of RAM |
| Plus Granite 4.0 H Tiny Q4_K_M at 64k context, on the CPU | About 4.5 GB of RAM (estimate: the file is 4.23 GB) |

[docs/llm.md](docs/llm.md) has the full table for each model and quant.

## Documentation

| Document | Content |
|---|---|
| [docs/voice.md](docs/voice.md) | STT and TTS: models, other languages and voices, GPU notes, podman, test clients |
| [docs/llm.md](docs/llm.md) | The conversation agent: cloud models, the local LLM, supported models, Vulkan or CPU, prompt cache |
| [docs/search.md](docs/search.md) | The web search tool for the agent |
| [docs/configuration.md](docs/configuration.md) | All environment variables, build arguments and ports |
| [docs/home-llm-prompt.md](docs/home-llm-prompt.md) | A tested system prompt for the Local LLM (HomeLLM) add-on |
| [docs/hardware-notes.md](docs/hardware-notes.md) | Notes for old AMD GPUs (GCN 1.0, 2 GB) |

## Attributions

- [parakeet.cpp](https://github.com/mudler/parakeet.cpp): ggml Parakeet ASR (Apache-2.0). Models from [NVIDIA NeMo](https://github.com/NVIDIA-NeMo/NeMo).
- [audio.cpp](https://github.com/0xShug0/audio.cpp): ggml audio engine. PocketTTS by [Kyutai](https://github.com/Kyutai-Labs/pocket-tts) (Apache-2.0).
- [llama.cpp](https://github.com/ggml-org/llama.cpp): llama-server (MIT).
- [SearXNG](https://github.com/searxng/searxng): metasearch engine (AGPL-3.0).
- [wyoming](https://github.com/rhasspy/pywyoming): protocol library (MIT).
