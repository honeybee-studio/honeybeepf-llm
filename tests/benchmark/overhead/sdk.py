"""In-process instrumentation: the same calls through the official `openai` SDK with and without an instrumentation
library turned on, and a Markdown report.

The third way to count LLM calls: a library in the app's own process. Each library runs with its defaults, its data
kept in memory (an OpenTelemetry in-memory span exporter and metric reader, nothing exported), so the cost measured
is the instrumentation's own. Off and on use the same Python environment, so only turning the library on differs.

    python3 -m venv /opt/sdk-otel && /opt/sdk-otel/bin/pip install openai==3.51.0 httpx==0.28.1 \\
        opentelemetry-sdk==1.45.2 opentelemetry-instrumentation-openai-v2==2.4b0 opentelemetry-util-genai==0.4b0
    python3 sdk.py --python /opt/sdk-otel/bin/python --instrument otel --out results.md
"""
import argparse
import json
import os
import random
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import overhead as oh

CASES = {"llm": oh.CASES["llm"], "max": oh.CASES["max"]}
NAMES = {"otel": "OpenTelemetry openai-v2", "openllmetry": "OpenLLMetry", "openlit": "OpenLIT"}
PACKAGES = {"otel": ["opentelemetry-instrumentation-openai-v2", "opentelemetry-util-genai"],
            "openllmetry": ["opentelemetry-instrumentation-openai"], "openlit": ["openlit"]}


def instrument(name, pricing):
    """In-memory OpenTelemetry providers, then the library turned on with its defaults."""
    from opentelemetry import metrics, trace
    from opentelemetry.sdk.metrics import MeterProvider
    from opentelemetry.sdk.metrics.export import InMemoryMetricReader
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import BatchSpanProcessor
    from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

    spans, reader = InMemorySpanExporter(), InMemoryMetricReader()
    tp, mp = TracerProvider(), MeterProvider(metric_readers=[reader])
    tp.add_span_processor(BatchSpanProcessor(spans))
    trace.set_tracer_provider(tp)
    metrics.set_meter_provider(mp)
    if name == "otel":
        from opentelemetry.instrumentation.openai_v2 import OpenAIInstrumentor
        OpenAIInstrumentor().instrument(tracer_provider=tp, meter_provider=mp)
    elif name == "openllmetry":
        from opentelemetry.instrumentation.openai import OpenAIInstrumentor
        OpenAIInstrumentor().instrument(tracer_provider=tp, meter_provider=mp)
    else:
        import openlit
        # With providers already set, OpenLIT adds to them; a local price file keeps init from downloading one.
        openlit.init(pricing_json=pricing)
    return tp, spans


def recorded(tp, spans):
    """Input and output tokens on the library's spans."""
    tp.force_flush()
    total = 0
    for span in spans.get_finished_spans():
        attrs = span.attributes or {}
        total += int(attrs.get("gen_ai.usage.input_tokens", 0)) + int(attrs.get("gen_ai.usage.output_tokens", 0))
    return total


def client(seed, rate, duration, out, name, pricing):
    import httpx
    import resource

    tp = spans = None
    if name != "off":
        tp, spans = instrument(name, pricing)
    from openai import OpenAI

    api = OpenAI(base_url=f"https://127.0.0.1:{oh.PORT}/v1", api_key="sk-bench", max_retries=0,
                 http_client=httpx.Client(verify=False))
    rng = random.Random(seed)
    lat, tokens = [], 0
    r0 = resource.getrusage(resource.RUSAGE_SELF)
    t0 = time.perf_counter()
    while (now := time.perf_counter()) - t0 < duration:
        start = now
        if rate:
            start = t0 + len(lat) / rate  # from the scheduled start, as overhead.py
            if start > now:
                time.sleep(start - now)
        rng.random()  # the share draw overhead.py makes, so the same seed asks for the same calls
        k = rng.randint(1, 512)
        args = {"model": "gpt-4o-mini", "max_tokens": k, "messages": [{"role": "user", "content": "hi"}]}
        if rng.random() < 0.5:
            usage = None
            for chunk in api.chat.completions.create(**args, stream=True, stream_options={"include_usage": True}):
                usage = chunk.usage or usage
        else:
            usage = api.chat.completions.create(**args).usage
        tokens += usage.prompt_tokens + usage.completion_tokens
        lat.append(time.perf_counter() - start)
    wall = time.perf_counter() - t0
    r1 = resource.getrusage(resource.RUSAGE_SELF)
    got = recorded(tp, spans) if tp else None
    Path(out).write_text(json.dumps({"n": len(lat), "tokens": tokens, "recorded": got, "wall": wall, "lat": lat,
                                     "cpu": r1.ru_utime + r1.ru_stime - r0.ru_utime - r0.ru_stime,
                                     "maxrss": r1.ru_maxrss / 1024}))


