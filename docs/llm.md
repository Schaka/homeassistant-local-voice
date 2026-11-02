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

The image contains llama-server from llama.cpp (prebuilt Vulkan release). If you set a model, it starts on 127.0.0.1, and `assistant-hub` serves its OpenAI-compatible API on the main port as `/llm/v1`. Without a model, it does not start.

### 1. Choose a model

Set one of these two variables, not both:

| Variable | Example | Effect |
|---|---|---|
| `LLM_HF_REPO` | `inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M` | llama-server downloads the model from Hugging Face at the first start, into `/models/llm/hf` |
| `LLM_MODEL_FILENAME` | `Ling-3.0-tiny-Q4_K_M.gguf` | Uses a file that is already in `/models/llm` |

Mount a host folder on `/models/llm`. Then the model, the download cache and the saved prompt cache stay on the host.

```bash
docker run -d --name assistant-hub \
  -v /srv/voice-models/llm:/models/llm \
  -e LLM_HF_REPO=inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M \
  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -p 10300:10300 --restart unless-stopped \
  ghcr.io/schaka/homeassistant-local-voice:latest
```

The image never deletes a model. When the model of `LLM_HF_REPO` is in the cache, llama-server starts with `--offline` and does not contact Hugging Face. Several quants can stay in the cache. To get a newer upstream file, delete its folder under `/models/llm/hf`.

### 2. Connect Home Assistant

1. Go to Settings > Devices & services > Add Integration > llama.cpp (Home Assistant 2026.8 or later).
2. Set the URL to `http://<host>:10300/llm/v1`. Leave the API key empty, or use the value of `LLM_API_KEY`.
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

For the models with thinking, the thinking text goes to `reasoning_content`, so TTS does not speak it. Thinking adds time before the reply. `LLM_REASONING=off` turns it off, and `LLM_REASONING_BUDGET` limits it. The default budget depends on the family, see "Speed settings per family" below.

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
2. MiniCPM5-2B Q4_K_M: good answers, smaller file. 15 to 17 s with the original template and a budget of 512 tokens, 6 s per question with the defaults on a Core Ultra 7 270K (see "Speed settings per family").
3. LFM3-1.7B Q4_K_M: all answers correct. 23 to 26 s with the original template and a budget of 512 tokens, 8 s per question with the defaults on a Core Ultra 7 270K (see "Speed settings per family"). Keep its thinking on (`LLM_REASONING=on`, the default).
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

An Assist prompt with all tools has about 4,500 to 7,000 tokens. The first request prefills all of it, which takes 30 to 70 s on a CPU. `docker exec <name> assistant-hub probe llm "Turn on the kitchen light." --devices 100` sends such a prompt for a test.

## Prompt cache

After the first request, llama-server keeps the processed prompt in its cache. Home Assistant puts the instructions, the device list and the tools before the current time, so the start of the prompt stays the same. A later request prefills only the new part, often about 20 tokens. Most of the remaining time is generation and thinking: 1 to 5 s.

A restart empties this cache. With `LLM_SLOT_CACHE=on` (the default), `assistant-hub` saves the KV cache of the last Assist conversation to `/models/llm/slots`:

1. If the cache changed, it saves it every 5 minutes. It also saves it at each container stop.
2. When llama-server starts, it restores the file once. A slow health check under full CPU load is no restart, and a restore never runs during a request. A restore during a request would replace the current cache with the saved one, and the request would prefill the full prompt again.
3. For 6,200 tokens of Ling-3.0-tiny, the file is 124 MB. A save or a restore takes 0.3 s or less (13 ms and 25 ms for 3,450 tokens in a test).

The first request after a restart then takes about 0.5 s instead of more than 60 s. `/models/llm` must be writable. The cache is empty again after each change of the prompt, of the exposed entities or of the tools.

If a backup tool stops the container every night, the cache survives, but voice control is off during the backup. Exclude the container from such a backup. You can download the models again.

## The tool-result fix of the chat templates

Home Assistant's llama.cpp integration sends the text and the tool calls of earlier answers back to the model, but not their thinking. The templates of Ling 3.0, LFM3 and MiniCPM5 open a new thinking block after every tool result. So the model thinks again from scratch before it reports a result, and Ling often writes its answer into the open block, which leaves the reply empty.

