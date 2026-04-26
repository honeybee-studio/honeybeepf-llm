import numpy as np


def compute_latency_stats(
    latencies: list[float],
    duration_secs: float,
    error_count: int = 0,
) -> dict:
    total = len(latencies) + error_count

    if not latencies:
        return {
            "count": 0,
            "error_count": error_count,
            "total_requests": total,
            "error_rate": 1.0 if total > 0 else 0.0,
            "mean": 0.0,
            "min": 0.0,
            "max": 0.0,
            "p50": 0.0,
            "p95": 0.0,
            "p99": 0.0,
            "throughput_rps": 0.0,
        }

    arr = np.array(latencies)

    return {
        "count": len(latencies),
        "error_count": error_count,
        "total_requests": total,
        "error_rate": error_count / total if total > 0 else 0.0,
        "mean": float(np.mean(arr)),
        "min": float(np.min(arr)),
        "max": float(np.max(arr)),
        "p50": float(np.percentile(arr, 50)),
        "p95": float(np.percentile(arr, 95)),
        "p99": float(np.percentile(arr, 99)),
        "throughput_rps": len(latencies) / duration_secs if duration_secs > 0 else 0.0,
    }


def format_report(results: dict[str, dict]) -> str:
    header = f"{'Metric':<25} {'Baseline':>12} {'LiteLLM Proxy':>14} {'eBPF':>12} {'Proxy Overhead':>15}"
    sep = "-" * len(header)
    lines = [header, sep]

    baseline = results.get("baseline", {})
    proxy = results.get("proxy", {})
    ebpf = results.get("ebpf", {})

    rows = [
        ("Requests (success)", "count", "d"),
        ("Errors", "error_count", "d"),
        ("Error Rate", "error_rate", ".1%"),
        ("Mean Latency (ms)", "mean", ".2f", 1000),
        ("p50 Latency (ms)", "p50", ".2f", 1000),
        ("p95 Latency (ms)", "p95", ".2f", 1000),
        ("p99 Latency (ms)", "p99", ".2f", 1000),
        ("Throughput (req/s)", "throughput_rps", ".1f"),
    ]

    for row in rows:
        label, key, fmt = row[0], row[1], row[2]
        mult = row[3] if len(row) > 3 else 1

        b = baseline.get(key, 0) * mult
        p = proxy.get(key, 0) * mult
        e = ebpf.get(key, 0) * mult

        if key in ("mean", "p50", "p95", "p99") and b > 0:
            overhead = f"+{((p - b) / b) * 100:.1f}%"
        else:
            overhead = "-"

        lines.append(
            f"{label:<25} {format(b, fmt):>12} {format(p, fmt):>14} {format(e, fmt):>12} {overhead:>15}"
        )

    return "\n".join(lines)
