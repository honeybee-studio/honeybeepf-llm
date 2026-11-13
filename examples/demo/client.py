#!/usr/bin/env python3
"""Demo LLM client: `client.py <response>[,<response>...] <seconds>` sends one call about every <seconds>,
cycling through the mock's responses. It trusts the demo CA through SSL_CERT_FILE and uses fake keys."""
import http.client
import itertools
import random
import sys
import time

ROUTES = {
    "openai": ("api.openai.com", "/v1/chat/completions", ("Authorization", "Bearer sk-demo-not-a-real-key"),
               '{"model":"gpt-5.4-mini","messages":[{"role":"user","content":"summarize this support ticket"}]}'),
    "anthropic": ("api.anthropic.com", "/v1/messages", ("x-api-key", "sk-ant-demo-not-a-real-key"),
                  '{"model":"claude-sonnet-4-5","max_tokens":512,'
                  '"messages":[{"role":"user","content":"draft a customer reply"}]}'),
}

names = sys.argv[1].split(",")
every = float(sys.argv[2])
for name in itertools.cycle(names):
    host, path, (header, key), body = ROUTES[name.split("-")[0]]
    try:
        c = http.client.HTTPSConnection(host, 443, timeout=10)
        c.request("POST", path, body, {"X-Demo-Response": name, header: key, "Content-Type": "application/json"})
        r = c.getresponse()
        # Read the body: the agent reads usage from the response as the app reads it.
        print(f"{name} -> HTTP {r.status}, {len(r.read())} B", flush=True)
        c.close()
    except OSError as e:
        print(f"{name} -> {e}", flush=True)
    time.sleep(every * random.uniform(0.6, 1.4))
