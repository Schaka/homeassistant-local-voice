# One container, two Wyoming services for Home Assistant: STT + TTS on a
# cheap AMD GPU (R7 250, 2 GB, Oland/GCN1) via Vulkan. No Python at inference,
# both models resident in VRAM.

- **STT** (port **10300**): [parakeet.cpp](https://github.com/mudler/parakeet.cpp) — NVIDIA Parakeet CTC 0.6B (q8_0), ggml + Vulkan
- **TTS** (port **10301**): [audio.cpp](https://github.com/0xShug0/audio.cpp) — PocketTTS-100M (q8_0), ggml + Vulkan

Verified on an R7 250 (Oland, GCN 1.0, no fp16): STT ≈ **12× realtime**, TTS ≈ **1.6 s warm per sentence**, **1.68 GB / 2.00 GB VRAM with both models resident**.

## Why it exists / how it differs from the obvious options

- Whisper.cpp/parakeet.cpp for STT: Parakeet CTC is far cheaper than Whisper for the same accuracy, runs full-speed on a 5-10€ GPU.
- Piper for TTS: Piper is fine, but PocketTTS through audio.cpp is a real neural TTS (voice cloning capable) with GPU acceleration and no Python runtime.
- No LLM runs here: 2 GB VRAM cannot do tool-calling reliably. A separate conversation agent (Claude/OpenCode/OpenRouter subscription) does the reasoning; this image only does voice.

## Layout

```
Dockerfile                 final image (Ubuntu 26.04, Vulkan backend, models baked in)
Dockerfile.build-audiocpp  builder: audio.cpp + glslc (shaderc) built from source
entrypoint.sh              PID-1 supervisor: audiocpp_server + wyoming_stt + wyoming_tts
server.json                audiocpp_server config (vulkan, device 0, pocket-tts, voice 'alba')
wyoming_stt.py             Wyoming STT handler (ctypes -> libparakeet.so, model resident)
wyoming_tts.py             Wyoming TTS handler (Wyoming -> audiocpp_server REST, streams WAV)
radeon_icd.container.json  container-local ICD manifest (points at /opt/vk/...)
model_specs/pocket_tts.json  audio.cpp model spec (required by audiocpp_server)
vk-libs/                   host RADV driver + Fedora-soname deps (created by collect script, gitignored)
models/                    GGUF model files (created by download script, gitignored)
scripts/                   collect-vk-libs.sh, download-models.sh, build.sh, run.sh, test clients
docs/hardware-notes.md     everything learned the hard way
```

## Build (on the GPU host)

```bash
./scripts/collect-vk-libs.sh   # once: copies host RADV + distro-specific deps into vk-libs/
./scripts/download-models.sh   # once: fetches the two GGUF models + voice embedding
./scripts/build.sh             # builds audiocpp-vk-build, then wyoming-voice
./scripts/run.sh               # starts the container
```

The image needs only `--device /dev/dri/renderD128` and the render group — the
driver is baked in (host RADV + its dependencies), no host mounts, no driver
install. `vk-libs/` is host-specific; re-run `collect-vk-libs.sh` when moving
to a different machine.

## Home Assistant

Add the **Wyoming Protocol** integration twice:
1. host `192.168.1.214`, port **10300** → Speech-to-Text (English)
2. host `192.168.1.214`, port **10301** → Text-to-Speech (voice `alba`)

Then point a Voice Assistant at both, plus a wake word and a conversation agent.

## Test (without Home Assistant)

```bash
# STT: sends a 16k mono wav, prints the transcript
python3 scripts/test/wyoming_stt_client.py clip16k.wav

# TTS: synthesizes a sentence, saves the wav
python3 scripts/test/wyoming_tts_client.py "Hello from the voice assistant." out.wav
```

## Model sizes / VRAM budget

| model | file | size |
|---|---|---|
| Parakeet CTC 0.6B (STT) | `ctc-0.6b-q8_0.gguf` | 875 MB |
| PocketTTS-100M (TTS) | `pocket-tts-english-q8_0.gguf` | 128 MB |
| voice embedding | `embeddings/alba.safetensors` | 6 MB |

Idle GPU: ~1.68 GB of 2.00 GB used, ~330 MB headroom — both models resident.