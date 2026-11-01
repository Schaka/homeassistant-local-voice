# Web search and YouTube search

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
| The name or handle of a channel, for example "Linus Tech Tips" or "mkbhd" | The newest long video of the channel. Its uploads playlist `UULF...` has no Shorts. | `channel_latest` |
| A genre or mood: it starts with "some" or "any", or it ends with "music", "mix", "radio", "playlist" or "beats" | The first livestream or video of at least 20 minutes | `live` or `mix` |
| Anything else | The first video that is no Short and no upcoming premiere | `video` or `live` |

If nothing fits, the answer is 404 (`no_result`). If YouTube fails, the answer is 502. A change of the YouTube API needs a fix in `src/youtube.rs`. Its tests use real answers in `tests/fixtures/`.

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
