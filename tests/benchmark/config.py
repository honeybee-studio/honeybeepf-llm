from dataclasses import dataclass

from load_generator import LoadProfile

DIRECT_URL = "http://localhost:8080"
PROXY_URL = "http://localhost:4000"
PROXY_API_KEY = "sk-benchmark-key"

CONTAINER_NAMES = ["benchmark-mock-llm-1", "benchmark-litellm-1"]


@dataclass
class Scenario:
    name: str
    profile: LoadProfile
    description: str


SCENARIOS: dict[str, Scenario] = {
    "quick": Scenario(
        name="Quick Smoke",
        profile=LoadProfile(rate_rps=5, duration_secs=10, concurrency=10),
        description="5 req/s for 10 seconds — fast validation",
    ),
    "steady": Scenario(
        name="Steady Load",
        profile=LoadProfile(rate_rps=10, duration_secs=300, concurrency=20),
        description="10 req/s for 5 minutes — measures sustained latency overhead",
    ),
    "burst": Scenario(
        name="Burst",
        profile=LoadProfile(concurrency=100),
        description="100 concurrent requests — measures throughput ceiling",
    ),
    "stress": Scenario(
        name="Stress",
        profile=LoadProfile(rate_rps=100, duration_secs=60, concurrency=50),
        description="100 req/s for 1 minute — finds breaking point",
    ),
}
