# Old AMD GPUs (GCN 1.0, 2 GB)

These notes come from an AMD Radeon R7 250 (Oland XT, GCN 1.0, 2 GB), a GPU that costs 10 to 15 € used. With it, STT and TTS run on the GPU, and the CPU stays free for the LLM. [hardware.md](hardware.md) covers all other hardware.

## What works

| Item | Value |
|---|---|
| Driver | Mesa RADV from the image (`AMD Radeon R7 200 Series (RADV OLAND)`, Vulkan 1.3). Nothing is installed on the host. |
| Half precision | Not available (`shaderFloat16=false`). ggml uses 32-bit math instead, which works. |
| GPU memory | STT (Parakeet CTC 0.6B q8_0, about 0.9 GB) and TTS (PocketTTS q8_0, about 0.5 GB) use 1.68 of 2.00 GB. The LLM does not fit next to them and runs on the CPU. |
| Runtime options | `--device /dev/dri/renderD128 --group-add <render group>` |

## Measured

| Item | Value |
|---|---|
| STT model load | 0.68 s, then the model stays loaded |
| STT, 15.2 s of speech | 1.27 s (12 times faster than real time) |
| TTS, one sentence | About 1.6 s from request to the last audio chunk. The first request also loads the model (about 2 s). |
| TTS output | 24 kHz, 16-bit, mono |
| Round trip (TTS, then STT) | The text came back word for word |
| Load test | 4 STT and 4 TTS requests at the same time ran one after the other without a GPU fault |

## Why the server protects the GPU

1. The amdgpu driver resets a GPU when one job runs longer than 2 s. STT and TTS use two Vulkan contexts (STT in `assistant-hub`, TTS in `audiocpp_server`). Two jobs at the same time can exceed the limit. A lock in `assistant-hub` lets only one of them use the GPU at a time (`src/gpu.rs`). A request waits at most 20 s for the lock, then it fails instead of blocking Home Assistant.
2. A GPU reset kills both contexts for good: TTS answers with HTTP 500, STT with `VK_ERROR_DEVICE_LOST`. After three such faults in a row, `assistant-hub` exits with an error, and the restart policy of the container starts it with fresh contexts. Use `--restart unless-stopped`.
3. Do not share the GPU with another program, for example a transcoder or a desktop.

## Requirements of the image

Current Mesa needs glibc 2.41 or newer. The image is based on Debian 13 (glibc 2.41), which is the reason for the base image.
