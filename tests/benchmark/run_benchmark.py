#!/usr/bin/env python3
import argparse
import asyncio
import json
import subprocess
import sys
import time
from dataclasses import dataclass

from load_generator import LoadGenerator, LoadProfile
from stats import compute_latency_stats, format_report


DIRECT_URL = "http://localhost:8080"
PROXY_URL = "http://localhost:4000"
PROXY_AUTH = {"Authorization": "Bearer sk-benchmark-key"}


@dataclass
class ScenarioConfig:
    name: str
    profile: LoadProfile
    description: str


SCENARIOS = {
    "steady": ScenarioConfig(
        name="Steady Load",
        profile=LoadProfile(rate_rps=10, duration_secs=300, concurrency=20),
        description="10 req/s for 5 minutes — measures sustained latency overhead",
    ),
    "burst": ScenarioConfig(
        name="Burst",
        profile=LoadProfile(concurrency=100, duration_secs=0),
        description="100 concurrent requests — measures throughput ceiling",
    ),
    "stress": ScenarioConfig(
        name="Stress",
        profile=LoadProfile(rate_rps=100, duration_secs=60, concurrency=50),
        description="100 req/s for 1 minute — finds breaking point",
    ),
    "quick": ScenarioConfig(
        name="Quick Smoke",
        profile=LoadProfile(rate_rps=5, duration_secs=10, concurrency=10),
        description="5 req/s for 10 seconds — fast validation",
    ),
}


def get_docker_stats(container_name: str) -> dict | None:
    try:
        out = subprocess.check_output(
            ["docker", "stats", "--no-stream", "--format",
             '{"cpu":"{{.CPUPerc}}","mem":"{{.MemUsage}}","net":"{{.NetIO}}"}',
             container_name],
            text=True, timeout=5,
        )
        return json.loads(out.strip())
    except Exception:
        return None


def check_services() -> dict[str, bool]:
    import httpx

    status = {}
    for name, url in [("mock-llm", f"{DIRECT_URL}/health"), ("litellm", f"{PROXY_URL}/health")]:
        try:
            r = httpx.get(url, timeout=3)
            status[name] = r.status_code == 200
        except Exception:
            status[name] = False
    return status


async def run_config(label: str, url: str, profile: LoadProfile, extra_headers: dict | None = None) -> dict:
    gen = LoadGenerator(base_url=url, timeout=30.0)
    if extra_headers:
        gen._extra_headers = extra_headers

    print(f"    [{label}] Running {profile.total_requests} requests...")
    result = await gen.run(profile)
    stats = compute_latency_stats(result.latencies, result.wall_time_secs, result.error_count)
    print(f"    [{label}] Done: {result.success_count} ok, {result.error_count} err, "
          f"p50={stats['p50']*1000:.1f}ms, p99={stats['p99']*1000:.1f}ms")
    return stats


async def run_scenario(scenario: ScenarioConfig, configs: list[str]) -> dict[str, dict]:
    print(f"\n{'='*60}")
    print(f"  Scenario: {scenario.name}")
    print(f"  {scenario.description}")
    print(f"{'='*60}")

    results = {}

    if "baseline" in configs:
        results["baseline"] = await run_config("Baseline", DIRECT_URL, scenario.profile)

    if "proxy" in configs:
        results["proxy"] = await run_config("LiteLLM Proxy", PROXY_URL, scenario.profile)

    if "ebpf" in configs:
        results["ebpf"] = await run_config("eBPF", DIRECT_URL, scenario.profile)

    return results


