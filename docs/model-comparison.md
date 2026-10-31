# Model comparison

This page shows which small models work as the conversation agent of Home Assistant on a CPU, and which do not. Read it before you try another model: most of the tested models were too slow or not reliable enough.

All results are from 2026-10-31.

## How the models were tested

| Item | Value |
|---|---|
| Hardware | Intel Core i5-12400T, 6 threads, CPU only (`--device none`), 32 GB RAM |
| Server | llama-server b11966 from this image, context 32k |
| Client | Home Assistant 2026.10.4, llama.cpp integration, temperature 0.7 and top-p 1.0 (sent by Home Assistant) |
| Prompt and tools | The same voice prompt (about 5,500 characters) and 28 tools for every model: Assist intents, live context, scripts for weather, bins, meal plan and recipes, a web search tool |
| Round 1 | 13 questions. Topics: weather, bins, light states, sensor values, shopping list, clothing advice, meal plan, recipes, web search, music, one action. |
| Conversation | 5 questions in one conversation, including the half sentence "when is the next" |
| Round 2 | 12 other questions. Topics: rain, the date, sensor values, light states, shopping list, recipes, web search, current weather, leaf bags, one action. |

"Correct" means that the answer has the right facts. The test checked each answer against the real state of the house at that time. "Median" is the median time from the question to the full answer. The first question of each model prefilled the full prompt of about 6,000 tokens. Later questions used the prompt cache.

## Results

| Model | File | Round 1 | Round 2 | Median time | Result |
|---|---|---|---|---|---|
| Ling-3.0-tiny Q4_K_M, patched template | 4.82 GB | 13 of 13 | 10 of 10 | 5 to 7 s | Default. The most reliable. |
| Granite 4.0 H Tiny Q4_K_M | 4.23 GB | 13 of 13 | 8 of 10 | 3 s | Fastest. Wrong date and wrong rain answer in round 2. |
| Granite 4.0 H Tiny Q8_0 | 7.39 GB | 13 of 13 | 8 of 12 | 3.5 to 4 s | Not better than Q4. Wrong date, did not find exposed lights. |
| Qwen3.5-4B Q4_K_M | 2.74 GB | 13 of 13 | not run | 20.5 s | Correct, but 4 times slower than Ling |
| Gemma 4 E4B q4_0 (QAT) | 5.15 GB | 13 of 13 | not run | 26.5 s | Correct, but 5 times slower than Ling |
| LFM2.5-8B-A1B Q4_K_M | 5.16 GB | 11 of 13 | not run | 14 s | Answers in markdown lists, one tool loop (169 s) |
| Granite 4.2 3B Q4_K_M | 2.24 GB | most correct | stopped | 50 to 265 s | Far too slow |
| MiniCPM5-1B Q4_K_M | 0.69 GB | 4 of 13 | stopped | 15 s | Wrong tools (a shopping list action for the weather), invented facts. Removed from the image. |
| MiniCPM5-2B Q4_K_M | 1.56 GB | 12 of 13 | 11 of 12 | 15 to 20 s | Good answers, but 3 times slower than Ling. For questions about the shopping list, it added items to it. Removed from the image. |

Round 2 ran 12 questions. For Ling and Granite Q4, two of them changed between the runs, so only 10 are compared.

## What the results mean

Speed on a CPU depends on the number of active parameters per token, not on the file size:

| Model | Active parameters per token | Type |
|---|---|---|
| Granite 4.0 H Tiny | about 1B | MoE, mostly Mamba-2 layers |
| Ling-3.0-tiny | 1.3B | MoE |
| LFM2.5-8B-A1B | about 1.5B | MoE |
| Granite 4.2 3B | 3B | Dense, with thinking |
| Qwen3.5-4B | 4B | Dense, with thinking |
| Gemma 4 E4B | about 4B | Dense |
| MiniCPM5-2B | 2B | Dense, with thinking |

A dense model with 3B or more active parameters needs 20 s or more for an answer on this CPU. That is too slow for voice.

MiniCPM5-2B ranks above Granite 4.2 3B and Gemma 4 E2B in public benchmarks, and its answers were good. But all its 2B parameters are active for each token, so it needed 15 to 20 s per answer. It also added items to the shopping list twice, for questions that only asked what is on it. MiniCPM5-1B did think, and llama.cpp read its tool calls correctly. It chose wrong tools in most questions. With 28 tools and a long device list, the 1B model loses track.

A fast model is not enough. Granite 4.0 H Tiny was the fastest, but it gave answers without checking: a date from 2023, rain that was not in the forecast, and "already off" for a light that it never switched. Ling checked with its tools first and was correct in every question.

## Models that were not tested

| Model | Reason |
|---|---|
| Granite 4.0 H Small (32B MoE, 9B active) | The smallest quant (Q2_K, 11.8 GB) does not fit next to the voice container in the free memory. 9B active parameters are also slow on a CPU. |
| Granite 4.2 8B and 30B | Dense. The 3B was already far too slow. |
| Fine-tunes of Qwen 2.5 or Qwen 3 (for example Home-LLM models) | Older base models. The newer Qwen3.5-4B was already 4 times slower than Ling. |

## Problems found on the way

| Problem | Fix |
|---|---|
| With thinking on, Ling sometimes wrote its answer after a tool result into an open `<think>` block. llama.cpp then returned no answer, and Home Assistant gave up with "Unable to get response". | The image ships a patched Ling template (`templates/ling3.jinja`), see [llm.md](llm.md#patched-chat-template-for-ling). |
| In long conversations, Ling stopped thinking, because the template marks every earlier answer with an empty thinking block. | Not changed. With thinking in long conversations, the answers took much longer. |
| Small models put the year of their training data into web search queries ("World Cup 2022"). | The search tool replaces an older year with the current year in questions about recent events. |
| Models chose the speaker of a voice satellite or a TV box as the music player. | Expose only the music players of the rooms to Assist. |
