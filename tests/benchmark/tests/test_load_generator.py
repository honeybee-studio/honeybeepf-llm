import asyncio

import pytest

from load_generator import LoadGenerator, LoadProfile


def test_load_profile_steady():
    p = LoadProfile(rate_rps=10, duration_secs=5)
    assert p.total_requests == 50


def test_load_profile_burst():
    p = LoadProfile(concurrency=100, duration_secs=0)
    assert p.total_requests == 100


@pytest.mark.asyncio
async def test_load_generator_against_mock():
    from mock_server import app
    import uvicorn

    config = uvicorn.Config(app, host="127.0.0.1", port=19876, log_level="error")
    server = uvicorn.Server(config)
    task = asyncio.create_task(server.serve())

    await asyncio.sleep(0.3)

    try:
        gen = LoadGenerator(base_url="http://127.0.0.1:19876")
        profile = LoadProfile(rate_rps=20, duration_secs=1)
        result = await gen.run(profile)

        assert result.success_count > 0
        assert result.error_count == 0
        assert len(result.latencies) == result.success_count
        assert all(lat > 0 for lat in result.latencies)
    finally:
        server.should_exit = True
        await task
