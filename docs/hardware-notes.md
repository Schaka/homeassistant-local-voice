# Wyoming STT+TTS on an R7 250 2GB (Oland) via Vulkan

One image, one container, two Wyoming services for Home Assistant:

| service | port | engine | model | VRAM |
|---|---|---|---|---|
| STT | 10300 | parakeet.cpp (ggml, Vulkan) | Parakeet CTC 0.6B q8_0 (875 MB) | ~0.9 GB |
| TTS | 10301 | audio.cpp (ggml, Vulkan) | PocketTTS-100M English q8_0 (128 MB) | ~0.5 GB |

**Both models stay resident in VRAM: ~1.68 GB of 2.00 GB used, ~330 MB headroom.**

## Verified on 192.168.1.214 (Fedora 44, kernel 7.1.7, rootless podman)

- GPU: `Oland XT` = R7 250 / HD 8670, GCN 1.0, **2 GiB VRAM**, amdgpu + host RADV (mesa 26.1.6, Vulkan 1.3, no fp16 — ggml-vulkan tolerates it).
- Container: `localhost/wyoming-voice:latest` (1.9 GB), Ubuntu 26.04 (glibc 2.43 = host glibc, required by the baked host RADV driver).
- Runtime mounts: only `--device /dev/dri/renderD128 --group-add 105`. The host RADV driver + its Fedora-soname deps are baked into the image at `/opt/vk/` with a container-local ICD JSON (`VK_ICD_FILENAMES=/opt/vk/radeon_icd.container.json`).

### Deploy / run

```bash
podman run -d --name wyoming-voice \
  --device /dev/dri/renderD128 --group-add 105 \
  --security-opt seccomp:unconfined --security-opt label=disable --ipc host \
  -p 10300:10300 -p 10301:10301 \
  --restart unless-stopped \
  localhost/wyoming-voice:latest
```

On this box, replace `localhost/` with `ghcr.io/...` once pushed, or `podman save`/`load` the tarball.

### Home Assistant registration

Add the **Wyoming Protocol** integration twice (Settings → Devices & Services → Add Integration → Wyoming Protocol):

1. **STT**: host `192.168.1.214`, port **10300** → Speech-to-Text.
2. **TTS**: host `192.168.1.214`, port **10301** → Text-to-Speech (voice `alba`).

Then under Settings → Voice assistants, point the assistant at:
- Speech-to-text: the parakeet service (English)
- Text-to-speech: the audiocpp/PocketTTS service
- Wake word: your satellite's own / openwakeword
- Conversation agent: your Claude/OpenCode/OpenRouter service (not run here — 2 GB VRAM cannot do tool-calling reliably)

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
| VRAM both-resident | 1.68 GB / 2.00 GB (330 MB headroom) |

## Build (all sources in this directory + on box at /data/llamacpp-gfx803/voice/)

```bash
# 1. audio.cpp builder: Ubuntu 24.04 + glslc built from shaderc source (ggml-vulkan needs glslc; Ubuntu has none)
podman build -t audiocpp-vk-build -f Dockerfile.build-audiocpp .

# 2. Final image (uses audiocpp-vk-build + prebuilt parakeet binaries + baked vk-libs + models)
podman build -t wyoming-voice -f Dockerfile .
```

Engine versions:
- parakeet.cpp v0.5.0 (mudler) — prebuilt Vulkan binary + libparakeet.so C-API.
- audio.cpp 0.6.0 (0xShug0) — custom composite build (`pocket_tts` only), Vulkan backend.
- Models: `mudler/parakeet-cpp-gguf` `ctc-0.6b-q8_0.gguf`; `audio-cpp/audio.cpp-gguf` `PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf` + `embeddings/alba.safetensors`.

## Architecture / files

- `entrypoint.sh` — PID 1 supervisor; runs three children, auto-restarts each:
  - `audiocpp_server --config /app/server.json` (TTS model resident, REST on 127.0.0.1:8100)
  - `wyoming_stt.py` (ctypes → `libparakeet.so`, resident model, offline full-utterance transcribe)
  - `wyoming_tts.py` (Wyoming → `POST /v1/audio/speech` on audiocpp_server, streams WAV back)
- `wyoming_stt.py` / `wyoming_tts.py` — from-scratch Wyoming protocol handlers (official `wyoming` pip package), model loaded once, process-global.
- `server.json` — audiocpp_server config (backend vulkan, device 0, model pocket-tts, voice preset `alba`).
- `vk-libs/` — host RADV 26.1.6 + Fedora-soname deps (libLLVM.so.22.1, libSPIRV-Tools, libedit.so.0, libxml2.so.2, libdisplay-info.so.3) + `radeon_icd.container.json`. **Host-specific**: rebuilding on a different host requires re-copying these from that host (`/usr/lib64`).
- `model_specs/` — audio.cpp model specs (needed by audiocpp_server for pocket_tts).
- `models/` — staged model files, baked into the image.

## Gotchas learned (this hardware)

- Oland (GCN 1.0) has **no fp16 ALU** (`shaderFloat16=false`) — ggml-vulkan still works, falls back to fp32 emulation.
- Host mesa 26.1.6 needs **glibc ≥ 2.41** (`GLIBC_ABI_GNU2_TLS`) — Ubuntu 24.04 (2.39) cannot load the host RADV driver; Ubuntu 26.04 (2.43) can.
- Fedora-built driver has Fedora sonames (`libedit.so.0`, `libxml2.so.2`, `libLLVM.so.22.1`, `libSPIRV-Tools.so`, `libdisplay-info.so.3`) that do not exist in Ubuntu — bake host copies; provide the rest from Ubuntu packages.
- Never set `LD_LIBRARY_PATH` container-wide — it breaks bash/python (host libc/libstdc++ poisoning). The baked `/opt/vk` + `ldconfig` + custom ICD JSON avoids it entirely.
- ICD JSON's `library_path` must point inside the container (`/opt/vk/libvulkan_radeon.so`), not the host path.
- `pkill -f <scriptname>` kills the very ssh session running it — use the `[s]cript` bracket trick.
- Both llama.cpp-style servers must never share this GPU at the same time (2 GB); this image owns the whole card.