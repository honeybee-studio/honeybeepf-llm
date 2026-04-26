import pytest
from fastapi.testclient import TestClient
from mock_server import app


@pytest.fixture
def client():
    return TestClient(app)


def test_chat_completions_returns_openai_format(client):
    body = {
        "model": "gpt-4o-mini",
        "messages": [{"role": "user", "content": "Hello"}],
    }
    resp = client.post("/v1/chat/completions", json=body)

    assert resp.status_code == 200
    data = resp.json()
    assert "id" in data
    assert data["object"] == "chat.completion"
    assert data["model"] == "gpt-4o-mini"
    assert len(data["choices"]) == 1
    assert data["choices"][0]["message"]["role"] == "assistant"

    usage = data["usage"]
    assert "prompt_tokens" in usage
    assert "completion_tokens" in usage
    assert "total_tokens" in usage
    assert usage["total_tokens"] == usage["prompt_tokens"] + usage["completion_tokens"]


def test_chat_completions_echoes_model(client):
    body = {
        "model": "claude-sonnet-4-20250514",
        "messages": [{"role": "user", "content": "Hi"}],
    }
    resp = client.post("/v1/chat/completions", json=body)
    assert resp.json()["model"] == "claude-sonnet-4-20250514"


def test_token_counts_scale_with_input(client):
    short = {"model": "m", "messages": [{"role": "user", "content": "Hi"}]}
    long_msg = {"model": "m", "messages": [{"role": "user", "content": "Hello " * 200}]}

    short_tokens = client.post("/v1/chat/completions", json=short).json()["usage"]["prompt_tokens"]
    long_tokens = client.post("/v1/chat/completions", json=long_msg).json()["usage"]["prompt_tokens"]

    assert long_tokens > short_tokens


def test_health_endpoint(client):
    resp = client.get("/health")
    assert resp.status_code == 200


def test_metrics_endpoint(client):
    client.post("/v1/chat/completions", json={
        "model": "m",
        "messages": [{"role": "user", "content": "Hi"}],
    })
    resp = client.get("/metrics")
    assert resp.status_code == 200
    data = resp.json()
    assert data["total_requests"] >= 1
