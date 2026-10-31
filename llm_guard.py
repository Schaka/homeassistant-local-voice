#!/usr/bin/env python3
"""Small guard proxy in front of llama-server: no empty replies with thinking on.

Problem: with thinking on, a hybrid reasoning model (seen with Ling-3.0-tiny) now
and then writes its whole answer inside the thinking block and never closes it.
llama-server then returns the text only as `reasoning_content`, and the reply
has no `content` and no tool call. Home Assistant shows no thinking, so it sees
an empty reply, asks again, and gives up after 10 rounds with "Unable to get
response".

This proxy listens on the public port and forwards to llama-server on an
internal port. For POST /v1/chat/completions it reads the complete reply first.
If the reply has text or a tool call, the client gets it unchanged. If not, the
proxy sends the same request once more with thinking off
(`chat_template_kwargs.enable_thinking = false`) and returns that reply. So the
model still thinks in the normal case, and the client never gets an empty reply.

All other requests (models list, health, slots) pass through unchanged.

  llm_guard.py --listen 0.0.0.0:8080 --backend 127.0.0.1:8081
"""
import argparse
import http.client
import http.server
import json
import logging

_LOGGER = logging.getLogger("llm-guard")
_HOP_HEADERS = {"connection", "keep-alive", "transfer-encoding", "content-length", "host"}


def _reply_has_answer(body: bytes, streamed: bool) -> bool:
    """Return True if the reply has answer text or a tool call."""
    text = body.decode("utf-8", "replace")
    if not streamed:
        try:
            message = json.loads(text)["choices"][0]["message"]
        except (ValueError, KeyError, IndexError, TypeError):
            return True  # unknown format: do not interfere
        return bool((message.get("content") or "").strip() or message.get("tool_calls"))
    for line in text.splitlines():
        if not line.startswith("data: ") or line.strip() == "data: [DONE]":
            continue
        try:
            chunk = json.loads(line[6:])
        except ValueError:
            continue
        for choice in chunk.get("choices") or []:
            delta = choice.get("delta") or {}
            if (delta.get("content") or "").strip() or delta.get("tool_calls"):
                return True
        if chunk.get("error"):
            return True  # pass errors through unchanged
    return False


class GuardHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    backend = ("127.0.0.1", 8081)

    def log_message(self, fmt, *args):  # quiet: llama-server logs every request itself
        return

    def _send_upstream(self, body: bytes):
        conn = http.client.HTTPConnection(*self.backend, timeout=900)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in _HOP_HEADERS}
        headers["Content-Length"] = str(len(body))
        conn.request(self.command, self.path, body=body if body else None, headers=headers)
        return conn, conn.getresponse()

    def _relay_buffered(self, resp, payload: bytes):
        self.send_response(resp.status)
        for key, value in resp.getheaders():
            if key.lower() not in _HOP_HEADERS:
                self.send_header(key, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _relay_streaming(self, resp):
        self.send_response(resp.status)
        for key, value in resp.getheaders():
            if key.lower() not in _HOP_HEADERS:
                self.send_header(key, value)
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        while True:
            chunk = resp.read1(65536)
            if not chunk:
                break
            self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")

    def _handle(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        is_chat = self.command == "POST" and self.path.rstrip("/").endswith("/chat/completions")
        if not is_chat:
            conn, resp = self._send_upstream(body)
            try:
                self._relay_streaming(resp)
            finally:
                conn.close()
            return

        try:
            request = json.loads(body)
        except ValueError:
            request = None
        streamed = bool(isinstance(request, dict) and request.get("stream"))

        conn, resp = self._send_upstream(body)
        try:
            payload = resp.read()
        finally:
            conn.close()
        if resp.status != 200 or request is None or _reply_has_answer(payload, streamed):
            self._relay_buffered(resp, payload)
            return

        # Empty reply: the answer is stuck in the thinking block. Ask once more
        # without thinking, and give the client that reply.
        _LOGGER.warning("empty reply with thinking on, retrying once without thinking")
        kwargs = dict(request.get("chat_template_kwargs") or {})
        kwargs["enable_thinking"] = False
        request["chat_template_kwargs"] = kwargs
        conn, resp = self._send_upstream(json.dumps(request).encode())
        try:
            self._relay_streaming(resp)
        finally:
            conn.close()

    def do_GET(self):  # noqa: N802 - http.server naming
        self._handle()

    def do_POST(self):  # noqa: N802
        self._handle()

    def do_DELETE(self):  # noqa: N802
        self._handle()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--listen", default="0.0.0.0:8080")
    parser.add_argument("--backend", default="127.0.0.1:8081")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    host, port = args.listen.rsplit(":", 1)
    b_host, b_port = args.backend.rsplit(":", 1)
    GuardHandler.backend = (b_host, int(b_port))
    server = http.server.ThreadingHTTPServer((host, int(port)), GuardHandler)
    server.daemon_threads = True
    _LOGGER.info("guard on %s, llama-server at %s", args.listen, args.backend)
    server.serve_forever()


if __name__ == "__main__":
    main()
