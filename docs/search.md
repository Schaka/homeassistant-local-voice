# Web search, YouTube search and Jellyfin search

## Web search for the agent

The server can give the conversation agent a web search tool. It uses [SearXNG](https://github.com/searxng/searxng) (a private metasearch engine) as a Python library inside the Rust server. SearXNG's own web server never starts. Each search is a function call in the same process and takes about 0.6 to 1 s. MCP (Model Context Protocol) is the standard way for Home Assistant to add tools from other programs to an agent.

### Turn it on

1. Set `SEARCH_ENABLED=on`. Port 10300 is the only port.
2. In Home Assistant, go to Settings > Devices & services > Add Integration > Model Context Protocol.
3. Set the URL to `http://<host>:10300/mcp`.
4. Open the conversation agent and select Reconfigure. Add the new MCP server to the LLM APIs, next to "Assist".
5. Add a search rule to the prompt of the agent, for example: "For news, sports results or facts that can change, always call web_search first. Do not answer such questions from memory."

After a successful load of SearXNG, `GET /health` shows `"web_search": true`.

### What the tool returns

The MCP endpoint offers the tool `web_search` with the inputs `query` and `max_results` (1 to 5, default 3). It returns:

1. One line with the date of today. The line says that the results are newer than the knowledge of the model.
2. A direct answer or an infobox. Only some searches have one.
3. The results: title, source domain, date and a snippet of at most 300 characters.

A result has about 150 to 300 tokens. Generic search MCP servers offer several tools with long descriptions and can return whole web pages. On a CPU, that costs a small model many seconds of prefill per question.

Some queries ask for something recent, with words such as "last", "latest", "current" or "this year". If such a query names no year, the server adds the current year to it. Small models rarely do that themselves. Without the year, the search engines put general pages first.

The date line is there because small models trust their training data. In a test, Ling-3.0-tiny found "Spain 2026" in the results and still answered "Argentina 2022" until the tool result had that line.

### REST

`GET /web/search?q=<query>&max_results=3` returns the same search as JSON: `query` (as sent, with the year), `text` (what the tool returns) and `results` (title, URL, snippet, date).

## YouTube search

`GET /youtube/search?q=<query>` returns one video to play:

```json
{"kind": "channel_latest", "query": "linus tech tips", "video_id": "H3Cnyj0I62c",
 "title": "I Doubled My GPU's Memory…", "channel": "Linus Tech Tips",
 "url": "https://www.youtube.com/watch?v=H3Cnyj0I62c"}
```

It uses InnerTube, the API behind the YouTube website. It needs no key. It only searches. The app on the player plays the video, so no stream extraction is necessary. The rules for the pick:

| Query | Pick | `kind` |
|---|---|---|
| The name or handle of a channel, for example "Linus Tech Tips" or "mkbhd". Spoken numbers ("level one techs") and abbreviations with the same first letter and at least 70 % of the letters in order ("lvl1techs") also count. | The newest long video of the channel. Its uploads playlist `UULF...` has no Shorts. | `channel_latest` |
| A genre or mood: it starts with "some" or "any", or it ends with "music", "mix", "radio", "playlist" or "beats" | The first livestream or video of at least 20 minutes | `live` or `mix` |
| Anything else | The first video that is no Short and no upcoming premiere | `video` or `live` |

If nothing fits, the answer is 404 (`no_result`). If YouTube fails, the answer is 502. A change of the YouTube API needs a fix in `src/youtube.rs`. Its tests use real answers in `tests/fixtures/`.

## Jellyfin search

`GET /jellyfin/search?q=<query>[&devices=<names>][&user=<name>]` returns one episode or movie of your Jellyfin library to play:

```json
{"kind": "episode", "query": "spy family", "item_id": "<32 hex>",
 "title": "Spy x Family", "episode_title": "Will They Pass or Fail?", "season": 1, "episode": 5,
 "resume_seconds": 599, "user": "alex", "user_id": "<32 hex>", "server_id": "<32 hex>"}
```

| Query | Pick | `kind` |
|---|---|---|
| A series, for example "spy family" | The next episode of the user ("next up"), with the resume position. If the user watched everything, the first episode. | `episode` |
| A series with "season 2 episode 3" | That episode | `episode` |
| A movie, for example "dune part two" | The movie. Of a film series ("lord of the rings"), the oldest movie that the user did not watch. | `movie` |

The user decides the next episode and the resume position. The server takes the first that applies:

1. `user`, if the request names one.
2. The user of a Jellyfin session on one of `devices` (Jellyfin device names, comma-separated, for example `UGOOS-AM6,Kodi (CoreELEC)`), active in the last 15 minutes. A session that plays something wins. So the person who has Jellyfin open on the TV box continues their own series.
3. `JELLYFIN_USER`.

Jellyfin's own search needs the title words in order, so it does not find "Spy x Family" for "spy family". The server keeps the list of all series and movie titles (refreshed every 10 minutes, about 1 s for 900 titles) and matches the spoken words against it:

1. The whole title, with spoken numbers as digits ("dune part two" is "Dune: Part Two").
2. The title before a colon ("frieren" is "Frieren: Beyond Journey's End").
3. All words of the query are words of the title, and they are more than half of its words. The words "the", "x", "by", "and" and "of" do not count, because people and STT add or drop them ("spy by family").

If no title matches, the search goes on in stages: trigram similarity for typos ("lord of the rinks"), then the consonants for misheard vowels ("freeran" for "Frieren"), then the optional media index for descriptions and misheard titles, then Jellyfin's own search for items newer than the title list. Each later stage needs a minimum similarity, because Jellyfin's own search finds "Office Space" for "the office". [media-index.md](media-index.md) describes the stages. The answer names the stage in `match` and its score in `score`. If nothing matches, the answer is 404 (`no_result`).

The apps open the item by `item_id`: Wholphin with `wholphin://play?itemId=...&position=<ms>&userId=...&serverId=...`, the official Kodi add-on with `plugin://plugin.video.jellyfin/?mode=play&id=...`. [examples/home-assistant/scripts/play_jellyfin.yaml](../examples/home-assistant/scripts/play_jellyfin.yaml) uses both.

## Variables

| Variable | Default | Purpose |
|---|---|---|
| `SEARCH_ENABLED` | `off` | `on` loads SearXNG and offers `web_search` |
| `SEARCH_LANGUAGE` | `en` | Language of the results, for example `de`. Empty gives all languages. |
| `YOUTUBE_ENABLED` | `on` | `off` turns `/youtube/search` off |
| `YOUTUBE_LANGUAGE` | `en` | Language of titles (`hl`) |
| `YOUTUBE_REGION` | `DE` | Region of the results (`gl`) |
| `YOUTUBE_CLIENT_VERSION` | `2.20261030.00.00` | Web client version that the server reports to YouTube |

## Notes

1. SearXNG adds about 90 MB of memory. With `SEARCH_ENABLED=off`, the server does not load Python.
2. `/mcp`, `/web/search` and `/youtube/search` have no authentication. Do not publish port 10300 outside your LAN.
3. SearXNG gets a new random secret at each start, because it keeps no state.
4. Some search engines block SearXNG now and then, for example with a CAPTCHA. SearXNG then uses the other engines.
5. The container log shows each search as `search "<query>"` and each YouTube pick as `youtube "<query>" -> ...`.
