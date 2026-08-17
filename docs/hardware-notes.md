# Wyoming STT+TTS on an R7 250 2GB (Oland) via Vulkan

Everything learned the hard way getting a 10-15 € e-waste GPU to run a
Home Assistant voice assistant. Now self-contained and released as one image:
one container, one Wyoming server on one port (10300), advertising both ASR
and TTS.

| service | port | engine | model | VRAM resident |
|---|---|---|---|---|
| STT | 10300 | parakeet.cpp (ggml, Vulkan) | Parakeet CTC 0.6B q8_0 (875 MB) | ~0.9 GB |
| TTS | 10300 | audio.cpp (ggml, Vulkan) | PocketTTS-100M English q8_0 (128 MB) | ~0.5 GB |

**Both models stay resident in VRAM: ~1.68 GB of 2.00 GB used (~330-420 MB headroom).**

## Verified on 192.168.1.214 (Fedora 44, kernel 7.1.7, rootless podman)

- GPU: `Oland XT` = R7 250 / HD 8670, GCN 1.0, **2 GiB VRAM**, no fp16
  (`shaderFloat16=false`) — ggml-vulkan tolerates it via fp32 emulation.
- Driver: **Mesa RADV 26.0.3-1ubuntu1, shipped inside the image**
  (`apt install mesa-vulkan-drivers` on ubuntu:26.04). Verified driving the
  Oland: `deviceName = AMD Radeon R7 200 Series (RADV OLAND)`, Vulkan 1.3.335.
  The same image provides llvmpipe (CPU) as GPU1 — a no-GPU fallback exists.
- The original host-driver bake (copy host RADV + Fedora sonames into the
  image) is **obsolete** — Ubuntu 26.04's own Mesa works. Simpler and portable.
- Runtime mounts: only `--device /dev/dri/renderD128 --group-add 105`.

## Verified numbers

| metric | value |
|---|---|
| STT cold load | 0.68 s (model stays loaded) |
| STT 15.2 s clip | 1.27 s wall ≈ **12× realtime** |
| STT quality | perfect on clean speech; lowercase, no punctuation (Parakeet CTC) |
| TTS cold (incl. model load) | ~2 s |
| TTS warm round-trip (client+synth+stream) | ~1.6 s for a sentence |
| TTS output | 24 kHz, 16-bit, mono PCM WAV |
| Loopback (TTS → STT) | transcribed back **verbatim** |
| Concurrency | 4× STT + 4× TTS fired simultaneously → all serialized, no GPU crash |

## Reliability engineering (why it survives the Oland)

- **GPU fault auto-restart**: an amdgpu reset kills both Vulkan contexts
  permanently (TTS → audiocpp HTTP 500, STT → `VK_ERROR_DEVICE_LOST`). A fault
  detector counts ≥3 consecutive GPU-signature failures and SIGTERMs PID 1; the
  entrypoint trap exits and the orchestrator's `--restart unless-stopped`
  brings the container back with fresh contexts.
- **Cross-process GPU lock**: STT (parakeet.cpp, own Vulkan context) and TTS
  (audiocpp_server, own Vulkan context) hold a shared file lock around every
  GPU call, so only **one kernel is in flight at a time** — the 2 s amdgpu
  scheduler timeout is never contested. 20 s bound on the lock so a stuck
  request errors instead of hanging HA.
- **Env-driven config**: the entrypoint renders `server.json` and every model
  path from env vars, so bigger models are a build arg or an env var away.

## Build / run

```bash
# The whole build is one command now (self-contained, downloads its own models):
./scripts/build.sh          # podman build -t wyoming-voice -f Dockerfile .
./scripts/run.sh            # auto-detects render device + group
```

Or with buildx/bake (what CI does): `docker buildx bake final`.

## Engine versions

- parakeet.cpp v0.5.0 (mudler) — prebuilt Vulkan binary + `libparakeet.so` C-API.
- audio.cpp 0.6.0 (0xShug0) — custom composite build (`pocket_tts` only), Vulkan
  backend; glslc (shaderc) built from source (Ubuntu ships no binary).
- Models: `mudler/parakeet-cpp-gguf` `ctc-0.6b-q8_0.gguf`;
  `audio-cpp/audio.cpp-gguf` `PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf`
  + `embeddings/alba.safetensors`. All overridable at build (ARGs) or runtime (env).

## Architecture / files

- `entrypoint.sh` — PID 1 supervisor; runs two children, auto-restarts each:
  - `audiocpp_server` (TTS model resident, REST on 127.0.0.1:8100)
  - `wyoming_voice.py` — the single Wyoming server (ASR + TTS on 10300)
- `wyoming_voice.py` — from-scratch Wyoming handler (official `wyoming` pip
  package): STT via ctypes → `libparakeet.so`, TTS via REST →
  `POST /v1/audio/speech`; models loaded once, process-global; owns the GPU
  fault detector + cross-process lock.
- `model_specs/` — audio.cpp model specs (needed by audiocpp_server for
  `pocket_tts`).
- `docker-bake.hcl` + `.github/workflows/build.yml` — CI publish to ghcr
  (mirrors the rocm-migraphx-ort-builder pipeline pattern).

## Gotchas learned (this hardware)

- Oland (GCN 1.0) has **no fp16 ALU** (`shaderFloat16=false`) — ggml-vulkan
  still works, falls back to fp32 emulation.
- Mesa needs **glibc ≥ 2.41** (`GLIBC_ABI_GNU2_TLS`) — Ubuntu 24.04 (2.39)
  cannot run current Mesa; **Ubuntu 26.04 (2.43)** is the runtime base.
- The original host-driver bake taught us: never `LD_LIBRARY_PATH`
  container-wide (breaks bash/python); ICD `library_path` must point inside the
  container; Fedora sonames don't exist in Ubuntu. All moot now that the image
  ships its own Mesa.
- HA's Wyoming TTS reader ends on an `audio-stop` event — the server must send
  `AudioStop` after the chunks; `synthesize-stop` (request-side) hangs it.
- This image owns the whole card — never share it with another GPU workload.