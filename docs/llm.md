# Conversation agent

The conversation agent is the LLM that understands free-form requests and calls Home Assistant tools. You have two choices: a cloud model, or the local LLM in this container.

If "Prefer handling commands locally" is on in the voice assistant, Home Assistant runs simple commands ("turn off the kitchen light") without an LLM. The LLM handles only the rest.

## Option 1: a cloud model

| Integration | Notes |
|---|---|
| OpenRouter (official, Home Assistant 2025.8 or later) | Paste an OpenRouter API key and select a model. The free `:free` models (list at [openrouter.ai/models](https://openrouter.ai/models)) give a few commands per day. Set a budget limit on the key. |
| Local LLM (HomeLLM) add-on with an OpenAI-compatible backend | Gives full control over the prompt and the device list. [home-llm-prompt.md](home-llm-prompt.md) has a tested prompt and the backend setup. |

Results with HomeLLM and OpenRouter:

| Model | Result |
|---|---|
| `deepseek/deepseek-v4-flash` | Works well |
| `gpt-5.6-luna` | Does not work. It rejects the `strict` tool schemas of HomeLLM with HTTP 400. |
| Free routes | Work, but return HTTP 429 after a few commands per day |

If a model returns HTTP 400, try another model first. The backend is only a URL and a key.

## Option 2: the local LLM

The image contains llama-server from llama.cpp (prebuilt Vulkan release). If you set a model, it starts and serves an OpenAI-compatible API on port 8080 (`/v1`). Without a model, it does not start.

### 1. Choose a model

Set one of these two variables, not both:

| Variable | Example | Effect |
|---|---|---|
| `LLM_HF_REPO` | `inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M` | llama-server downloads the model from Hugging Face at the first start, into `/models/llm/hf` |
| `LLM_MODEL_FILENAME` | `Ling-3.0-tiny-Q4_K_M.gguf` | Uses a file that is already in `/models/llm` |

Mount a host folder on `/models/llm`. Then the model, the download cache and the saved prompt cache stay on the host.

```bash
docker run -d --name wyoming-voice \
  -v /srv/voice-models/llm:/models/llm \
  -e LLM_HF_REPO=inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M \
  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -p 10300:10300 -p 8080:8080 --restart unless-stopped \
  ghcr.io/schaka/homeassistant-local-voice:latest
```

The image never deletes a model. When the model of `LLM_HF_REPO` is in the cache, llama-server starts with `--offline` and does not contact Hugging Face. Several quants can stay in the cache. To get a newer upstream file, delete its folder under `/models/llm/hf`.

### 2. Connect Home Assistant

1. Go to Settings > Devices & services > Add Integration > llama.cpp (Home Assistant 2026.8 or later).
2. Set the URL to `http://<host>:8080/v1`. Leave the API key empty.
3. Select the model.
4. In Settings > Voice assistants, select the new llama.cpp agent as the conversation agent.

Other OpenAI-compatible clients use the same URL, for example the "llama.cpp server" backend of HomeLLM. If you set `LLM_API_KEY`, the server accepts only that key.

## Supported models

`LLM_FAMILY` selects the sampling values from the model card and, for Ling, the patched chat template. If it is empty, the image detects the family from the model name. For any other model, set it.

| `LLM_FAMILY` | Model | Type | Thinking |
|---|---|---|---|
| `ling3` | Ling-3.0-tiny | 7.9B MoE, 1.3B active per token | Yes |
| `minicpm5` | MiniCPM5-2B (and 1B, not recommended) | 2B dense | Yes |
| `lfm3` | LFM3-1.7B | 1.7B dense, mostly convolution layers | Yes |
| `qwen35` | Qwen3.5-4B | 4B dense | Yes |
| `granite4` | Granite 4.0 H Tiny (and H Micro, not tested) | 7B MoE, about 1B active per token, mostly Mamba-2 layers | No |

For the models with thinking, the thinking text goes to `reasoning_content`, so TTS does not speak it. Thinking adds time before the reply. `LLM_REASONING=off` turns it off, and `LLM_REASONING_BUDGET` (default 512 tokens) limits it.

### Set the sampling in Home Assistant

The llama.cpp integration of Home Assistant sends a temperature and a top-p value with each request. These override the values of the image. With "Recommended model settings" on, Home Assistant sends temperature 0.7 and top-p 1.0, which fit only some models. Turn "Recommended model settings" off in the options of the conversation agent, and set the values of the model. [model-comparison.md](model-comparison.md) has the results for the values in the test:

| Model | Temperature and Top P in the test | Model card |
|---|---|---|
| Ling-3.0-tiny | 0.7 and 1.0 (the Home Assistant defaults) | 1.0 and 0.95 |
| MiniCPM5-2B | 0.9 and 0.95 | 0.9 and 0.95 |
| LFM3-1.7B | 0.1 and 1.0, thinking on | 0.1 (top-p not given) |
| Qwen3.5-4B | 0.7 and 1.0 | 1.0 and 0.95 |
| Granite 4.0 H Tiny | 0.7 and 1.0 | 0.0 (greedy) |

The other values of the model card (top-k, min-p, penalties) come from the image.

### Files and memory

The smallest supported quant is Q3. Q2, IQ2 and Q1 are not supported.

| `LLM_HF_REPO` | File | LLM memory at 64k context |
|---|---|---|
| `inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M` | 4.82 GB | 5.1 GiB (measured) |
| `bartowski/Ling-3.0-tiny-GGUF:Q3_K_M` | 3.79 GB | About 4.4 GB |
| `inclusionAI/Ling-3.0-tiny-GGUF:Q8_0` | 8.41 GB | About 9.1 GB |
| `openbmb/MiniCPM5-2B-GGUF:Q4_K_M` | 1.56 GB | About 4.7 GB |
| `openbmb/MiniCPM5-2B-GGUF:Q8_0` | 2.68 GB | About 5.8 GB |
| `LiquidAI/LFM3-1.7B-GGUF:Q4_K_M` | 1.10 GB | About 2 GB (estimate) |
| `LiquidAI/LFM3-1.7B-GGUF:Q8_0` | 1.85 GB | About 2.8 GB (estimate) |
| `unsloth/Qwen3.5-4B-GGUF:Q4_K_M` | 2.74 GB | About 4.5 GB (measured at 32k) |
| `ibm-granite/granite-4.0-h-tiny-GGUF:Q4_K_M` | 4.23 GB | About 4.5 GB (estimate) |

Add about 1.7 GB for STT and TTS (about 2.4 GB with the STT model `tdt-1.1b-q8_0`). The KV cache (the memory that holds the context) differs a lot. Ling keeps it in only 6 of its 24 layers (0.4 GiB at 64k), and LFM3 and Granite mostly use layers without a KV cache. MiniCPM5-2B needs 2.6 GiB at 64k, so `LLM_CTX=32k` halves its memory need.

`./scripts/download-models.sh <file>.gguf` downloads the official files with a size test, for use with `LLM_MODEL_FILENAME`.

## Which model to use

Measured through Home Assistant on a CPU (i5-12400T), see [model-comparison.md](model-comparison.md):

1. Ling-3.0-tiny Q4_K_M: the default. All answers correct, 5 to 7 s.
2. MiniCPM5-2B Q4_K_M: good answers, 15 to 17 s, smaller file.
3. LFM3-1.7B Q4_K_M: all answers correct, but 23 to 26 s. Keep its thinking on (`LLM_REASONING=on`, the default).
4. Granite 4.0 H Tiny Q4_K_M: the fastest (3 s), but less reliable.
5. Qwen3.5-4B Q4_K_M: correct, but 20 s.

## Vulkan or CPU

The LLM runs on the CPU by default (`LLM_DEVICE=none`). On the Intel UHD 730 iGPU of an i5-12400T, Ling-3.0-tiny runs fully on Vulkan, but it generates much slower there than on the CPU:

| Ling-3.0-tiny Q4_K_M, 5,800-token prompt | Prefill | Generation |
|---|---|---|
| CPU, 6 threads | 125 tokens/s | 33 tokens/s |
| UHD 730 (Vulkan), `-fa off` | 92 tokens/s | 10 tokens/s |
| UHD 730 (Vulkan), flash attention on | 29 tokens/s | 11 tokens/s |

If you use a GPU, set `LLM_EXTRA_ARGS=-fa off`. On a newer iGPU (Arrow Lake) or a dedicated GPU with enough memory, `LLM_DEVICE=Vulkan0` can be faster than the CPU. [igpu-backends.md](igpu-backends.md) has all measurements, also for SYCL and OpenVINO.

An Assist prompt with all tools has about 4,500 to 7,000 tokens. The first request prefills all of it, which takes 30 to 70 s on a CPU. `python3 scripts/test/llm_client.py "Turn on the kitchen light." --devices 100` sends such a prompt for a test.

## Prompt cache

After the first request, llama-server keeps the processed prompt in its cache. Home Assistant puts the instructions, the device list and the tools before the current time, so the start of the prompt stays the same. A later request prefills only the new part, often about 20 tokens. Most of the remaining time is generation and thinking: 1 to 5 s.

A restart empties this cache. With `LLM_SLOT_CACHE=on` (the default), `llm_slot_cache.py` saves the KV cache of the last Assist conversation to `/models/llm/slots`:

1. If the cache changed, it saves it every 5 minutes. It also saves it at each container stop.
2. When llama-server starts, it restores the file.
3. For 6,200 tokens of Ling-3.0-tiny, the file is 124 MB, and a save or a restore takes 0.3 s.

The first request after a restart then takes about 0.5 s instead of more than 60 s. `/models/llm` must be writable. The cache is empty again after each change of the prompt, of the exposed entities or of the tools.

If a backup tool stops the container every night, the cache survives, but voice control is off during the backup. Exclude the container from such a backup. You can download the models again.

## Patched chat template for Ling

After a tool result, the original Ling template starts the answer with an open `<think>`. Ling then often writes its answer at once and never closes the block. llama-server then returns the answer as thinking (`reasoning_content`), and the reply text is empty. With the patched template `templates/ling3.jinja`, the turn after a tool result starts with a closed `<think></think>`, and the problem did not occur. Ling still thinks after a user message, where it decides which tool to call.

Upstream status: llama.cpp b11057 added a Ling 3.0 parser for a tool call inside an unclosed thinking block (issue #27462). It does not cover a plain answer without `</think>`, which is the case that the patched template fixes. The case can be reported in that issue with a recorded request, and the template patch can go to the model's Hugging Face repository.

## Thinking guard

Ling does not need the guard with the patched template. The guard is an option for other models that show the same problem.

With thinking on, a small model now and then writes its whole answer into the thinking block and does not close it. Then the reply has no text. Home Assistant does not show thinking, so it sees an empty reply, asks again, and stops after 10 rounds with "Unable to get response".

With `LLM_THINKING_GUARD=on` (off by default), `llm_guard.py` takes port 8080, and llama-server moves to the internal port 8081. The guard reads each chat reply completely. If the reply has no text and no tool call, it sends the request once more with thinking off. The model still thinks for all other replies. Because the guard reads the complete reply first, the client gets it at once at the end, not word by word.

## Limits

1. Without `LLM_API_KEY`, every client on your network can use the server, and CORS allows all origins. If port 8080 is reachable from outside your LAN, set `LLM_API_KEY`.
2. With thinking on, a small model can think for a long time. `LLM_REASONING_BUDGET` limits it to 512 tokens by default.
3. MTP speculative decoding is off. For Ling-3.0-tiny, it hangs on RADV (llama.cpp PR #26608).
4. If the container has a memory limit, do not start a second llama-server in it. Two Ling instances need about 10 GB.

All LLM variables are in [configuration.md](configuration.md#conversation-agent-llm).
