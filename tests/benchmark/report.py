def format_report(baseline: dict, proxy: dict) -> str:
    header = f"{'Metric':<25} {'Baseline':>12} {'LiteLLM Proxy':>14} {'Overhead':>12}"
    sep = "-" * len(header)
    lines = [header, sep]

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

        if key in ("mean", "p50", "p95", "p99") and b > 0:
            overhead = f"+{((p - b) / b) * 100:.1f}%"
        else:
            overhead = "-"

        lines.append(f"{label:<25} {format(b, fmt):>12} {format(p, fmt):>14} {overhead:>12}")

    return "\n".join(lines)
