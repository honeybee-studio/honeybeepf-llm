#!/usr/bin/env python3
"""Mock LLM provider for the demo: answers OpenAI, Anthropic and Gemini calls over TLS with fixed usage.

The client picks the response with the `X-Demo-Response` header. Model names are real and priced, so the
cost dashboard fills in; no request ever leaves the cluster. Usage: mock.py <cert> <key> [port]
"""
import http.server
import json
import socketserver
import ssl
import sys


def sse(events):
    return b"".join(f"data: {e if isinstance(e, str) else json.dumps(e)}\n\n".encode() for e in events)


def openai(prompt, completion, cached=0):
    usage = {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion}
    if cached:
        usage["prompt_tokens_details"] = {"cached_tokens": cached}
    return {"id": "demo", "object": "chat.completion", "model": "gpt-5.4-mini", "usage": usage,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]}


def anthropic(model, inp, out, cache_read=0, **extra):
    usage = {"input_tokens": inp, "output_tokens": out,
             "cache_read_input_tokens": cache_read, "cache_creation_input_tokens": 0, **extra}
    return {"id": "demo", "type": "message", "model": model, "role": "assistant",
            "content": [{"type": "text", "text": "ok"}], "usage": usage}


GEMINI_USAGE = {"promptTokenCount": 2200, "candidatesTokenCount": 300, "totalTokenCount": 2500}
# A server-side fallback: claude-opus-5-5 declined before any output, claude-sonnet-4-5 answered.
# The declined attempt is billed under its own model, so it shows apart in the cost dashboard.
FALLBACK = anthropic("claude-sonnet-4-5", 1500, 400, iterations=[
    {"type": "message", "model": "claude-opus-5-5", "input_tokens": 3000, "output_tokens": 0,
     "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
    {"type": "fallback_message", "model": "claude-sonnet-4-5", "input_tokens": 1500, "output_tokens": 400,
     "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}])
FALLBACK["content"].insert(0, {"type": "fallback", "from": {"model": "claude-opus-5-5"},
                               "to": {"model": "claude-sonnet-4-5"},
                               "trigger": {"type": "refusal", "category": "frontier_llm"}})

RESPONSES = {
    "openai-json": ("application/json", json.dumps(openai(1800, 350)).encode()),
    "openai-cache": ("application/json", json.dumps(openai(2000, 200, cached=1600)).encode()),
    "openai-sse": ("text/event-stream", sse([
        {"model": "gpt-5.4-mini", "choices": [{"index": 0, "delta": {"content": "ok"}}]},
        {"model": "gpt-5.4-mini", "choices": [],
         "usage": {"prompt_tokens": 1800, "completion_tokens": 350, "total_tokens": 2150}},
        "[DONE]"])),
    "anthropic-json": ("application/json", json.dumps(anthropic("claude-sonnet-4-5", 1500, 400)).encode()),
    "anthropic-cache": ("application/json",
                        json.dumps(anthropic("claude-sonnet-4-5", 500, 300, cache_read=8000)).encode()),
    "anthropic-fallback": ("application/json", json.dumps(FALLBACK).encode()),
    "gemini-json": ("application/json", json.dumps({
        "candidates": [{"content": {"parts": [{"text": "ok"}], "role": "model"}, "finishReason": "STOP"}],
        "modelVersion": "gemini-2.5-flash", "usageMetadata": GEMINI_USAGE}).encode()),
    "gemini-sse": ("text/event-stream", sse([
        {"candidates": [{"content": {"parts": [{"text": "o"}], "role": "model"}}], "modelVersion": "gemini-2.5-flash"},
        {"candidates": [{"content": {"parts": [{"text": "k"}], "role": "model"}, "finishReason": "STOP"}],
         "modelVersion": "gemini-2.5-flash", "usageMetadata": GEMINI_USAGE}])),
}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        found = RESPONSES.get(self.headers.get("X-Demo-Response", ""))
        status, (ctype, body) = (200, found) if found else (404, ("application/json", b'{"error":"unknown"}'))
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        pass


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(sys.argv[1], sys.argv[2])
srv = Server(("0.0.0.0", int(sys.argv[3]) if len(sys.argv) > 3 else 8443), Handler)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
