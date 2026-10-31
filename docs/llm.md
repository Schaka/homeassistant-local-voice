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

`LLM_FAMILY` selects the sampling values that the model card recommends and, for Ling, the patched chat template. If it is empty, the image detects the family from the model name. For any other model, set it.

| `LLM_FAMILY` | Model | Architecture |
|---|---|---|
| `ling3` | Ling-3.0-tiny (7.9B MoE, 1.3B parameters active per token) | `bailingmoe3` |
| `granite4` | IBM Granite 4.0 H Tiny (7B MoE, about 1B parameters active per token). H Micro (3B) uses the same family, but was not tested. | `granitehybrid` (Mamba-2 and attention layers) |

Ling 3.0 is a hybrid reasoning model, and thinking is on by default. The thinking text goes to `reasoning_content`, so TTS does not speak it. Thinking adds time before the reply. `LLM_REASONING=off` turns it off, and `LLM_REASONING_BUDGET` (default 512 tokens) limits it.

The smallest supported quant is Q3. Q2, IQ2 and Q1 are not supported.

| `LLM_HF_REPO` | File | LLM memory at 64k context |
|---|---|---|
| `bartowski/Ling-3.0-tiny-GGUF:Q3_K_M` | 3.79 GB | About 4.4 GB |
| `bartowski/Ling-3.0-tiny-GGUF:IQ4_XS` | 4.39 GB | About 5.0 GB |
| `inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M` | 4.82 GB | 5.1 GiB (measured) |
| `inclusionAI/Ling-3.0-tiny-GGUF:Q6_K` | 6.50 GB | About 7.2 GB |
| `inclusionAI/Ling-3.0-tiny-GGUF:Q8_0` | 8.41 GB | About 9.1 GB |

Granite 4.0 has no thinking mode, so `LLM_REASONING` has no effect for `granite4`. Most of its layers are Mamba-2 layers, which keep a fixed-size state instead of a KV cache. So a long Home Assistant prompt needs little memory and prefills fast.

| `LLM_HF_REPO` | File |
|---|---|
| `ibm-granite/granite-4.0-h-micro-GGUF:Q4_K_M` | 1.94 GB |
| `ibm-granite/granite-4.0-h-tiny-GGUF:Q4_K_M` | 4.23 GB |
| `ibm-granite/granite-4.0-h-tiny-GGUF:Q8_0` | 7.39 GB |

Add about 1.7 GB for STT and TTS (about 2.4 GB with the STT model `tdt-1.1b-q8_0`). Both families need little memory for the context. Ling keeps a KV cache (the memory that holds the context) in only 6 of its 24 layers: 0.4 GiB at 64k context.

The bartowski repo gives the smaller Ling quants. `./scripts/download-models.sh <file>.gguf` downloads the official files with a size test, for use with `LLM_MODEL_FILENAME`.

## Which model to use

Ling-3.0-tiny on the CPU is the default and the most reliable choice that we measured. [model-comparison.md](model-comparison.md) has the test of eight models and quants through Home Assistant: correct answers, time per answer, and why the others were not chosen.

## Vulkan or CPU

Both families run on the CPU by default (`LLM_DEVICE=none`). On an Intel UHD 730 iGPU, Ling-3.0-tiny runs fully on Vulkan, but it is slower there than on the CPU:

| Ling-3.0-tiny Q4_K_M | Prefill | Generation |
|---|---|---|
| Intel iGPU (Vulkan) | 38 tokens/s | 11 tokens/s |
| Desktop CPU | 101 to 140 tokens/s | 31 to 35 tokens/s |

On a dedicated GPU with enough memory, `LLM_DEVICE=Vulkan0` can be faster.

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