At each start, `assistant-hub` reads the chat template from the model file, changes one line, and passes the result to llama-server (`src/llm/template.rs`): after a tool result, the answer starts with a closed thinking block. The model still thinks after a user message, where it decides which tool to call. The repository holds no copy of the templates, so a new upstream template is used as soon as it ships. If the line to change is missing, the log warns and the original template is used. `LLM_TEMPLATE_FIX=off` turns the fix off. For a model from `LLM_HF_REPO`, the fix starts with the second start, after the download.

Upstream status: llama.cpp b11057 added a Ling 3.0 parser for a tool call inside an unclosed thinking block (issue #27462). It does not cover a plain answer without `</think>`, which is the case that the fix covers.

## Speed settings per family

Measured on 2026-11-01 with 10 Home Assistant style questions: a question, a tool call, the tool result, the answer (lights, weather, sensor values, music, small talk). The history was sent the way Home Assistant sends it, without thinking. llama-server b11966, 6 threads of a Core Ultra 7 270K. The time is the mean per question after the first one, which also prefills the whole prompt.

| Model | Setting | Correct | Time per question |
|---|---|---|---|
| LFM3-1.7B Q4_K_M | Original template, budget 512 | 9 of 10 | 14.9 s |
| LFM3-1.7B Q4_K_M | Fix, budget 256 | 9 of 10 | 9.9 s |
| LFM3-1.7B Q4_K_M | Fix, budget 128 (default) | 9 of 10 | 8.3 s |
| MiniCPM5-2B Q4_K_M | Original template, budget 512 | 9 of 10 | 11.7 s |
| MiniCPM5-2B Q4_K_M | Original template, budget 128 | 9 of 10 | 8.5 s, but one answer continued its thinking aloud |
| MiniCPM5-2B Q4_K_M | Fix, budget 512 | 9 of 10 | 8.0 s |
| MiniCPM5-2B Q4_K_M | Fix, budget 256 (default) | 9 of 10 | 5.9 s |
| MiniCPM5-2B Q4_K_M | Fix, budget 128 | 7 of 10 | 6.2 s |
| Ling-3.0-tiny Q4_K_M | Fix, budget 512 (default) | 9 of 10 | 2.9 s |
| Ling-3.0-tiny Q4_K_M | Fix, budget 128 | 9 of 10 | 3.3 s |

So the defaults of `LLM_REASONING_BUDGET` are 128 for `lfm3`, 256 for `minicpm5` and 512 for the others. Set the variable, or `--reasoning-budget` in `LLM_EXTRA_ARGS`, to choose another value. `LLM_MAX_TOKENS` (default 1024) caps the whole reply with `--n-predict`, so a template that never closes its thinking block cannot generate until the 64k context is full. The question that all models got wrong was "turn off all the lights downstairs": they switched one room instead of the floor. The prompt of Home Assistant, with floors and areas, helps there.

The token speed was not the problem: LFM3 generated 42 tokens per second, MiniCPM5 25 and Ling 35 on this CPU. The time went into thinking. Other settings did not help:

1. A smaller micro-batch (`-ub 128` or `-ub 256`) or denser context checkpoints (`--checkpoint-min-step 0`). LFM3 and Ling have layers without a KV cache, which llama.cpp can only rewind to a saved checkpoint. With the defaults, LFM3 processes about 500 tokens again for each request (about 3 s here). The smaller micro-batches made one of the two request types fast and the other one process the whole prompt again. Ling processes only 60 to 150 tokens again with the defaults.

## Limits

1. Without `LLM_API_KEY`, every client on your network can use `/llm/...`. If port 10300 is reachable from outside your LAN, set `LLM_API_KEY`.
2. With thinking on, a small model can think for a long time. `LLM_REASONING_BUDGET` limits it. The default is 128 to 512 tokens, depending on the family.
3. MTP speculative decoding is off. For Ling-3.0-tiny, it hangs on RADV (llama.cpp PR #26608).
4. If the container has a memory limit, do not start a second llama-server in it. Two Ling instances need about 10 GB.

All LLM variables are in [configuration.md](configuration.md#conversation-agent-llm).
