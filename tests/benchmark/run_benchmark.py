import argparse
import asyncio
import contextlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import httpx

from config import DIRECT_URL, PROXY_URL, CONTAINER_NAMES, SCENARIOS, Scenario
from load_generator import LoadGenerator, LoadProfile
from report import format_markdown, format_report
from stats import compute_latency_stats


def check_services() -> bool:
    all_ok = True
    for name, url in [("mock-llm", f"{DIRECT_URL}/health"), ("litellm", f"{PROXY_URL}/health")]:
        try:
            ok = httpx.get(url, timeout=3, verify=False).status_code == 200
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


AGENT_BOOT_WAIT_SECS = 3
AGENT_TERMINATE_TIMEOUT_SECS = 5


@contextlib.contextmanager
def ebpf_agent(binary_path: str):
    """Start the honeybeepf-llm eBPF agent, wait for it to attach probes, yield, then terminate."""
    env = {
        **os.environ,
        "OTEL_EXPORTER_OTLP_ENDPOINT": "http://localhost:4317",
        "BUILTIN_PROBES__LLM": "true",
        "RUST_LOG": "warn",
    }
    proc = subprocess.Popen(
        [binary_path],
        env=env,
        stderr=sys.stderr,
        stdout=subprocess.DEVNULL,
    )

    try:
        time.sleep(AGENT_BOOT_WAIT_SECS)
        if proc.poll() is not None:
            raise RuntimeError(f"eBPF agent exited early with code {proc.returncode}")
        yield proc
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=AGENT_TERMINATE_TIMEOUT_SECS)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


OTEL_OUTPUT_DIR = Path(__file__).parent / "otel-output"
OTEL_OUTPUT_FILE = OTEL_OUTPUT_DIR / "otel.jsonl"
KNOWN_OTEL_SOURCES = ["litellm", "honeybeepf-llm"]


def parse_otel_event_counts(path: Path) -> dict[str, int]:
    """Parse otel.jsonl and return event counts grouped by service.name."""
    counts: dict[str, int] = {}
    if not path.exists():
        return counts

    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            for key in ("resourceSpans", "resourceMetrics", "resourceLogs"):
                for rs in obj.get(key, []) or []:
                    attrs = rs.get("resource", {}).get("attributes", []) or []
                    svc = next(
                        (a.get("value", {}).get("stringValue")
                         for a in attrs
                         if a.get("key") == "service.name"),
                        None,
                    )
                    if svc:
                        counts[svc] = counts.get(svc, 0) + 1
    return counts


def print_otel_summary(counts: dict[str, int]):
    print(f"\n{'='*60}")
    print("  OTel events received (informational)")
    print(f"{'='*60}")
    if not counts:
        print("  (no events found in otel-output/otel.jsonl)")
        return
    for source in KNOWN_OTEL_SOURCES:
        n = counts.get(source, 0)
        print(f"  - {source}: {n}")
    other = {k: v for k, v in counts.items() if k not in KNOWN_OTEL_SOURCES}
    if other:
        for source, n in sorted(other.items()):
            print(f"  - {source} (unexpected): {n}")
    print("\n  Note: LiteLLM emits per-request multi-span batches; honeybeepf-llm")
    print("  emits aggregate batches at 30s cadence. Absolute counts are not")
    print("  directly comparable.")


async def run_load(label: str, url: str, profile: LoadProfile) -> dict:
    gen = LoadGenerator(base_url=url)
    print(f"    [{label}] Running {profile.total_requests} requests...")
    result = await gen.run(profile)
    stats = compute_latency_stats(result.latencies, result.wall_time_secs, result.error_count)
    print(f"    [{label}] Done: {result.success_count} ok, {result.error_count} err, "
          f"p50={stats['p50']*1000:.1f}ms, p99={stats['p99']*1000:.1f}ms")
    return stats


async def run_scenario(scenario: Scenario, ebpf_binary: str | None = None) -> dict[str, dict]:
    print(f"\n{'='*60}")
    print(f"  Scenario: {scenario.name}")
    print(f"  {scenario.description}")
    print(f"{'='*60}")

    baseline = await run_load("Baseline", DIRECT_URL, scenario.profile)
    proxy = await run_load("LiteLLM Proxy", PROXY_URL, scenario.profile)

    result = {"baseline": baseline, "proxy": proxy}

    if ebpf_binary:
        print(f"    [eBPF] Starting honeybeepf-llm agent: {ebpf_binary}")
        with ebpf_agent(ebpf_binary):
            ebpf_stats = await run_load("eBPF", DIRECT_URL, scenario.profile)
        result["ebpf"] = ebpf_stats

    return result


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
    parser = argparse.ArgumentParser(description="honeybeepf-llm benchmark: Baseline vs LiteLLM Proxy vs eBPF agent")
    parser.add_argument(
        "--scenario",
        choices=list(SCENARIOS.keys()) + ["all", "kill"],
        default="quick",
    )
    parser.add_argument("--output", help="Save results to JSON file")
    parser.add_argument("--markdown", help="Save results as markdown file")
    parser.add_argument(
        "--ebpf-binary",
        help="Path to honeybeepf-llm binary. When provided, runs a third measurement arm with the agent attached.",
    )
    return parser.parse_args()


async def run_all_scenarios(scenario_name: str, ebpf_binary: str | None) -> dict:
    results = {}

    if scenario_name == "kill":
        results["kill"] = await run_kill_test()
    elif scenario_name == "all":
        for name, scenario in SCENARIOS.items():
            r = await run_scenario(scenario, ebpf_binary)
            results[name] = r
            print(f"\n{format_report(r['baseline'], r['proxy'], ebpf=r.get('ebpf'))}\n")
        results["kill"] = await run_kill_test()
    else:
        r = await run_scenario(SCENARIOS[scenario_name], ebpf_binary)
        results[scenario_name] = r
        print(f"\n{format_report(r['baseline'], r['proxy'], ebpf=r.get('ebpf'))}")

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

    OTEL_OUTPUT_DIR.mkdir(exist_ok=True)

    print("Checking services...")
    if not check_services():
        print("\nRun: docker compose up -d")
        sys.exit(1)

    results = await run_all_scenarios(args.scenario, args.ebpf_binary)
    print_summary(results)

    if args.ebpf_binary:
        counts = parse_otel_event_counts(OTEL_OUTPUT_FILE)
        print_otel_summary(counts)

    if args.output:
        with open(args.output, "w") as f:
            json.dump(results, f, indent=2)
        print(f"\nResults saved to {args.output}")

    if args.markdown:
        with open(args.markdown, "w") as f:
            f.write(format_markdown(results))
        print(f"Markdown saved to {args.markdown}")


if __name__ == "__main__":
    asyncio.run(main())
