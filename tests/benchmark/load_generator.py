import asyncio
import time
from dataclasses import dataclass, field

import httpx


@dataclass
class LoadProfile:
    rate_rps: float = 0
    concurrency: int = 10
    duration_secs: float = 0

    @property
    def total_requests(self) -> int:
        if self.rate_rps > 0 and self.duration_secs > 0:
            return int(self.rate_rps * self.duration_secs)
        return self.concurrency


@dataclass
class LoadResult:
    latencies: list[float] = field(default_factory=list)
    success_count: int = 0
    error_count: int = 0
    wall_time_secs: float = 0.0


REQUEST_BODY = {
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Benchmark test prompt. Respond briefly."}],
    "max_tokens": 50,
}


class LoadGenerator:
    def __init__(self, base_url: str, timeout: float = 30.0):
        self._base_url = base_url.rstrip("/")
        self._timeout = timeout

    async def _send_one(self, client: httpx.AsyncClient) -> tuple[float, bool]:
        url = f"{self._base_url}/v1/chat/completions"
        t0 = time.monotonic()
        try:
            resp = await client.post(url, json=REQUEST_BODY)
            latency = time.monotonic() - t0
            ok = resp.status_code == 200 and "error" not in resp.json()
            return latency, ok
        except Exception:
            return time.monotonic() - t0, False

    async def run(self, profile: LoadProfile) -> LoadResult:
        result = LoadResult()
        total = profile.total_requests
        sem = asyncio.Semaphore(profile.concurrency)

        async def worker(client: httpx.AsyncClient):
            async with sem:
                latency, ok = await self._send_one(client)
                if ok:
                    result.latencies.append(latency)
                    result.success_count += 1
                else:
                    result.error_count += 1

        t_start = time.monotonic()

        async with httpx.AsyncClient(timeout=self._timeout) as client:
            if profile.rate_rps > 0 and profile.duration_secs > 0:
                interval = 1.0 / profile.rate_rps
                tasks = []
                for i in range(total):
                    tasks.append(asyncio.create_task(worker(client)))
                    if i < total - 1:
                        await asyncio.sleep(interval)
                await asyncio.gather(*tasks)
            else:
                tasks = [asyncio.create_task(worker(client)) for _ in range(total)]
                await asyncio.gather(*tasks)

        result.wall_time_secs = time.monotonic() - t_start
        return result
