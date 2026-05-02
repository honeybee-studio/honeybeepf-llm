import argparse
import asyncio
import json
import subprocess
import sys

import httpx

from config import DIRECT_URL, PROXY_URL, CONTAINER_NAMES, SCENARIOS, Scenario
from load_generator import LoadGenerator, LoadProfile
from report import format_report
from stats import compute_latency_stats


def check_services() -> bool:
    all_ok = True
    for name, url in [("mock-llm", f"{DIRECT_URL}/health"), ("litellm", f"{PROXY_URL}/health")]:
        try:
            ok = httpx.get(url, timeout=3).status_code == 200
        except Exception:
            ok = False
        print(f"  {name}: {'OK' if ok else 'NOT RUNNING'}")
        if not ok:
            all_ok = False
    return all_ok


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


async def run_load(label: str, url: str, profile: LoadProfile) -> dict:
    gen = LoadGenerator(base_url=url)
    print(f"    [{label}] Running {profile.total_requests} requests...")
    result = await gen.run(profile)
    stats = compute_latency_stats(result.latencies, result.wall_time_secs, result.error_count)
    print(f"    [{label}] Done: {result.success_count} ok, {result.error_count} err, "
          f"p50={stats['p50']*1000:.1f}ms, p99={stats['p99']*1000:.1f}ms")
    return stats


async def run_scenario(scenario: Scenario) -> dict[str, dict]:
    print(f"\n{'='*60}")
    print(f"  Scenario: {scenario.name}")
    print(f"  {scenario.description}")
    print(f"{'='*60}")

    baseline = await run_load("Baseline", DIRECT_URL, scenario.profile)
    proxy = await run_load("LiteLLM Proxy", PROXY_URL, scenario.profile)
    return {"baseline": baseline, "proxy": proxy}


async def run_kill_test() -> dict:
    print(f"\n{'='*60}")
    print(f"  Scenario: Proxy Kill Test")
    print(f"  Kill LiteLLM mid-load — measure service impact")
    print(f"{'='*60}")

    profile = LoadProfile(rate_rps=10, duration_secs=30, concurrency=10)

    print("    Starting load through LiteLLM...")
    gen = LoadGenerator(base_url=PROXY_URL)
    task = asyncio.create_task(gen.run(profile))

    await asyncio.sleep(10)
    print("    Killing LiteLLM container...")
    subprocess.run(["docker", "compose", "stop", "litellm"], capture_output=True, cwd=".")
    await asyncio.sleep(2)

    result = await task
    stats = compute_latency_stats(result.latencies, result.wall_time_secs, result.error_count)

    print(f"    Result: {result.success_count} ok, {result.error_count} FAILED")
    print(f"    Error rate: {stats['error_rate']:.0%}")

    print("\n    Restarting LiteLLM...")
    subprocess.run(["docker", "compose", "start", "litellm"], capture_output=True, cwd=".")
    await asyncio.sleep(5)

    return {
        "success": result.success_count,
        "errors": result.error_count,
        "error_rate": stats["error_rate"],
    }


def parse_args():
    parser = argparse.ArgumentParser(description="honeybeepf-llm benchmark: Baseline vs LiteLLM Proxy")
    parser.add_argument(
        "--scenario",
        choices=list(SCENARIOS.keys()) + ["all", "kill"],
        default="quick",
    )
    parser.add_argument("--output", help="Save results to JSON file")
    return parser.parse_args()


async def run_all_scenarios(scenario_name: str) -> dict:
    results = {}

    if scenario_name == "kill":
        results["kill"] = await run_kill_test()
    elif scenario_name == "all":
        for name, scenario in SCENARIOS.items():
            r = await run_scenario(scenario)
            results[name] = r
            print(f"\n{format_report(r['baseline'], r['proxy'])}\n")
        results["kill"] = await run_kill_test()
    else:
        r = await run_scenario(SCENARIOS[scenario_name])
        results[scenario_name] = r
        print(f"\n{format_report(r['baseline'], r['proxy'])}")

    return results


def print_summary(results: dict):
    print(f"\n{'='*60}")
    print("  Resource Usage")
    print(f"{'='*60}")
    for container in CONTAINER_NAMES:
        stats = get_docker_stats(container)
        if stats:
            print(f"  {container}: CPU={stats['cpu']}, Mem={stats['mem']}")

    if "kill" in results:
        k = results["kill"]
        print(f"\n--- Kill Test Summary ---")
        print(f"  Proxy killed: {k['errors']} requests FAILED ({k['error_rate']:.0%} error rate)")


async def main():
    args = parse_args()

    print("Checking services...")
    if not check_services():
        print("\nRun: docker compose up -d")
        sys.exit(1)

    results = await run_all_scenarios(args.scenario)
    print_summary(results)

    if args.output:
        with open(args.output, "w") as f:
            json.dump(results, f, indent=2)
        print(f"\nResults saved to {args.output}")


if __name__ == "__main__":
    asyncio.run(main())
