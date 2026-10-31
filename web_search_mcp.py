#!/usr/bin/env python3
"""Small MCP server (Streamable HTTP) with one tool: web_search.

The conversation agent of a voice assistant is often a small local model on a
CPU. Every tool definition and every token of a tool result costs prefill time
(about 100 tokens per second for Ling-3.0-tiny on a CPU). Generic search MCP
servers offer several tools with long schemas and return whole pages. This
server offers one short tool and returns only a few trimmed results, plus the
direct answer or infobox when SearXNG has one.

It speaks the JSON-response variant of the MCP Streamable HTTP transport
(POST /mcp, no server-sent events), which the Home Assistant "Model Context
Protocol" integration supports. It uses only the Python standard library.

  web_search_mcp.py --searxng http://127.0.0.1:8888 --port 8090
"""
import argparse
import json
import logging
import urllib.parse
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_LOGGER = logging.getLogger("web-search-mcp")
PROTOCOL_VERSIONS = ("2025-06-18", "2025-03-26", "2024-11-05")
SNIPPET_CHARS = 300

TOOL = {
    "name": "web_search",
    "description": (
        "Searches the internet. Use it for news, current events, facts that "
        "can change, and general knowledge you are not sure about. Returns a "
        "few short results with their source."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "What to search for."},
            "max_results": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5,
                "description": "Number of results. Default 3.",
            },
        },
        "required": ["query"],
    },
}


def _trim(text, limit=SNIPPET_CHARS):
    text = " ".join((text or "").split())
    return text if len(text) <= limit else text[: limit - 1].rsplit(" ", 1)[0] + "…"


class Searcher:
    def __init__(self, base_url, language):
        self._base = base_url.rstrip("/")
        self._language = language

    def search(self, query, max_results):
        params = {"q": query, "format": "json", "safesearch": "1"}
        if self._language:
            params["language"] = self._language
        url = f"{self._base}/search?{urllib.parse.urlencode(params)}"
        # SearXNG's bot detection logs an error for requests without these.
        req = urllib.request.Request(url, headers={"X-Real-IP": "127.0.0.1", "X-Forwarded-For": "127.0.0.1"})
        with urllib.request.urlopen(req, timeout=20) as resp:
            data = json.load(resp)
        lines = []
        for answer in data.get("answers") or []:
            text = answer.get("answer") if isinstance(answer, dict) else answer
            if text:
                lines.append(f"Direct answer: {_trim(str(text))}")
        for box in (data.get("infoboxes") or [])[:1]:
            if box.get("content"):
                lines.append(f"{box.get('infobox', 'Info')}: {_trim(box['content'])}")
        for i, res in enumerate((data.get("results") or [])[:max_results], 1):
            host = urllib.parse.urlparse(res.get("url", "")).netloc.removeprefix("www.")
            date = (res.get("publishedDate") or "")[:10]
            meta = ", ".join(x for x in (host, date) if x)
            lines.append(f"{i}. {_trim(res.get('title'), 120)} ({meta}): {_trim(res.get('content'))}")
        return "\n".join(lines) or "No results."


class Handler(BaseHTTPRequestHandler):
    searcher: Searcher = None
    server_version = "web-search-mcp/1.0"

    def log_message(self, fmt, *args):
        _LOGGER.debug("%s " + fmt, self.address_string(), *args)

    def _send(self, status, body=None, headers=None):
        payload = json.dumps(body).encode() if body is not None else b""
        self.send_response(status)
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        if body is not None:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path.rstrip("/") == "/health":
            self._send(200, {"status": "ok"})
        else:
            # No server-initiated stream: the spec allows 405 here.
            self._send(405, headers={"Allow": "POST, DELETE"})

    def do_DELETE(self):
        self._send(200)

    def do_POST(self):
        if urllib.parse.urlparse(self.path).path.rstrip("/") != "/mcp":
            self._send(404)
            return
        try:
            length = int(self.headers.get("Content-Length") or 0)
            message = json.loads(self.rfile.read(length) or b"null")
        except (ValueError, OSError):
            self._send(400, {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32700, "message": "Parse error"}})
            return
        batch = isinstance(message, list)
        replies = [r for r in map(self._handle, message if batch else [message]) if r]
        headers = {}
        if not batch and isinstance(message, dict) and message.get("method") == "initialize":
            headers["Mcp-Session-Id"] = uuid.uuid4().hex
        if not replies:
            self._send(202, headers=headers)  # only notifications or responses
        else:
            self._send(200, replies if batch else replies[0], headers)

    def _handle(self, msg):
        if not isinstance(msg, dict) or "method" not in msg:
            return None
        method, msg_id = msg["method"], msg.get("id")
        if msg_id is None:
            return None  # notification, e.g. notifications/initialized
        params = msg.get("params") or {}
        try:
            result = self._dispatch(method, params)
        except KeyError:
            return {"jsonrpc": "2.0", "id": msg_id,
                    "error": {"code": -32601, "message": f"Method not found: {method}"}}
        return {"jsonrpc": "2.0", "id": msg_id, "result": result}

    def _dispatch(self, method, params):
        if method == "initialize":
            asked = params.get("protocolVersion")
            return {
                "protocolVersion": asked if asked in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0],
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": "web-search", "version": "1.0"},
            }
        if method == "ping":
            return {}
        if method == "tools/list":
            return {"tools": [TOOL]}
        if method == "tools/call":
            if params.get("name") != TOOL["name"]:
                return {"isError": True,
                        "content": [{"type": "text", "text": f"Unknown tool {params.get('name')}"}]}
            args = params.get("arguments") or {}
            query = str(args.get("query") or "").strip()
            if not query:
                return {"isError": True, "content": [{"type": "text", "text": "query is empty"}]}
            try:
                count = min(max(int(args.get("max_results") or 3), 1), 5)
            except (TypeError, ValueError):
                count = 3
            try:
                text = self.searcher.search(query, count)
            except Exception as err:  # noqa: BLE001 - report every failure to the model
                _LOGGER.warning("search failed for %r: %s", query, err)
                return {"isError": True, "content": [{"type": "text", "text": f"Search failed: {err}"}]}
            _LOGGER.info("search %r -> %d chars", query, len(text))
            return {"content": [{"type": "text", "text": text}]}
        raise KeyError(method)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--searxng", default="http://127.0.0.1:8888")
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8090)
    parser.add_argument("--language", default="", help="SearXNG language, e.g. en or de. Empty: all")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    Handler.searcher = Searcher(args.searxng, args.language)
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    _LOGGER.info("MCP endpoint on http://%s:%d/mcp, SearXNG at %s", args.host, args.port, args.searxng)
    server.serve_forever()


if __name__ == "__main__":
    main()
