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