def run_case(args, tmp, case, on, seed, duration=None):
    rate, _ = CASES[case]
    rate = args.rate if rate is None else rate
    outs = [tmp / f"client-{w}.json" for w in range(args.workers)]
    procs = [oh.spawn([args.python, __file__, "client", str(seed * 100 + w), str(rate / args.workers),
                       str(duration or args.duration), str(o), args.instrument if on else "off", str(args.pricing)])
             for w, o in enumerate(outs)]
    for p in procs:
        if p.wait():
            sys.exit(f"client failed in case {case}")
    res = [json.loads(o.read_text()) for o in outs]
    lat = [x for r in res for x in r["lat"]]
    n = len(lat)
    return {"n": n, "rps": n / max(r["wall"] for r in res), "p50": statistics.median(lat) * 1e3,
            "p99": statistics.quantiles(lat, n=100)[98] * 1e3, "sent": sum(r["tokens"] for r in res),
            "cpu": sum(r["cpu"] for r in res) / n * 1000, "rss": statistics.median(r["maxrss"] for r in res),
            "recorded": None if not on else sum(r["recorded"] for r in res)}


def versions(python, name):
    pkgs = ["openai", *PACKAGES[name]]
    out = subprocess.run([python, "-m", "pip", "show", *pkgs], capture_output=True, text=True).stdout
    names = [l.split(": ", 1)[1] for l in out.splitlines() if l.startswith("Name: ")]
    vers = [l.split(": ", 1)[1] for l in out.splitlines() if l.startswith("Version: ")]
    py = subprocess.run([python, "-V"], capture_output=True, text=True).stdout.strip()
    return py + ", " + ", ".join(f"{n} {v}" for n, v in zip(names, vers))


def report(args, rows, ver):
    med = lambda case, on, key: statistics.median(r[key] for r in rows[case, on])
    name = NAMES[args.instrument]
    env = oh.environment(argparse.Namespace(agent=None, obi=False, agentsight=False, workers=args.workers,
                                            rate=args.rate, duration=args.duration, reps=args.reps, seed=args.seed,
                                            client_delay=0))
    lines = [f"# {name} in the app ({time.strftime('%Y-%m-%d')})", "", *env[:2], f"- {ver}",
             "- off: the openai SDK alone; on: the same environment with the library turned on, its defaults, "
             "data in memory", "",
             "## Application impact", "",
             "| case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |", "|---|---|---|---|---|---|---|"]
    for case in args.cases:
        cell = lambda key: f"{med(case, False, key):.2f} | {med(case, True, key):.2f}"
        lines.append(f"| {case} | {cell('rps')} | {cell('p50')} | {cell('p99')} |")
    lines += ["", "## Cost in the app's processes", "",
              "| case | app CPU s / 1k req off | on | added | max RSS MB off | on | tokens recorded / returned |",
              "|---|---|---|---|---|---|---|"]
    for case in args.cases:
        off, on = med(case, False, "cpu"), med(case, True, "cpu")
        sent = sum(r["sent"] for r in rows[case, True])
        acc = f"{sum(r['recorded'] for r in rows[case, True]) / sent:.2%}"
        lines.append(f"| {case} | {off:.3f} | {on:.3f} | {on - off:.3f} | {med(case, False, 'rss'):.1f} | "
                     f"{med(case, True, 'rss'):.1f} | {acc} |")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--python", required=True, help="the interpreter of an environment with openai and the library")
    ap.add_argument("--instrument", required=True, choices=list(NAMES))
    ap.add_argument("--pricing", default="", help="OpenLIT's pricing.json, saved locally")
    ap.add_argument("--rate", type=float, default=200)
    ap.add_argument("--duration", type=float, default=30)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--seed", type=int, default=93)
    ap.add_argument("--cases", nargs="+", choices=list(CASES), default=list(CASES))
    ap.add_argument("--out")
    args = ap.parse_args()
    if not socket.socket().connect_ex(("127.0.0.1", oh.PORT)):
        sys.exit(f"port {oh.PORT} is already in use")
    ver = versions(args.python, args.instrument)
    with tempfile.TemporaryDirectory(prefix="hb-sdk-") as d:
        tmp = Path(d)
        subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
                        "-subj", "/CN=bench", "-days", "2", "-keyout", tmp / "key.pem", "-out", tmp / "cert.pem"],
                       check=True, capture_output=True)
        rows = {}
        try:
            server = oh.spawn([sys.executable, oh.__file__, "mock", tmp / "cert.pem", tmp / "key.pem"])
            while socket.socket().connect_ex(("127.0.0.1", oh.PORT)):
                if server.poll() is not None:
                    sys.exit("the mock did not start")
                time.sleep(0.05)
            for case in args.cases:
                run_case(args, tmp, case, False, args.seed, 5)
            for rep in range(args.reps):
                for on in [False, True] if rep % 2 == 0 else [True, False]:
                    for case in args.cases:
                        rows.setdefault((case, on), []).append(run_case(args, tmp, case, on, args.seed + rep))
                        print(case, "on" if on else "off", {k: v for k, v in rows[case, on][-1].items()},
                              file=sys.stderr)
        finally:
            for p in oh.CHILDREN:
                p.kill()
                p.wait()
    md = report(args, rows, ver)
    print(md)
    if args.out:
        Path(args.out).write_text(md)


if __name__ == "__main__":
    if sys.argv[1:2] == ["client"]:
        a = sys.argv[2:]
        sys.path.insert(0, str(Path(__file__).parent))
        client(int(a[0]), float(a[1]), float(a[2]), a[3], a[4], a[5])
    else:
        main()
