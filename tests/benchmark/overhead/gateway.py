"""Calls straight to the mock provider and through LiteLLM on this host, and writes a Markdown report.

The other way to count LLM calls: a gateway the apps call instead of the provider. The same mock, clients and load
as overhead.py; the gateway runs as LiteLLM's production guide says for one VM (a worker per vCPU, production mode,
a master key, error logs only, no database). Only `ssl_verify: false` is not production: the mock's certificate is self-signed.
"""
import argparse
import json
import os
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

import overhead as oh

GATEWAY_PORT = 4000
CASES = {"llm": oh.CASES["llm"], "max": oh.CASES["max"]}
CONFIG = f"""model_list:
  - model_name: gpt-4o-mini
    litellm_params:
      model: openai/gpt-4o-mini
      api_base: https://127.0.0.1:{oh.PORT}/v1
      api_key: sk-bench
litellm_settings:
  set_verbose: false
  json_logs: true
  ssl_verify: false
  request_timeout: 600
general_settings:
  disable_error_logs: true
"""


class Gateway:
    def __init__(self, binary, tmp):
        cfg = tmp / "litellm.yaml"
        cfg.write_text(CONFIG)
        env = {**os.environ, "LITELLM_MODE": "PRODUCTION", "LITELLM_LOG": "ERROR",
               "LITELLM_MASTER_KEY": os.environ["HB_GATEWAY_KEY"]}
        self.log = tmp / "litellm.log"
        t0 = time.perf_counter()
        with open(self.log, "w") as out:
            self.proc = oh.spawn([binary, "--config", str(cfg), "--port", str(GATEWAY_PORT),
                                  "--num_workers", str(os.cpu_count())], env=env, stdout=out, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        while not self.ready():
            if self.proc.poll() is not None or time.perf_counter() - t0 > 180:
                sys.exit(f"LiteLLM did not start:\n{self.log.read_text()[-2000:]}")
            time.sleep(0.2)
        self.startup = time.perf_counter() - t0
        self.warm()

    @staticmethod
    def warm():
        """Calls from 8 threads for 5 s, JSON and SSE, so every worker has loaded its code before the measured run,
        as the agents are measured only once attached."""
        import threading

        def calls(i):
            end = time.perf_counter() + 5
            while time.perf_counter() < end:
                body = {"model": "gpt-4o-mini", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}],
                        "stream": i % 2 == 1}
                req = urllib.request.Request(f"http://127.0.0.1:{GATEWAY_PORT}/v1/chat/completions",
                                             json.dumps(body).encode(), {"Content-Type": "application/json",
                                             "Authorization": "Bearer " + os.environ["HB_GATEWAY_KEY"]})
                urllib.request.urlopen(req, timeout=30).read()

        threads = [threading.Thread(target=calls, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

    @staticmethod
    def ready():
        try:
            return urllib.request.urlopen(f"http://127.0.0.1:{GATEWAY_PORT}/health/liveliness", timeout=1).status == 200
        except OSError:
            return False

    def pids(self):
        """The server and its workers: every process in its session."""
        sid = os.getsid(self.proc.pid)
        out = []
        for d in os.listdir("/proc"):
            if d.isdigit():
                try:
                    if os.getsid(int(d)) == sid:
                        out.append(int(d))
                except OSError:
                    pass
        return out

    def cpu(self):
        total = 0.0
        for pid in self.pids():
            try:
                f = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                total += (int(f[11]) + int(f[12])) / oh.HZ
            except OSError:
                pass
        return total

    def mem(self):
        anon = pss = 0
        for pid in self.pids():
            try:
                anon += int(oh.re.search(r"^RssAnon:\s+(\d+)", Path(f"/proc/{pid}/status").read_text(), oh.re.M)[1])
                pss += int(oh.re.search(r"^Pss:\s+(\d+)", Path(f"/proc/{pid}/smaps_rollup").read_text(), oh.re.M)[1])
            except (OSError, TypeError):
                pass
        return anon / 1024, pss / 1024, len(self.pids())

    def stop(self):
        os.killpg(self.proc.pid, signal.SIGTERM)
        self.proc.wait(30)


def run_case(args, tmp, case, via, seed, duration=None):
    rate, share = CASES[case]
    rate = args.rate if rate is None else rate
    gw = Gateway(args.litellm, tmp) if via else None
    c0 = gw.cpu() if gw else 0
    outs = [tmp / f"client-{w}.json" for w in range(args.workers)]
    procs = [oh.spawn([sys.executable, oh.__file__, "client", str(seed * 100 + w), str(rate / args.workers),
                       str(duration or args.duration), str(share), str(o), "0", *([str(GATEWAY_PORT)] if via else [])])
             for w, o in enumerate(outs)]
    for p in procs:
        if p.wait():
            sys.exit(f"client failed in case {case}")
    res = [json.loads(o.read_text()) for o in outs]
    lat = [x for r in res for x in r["lat"]]
    n = len(lat)
    row = {"n": n, "rps": n / max(r["wall"] for r in res), "p50": statistics.median(lat) * 1e3,
           "p99": statistics.quantiles(lat, n=100)[98] * 1e3}
    if gw:
        row |= {"cpu": (gw.cpu() - c0) / n * 1000, "mem": gw.mem(), "startup": gw.startup}
        gw.stop()
    return row


def report(args, rows, version):
    med = lambda case, on, key: statistics.median(r[key] for r in rows[case, on])
    lines = [f"# LiteLLM gateway ({time.strftime('%Y-%m-%d')})", "", *oh.environment(argparse.Namespace(
                 agent=None, obi=False, workers=args.workers, rate=args.rate, duration=args.duration, reps=args.reps,
                 seed=args.seed, client_delay=0))[:3],
             f"- LiteLLM {version}, {os.cpu_count()} workers, production mode, no database", "",
             "## Application impact (off: straight to the mock, on: through LiteLLM)", "",
             "| case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |", "|---|---|---|---|---|---|---|"]
    for case in args.cases:
        cell = lambda key: f"{med(case, False, key):.2f} | {med(case, True, key):.2f}"
        lines.append(f"| {case} | {cell('rps')} | {cell('p50')} | {cell('p99')} |")
    lines += ["", "## Gateway cost (the server and its workers)", "",
              "| case | CPU s / 1k req | RSS anon MB | PSS MB | processes |", "|---|---|---|---|---|"]
    for case in args.cases:
        mem = [statistics.median(r["mem"][i] for r in rows[case, True]) for i in range(3)]
        lines.append(f"| {case} | {med(case, True, 'cpu'):.3f} | {mem[0]:.1f} | {mem[1]:.1f} | {mem[2]:.0f} |")
    start = statistics.median(r["startup"] for c in args.cases for r in rows[c, True])
    lines += ["", f"Start to `/health/liveliness` answering: {start:.1f} s (median)"]
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--litellm", required=True, help="the litellm command (pip install 'litellm[proxy]')")
    ap.add_argument("--rate", type=float, default=200)
    ap.add_argument("--duration", type=float, default=30)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--seed", type=int, default=93)
    ap.add_argument("--cases", nargs="+", choices=list(CASES), default=list(CASES))
    ap.add_argument("--out")
    args = ap.parse_args()
    for port in (oh.PORT, GATEWAY_PORT):
        if not socket.socket().connect_ex(("127.0.0.1", port)):
            sys.exit(f"port {port} is already in use")
    version = subprocess.run([str(Path(args.litellm).parent / "pip"), "show", "litellm"], capture_output=True,
                             text=True).stdout
    version = next((v.split()[1] for v in version.splitlines() if v.startswith("Version:")), "?")
    os.environ["HB_GATEWAY_KEY"] = "sk-" + os.urandom(32).hex()  # the clients send it; production refuses no key
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    with tempfile.TemporaryDirectory(prefix="hb-gateway-") as d:
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
                for via in [False, True] if rep % 2 == 0 else [True, False]:
                    for case in args.cases:
                        rows.setdefault((case, via), []).append(run_case(args, tmp, case, via, args.seed + rep))
                        print(case, "gateway" if via else "direct", rows[case, via][-1], file=sys.stderr)
        finally:
            for p in oh.CHILDREN:
                p.kill()
                p.wait()
    md = report(args, rows, version)
    print(md)
    if args.out:
        Path(args.out).write_text(md)


if __name__ == "__main__":
    main()
