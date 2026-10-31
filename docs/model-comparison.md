# Model comparison

This page shows which small models work as the conversation agent of Home Assistant on a CPU, and which do not. Read it before you try another model: most of the tested models were too slow or not reliable enough.

All results are from 2026-10-31.

## Recommendations

| Rank | Model | `LLM_FAMILY` | Correct answers | Median time | Use it when |
|---|---|---|---|---|---|
| 1 | Ling-3.0-tiny Q4_K_M | `ling3` | 23 of 23 | 5 to 7 s | Default. Fast and reliable on a CPU. |
| 2 | MiniCPM5-2B Q4_K_M | `minicpm5` | 24 of 25 | 15 to 17 s | You want a smaller file (1.56 GB) and accept a slower answer. For a question about the shopping list, it sometimes adds an item to it. |
| 3 | LFM3-1.7B Q4_K_M | `lfm3` | 25 of 25 | 23 to 26 s | You have little memory (1.1 GB file). Keep its thinking on: without thinking, it failed often. |
| 4 | Granite 4.0 H Tiny Q4_K_M | `granite4` | 21 of 23 | 3 s | Speed matters more than reliability. It sometimes answers without checking (a wrong date, a wrong rain forecast). |
| 5 | Qwen3.5-4B Q4_K_M | `qwen35` | 13 of 13 (round 1 only) | 20.5 s | You prefer Qwen. Correct, but slow on a CPU. |

Set the temperature and top-p of each model in the Home Assistant agent, see [llm.md](llm.md#set-the-sampling-in-home-assistant). The other models in the results below are not supported by the image.

## How the models were tested

| Item | Value |
|---|---|
| Hardware | Intel Core i5-12400T, 6 threads, CPU only (`--device none`), 32 GB RAM |
| Server | llama-server b11966 from this image, context 32k |
| Client | Home Assistant 2026.10.4, llama.cpp integration. Temperature and top-p: 0.7 and 1.0 (the Home Assistant defaults), unless the table names other values. |
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
| Gemma 4 E2B q4_0 (QAT) | 3.35 GB | stopped after 7 | not run | 9 to 52 s | Said the office light was off while it was on. Stopped. |
| LFM2.5-8B-A1B Q4_K_M | 5.16 GB | 11 of 13 | not run | 14 s | Answers in markdown lists, one tool loop (169 s) |
| Granite 4.2 3B Q4_K_M | 2.24 GB | most correct | stopped | 50 to 265 s | Far too slow |
| MiniCPM5-1B Q4_K_M, 0.7 and 1.0 | 0.69 GB | 4 of 13 | stopped | 15 s | Wrong tools (a shopping list action for the weather), invented facts. Not recommended. |
| MiniCPM5-1B Q4_K_M, 0.9 and 0.95 (model card) | 0.69 GB | 7 of 13 | not run | 16 s | Better, but still "I don't have access" for the weather and the World Cup, markdown lists, and no action for "turn on the office light". Not recommended. |
| MiniCPM5-2B Q4_K_M, 0.7 and 1.0 | 1.56 GB | 12 of 13 | 11 of 12 | 15 to 20 s | Good answers. Added items to the shopping list for two questions about it. |
| MiniCPM5-2B Q4_K_M, 0.9 and 0.95 (model card) | 1.56 GB | 13 of 13 | 11 of 12 | 15 to 17 s | Same quality. Still added milk to the shopping list for "do we have milk on the shopping list". |
| LFM3-1.7B Q4_K_M, 0.1, thinking on (model card) | 1.10 GB | 13 of 13 | 12 of 12 | 23 to 26 s | All correct, short answers. Thinks for about 800 characters per step, so it is slow. |
| LFM3-1.7B Q4_K_M, 0.1, thinking off | 1.10 GB | 10 of 13 | 8 of 12 | 20 s | Five times "Unable to get response", answers that start with its own notes, items added to the shopping list. Its model card warns that thinking off works inconsistently. |

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
| LFM3-1.7B | 1.7B | Dense, mostly convolution layers, with thinking |

A dense model with 3B or more active parameters needs 20 s or more for an answer on this CPU. That is too slow for voice.

MiniCPM5-2B and LFM3-1.7B rank above Granite 4.2 3B and Gemma 4 E2B in public benchmarks, and their answers were good. But they are dense and think before each step, so they needed 15 to 26 s per answer. LFM3 generates as fast as Ling (about 30 tokens/s), but it thinks for about 800 characters per step. It also added items to the shopping list twice, for questions that only asked what is on it. MiniCPM5-1B did think, and llama.cpp read its tool calls correctly. It chose wrong tools in most questions. With 28 tools and a long device list, the 1B model loses track.

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
