# Media index

The media index makes `/jellyfin/search` understand descriptions, typos and misheard titles. It is optional and off by default (`MEDIA_INDEX=on`).

| Request | Without the index | With the index |
|---|---|---|
| "spy family" | Spy x Family (title match) | Same |
| "lord of the rinks" | The Lord of the Rings (trigram match) | Same |
| "freeran" (misheard "Frieren") | Frieren: Beyond Journey's End (sound match) | Same |
| "that desert planet movie with the giant sand worms" | Nothing | Dune |
| "the spy family episode where they go to penguin park" | Nothing | Spy x Family, season 1 episode 12 |
| "breaking bad" (not in the library) | Nothing | Nothing |

## How a search works

The search runs in stages. Each stage answers only if it is sure, else the next one tries:

1. Title: the whole title, the title before a colon, or all spoken words in the title ([search.md](search.md#jellyfin-search)).
2. Trigrams: the share of common three-letter pieces between the query and a title is at least 0.5. This finds typos and small mishearings without a model.
3. Sound: the consonants of the query equal the consonants of exactly one title. "freeran", "fryren" and "Frieren" all become `frn`. STT often gets the vowels of unknown names wrong. This stage needs no model either.
4. Semantic: the media index. EmbeddingGemma 2 turns the query and every item (title, year, genres, actors, plot) into vectors, and the closest item wins if it passes the gate below.
5. Jellyfin's own search, for items that came after the cached title list. Only a title with a trigram similarity of at least 0.5 counts.

The answer names the stage in `match` (`title`, `fuzzy`, `sound`, `semantic` or `jellyfin`) and its score in `score`.

### The gate

The similarity scores of EmbeddingGemma 2 sit close together. For most items of a library they are between 0.65 and 0.8, also for items that do not fit the query. So the semantic stage needs two conditions:

1. The best score is at least `MEDIA_MIN_SCORE` (default 0.62).
2. The best score is at least `MEDIA_MIN_MARGIN` (default 0.05) above the mean score of the places 6 to 25. A title that is not in the library has no item that stands out like this.

A third rule applies to short queries that look like a title (four words or fewer, no words like "about", "with" or "movie"): the semantic winner must share letters with the query (trigram similarity 0.2, or the same start of its consonants). Without this rule, the model played a related work: "House of the Dragon" for "game of thrones", "Franklin & Bash" for "breaking bad".

Episodes rank only against episodes, and series and movies only against each other. Episodes take part only if the query asks for one ("episode", "the one where", "scene"). In one shared ranking, episodes pushed the right series down: a desert episode of "Planet Earth" beat "Dune" for "desert planet with giant sand worms". "Anime about a spy who adopts a girl" plays the next episode of Spy x Family. "The episode where they go to penguin park" plays that episode.

## How the index follows the library

1. At start, the server loads the saved index and compares it with the whole library. It embeds only items whose text is new or changed (an FNV-1a hash of the text decides).
2. Jellyfin's WebSocket sends `LibraryChanged` messages with the IDs of added, updated and removed items. The server applies them after 20 s without new messages.
3. Every 6 hours, and after each reconnect of the WebSocket, it compares the whole library again. This catches changes that the WebSocket missed, for example during a restart.

The index lives in `MEDIA_INDEX_DIR` (default `/models/llm/media-index`): `entries.json` with the items and `vectors.f32` with the vectors. A change of the model, of `EMBED_DIMS`, of `MEDIA_INDEX_EPISODES` or of the text layout builds a new index.

## Resources

| Item | Value |
|---|---|
| Model | EmbeddingGemma 2 text model, Q8_0, 310 MB. The server downloads it at the first start to `EMBED_MODEL`. |
| RAM | About 0.5 GB for the model and two contexts (queries and indexing), plus the vectors: 2 KB per item at 512 dimensions (33 MB for 16,000 items) |
| Disk | The model and the index files, about 350 MB together |
| Query | 30 to 90 ms for the semantic stage on 4 modern cores, about 0.2 s on a 4th-generation Core i5 (estimate). A title match takes 5 to 40 ms. A query that finds nothing takes about 0.8 s, because the last stage asks Jellyfin's own search. |
| First indexing | 8 items per second with 4 threads of a Core Ultra 7 270K: 16,389 items (362 series, 546 movies, 15,481 episodes) took 34 minutes. Expect 1 to 2 hours on older CPUs with the default threads. Later starts embed only changes (18 s for a full comparison without changes). |
| CPU priority | Indexing runs on its own thread at nice 10 with half of `EMBED_THREADS`, so voice requests and the LLM keep the CPU. |

The model runs inside `assistant-hub` through `libllama.so` of the image, on the CPU only. There is no second server process, and the GPU stays with STT and TTS.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `MEDIA_INDEX` | `off` | `on` builds and uses the index. Needs `JELLYFIN_URL` and `JELLYFIN_API_KEY`. |
| `MEDIA_INDEX_DIR` | `/models/llm/media-index` | Where the index is saved. Mount it writable. |
| `MEDIA_INDEX_EPISODES` | `on` | Also index episodes. `off` indexes only series and movies: about 2 minutes instead of 30 to 120 minutes for the first build, but no search for single episodes. |
| `EMBED_MODEL` | `/models/llm/embed/embeddinggemma-2-Q8_0.gguf` | The model file. It is downloaded from `EMBED_MODEL_URL` if it is missing. |
| `EMBED_MODEL_URL` | ggml-org EmbeddingGemma 2 Q8_0 on Hugging Face | Download address of the model |
| `EMBED_DIMS` | `512` | Vector size (64 to 768). EmbeddingGemma 2 is trained for shorter vectors. 512 was the most accurate in the evaluation below. |
| `EMBED_THREADS` | Number of CPUs, at most 4 | Threads for a query. Indexing uses half. |
| `MEDIA_MIN_SCORE`, `MEDIA_MIN_MARGIN` | `0.62`, `0.05` | The gate of the semantic stage |
| `LLAMA_LIB` | `/opt/llama.cpp/libllama.so` | llama.cpp library of the image |

## Routes

| Route | Purpose |
|---|---|
| `GET /media/status` | State (`loading_model`, `indexing` with progress, `ready`, `failed`), number of items per type, time of the last full comparison |
| `GET /media/search?q=...&limit=10` | The ranking of the index and the decision of the gate, for tests and other clients (for example a Jellyfin plugin later) |
| `GET /jellyfin/search?q=...` | The search with all stages, see [search.md](search.md#jellyfin-search) |

## Test it

The unit tests use a fake embedder and a fake Jellyfin server (`cargo test`). They cover the text of each item, saving and loading, full and live syncs, the WebSocket, the gate and every stage of the search.

`src/media/eval.rs` tests the whole search against a real library with the real model. It builds the index and runs labeled queries: exact titles, spoken titles, typos, descriptions, single episodes and titles that are not in the library. The comments in the file show the command. For your own library, write your own cases into a file and set `EVAL_CASES`.

## Measured accuracy

`src/media/eval.rs` on the author's library (2026-11-01), all stages, 512 dimensions:

| Category | Correct | Examples |
|---|---|---|
| Exact titles | 7 of 7 | "dune part two", "the bear" |
| Spoken titles | 8 of 8 | "spy family", "moana two", "harry potter" |
| Typos and mishearings | 16 of 16 | "spy famly", "freeran", "fryren", "atack on titen", "arcain" |
| Descriptions | 11 of 13 | "a chef running a sandwich shop in chicago" → The Bear |
| Single episodes | 2 of 3 | "the spy family episode with the dodgeball game" → S1E10 |
| Not in the library | 10 of 10 | "breaking bad", "game of thrones", "the matrix": nothing |
| Total | 54 of 57 | |

The three misses found nothing instead of a wrong title: the model ranked another work first, and the gate rejected it. With 768 dimensions, two descriptions played a wrong title (10 of 13). With 256 dimensions, 9 of 13 descriptions were right.

## Why this design

1. EmbeddingGemma 2 understood typos better than expected: in a test on a real library, it found the right title first for 17 of 18 queries with typos or descriptions.
2. Trigrams found a typo that the model missed ("lord of the rinks"). An even mix of both rankings was worse (11 of 18), because trigrams are noise for descriptions. So the stages run one after the other.
3. A classic full-text index (words split into tokens) does not help here: it misses typos and descriptions, and the title stage already covers exact words.
4. Meilisearch adds a server and gives typo tolerance only.

