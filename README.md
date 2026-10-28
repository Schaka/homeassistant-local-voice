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
- **By default the reasoning agent does not run here** (2 GB VRAM cannot do tool-calling
  reliably). Offload it to OpenRouter's **free** models (Gemma 3 27B,
  GPT-OSS-20B, …) via the official Home Assistant OpenRouter integration — free
  tiers cover a few voice commands per day easily. Or use the Local LLM
  (HomeLLM) add-on. Details below.
- **Optional local agent**: on a bigger GPU or an iGPU, the same image can
  also run the agent with llama.cpp (MiniCPM5 or Ling-3.0-tiny). See
  [Local conversation agent](#local-conversation-agent-optional).

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

Also tested on Intel **UHD730** (iGPU) with Parakeet q8 and q4: both quants run
pretty fast, q8 a bit slower than on the R7 250.

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
      ├─ or Local LLM (HomeLLM) add-on
      └─ or llama.cpp integration → tcp://<host>:8080/v1  (optional, this container)
           └─ llama-server (MiniCPM5 / Ling-3.0-tiny)         [Vulkan ctx 3]
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

## Local conversation agent (optional)

The image also contains llama-server from [llama.cpp](https://github.com/ggml-org/llama.cpp)
(prebuilt Vulkan release, pinned build). If you give it a model, it runs the
conversation agent in the same container. It serves an OpenAI-compatible API
on port 8080 (`/v1`). If you give it no model, it does not start.

The 2 GB R7 250 cannot hold an LLM next to STT and TTS. Use a GPU with 4 GB or
more, or an iGPU (it uses system RAM).

### 1. Choose a model

You have two ways to choose a model. Set only one of them.

| variable | example | what it does |
|---|---|---|
| `LLM_HF_REPO` | `openbmb/MiniCPM5-1B-GGUF:Q4_K_M` | llama-server downloads the model from Hugging Face on the first start. It keeps the download in `/models/llm/hf`. |
| `LLM_MODEL_FILENAME` | `MiniCPM5-1B-Q4_K_M.gguf` | Uses a file that is already in `/models/llm` (mounted or baked). |

Mount a host directory on `/models/llm`. The models then stay on the host
drive. If you do not mount it, the model is inside the container. If you
remove the container, the model is lost.

The image never deletes a model. When `LLM_HF_REPO` is already in the cache,
llama-server starts with `--offline` and does not contact Hugging Face. This
prevents an automatic re-download. If the upstream file changes and
llama-server is online, it deletes the cached copy and downloads the new file.
The cache uses the Hugging Face layout under
`/models/llm/hf`. To get a newer upstream version, delete that model folder
yourself. Several quants can stay in the cache, and `LLM_HF_REPO` selects one.

```bash
docker run -d --name wyoming-voice \
  -v /srv/voice-models/llm:/models/llm \
  -e LLM_HF_REPO=openbmb/MiniCPM5-1B-GGUF:Q4_K_M \
  -e LLM_FAMILY=minicpm5 \
  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -p 10300:10300 -p 8080:8080 --restart unless-stopped \
  ghcr.io/schaka/homeassistant-local-voice:latest
```

### 2. Connect Home Assistant

1. Go to Settings > Devices & services > Add Integration > llama.cpp (Home Assistant 2026.8 or later).
2. Set the URL to `http://<host>:8080/v1`. Leave the API key empty.
3. Select the model.
4. In Settings > Voice assistants, set the conversation agent to the new llama.cpp agent.

Other OpenAI-compatible clients use the same URL. They can send any API key,
or no key. If you set `LLM_API_KEY`, the server accepts only that key. The
Home-LLM add-on also works with its "llama.cpp server" backend.

### Supported models

Two model families are supported. `LLM_FAMILY` selects the sampling settings
that the model card recommends.

| `LLM_FAMILY` | model | architecture |
|---|---|---|
| `minicpm5` | MiniCPM5-1B, MiniCPM5-2B (dense) | `llama` |
| `ling3` | Ling-3.0-tiny (7.9B MoE, 1.3B parameters run per token) | `bailingmoe3` |

Both families are hybrid reasoning models. Thinking is on by default. The
thinking text goes to `reasoning_content`, so TTS does not speak it. Thinking
adds time before the spoken reply. To turn it off, set `LLM_REASONING=off`.

The smallest supported quant is Q3. Lower quants (Q2, IQ2, Q1) are not supported.

| `LLM_HF_REPO` | file | LLM memory at 64k context |
|---|---|---|
| `openbmb/MiniCPM5-1B-GGUF:Q4_K_M` | 0.69 GB | about 2.5 GB |
| `openbmb/MiniCPM5-1B-GGUF:Q8_0` | 1.15 GB | about 3.0 GB |
| `bartowski/MiniCPM5-2B-GGUF:Q3_K_M` | 1.24 GB | about 4.3 GB |
| `openbmb/MiniCPM5-2B-GGUF:Q4_K_M` | 1.56 GB | about 4.7 GB |
| `openbmb/MiniCPM5-2B-GGUF:Q8_0` | 2.68 GB | about 5.8 GB |
| `bartowski/Ling-3.0-tiny-GGUF:Q3_K_M` | 3.79 GB | about 4.4 GB |
| `bartowski/Ling-3.0-tiny-GGUF:IQ4_XS` | 4.39 GB | about 5.0 GB |
| `inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M` | 4.82 GB | 5.1 GiB (measured) |
| `inclusionAI/Ling-3.0-tiny-GGUF:Q6_K` | 6.50 GB | about 7.2 GB |
| `inclusionAI/Ling-3.0-tiny-GGUF:Q8_0` | 8.41 GB | about 9.1 GB |

Add about 1.7 GB for STT and TTS. The KV cache (the memory that holds the
context) differs a lot between the families. At 64k it is 1.5 GiB for
MiniCPM5-1B and 2.6 GiB for MiniCPM5-2B. Ling-3.0-tiny keeps a KV cache in
only 6 of its 24 layers, so it needs 0.4 GiB. If MiniCPM5 does not fit, set
`LLM_CTX=32k` to halve the KV cache.

No unsloth quants exist for these models. The bartowski repos give the
smaller quants. `./scripts/download-models.sh <file>.gguf` stages the official
files with a size test, for use with `LLM_MODEL_FILENAME`.

### Configuration

| variable | default | purpose |
|---|---|---|
| `LLM_FAMILY` | `minicpm5` | `minicpm5` or `ling3` |
| `LLM_MODEL_NAME` | from the file or repo | model name that Home Assistant shows |
| `LLM_DEVICE` | `Vulkan0` | `none` runs the LLM on the CPU |
| `LLM_GPU_LAYERS` | `auto` | layers on the GPU. `auto` fits the model to free VRAM |
| `LLM_CTX` | `64k` | context size: `32k`, `64k`, `128k` or `256k` (`256k` only for `ling3`, with YaRN rope scaling) |
| `LLM_REASONING` | `on` | thinking mode: `on` or `off` |
| `LLM_REASONING_BUDGET` | `512` | maximum thinking tokens per reply. `-1` removes the limit |
| `LLM_API_KEY` | empty | empty accepts any key |
| `LLM_EXTRA_ARGS` | empty | more llama-server arguments, for example `--n-cpu-moe 8` |
| `HF_TOKEN` | empty | token for gated Hugging Face repos |

### Tested

Intel Arrow Lake-S iGPU (4 Xe cores, Mesa ANV) and its 24-core CPU, Q4_K_M,
64k context, thinking on. The test prompt has 4.5k tokens, about the size of an
Assist prompt with 100 exposed entities. Run it with
`python3 scripts/test/llm_client.py "Turn on the kitchen light." --devices 100`.

| model | device | first request | prompt | generation |
|---|---|---|---|---|
| MiniCPM5-1B | iGPU | 16.6 s | 422 tok/s | 17 tok/s |
| MiniCPM5-1B | CPU (`LLM_DEVICE=none`) | 75 s (1568 thinking tokens) | 164 tok/s | 33 tok/s |
| Ling-3.0-tiny | iGPU | more than 120 s | 38 tok/s | 11 tok/s |
| Ling-3.0-tiny | CPU (`LLM_DEVICE=none`) | 49 s | 101 tok/s | 31 tok/s |

After the first request, llama-server reuses the cached prompt prefix. Home
Assistant puts the instructions and the entity list before the current time,
so this prefix stays the same. Later requests to Ling-3.0-tiny on the CPU took
0.6 to 1.4 s.

Results:

- Both families give correct tool calls through llama.cpp's own parsers.
- MiniCPM5-1B called `HassTurnOn` for the question "Is the bedroom light on?".
  Ling-3.0-tiny answered it correctly.
- On this iGPU, Ling-3.0-tiny runs fully on Vulkan (no CPU fallback), but
  slower than on the CPU. On an Intel iGPU, use `LLM_DEVICE=none` for Ling.

### Limits

- The GPU lock of `wyoming_voice.py` does not cover llama-server. In one voice
  pipeline, STT, the agent and TTS run one after the other. Two satellites at
  the same time can overlap on the GPU.
- Without `LLM_API_KEY`, any client on your network can use the server, and
  CORS allows all origins. If port 8080 is reachable from outside your LAN,
  set `LLM_API_KEY`.
- With thinking on, a small model can think for a long time. MiniCPM5-1B once
  used 1568 tokens. `LLM_REASONING_BUDGET` limits this to 512 tokens by default.
- MTP speculative decoding stays off. For Ling-3.0-tiny it hangs on RADV
  (llama.cpp PR #26608).

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

## Test without Home Assistant

```bash
# STT: sends a 16k mono wav, prints the transcript
python3 scripts/test/wyoming_stt_client.py clip16k.wav
# TTS: synthesizes a sentence to a wav
python3 scripts/test/wyoming_tts_client.py "Hello from the voice assistant." out.wav
```

Both talk to port 10300 — STT and TTS share the one Wyoming server.

## Attributions

- [parakeet.cpp](https://github.com/mudler/parakeet.cpp) — ggml Parakeet ASR
  (Apache-2.0); models from [NVIDIA NeMo](https://github.com/NVIDIA-NeMo/NeMo).
- [audio.cpp](https://github.com/0xShug0/audio.cpp) — ggml audio engine;
  PocketTTS by [Kyutai](https://github.com/Kyutai-Labs/pocket-tts) (Apache-2.0).
- [wyoming](https://github.com/rhasspy/pywyoming) protocol lib (MIT).
- Home Assistant **Wyoming Protocol** integration for the client side.
