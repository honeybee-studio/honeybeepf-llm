import asyncio
import os
import time
import uuid

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse

app = FastAPI()

RESPONSE_DELAY_MS = int(os.environ.get("MOCK_RESPONSE_DELAY_MS", "50"))
COMPLETION_TOKENS = int(os.environ.get("MOCK_COMPLETION_TOKENS", "30"))

_request_count = 0
_start_time = time.monotonic()


def _estimate_prompt_tokens(messages: list[dict]) -> int:
    text = " ".join(m.get("content", "") for m in messages if isinstance(m.get("content"), str))
    return max(1, len(text.split()) * 4 // 3)


@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    global _request_count
    _request_count += 1

    body = await request.json()
    model = body.get("model", "mock-model")
    messages = body.get("messages", [])
    prompt_tokens = _estimate_prompt_tokens(messages)
    completion_tokens = COMPLETION_TOKENS

    if RESPONSE_DELAY_MS > 0:
        await asyncio.sleep(RESPONSE_DELAY_MS / 1000.0)

    return JSONResponse({
        "id": f"chatcmpl-{uuid.uuid4().hex[:12]}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "This is a mock response for benchmarking.",
                },
                "finish_reason": "stop",
            }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    })


@app.get("/health")
async def health():
    return {"status": "ok"}


@app.get("/metrics")
async def metrics():
    elapsed = time.monotonic() - _start_time
    return {
        "total_requests": _request_count,
        "uptime_seconds": round(elapsed, 1),
        "requests_per_second": round(_request_count / elapsed, 2) if elapsed > 0 else 0,
    }


if __name__ == "__main__":
    import uvicorn
    port = int(os.environ.get("MOCK_SERVER_PORT", "8080"))
    uvicorn.run(app, host="0.0.0.0", port=port, log_level="warning")
