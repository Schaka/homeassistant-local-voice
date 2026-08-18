# homeassistant-local-voice

The cheapest possible **self-hosted voice assistant for Home Assistant**: local
**STT + TTS on one container, one port**, GPU-accelerated via Vulkan — tested
on a 5-10 € AMD **R7 250 (2 GB, GCN 1.0)** that most people would consider e-waste.

- **One image, one Wyoming server, one port (10300)** advertising **both** an
  ASR and a TTS program — Home Assistant registers both services from a single
  *Wyoming Protocol* integration.
- **STT**: [parakeet.cpp](https://github.com/mudler/parakeet.cpp) running
  NVIDIA Parakeet CTC 0.6B (q8_0), ggml + Vulkan. ≈ **12× realtime** on the R7 250.
- **TTS**: [audio.cpp](https://github.com/0xShug0/audio.cpp) running
  PocketTTS-100M (q8_0), ggml + Vulkan. **~1.6 s warm** per sentence.
- **Both models resident in VRAM**: ~1.68 GB / 2.00 GB used.
- **Fully self-contained**: ships its own Mesa Vulkan drivers (RADV — including
  ancient GCN1 like the R7 250 — plus llvmpipe for CPU-only hosts). **No ROCm,
  no CUDA, no host driver install, no host mounts.** You pass through one GPU
  device node and it just works.
- **The reasoning agent does not run here** (2 GB VRAM cannot do tool-calling
  reliably). Offload it to OpenRouter's **free** models (Gemma 3 27B,
  GPT-OSS-20B, …) via the official Home Assistant OpenRouter integration — free
  tiers cover a few voice commands per day easily. Or use the Local LLM
  (HomeLLM) add-on. Details below.

## Verified on hardware

| | |
|---|---|
| GPU | AMD **R7 250 / HD 8670 "Oland"**, GCN 1.0, 2 GiB VRAM, **no fp16** |
| Driver | Mesa **RADV 26.0.3** (shipped inside the image), Vulkan 1.3.335 |
| Host | Fedora 44, rootless podman — no host driver/mesa involvement |
| STT cold load | 0.68 s (model stays loaded) |
| STT 15.2 s clip | 1.27 s wall ≈ **12× realtime** |
| STT quality | perfect on clean speech; lowercase, no punctuation (Parakeet CTC) |
| TTS cold (incl. model load) | ~2 s |
| TTS warm round-trip | ~1.6 s for a sentence |
| Loopback (TTS → STT) | transcribed back **verbatim** |
| VRAM both-resident | 1.68 GB / 2.00 GB |

## Why this is the cheapest

- **Parakeet CTC beats Whisper on cost per accuracy** at small sizes, and
  ggml runs it on any GPU that speaks Vulkan — no tensor cores, no CUDA, no ROCm.
- A working R7 250 / HD 7000-series card goes for **5-15 € used**; 2 GB is
  enough for both models resident.
- **No Python at inference**: parakeet.cpp is a C-API (`libparakeet.so`),
  audio.cpp is a standalone server. The tiny `wyoming_voice.py` bridge is
  Python only.
- **No GPU compute budget spent on the LLM**: the brain is a free cloud model.

## Architecture

```
Home Assistant
 ├─ Wyoming Protocol integration → tcp://<host>:10300   (this container)
 │    └─ wyoming_voice.py
 │         ├─ STT: ctypes → libparakeet.so (Parakeet CTC 0.6B)   [Vulkan ctx 1]
 │         └─ TTS: REST → audiocpp_server (127.0.0.1:8100)       [Vulkan ctx 2]
 └─ Conversation agent → official OpenRouter integration (free models)
      └─ or Local LLM (HomeLLM) add-on
```

`wyoming_voice.py` serializes all GPU work with a cross-process file lock — the
two Vulkan contexts (one per process) never overlap, which is what keeps the
ancient GPU from tripping its ~2 s scheduler timeout. If it still does, a fault
detector (3 consecutive device-lost / HTTP 500) SIGTERMs PID 1 and the
container restarts itself fresh.

## Quickstart

### 1. Run the container

```bash
docker pull ghcr.io/schaka/homeassistant-local-voice:latest

# docker:
docker run -d --name wyoming-voice \
  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -p 10300:10300 --restart unless-stopped \
  ghcr.io/schaka/homeassistant-local-voice:latest

# podman (rootless, as verified on the R7 250 box):
podman run -d --name wyoming-voice \
  --device /dev/dri/renderD128 --group-add 105 \
  --security-opt seccomp:unconfined --security-opt label=disable --ipc host \
  -p 10300:10300 --restart unless-stopped \
  ghcr.io/schaka/homeassistant-local-voice:latest
```

That is the whole hardware story: one device node + the render group. The
image finds the GPU itself via Mesa RADV. **No GPU?** It still works on CPU
via llvmpipe (slow, but functional) — a useful fallback for testing.

### 2. Register in Home Assistant

**Settings → Devices & Services → Add Integration → Wyoming Protocol**, host =
your machine, port **10300**. One integration yields both:

- **Speech-to-Text** service: `parakeet` (model `parakeet-ctc-0.6b`, English)
- **Text-to-Speech** service: `audiocpp` (PocketTTS, voice `alba`)

### 3. Assemble a Voice Assistant

**Settings → Voice assistants → Add**, then point it at:

| slot | choice |
|---|---|
| Speech-to-text | the parakeet service (English) |
| Text-to-speech | the audiocpp/PocketTTS service |
| Wake word | your satellite's own, or `openwakeword` |
| **Conversation agent** | **OpenRouter** (see next) or HomeLLM |

## The conversation agent (the brain)

No LLM runs on this GPU. Two proven options:

### Official: OpenRouter integration (free models)

Home Assistant ships an official **OpenRouter** integration (since 2025.8).
Add it in Settings → Devices & services, paste an OpenRouter API key, pick a
model. OpenRouter's free (`:free`) routes — e.g. `google/gemma-3-27b-it:free`,
`openai/gpt-oss-20b:free` — are enough for **a few voice commands per day**
within the free tier. The current free list changes; see
[openrouter.ai/models](https://openrouter.ai/models) (filter "Free").

Set a budget/billing limit on your OpenRouter key even if you only use free
routes — models get delisted and prices change.

### Unofficial: Local LLM (HomeLLM) add-on

If you'd rather keep even the brain on your own hardware (or already have a
bigger GPU), the **Local LLM (HomeLLM)** add-on works out of the box as the
conversation agent for the same Voice Assistant. Note that with a 2 GB GPU you
cannot run a tool-calling LLM locally *and* keep STT+TTS resident — this is why
the default story offloads the agent.

A known-good system prompt (with backend setup notes and pitfalls) lives in
[docs/home-llm-prompt.md](docs/home-llm-prompt.md).

### Tested agent configurations

Real-world experience on this stack (Home-LLM → OpenRouter, Voice Assistant
pipeline, STT+TTS from this image):

| agent / model | result |
|---|---|
| Home-LLM + `deepseek/deepseek-v4-flash` | ✅ **works well** |
| Home-LLM + `gpt-5.6-luna` | ❌ unusable — rejects Home-LLM's tool-call payload (HTTP 400) |
| Home-LLM + free-tier routes | ⚠️ usable but rate-limited (429s) for more than a few commands/day |

Model choice matters more than the backend: Home-LLM sends `strict` tool
schemas, which some models (flagship "luna"-style ones) reject outright while
smaller/compatibility-first models handle fine. If a model 400s, swap the model
first — the backend is just a URL + key.

## Using bigger models

Everything is a build arg / env var — swap models without touching code. Two
mechanisms:

### 1. Bake at build time (default)

The Dockerfile downloads models in a dedicated stage. Override the ARGs via
`docker build` or bake:

```bash
# docker build
docker build --build-arg STT_MODEL_URL=https://huggingface.co/.../ctc-1.1b-q8_0.gguf \
             --build-arg STT_MODEL_FILENAME=parakeet-ctc-1.1b-q8_0.gguf \
             --build-arg STT_MODEL_NAME=parakeet-ctc-1.1b \
             -t wyoming-voice .

# or bake
docker buildx bake final --set '*.args.STT_MODEL_URL=https://huggingface.co/.../ctc-1.1b-q8_0.gguf'
```

### 2. Override at runtime (no rebuild)

Mount a directory over `/models` and set env vars — the entrypoint reads every
path from the environment:

```bash
# stage models on the host (or reuse scripts/download-models.sh)
./scripts/download-models.sh
mkdir -p /tmp/models/stt
curl -L -o /tmp/models/stt/parakeet-ctc-1.1b-q8_0.gguf \
  https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-1.1b-q8_0.gguf

docker run -d --name wyoming-voice \
  -v /tmp/models:/models \
  -e STT_MODEL_FILENAME=parakeet-ctc-1.1b-q8_0.gguf \
  -e STT_MODEL_NAME=parakeet-ctc-1.1b \
  -e TTS_VOICE_ID=anna \          # any voice embedding in .../embeddings/
  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -p 10300:10300 ghcr.io/schaka/homeassistant-local-voice:latest
```

### VRAM budget

Resident VRAM ≈ STT gguf + TTS gguf + voice embedding + ~0.5 GB ggml/runtime
overhead. Verified: 875 + 128 + 6 MB + overhead ≈ **1.68 GB / 2.00 GB** on the
R7 250.

| GPU VRAM | STT choice | TTS choice |
|---|---|---|
| 2 GB | `ctc-0.6b-q8_0` (875 MB) — **verified** | `pocket-tts-english-q8_0` (128 MB) — **verified** |
| 4 GB | `ctc-1.1b-q8_0` (1.53 GB) or `tdt-0.6b-v3-q8_0` (941 MB, streaming) | `pocket-tts-english-bf16` (219 MB) |
| 6 GB+ | `tdt-1.1b-q8_0` (1.55 GB, streaming) or `ctc-0.6b-f16` (1.37 GB) | `bf16` + any language |

**STT options** (from [mudler/parakeet-cpp-gguf](https://huggingface.co/mudler/parakeet-cpp-gguf)):

| family | what it is | q8 size |
|---|---|---|
| `ctc-0.6b` / `ctc-1.1b` | classic single-shot ASR (default: 0.6b) | 875 MB / 1.53 GB |
| `tdt-0.6b-v3` / `tdt-1.1b` | streaming (TDT) ASR, emits words as spoken | 941 MB / 1.55 GB |
| `nemotron-3.5-asr-streaming-0.6b` | streaming ASR | 984 MB |
| `realtime_eou_120m` | end-of-utterance detection for streaming pipelines | 176 MB |

All quantizations (`f16`/`q8_0`/`q6_k`/`q5_k`/`q4_k`) are available; drop a
notch in quantization to fit a bigger family in the same VRAM.

**TTS options** (from [audio-cpp/audio.cpp-gguf](https://huggingface.co/audio-cpp/audio.cpp-gguf)):
PocketTTS English `q8_0` (128 MB) or `bf16` (219 MB), plus ~30 voice embeddings
under `PocketTTS-GGUF/english/embeddings/` (set `TTS_VOICE_ID`) and other
languages (`german`, `italian`, `portuguese`, `spanish` — set `TTS_LANGUAGE`
and `TTS_LANGUAGES_BCP`).

## Build & release

`docker-bake.hcl` is the single source of truth for tags, cache refs
and model args; `.github/workflows/build.yml` only hands variables to bake and
`--push`es. `docker buildx bake final` locally reproduces CI exactly.

```bash
docker buildx bake final                    # build, default models
docker buildx bake --print final            # resolved graph, no build
docker buildx bake final --push             # publish (logged in)
```

CI publishes to `ghcr.io/schaka/homeassistant-local-voice`:
- push to `main` → `:latest` + `:<date>`
- push a tag `v*` → additionally a stable `:vX.Y.Z`
- manual dispatch → pick a release tag and/or override the model URLs

The audiocpp stage compiles glslc (shaderc) + audio.cpp from source
(~30-60 min cold); the registry cache (`mode=max`) keeps rebuilds cheap.

## Repository layout

```
Dockerfile            multi-stage, self-contained (audiocpp build, parakeet
                      release, model download, runtime with Mesa Vulkan)
docker-bake.hcl       tags / cache / model args (single source of truth)
.github/workflows/    build.yml: ghcr publish (main, v*, manual)
entrypoint.sh         PID-1 supervisor; renders server.json from env
wyoming_voice.py      Wyoming handler: STT (ctypes → libparakeet.so) + TTS
                      (REST → audiocpp_server), GPU fault detector + lock
model_specs/          audio.cpp model specs (pocket_tts)
scripts/              build.sh, run.sh, download-models.sh, test clients
docs/hardware-notes.md  everything learned the hard way (Oland/GCN1)
```

## Test without Home Assistant

```bash
# STT: sends a 16k mono wav, prints the transcript
python3 scripts/test/wyoming_stt_client.py clip16k.wav
# TTS: synthesizes a sentence to a wav
python3 scripts/test/wyoming_tts_client.py "Hello from the voice assistant." out.wav
```

Both talk to port 10300 — STT and TTS share the one Wyoming server.

## Troubleshooting

- **Container keeps restarting after a GPU fault**: that is the self-heal
  working — an amdgpu reset kills both Vulkan contexts and the container
  restarts with fresh ones. Check `docker logs wyoming-voice` for
  `GPU fault N/3`.
- **Slow but never crashes**: the cross-process GPU lock is serializing
  everything — expected on a 2 GB GCN1 card.
- **CPU fallback**: no `/dev/dri` present → llvmpipe (very slow). Fine for
  smoke tests.

## Attributions

- [parakeet.cpp](https://github.com/mudler/parakeet.cpp) — ggml Parakeet ASR
  (Apache-2.0); models from [NVIDIA NeMo](https://github.com/NVIDIA-NeMo/NeMo).
- [audio.cpp](https://github.com/0xShug0/audio.cpp) — ggml audio engine;
  PocketTTS by [Kyutai](https://github.com/Kyutai-Labs/pocket-tts) (Apache-2.0).
- [wyoming](https://github.com/rhasspy/pywyoming) protocol lib (MIT).
- Home Assistant **Wyoming Protocol** integration for the client side.
