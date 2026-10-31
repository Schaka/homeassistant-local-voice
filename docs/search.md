# Web search for the agent

The container can give the conversation agent a web search tool. It runs [SearXNG](https://github.com/searxng/searxng) (a private metasearch engine) and a small MCP server (`web_search_mcp.py`) in front of it. MCP (Model Context Protocol) is the standard way for Home Assistant to add tools from other programs to an agent.

## Turn it on

1. Set `SEARCH_ENABLED=on` and publish port 8090:

   ```bash
   docker run -d --name wyoming-voice \
     ... \
     -e SEARCH_ENABLED=on -p 8090:8090 \
     ghcr.io/schaka/homeassistant-local-voice:latest
   ```

2. In Home Assistant, go to Settings > Devices & services > Add Integration > Model Context Protocol.
3. Set the URL to `http://<host>:8090/mcp`.
4. Open the conversation agent and select Reconfigure. Add the new MCP server to the LLM APIs, next to "Assist".
5. Add a search rule to the prompt of the agent, for example: "For news, sports results or facts that can change, always call web_search first. Do not answer such questions from memory."

While the server runs, `http://<host>:8090/health` returns `{"status": "ok"}`.

## What the tool returns

The MCP server offers one tool, `web_search`, with the inputs `query` and `max_results` (1 to 5, default 3). It returns:

1. One line with the date of today. The line says that the results are newer than the knowledge of the model.
2. A direct answer or an infobox. Only some searches have one.
3. The results: title, source domain, date and a snippet of at most 300 characters.

A result has about 150 to 300 tokens. Generic search MCP servers offer several tools with long descriptions and can return whole web pages. On a CPU, that costs a small model many seconds of prefill per question.

The date line is there because small models trust their training data. In a test, Ling-3.0-tiny found "Spain 2026" in the results and still answered "Argentina 2022" until the tool result had that line.

## Variables

| Variable | Default | Purpose |
|---|---|---|
| `SEARCH_ENABLED` | `off` | `on` starts SearXNG and the MCP server |
| `SEARCH_MCP_PORT` | `8090` | Port of the MCP endpoint `/mcp` |
| `SEARCH_LANGUAGE` | `en` | Language of the results, for example `de`. Empty gives all languages. |

## Notes

1. SearXNG listens on 127.0.0.1 only. The image adds about 130 MB for it.
2. The MCP endpoint has no authentication. Do not publish port 8090 outside your LAN.
3. SearXNG gets a new random secret at each start, because it keeps no state.
4. Some search engines block SearXNG now and then, for example with a CAPTCHA. SearXNG then uses the other engines.
5. Each search shows in the container log as `web-search-mcp: search '<query>'`.
