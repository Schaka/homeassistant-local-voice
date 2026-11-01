# Hardware

This page tells you what hardware the image needs and which models to use on it. The goal is a voice assistant that runs on old office PCs, thin clients and small servers, with answers that are fast enough for daily use.

Numbers marked "measured" come from real runs. Numbers marked "estimate" are calculated from memory bandwidth and core count. Before you buy hardware, run your own test (see "Measure your own machine" below).

## Minimum requirements

| Part | Requirement |
|---|---|
| CPU | x86-64 with AVX2 and FMA. The STT and TTS engines contain AVX2 code. |
| Operating system | Linux with Docker or Podman. The image is for `linux/amd64` only. |
| RAM for speech only | 2 GB free |
| RAM with the local LLM | 8 GB at least, 16 GB recommended |
| GPU | Optional. Any GPU with a Mesa Vulkan driver: AMD GCN 1.0 or newer, Intel Gen 8 or newer. The image has no NVIDIA driver. |
| Disk | 4 GB for the image, plus 2 to 9 GB for an LLM file |

CPUs with AVX2 and FMA:

1. Intel Core i3, i5 and i7 of the 4th generation ("Haswell", 2013) and newer.
2. Intel N100, N150, N200 and N97 (Alder Lake-N, 2023).
3. AMD Ryzen, and AMD A-series APUs of the "Excavator" generation (for example A10-9700).

CPUs without AVX2 do not work. Examples:

1. Intel Pentium and Celeron of the Core generations up to the 10th, for example G3220, G4560 and G5400.
2. The Celeron and Pentium Silver chips of most thin clients, for example J1900, J4105, J5005, N4100 and N5105.
3. AMD GX chips, for example in the HP t620 and t630.

To check a machine, run this command. It must print both `avx2` and `fma`:

```bash
grep -o -w -E 'avx2|fma' /proc/cpuinfo | sort -u
```

## What uses the hardware

