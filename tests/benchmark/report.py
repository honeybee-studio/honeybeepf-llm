ROWS = [
    ("Requests (success)", "count", "d"),
    ("Errors", "error_count", "d"),
    ("Error Rate", "error_rate", ".1%"),
    ("Mean Latency (ms)", "mean", ".2f", 1000),
    ("p50 Latency (ms)", "p50", ".2f", 1000),
    ("p95 Latency (ms)", "p95", ".2f", 1000),
    ("p99 Latency (ms)", "p99", ".2f", 1000),
    ("Throughput (req/s)", "throughput_rps", ".1f"),
]


def _overhead(key: str, b: float, p: float) -> str:
    if key in ("mean", "p50", "p95", "p99") and b > 0:
        return f"+{((p - b) / b) * 100:.1f}%"
    return "-"


def format_report(baseline: dict, proxy: dict) -> str:
    header = f"{'Metric':<25} {'Baseline':>12} {'LiteLLM Proxy':>14} {'Overhead':>12}"
    sep = "-" * len(header)
    lines = [header, sep]

    for row in ROWS:
        label, key, fmt = row[0], row[1], row[2]
        mult = row[3] if len(row) > 3 else 1
        b = baseline.get(key, 0) * mult
        p = proxy.get(key, 0) * mult
        lines.append(f"{label:<25} {format(b, fmt):>12} {format(p, fmt):>14} {_overhead(key, b, p):>12}")

    return "\n".join(lines)


def format_markdown(results: dict) -> str:
    lines = []

    for scenario_name, data in results.items():
        if scenario_name == "kill":
            continue

        baseline = data.get("baseline", {})
        proxy = data.get("proxy", {})

        lines.append(f"### {scenario_name}")
        lines.append("")
        lines.append("| Metric | Baseline | LiteLLM Proxy | Overhead |")
        lines.append("|--------|----------|---------------|----------|")

        for row in ROWS:
            label, key, fmt = row[0], row[1], row[2]
            mult = row[3] if len(row) > 3 else 1
            b = baseline.get(key, 0) * mult
            p = proxy.get(key, 0) * mult
            lines.append(f"| {label} | {format(b, fmt)} | {format(p, fmt)} | {_overhead(key, b, p)} |")

        lines.append("")

    if "kill" in results:
        k = results["kill"]
        lines.append("### Kill Test")
        lines.append("")
        lines.append(f"- Proxy killed mid-load: **{k['errors']}** requests failed ({k['error_rate']:.0%} error rate)")
        lines.append("")

    return "\n".join(lines)
