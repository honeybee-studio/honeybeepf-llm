import pytest
from stats import compute_latency_stats


def test_compute_latency_stats_basic():
    latencies = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]
    result = compute_latency_stats(latencies, duration_secs=10.0)

    assert result["count"] == 10
    assert result["error_count"] == 0
    assert pytest.approx(result["mean"], abs=0.01) == 0.55
    assert pytest.approx(result["p50"], abs=0.05) == 0.55
    assert pytest.approx(result["p95"], abs=0.05) == 0.955
    assert pytest.approx(result["p99"], abs=0.05) == 0.991
    assert pytest.approx(result["throughput_rps"], abs=0.1) == 1.0


def test_compute_latency_stats_with_errors():
    latencies = [0.1, 0.2, 0.3]
    result = compute_latency_stats(latencies, duration_secs=3.0, error_count=2)

    assert result["count"] == 3
    assert result["error_count"] == 2
    assert result["total_requests"] == 5
    assert pytest.approx(result["error_rate"], abs=0.01) == 0.4


def test_compute_latency_stats_empty():
    result = compute_latency_stats([], duration_secs=1.0)

    assert result["count"] == 0
    assert result["p50"] == 0.0
    assert result["throughput_rps"] == 0.0