| Part | Where it runs | Memory | What limits the speed |
|---|---|---|---|
| STT (Parakeet) | GPU, or CPU without a GPU | 0.65 to 1.6 GB, see below | CPU or GPU compute. Short commands are fast everywhere. |
| TTS (PocketTTS) | GPU, or CPU without a GPU | About 0.5 GB | CPU or GPU compute |
| LLM (llama-server) | CPU by default (`LLM_DEVICE=none`) | 2 to 9 GB, see [llm.md](llm.md#files-and-memory) | Memory bandwidth for the reply, CPU compute for the prompt |

At start, the server asks llama.cpp for the Vulkan GPUs. Without one, STT and TTS run on the CPU backend of ggml. You do not have to set anything. `TTS_BACKEND` and `PARAKEET_DEVICE` override the choice ([configuration.md](configuration.md#text-to-speech-tts)).

The LLM is the slow part on old hardware. Speech is fast on any CPU with AVX2.

## Measured: speech on the CPU

Container limited to 4 CPUs (`--cpus=4`) on an Intel Core Ultra 7 270K, no GPU. The sentence: "Turn on the kitchen light. The living room lights are off now." (3.2 s of audio).

| STT model | Time | Container memory | Text |
|---|---|---|---|
| `ctc-0.6b-q8_0` (default) | 0.24 s | 910 MB | Lowercase, no punctuation |
| `ctc-0.6b-q4_k` | 0.23 s | 645 MB | Lowercase, no punctuation |
| `tdt-0.6b-v3-q8_0` | 0.15 s | 1.0 GB | Upper case and punctuation |
| `tdt-0.6b-v3-q4_k` | 0.15 s | 740 MB | Upper case and punctuation |

TTS (PocketTTS English q8_0, 4 threads): 0.7 s for the same sentence, the first request included.

A 4th-generation Core CPU has about half the speed per core for this work (estimate). Expect about 0.5 s for STT and 1.5 to 2 s for TTS per sentence. That is fast enough.

[voice.md](voice.md#measured-stt-accuracy) has the word error rates. The TDT models made fewer errors in noise. For satellites with small microphones, such as the M5Stack Atom Echo, use a TDT model.

## Measured: the LLM

Ling-3.0-tiny Q4_K_M (7.9B parameters, 1.3B active per token), a voice prompt of 5,800 tokens. Intel Core i5-12400T, 32 GB RAM.

| Device | Prompt (prefill) | Reply (generation) | Answer time with the prompt cache |
|---|---|---|---|
| CPU, 6 threads | 125 tokens/s | 33 tokens/s | 5 to 7 s |
| Intel UHD 730 iGPU (Vulkan), `-fa off` | 92 tokens/s | 10 tokens/s | Slower than the CPU |

The first request after a change of the prompt prefills the full prompt: about 45 s on this CPU. The prompt cache keeps it across restarts ([llm.md](llm.md#prompt-cache)).

## Estimate: the LLM on older CPUs

Two rules give a good estimate:

1. The reply speed depends on memory bandwidth. Each token reads all active weights once. Ling-3.0-tiny Q4_K_M reads about 0.8 GB per token.
2. The prompt speed depends on CPU cores and their instructions. CPUs from 12th-generation Core and newer have AVX-VNNI, which roughly doubles the speed per core for this work.

| CPU and memory | Prompt (estimate) | Reply (estimate) | First request (6,000 tokens) | Later answers (estimate) |
|---|---|---|---|---|
| Core i5-12400T, DDR4 (measured) | 125 tokens/s | 33 tokens/s | 45 s | 5 to 7 s |
| Core i5 or i7, 6th to 9th generation, dual-channel DDR4-2400 | 50 to 70 tokens/s | 18 to 25 tokens/s | 1.5 to 2 min | 8 to 15 s |
| Core i5 or i7, 4th generation, dual-channel DDR3-1600 | 35 to 50 tokens/s | 12 to 18 tokens/s | 2 to 3 min | 10 to 30 s |
| Intel N100, single-channel DDR4 or DDR5 | 25 to 40 tokens/s | 12 to 20 tokens/s | 2.5 to 4 min | 15 to 30 s |

"Later answers" are answers with the prompt cache. They include the thinking of the model and one or two tool calls. One memory module halves the bandwidth of a dual-channel board, so use two.

## Recommendations

| Hardware | STT | TTS | LLM | Notes |
|---|---|---|---|---|
| Old office PC: Core i5 or i7, 4th to 7th generation, 16 GB, no GPU | `tdt-0.6b-v3-q4_k` on the CPU | Default, on the CPU | Ling-3.0-tiny Q4_K_M, `LLM_CTX=32k` | Simple commands do not need the LLM (see below). Answers from the LLM take 10 to 30 s (estimate). |
| Same PC with 8 GB | `tdt-0.6b-v3-q4_k` | Default | Ling-3.0-tiny Q3_K_M at `LLM_CTX=32k`, or a cloud model | Q3_K_M needs about 4.4 GB. It was not part of the quality test. |
| Mini PC with N100 or N150, 16 GB | `tdt-0.6b-v3-q4_k`, on the iGPU or the CPU | Default | Ling-3.0-tiny Q4_K_M, `LLM_CTX=32k` | The iGPU can take STT and TTS, so the 4 CPU cores stay free for the LLM. |
| Desktop or small server, 8th generation Core or Ryzen and newer, 16 to 32 GB | `tdt-0.6b-v3-q8_0` | Default | Ling-3.0-tiny Q4_K_M, `LLM_CTX=64k` | The reference setup: 5 to 7 s per answer on a 12th-generation Core |
| Any of the above with an old 2 GB GPU (for example an AMD R7 250) | Default, on the GPU | Default, on the GPU | Ling-3.0-tiny on the CPU | The LLM does not fit next to speech in 2 GB. See [hardware-notes.md](hardware-notes.md). |
| A GPU with 8 GB or more | `tdt-1.1b-q8_0` | `bf16` | Ling-3.0-tiny Q4_K_M with `LLM_DEVICE=Vulkan0` and `LLM_EXTRA_ARGS=-fa off` | Not measured. A dedicated GPU with enough memory is usually much faster than a CPU. |

### Why not Q2 for the LLM

On a CPU, a smaller quant makes the reply only a little faster. From Q4_K_M to Q3_K_M, the bytes per token drop by about 20 %, so the reply gets about 20 % faster. The prompt speed does not change much. A small model loses a lot of quality at 2 bits, and wrong tool calls are worse than a slow answer. The image supports Q3 as the smallest quant ([llm.md](llm.md#files-and-memory)). With 16 GB, use Q4_K_M.

### Make old hardware feel fast

1. Turn on "Prefer handling commands locally" in the voice assistant of Home Assistant. Then "turn off the kitchen light" runs without the LLM in less than a second, and only free questions go to the LLM.
2. Add sentence triggers for frequent requests ("good night", "play some jazz"). An automation with a conversation trigger answers without the LLM. [home-assistant.md](home-assistant.md) has examples.
3. Expose only the entities that you want to control by voice. Each exposed entity and each tool adds tokens to the prompt. A shorter prompt makes the first request and each tool call faster.
4. Keep `LLM_SLOT_CACHE=on` and mount `/models/llm` writable, so a restart does not cost a full prefill.
5. Use `LLM_CTX=32k`. An Assist conversation is far below 32k tokens, and the smaller context needs less memory.
6. If answers still take too long, try `LLM_REASONING=off`. The model then answers without thinking. This was not part of the quality test, so test your own common requests.

## Measure your own machine

1. Start the container with your models.
2. Test speech: `docker exec <name> assistant-hub probe tts "Turn on the kitchen light." /tmp/a.wav`, then `docker exec <name> assistant-hub probe stt /tmp/a.wav`. Both print the time.
3. Test the LLM with a realistic prompt from your workstation: `python3 scripts/test/llm_client.py "Turn on the kitchen light." --devices 100 --url http://<host>:10300/llm/v1`. Run it twice. The first run prefills the full prompt, the second uses the cache.
4. The container log shows the prompt and reply speed of each request (`prompt eval time` and `eval time` lines of llama-server).
