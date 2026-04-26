#!/usr/bin/env python3
"""
LLM Token Meter — Python-based LLM token usage collector for comparison with honeybeepf-llm.

Wraps LLM API calls via httpx, extracts token usage from responses (same fields as
honeybeepf-llm's eBPF-based collection), and exports identical OTel metrics:

  - llm_requests_total   (counter)   attrs: model, status
  - llm_tokens_total     (counter)   attrs: model, status, token_type
  - llm_latency_seconds  (histogram) attrs: model, status

Usage:
    export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
    export OPENAI_API_KEY=sk-...          # optional, per provider
    export ANTHROPIC_API_KEY=sk-ant-...   # optional
    export GEMINI_API_KEY=...             # optional
    python llm_token_meter.py
"""

import json
import os
import sys
import time
from dataclasses import dataclass, field
from typing import Optional

import httpx
from opentelemetry import metrics
from opentelemetry.exporter.otlp.proto.grpc.metric_exporter import OTLPMetricExporter
from opentelemetry.metrics import Observation
from opentelemetry.sdk.metrics import MeterProvider
from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
from opentelemetry.sdk.resources import Resource


# ---------------------------------------------------------------------------
# Provider config — mirrors honeybeepf-llm ProviderRegistry
# ---------------------------------------------------------------------------

@dataclass
class ResponseConfig:
    usage_path: str = "usage"
    prompt_tokens: str = "prompt_tokens"
    completion_tokens: str = "completion_tokens"
    thoughts_tokens: Optional[str] = None
    model_path: str = "model"


@dataclass
class ProviderConfig:
    name: str
    base_url: str
    default_path: str
    response: ResponseConfig = field(default_factory=ResponseConfig)
    auth_header: str = "Authorization"
    auth_prefix: str = "Bearer "
    api_key_env: str = ""


PROVIDERS: dict[str, ProviderConfig] = {
    "openai": ProviderConfig(
        name="openai",
        base_url="https://api.openai.com",
        default_path="/v1/chat/completions",
        response=ResponseConfig(
            usage_path="usage",
            prompt_tokens="prompt_tokens",
            completion_tokens="completion_tokens",
            thoughts_tokens="completion_tokens_details.reasoning_tokens",
            model_path="model",
        ),
        api_key_env="OPENAI_API_KEY",
    ),
    "anthropic": ProviderConfig(
        name="anthropic",
        base_url="https://api.anthropic.com",
        default_path="/v1/messages",
        response=ResponseConfig(
            usage_path="usage",
            prompt_tokens="input_tokens",
            completion_tokens="output_tokens",
            model_path="model",
        ),
        auth_header="x-api-key",
        auth_prefix="",
        api_key_env="ANTHROPIC_API_KEY",
    ),
    "gemini": ProviderConfig(
        name="gemini",
        base_url="https://generativelanguage.googleapis.com",
        default_path="/v1beta/models/{model}:generateContent",
        response=ResponseConfig(
            usage_path="usageMetadata",
            prompt_tokens="promptTokenCount",
            completion_tokens="candidatesTokenCount",
            thoughts_tokens="thoughtsTokenCount",
            model_path="modelVersion",
        ),
        api_key_env="GEMINI_API_KEY",
    ),
}


# ---------------------------------------------------------------------------
# Usage extraction — same logic as honeybeepf-llm's get_nested_value + parse_usage
# ---------------------------------------------------------------------------

@dataclass
class UsageInfo:
    prompt_tokens: int
    completion_tokens: int
    thoughts_tokens: Optional[int] = None
    model: Optional[str] = None


def _get_nested(data: dict, dotted_path: str):
    current = data
    for key in dotted_path.split("."):
        if not isinstance(current, dict):
            return None
        current = current.get(key)
        if current is None:
            return None
    return current


def parse_usage(response_json: dict, config: ResponseConfig) -> Optional[UsageInfo]:
    usage_obj = _get_nested(response_json, config.usage_path)
    if usage_obj is None:
        return None

    prompt = _get_nested(usage_obj, config.prompt_tokens)
    completion = _get_nested(usage_obj, config.completion_tokens)
    if prompt is None or completion is None:
        return None

    thoughts = None
    if config.thoughts_tokens:
        thoughts = _get_nested(usage_obj, config.thoughts_tokens)
        if thoughts is not None:
            thoughts = int(thoughts)

    model = _get_nested(response_json, config.model_path)

    return UsageInfo(
        prompt_tokens=int(prompt),
        completion_tokens=int(completion),
        thoughts_tokens=thoughts,
        model=str(model) if model else None,
    )


# ---------------------------------------------------------------------------
# OTel metrics — identical names/attributes to honeybeepf-llm telemetry.rs
# ---------------------------------------------------------------------------