async def run_kill_test():
    print(f"\n{'='*60}")
    print(f"  Scenario: Kill Test")
    print(f"  Kill monitoring mid-load — measure service impact")
    print(f"{'='*60}")

    profile = LoadProfile(rate_rps=10, duration_secs=30, concurrency=10)

    print("\n  --- Proxy Kill Test ---")
    print("    Starting load through LiteLLM...")

    gen_proxy = LoadGenerator(base_url=PROXY_URL)
    proxy_task = asyncio.create_task(gen_proxy.run(profile))

    await asyncio.sleep(10)
    print("    Killing LiteLLM container...")
    subprocess.run(["docker", "compose", "stop", "litellm"], capture_output=True, cwd=".")
    await asyncio.sleep(2)

    proxy_result = await proxy_task
    proxy_stats = compute_latency_stats(
        proxy_result.latencies, proxy_result.wall_time_secs, proxy_result.error_count
    )

    print(f"    Proxy killed: {proxy_result.success_count} ok, {proxy_result.error_count} FAILED")
    print(f"    Error rate: {proxy_stats['error_rate']:.0%}")

    print("\n    Restarting LiteLLM...")
    subprocess.run(["docker", "compose", "start", "litellm"], capture_output=True, cwd=".")
    await asyncio.sleep(5)

    print("\n  --- eBPF Kill Test (simulated) ---")
    print("    eBPF is passive — killing it has zero service impact.")
    print("    Running same load direct (simulates eBPF daemon down)...")

    gen_direct = LoadGenerator(base_url=DIRECT_URL)
    direct_result = await gen_direct.run(profile)
    direct_stats = compute_latency_stats(
        direct_result.latencies, direct_result.wall_time_secs, direct_result.error_count
    )

    print(f"    eBPF killed: {direct_result.success_count} ok, {direct_result.error_count} FAILED")
    print(f"    Error rate: {direct_stats['error_rate']:.0%}")

    return {
        "proxy_kill": {
            "success": proxy_result.success_count,
            "errors": proxy_result.error_count,
            "error_rate": proxy_stats["error_rate"],
        },
        "ebpf_kill": {
            "success": direct_result.success_count,
            "errors": direct_result.error_count,
            "error_rate": direct_stats["error_rate"],
        },
    }


async def main():
    parser = argparse.ArgumentParser(description="honeybeepf-llm benchmark")
    parser.add_argument(
        "--scenario", choices=list(SCENARIOS.keys()) + ["all", "kill"],
        default="quick",
        help="Scenario to run (default: quick)",
    )
    parser.add_argument(
        "--configs", nargs="+", choices=["baseline", "proxy", "ebpf"],
        default=["baseline", "proxy", "ebpf"],
        help="Configurations to test",
    )
    parser.add_argument("--output", help="Save results to JSON file")
    args = parser.parse_args()

    print("Checking services...")
    status = check_services()
    for svc, ok in status.items():
        print(f"  {svc}: {'OK' if ok else 'NOT RUNNING'}")
    if not all(status.values()):
        print("\nRun: docker compose up -d")
        sys.exit(1)

    all_results = {}

    if args.scenario == "kill":
        all_results["kill"] = await run_kill_test()
    elif args.scenario == "all":
        for name, scenario in SCENARIOS.items():
            results = await run_scenario(scenario, args.configs)
            all_results[name] = results
            print(f"\n{format_report(results)}\n")
        all_results["kill"] = await run_kill_test()
    else:
        scenario = SCENARIOS[args.scenario]
        results = await run_scenario(scenario, args.configs)
        all_results[args.scenario] = results
        print(f"\n{format_report(results)}")

    # Resource usage snapshot
    print(f"\n{'='*60}")
    print("  Resource Usage (docker stats snapshot)")
    print(f"{'='*60}")
    for container in ["benchmark-mock-llm-1", "benchmark-litellm-1"]:
        stats = get_docker_stats(container)
        if stats:
            print(f"  {container}: CPU={stats['cpu']}, Mem={stats['mem']}")

    if args.output:
        with open(args.output, "w") as f:
            json.dump(all_results, f, indent=2)
        print(f"\nResults saved to {args.output}")

    print("\n--- Kill Test Summary ---")
    if "kill" in all_results:
        k = all_results["kill"]
        print(f"  Proxy killed:  {k['proxy_kill']['errors']} requests FAILED "
              f"({k['proxy_kill']['error_rate']:.0%} error rate)")
        print(f"  eBPF killed:   {k['ebpf_kill']['errors']} requests FAILED "
              f"({k['ebpf_kill']['error_rate']:.0%} error rate)")


if __name__ == "__main__":
    asyncio.run(main())
