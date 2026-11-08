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

The image contains two programs named llama-server: the one of llama.cpp (prebuilt Vulkan release) and the one of ik_llama.cpp (built from source, CPU only). If you set a model, `assistant-hub` starts one of them on 127.0.0.1 and serves its OpenAI-compatible API on the main port as `/llm/v1`. You do not choose. The server picks the faster program for your model and device, see [Engines](#engines-llamacpp-and-ik_llamacpp). Without a model, neither program starts.

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
2. MiniCPM5-2B Q4_K_M: good answers, smaller file. 15 to 17 s with the original template and a budget of 512 tokens, 6 s per question with a budget of 128 tokens on a Core Ultra 7 270K (see "Speed settings per family").
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

## Engines: llama.cpp and ik_llama.cpp

ik_llama.cpp is a fork of llama.cpp with faster CPU code. It runs on the CPU only. The server uses it when all of these are true:

1. The model runs on the CPU (`LLM_DEVICE=none`).
2. The model is one of these. The server reads the architecture from the model file.

   | Model | `general.architecture` |
   |---|---|
   | Ling-3.0-tiny | `bailingmoe3` |
   | MiniCPM5-2B | `llama` |
   | LFM3-1.7B | `lfm3` |

3. The CPU has AVX2 and FMA.

In every other case, llama.cpp runs the model. The log names the engine and the reason at each start (`LLM engine: ...`). `LLM_ENGINE=llamacpp` always uses llama.cpp. `LLM_ENGINE=ik` uses ik_llama.cpp for any model on the CPU, also one that is not in the table. The server does not test such a model.

If ik_llama.cpp fails to start twice in a row, or stops within two minutes of a start twice in a row, llama.cpp runs the model until the next start of the container. A model from `LLM_HF_REPO` that is not downloaded yet starts on llama.cpp, because ik_llama.cpp cannot download by repository name. When the model answers, the server restarts on ik_llama.cpp. During this one restart, requests get a 503. The server finds the downloaded file by the tag of `LLM_HF_REPO` (`q4_k_m` without a tag). If no file name in the cache contains the tag, the model stays on llama.cpp, and the log names the cause.

The server hides these differences, so the same settings work with both programs:

1. ik_llama.cpp gets the model file, no `--device`, no `-ngl`, `--webui none` and `-n` for the reply limit.
2. ik_llama.cpp uses 8 threads by default. The server sets the number of physical cores, at most 8 and at most the CPUs that the container is pinned to. `-t` in `LLM_EXTRA_ARGS` overrides it. A CPU quota (`--cpus`) does not lower the number. Prompt speed on performance cores of the Core Ultra 7 270K Plus for Ling-3.0-tiny: 4 threads on 4 pinned cores read 354 tokens/s and 8 threads on the same 4 cores read 195. On 8 pinned cores, 8 threads read 654 and 6 threads read 502. With a quota of 4 CPUs on 8 cores, 8 threads read 665 and 4 threads read 348.
3. ik_llama.cpp saves a checkpoint 5 tokens before the end of each prompt. Ling and LFM3 have layers without a KV cache, so ik_llama.cpp can only go back to such a checkpoint. A new question then starts from a checkpoint far back, and the engine reads about 1,700 tokens again. The server sets `--ctx-checkpoints-tolerance 64`, so the checkpoint lies before the question and the engine reads about 60 tokens again. The values 16, 32, 64, 128 and 192 all worked, and the tokens read again grew with the value (median 22 to 74).
4. Each program saves its prompt cache in its own file (`MODEL-CTX.slot` and `MODEL-CTX-ik.slot`). ik_llama.cpp writes two more files next to each one (`.checkpoints` and `.tokens.json`) and needs them to reuse the cache.
5. ik_llama.cpp never answers a request to restore a file that does not exist. The server restores only files that exist, and it waits at most 30 s.

`LLM_EXTRA_ARGS` goes to the program that runs. If an argument is only known to llama.cpp, use `LLM_ENGINE=llamacpp`.

### Measured speed

Intel Core Ultra 7 270K Plus, 6 performance cores (`taskset -c 0-5`, 6 threads), CPU only, 2026-11-02. The models are Q4_K_M. llama.cpp is b12032 (the image build). ik_llama.cpp is commit `35e0be5`, built for AVX2 and FMA only (`scripts/build-ik-llama.sh`). `llama-bench` with 6 threads:

| Model | Engine | Prompt, 512 tokens | Prompt, 4,096 tokens | Generation |
|---|---|---|---|---|
| Ling-3.0-tiny | llama.cpp | 314 | 231 | 50.9 |
| Ling-3.0-tiny | ik_llama.cpp | 627 | 538 | 75.1 |
| MiniCPM5-2B | llama.cpp | 206 | 158 | 40.0 |
| MiniCPM5-2B | ik_llama.cpp | 340 | 257 | 48.4 |
| LFM3-1.7B | llama.cpp | 297 | 277 | 50.7 |
| LFM3-1.7B | ik_llama.cpp | 492 | 454 | 63.8 |

The numbers are tokens per second. The option `-rtr` (repack at load) made ik_llama.cpp slower for Q4_K_M, so the server does not set it.

`assistant-hub eval llm` asks 12 Home Assistant style questions three times each, with the command line of the server and the budget of 128 thinking tokens. The temperature and top-p were 0.7 and 1.0 (Ling), 0.9 and 0.95 (MiniCPM5), and 0.1 and 1.0 (LFM3). Each model has 33 counted answers. One more question per model is a known failure ("turn off all the lights downstairs") and does not count. The prompt has about 3,800 tokens.

| Model | Engine | Correct | First request | Per question (mean) | Tokens read again per question |
|---|---|---|---|---|---|
| Ling-3.0-tiny | llama.cpp | 30 of 33 | 19.2 s | 3.8 s | 88 |
| Ling-3.0-tiny | ik_llama.cpp | 31 of 33 | 9.5 s | 2.1 s | 101 |
| MiniCPM5-2B | llama.cpp | 30 of 33 | 24.7 s | 5.5 s | 78 |
| MiniCPM5-2B | ik_llama.cpp | 29 of 33 | 16.0 s | 3.7 s | 73 |
| LFM3-1.7B | llama.cpp | 33 of 33 | 18.6 s | 6.1 s | 717 |
| LFM3-1.7B | ik_llama.cpp | 32 of 33 | 11.5 s | 3.2 s | 103 |

The differences in correct answers are inside the noise of 33 answers. `assistant-hub eval compare` lets a new engine replace the old one only if it is within 5 points in quality and 20 % faster per question (or 30 % faster at the first request). All three models pass.

Only this one CPU was measured with ik_llama.cpp. The gain in generation speed comes from the compute part of the work, so it can be smaller on a PC with slow memory. The gain in prompt speed stays.

## Prompt cache

After the first request, llama-server keeps the processed prompt in its cache. Home Assistant puts the instructions, the device list and the tools before the current time, so the start of the prompt stays the same. A later request prefills only the new part, often about 20 tokens. Most of the remaining time is generation and thinking: 1 to 5 s.

A restart empties this cache. With `LLM_SLOT_CACHE=on` (the default), `assistant-hub` saves the KV cache of the last Assist conversation to `/models/llm/slots`. A change of the engine reads a different file, so the first request after such a change prefills the full prompt:

1. If the cache changed, it saves it every 5 minutes. It also saves it at each container stop.
2. When llama-server starts, it restores the file once. A slow health check under full CPU load is no restart, and a restore never runs during a request. A restore during a request would replace the current cache with the saved one, and the request would prefill the full prompt again.
3. For 6,200 tokens of Ling-3.0-tiny on llama.cpp, the file is 124 MB. A save or a restore takes 0.3 s or less (13 ms and 25 ms for 3,450 tokens in a test). On ik_llama.cpp, 4,750 tokens make 134 MB in three files, and the save and the restore take 14 ms and 31 ms.

The first request after a restart then takes about 0.5 s instead of more than 60 s. `/models/llm` must be writable. The cache is empty again after each change of the prompt, of the exposed entities or of the tools.

If a backup tool stops the container every night, the cache survives, but voice control is off during the backup. Exclude the container from such a backup. You can download the models again.

## The tool-result fix of the chat templates

Home Assistant's llama.cpp integration sends the text and the tool calls of earlier answers back to the model, but not their thinking. The templates of Ling 3.0, LFM3 and MiniCPM5 open a new thinking block after every tool result. So the model thinks again from scratch before it reports a result, and Ling often writes its answer into the open block, which leaves the reply empty.

At each start, `assistant-hub` reads the chat template from the model file, changes one line, and passes the result to llama-server (`src/llm/template.rs`): after a tool result, the answer starts with a closed thinking block. The model still thinks after a user message, where it decides which tool to call. The repository holds no copy of the templates, so a new upstream template is used as soon as it ships. If the line to change is missing, the log warns and the original template is used. `LLM_TEMPLATE_FIX=off` turns the fix off. For a model from `LLM_HF_REPO`, the fix starts with the second start, after the download.

Upstream status: llama.cpp b11057 added a Ling 3.0 parser for a tool call inside an unclosed thinking block (issue #27462). It does not cover a plain answer without `</think>`, which is the case that the fix covers.

## Empty replies and the repeat

Ling sometimes writes its tool call as text (`<tool_call>name ...`) inside its thinking. llama-server does not turn that text into a tool call, so the reply has no text and no tool call. Home Assistant then answers "Unable to get response" and keeps the question without an answer in the chat log, so the next questions of that conversation can fail the same way.

With `LLM_RETRY_EMPTY=on` and thinking on, the proxy in `src/llm/proxy.rs` watches each chat reply. A streamed reply goes to the client event by event, except for the events from the first `finish_reason` on. They wait until the proxy knows that the reply has text or a tool call. If it has neither, the proxy sends the request again with `chat_template_kwargs.enable_thinking=false` and passes on the second reply. If the second reply is empty too, the client gets the first one. A reply with text or a tool call passes without a delay. Only one request is repeated, never more.

The repeat costs about the time of a request with a cached prompt (prefill of the last 60 to 70 tokens, 8 generated tokens for a tool call). Without thinking, a tool call took 8 tokens instead of 33 on the Unraid server.

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
| MiniCPM5-2B Q4_K_M | Fix, budget 256 | 9 of 10 | 5.9 s |
| MiniCPM5-2B Q4_K_M | Fix, budget 128 (default) | 7 of 10 | 6.2 s |
| Ling-3.0-tiny Q4_K_M | Fix, budget 512 | 9 of 10 | 2.9 s |
| Ling-3.0-tiny Q4_K_M | Fix, budget 128 (default) | 9 of 10 | 3.3 s |

The defaults of `LLM_REASONING_BUDGET` are 128 for `ling3`, `minicpm5` and `lfm3`, and 512 for the others. The 7 of 10 of MiniCPM5 at 128 is one run of 10 questions. The larger test below measured no loss. Set the variable, or `--reasoning-budget` in `LLM_EXTRA_ARGS`, to choose another value. `LLM_MAX_TOKENS` (default 1024) sets the limit for one reply with `--n-predict`. As a result, a template that never closes its thinking block cannot generate until the 64k context is full. The question that all models got wrong was "turn off all the lights downstairs": they switched one room instead of the floor. The prompt of Home Assistant, with floors and areas, helps there.

The token speed was not the problem: LFM3 generated 42 tokens per second, MiniCPM5 25 and Ling 35 on this CPU. The time went into thinking. Other settings did not help:

1. A smaller micro-batch (`-ub 128` or `-ub 256`) or denser context checkpoints (`--checkpoint-min-step 0`). LFM3 and Ling have layers without a KV cache, which llama.cpp can only rewind to a saved checkpoint. With the defaults, LFM3 processes about 500 tokens again for each request (about 3 s here). The smaller micro-batches made one of the two request types fast and the other one process the whole prompt again. Ling processes only 60 to 150 tokens again with the defaults.

### Quality by thinking budget

A larger test compared the budgets 128 and 256 on a Radeon RX 9070 XT (llama.cpp b11966, Vulkan, 2026-11-02). It used 120 mock Home Assistant cases in 12 categories with 3 seeds each, so 360 runs per row. The prompt had about 2,000 tokens.

| Model | Thinking | Task success | 95 % interval | Thinking tokens per case |
|---|---|---|---|---|
| Ling-3.0-tiny | off | 76.4 % | 71.7 to 80.5 | 0 |
| Ling-3.0-tiny | budget 128 | 84.2 % | 80.0 to 87.6 | 104 |
| Ling-3.0-tiny | budget 256 | 83.3 % | 79.1 to 86.8 | 123 |
| MiniCPM5-2B | off | 78.3 % | 73.8 to 82.3 | 0 |
| MiniCPM5-2B | budget 128 | 83.9 % | 79.7 to 87.3 | 142 |
| MiniCPM5-2B | budget 256 | 86.1 % | 82.2 to 89.3 | 207 |

The paired difference between the two budgets includes zero for both models: Ling -0.8 points (-2.2 to +0.6), MiniCPM5 +2.2 points (-1.7 to +6.1). Thinking itself raised success by 5.6 to 7.8 points. Budget 128 gives the same quality with fewer thinking tokens.

With thinking on, Ling tried to unlock the front door in 12 of 360 runs, against 3 without thinking. The mock server blocked these attempts. Expose locks to Assist only if you accept this risk.

## Limits

1. Without `LLM_API_KEY`, every client on your network can use `/llm/...`. If port 10300 is reachable from outside your LAN, set `LLM_API_KEY`.
2. With thinking on, a small model can think for a long time. `LLM_REASONING_BUDGET` limits it. The default is 128 to 512 tokens, depending on the family.
3. MTP speculative decoding is off. For Ling-3.0-tiny, it hangs on RADV (llama.cpp PR #26608).
4. The server runs one engine at a time. Do not start a second llama-server in the container: two Ling instances need about 10 GB.

All LLM variables are in [configuration.md](configuration.md#conversation-agent-llm).