class TokenMeter:
    def __init__(self, endpoint: str):
        resource = Resource.create({
            "service.name": "honeybeepf-llm-python",
            "telemetry.sdk.language": "python",
        })
        exporter = OTLPMetricExporter(endpoint=endpoint, insecure=True)
        reader = PeriodicExportingMetricReader(exporter, export_interval_millis=30_000)
        self._provider = MeterProvider(resource=resource, metric_readers=[reader])
        metrics.set_meter_provider(self._provider)

        meter = metrics.get_meter("honeybeepf-llm")

        self._requests_total = meter.create_counter(
            name="llm_requests_total",
            description="Total number of LLM requests",
            unit="requests",
        )
        self._tokens_total = meter.create_counter(
            name="llm_tokens_total",
            description="Total number of LLM tokens processed",
            unit="tokens",
        )
        self._latency_seconds = meter.create_histogram(
            name="llm_latency_seconds",
            description="Latency of LLM requests",
            unit="s",
        )

    def record(self, usage: UsageInfo, latency_secs: float, is_error: bool = False):
        model = usage.model or "unknown"
        status = "error" if is_error else "success"
        base_attrs = {"model": model, "status": status}

        self._requests_total.add(1, base_attrs)
        self._latency_seconds.record(latency_secs, base_attrs)

        if not is_error:
            self._tokens_total.add(
                usage.prompt_tokens, {**base_attrs, "token_type": "prompt"}
            )
            self._tokens_total.add(
                usage.completion_tokens, {**base_attrs, "token_type": "completion"}
            )

    def shutdown(self):
        self._provider.shutdown()


# ---------------------------------------------------------------------------
# LLM client — thin wrapper that calls provider APIs and records metrics
# ---------------------------------------------------------------------------

class LLMClient:
    def __init__(self, meter: TokenMeter):
        self._meter = meter
        self._http = httpx.Client(timeout=120)

    def call(
        self,
        provider_name: str,
        body: dict,
        *,
        model_override: Optional[str] = None,
        path_override: Optional[str] = None,
    ) -> dict:
        config = PROVIDERS.get(provider_name)
        if config is None:
            raise ValueError(f"Unknown provider: {provider_name}. Available: {list(PROVIDERS)}")

        api_key = os.environ.get(config.api_key_env, "")
        if not api_key:
            raise EnvironmentError(f"Set {config.api_key_env} to use {provider_name}")

        path = path_override or config.default_path
        if "{model}" in path:
            m = model_override or body.get("model", "")
            path = path.replace("{model}", m)

        url = config.base_url.rstrip("/") + "/" + path.lstrip("/")

        headers = {"Content-Type": "application/json"}
        if config.auth_header == "x-api-key":
            headers[config.auth_header] = f"{config.auth_prefix}{api_key}"
        elif provider_name == "gemini":
            url += f"?key={api_key}"
        else:
            headers[config.auth_header] = f"{config.auth_prefix}{api_key}"

        if provider_name == "anthropic":
            headers["anthropic-version"] = "2023-06-01"

        t0 = time.monotonic()
        try:
            resp = self._http.post(url, content=json.dumps(body), headers=headers)
            latency = time.monotonic() - t0
            resp_json = resp.json()
        except Exception as exc:
            latency = time.monotonic() - t0
            self._meter.record(
                UsageInfo(0, 0, model=body.get("model")), latency, is_error=True
            )
            raise RuntimeError(f"Request to {provider_name} failed: {exc}") from exc

        if "error" in resp_json:
            self._meter.record(
                UsageInfo(0, 0, model=body.get("model")), latency, is_error=True
            )
            print(f"[ERROR] {provider_name}: {resp_json['error']}", file=sys.stderr)
            return resp_json

        usage = parse_usage(resp_json, config.response)
        if usage is None:
            print(f"[WARN] Could not parse usage from {provider_name} response", file=sys.stderr)
            usage = UsageInfo(0, 0, model=body.get("model"))

        self._meter.record(usage, latency)

        thoughts_str = f", Thoughts: {usage.thoughts_tokens}" if usage.thoughts_tokens else ""
        print(
            f"[LLM] {provider_name} | Model: {usage.model} | "
            f"Latency: {latency:.2f}s | "
            f"Tokens: {usage.prompt_tokens + usage.completion_tokens} "
            f"(Prompt: {usage.prompt_tokens}, Compl: {usage.completion_tokens}{thoughts_str})"
        )
        return resp_json

    def close(self):
        self._http.close()


# ---------------------------------------------------------------------------
# Demo / test harness
# ---------------------------------------------------------------------------

def _demo():
    endpoint = os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT", "")
    if not endpoint:
        print("OTEL_EXPORTER_OTLP_ENDPOINT not set. Metrics export disabled.", file=sys.stderr)
        print("Set it to e.g. http://localhost:4317 to export metrics.", file=sys.stderr)
        sys.exit(1)

    meter = TokenMeter(endpoint)
    client = LLMClient(meter)

    test_cases = []

    if os.environ.get("OPENAI_API_KEY"):
        test_cases.append(("openai", {
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "Say hello in one word."}],
            "max_tokens": 10,
        }))

    if os.environ.get("ANTHROPIC_API_KEY"):
        test_cases.append(("anthropic", {
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "Say hello in one word."}],
            "max_tokens": 10,
        }))

    if os.environ.get("GEMINI_API_KEY"):
        test_cases.append(("gemini", {
            "model": "gemini-2.0-flash",
            "contents": [{"parts": [{"text": "Say hello in one word."}]}],
        }))

    if not test_cases:
        print("No API keys set. Set at least one of:", file=sys.stderr)
        print("  OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY", file=sys.stderr)
        meter.shutdown()
        sys.exit(1)

    print(f"Running {len(test_cases)} test call(s)...\n")
    for provider_name, body in test_cases:
        try:
            client.call(provider_name, body)
        except Exception as exc:
            print(f"[FAIL] {provider_name}: {exc}", file=sys.stderr)
        print()

    print("Flushing metrics (waiting for export interval)...")
    client.close()
    meter.shutdown()
    print("Done.")


if __name__ == "__main__":
    _demo()
