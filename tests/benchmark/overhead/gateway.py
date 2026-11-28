"""Calls straight to the mock provider and through an LLM gateway on this host, and writes a Markdown report.

The other way to count LLM calls: a gateway the apps call instead of the provider. The same mock, clients and load
as overhead.py. Each gateway runs as its docs say to run it in production on one node (see GATEWAYS), and verifies
the mock's certificate: a CA made for the run is added to the system store while it lasts (run as root).
"""
import argparse
import json
import os
import re
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

CASES = {"llm": oh.CASES["llm"], "max": oh.CASES["max"]}
SYSTEM_CA = Path("/usr/local/share/ca-certificates/hb-bench-mock.crt")
SYSTEM_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"
UPSTREAM = f"https://127.0.0.1:{oh.PORT}"


def litellm(tmp, binary):
    """LiteLLM's production guide for one VM: a worker per vCPU, production mode, a master key, error logs only; the
    system CA bundle passed as `ssl_verify`, as for a private CA."""
    cfg = tmp / "litellm.yaml"
    cfg.write_text(f"""model_list:
  - model_name: gpt-4o-mini
    litellm_params:
      model: openai/gpt-4o-mini
      api_base: {UPSTREAM}/v1
      api_key: sk-bench
litellm_settings:
  set_verbose: false
  json_logs: true
  request_timeout: 600
  ssl_verify: {SYSTEM_BUNDLE}
general_settings:
  disable_error_logs: true
""")
    env = {"LITELLM_MODE": "PRODUCTION", "LITELLM_LOG": "ERROR", "LITELLM_MASTER_KEY": os.environ["HB_GATEWAY_KEY"],
           "SSL_CERT_FILE": SYSTEM_BUNDLE}
    return [binary, "--config", str(cfg), "--port", "4000", "--num_workers", str(os.cpu_count())], env


def bifrost(tmp, binary):
    """Bifrost's 4-vCPU profile (t3.xlarge in its benchmarks): pool 15,000, buffer 20,000, GOGC=200. Its inference
    auth needs virtual keys from its governance store, so it is off; the other gateways check a key per call."""
    app = tmp / "bifrost"
    app.mkdir(exist_ok=True)
    (app / "config.json").write_text(json.dumps({
        "client": {"initial_pool_size": 15000, "drop_excess_requests": False, "enforce_auth_on_inference": False,
                   "whitelisted_routes": ["/metrics"]},  # its token counters, behind dashboard auth otherwise
        "providers": {"openai": {
            "keys": [{"name": "bench", "value": "sk-bench", "models": ["gpt-4o-mini"], "weight": 1.0}],
            "network_config": {"base_url": UPSTREAM},
            "concurrency_and_buffer_size": {"concurrency": 5000, "buffer_size": 20000}}}}))
    return [binary, "-host", "127.0.0.1", "-port", "8080", "-app-dir", str(app), "-log-level", "warn"], {"GOGC": "200"}


def envoy_ai_gateway(tmp, binary):
    """`aigw run` with its generated OpenAI config; its docs present the standalone mode for local use and the
    Kubernetes controller for production."""
    env = {"OPENAI_BASE_URL": f"{UPSTREAM}/v1", "OPENAI_API_KEY": "sk-bench", "HOME": str(tmp)}
    return [binary, "run", "--run-id", "bench"], env


# name: start, port, path prefix the client puts before /v1/chat/completions, whether the client sends the gateway
# key, health URL, and the Prometheus series of the tokens it counted (None: not read)
GATEWAYS = {
    "litellm": (litellm, 4000, "", True, "http://127.0.0.1:4000/health/liveliness", None),
    "bifrost": (bifrost, 8080, "/openai", False, "http://127.0.0.1:8080/health",
                ("http://127.0.0.1:8080/metrics", r"^bifrost_(?:input|output)_tokens_total(?:\{[^}]*\})? (\S+)$")),
    # aigw's admin health answers before Envoy listens: its model list is the readiness check
    "envoy-ai-gateway": (envoy_ai_gateway, 1975, "", False, "http://127.0.0.1:1975/v1/models",
                         ("http://127.0.0.1:1064/metrics",
                          r'^gen_ai_client_token_usage_sum\{[^}]*gen_ai_token_type="(?:input|output)"[^}]*\} (\S+)$')),
}
NAMES = {"litellm": "LiteLLM", "bifrost": "Bifrost", "envoy-ai-gateway": "Envoy AI Gateway"}


def version(name, binary):
    if name == "bifrost":  # bifrost-http prints no version
        return "?"
    if name == "litellm":
        out = subprocess.run([str(Path(binary).parent / "pip"), "show", "litellm"], capture_output=True, text=True).stdout
        return next((v.split()[1] for v in out.splitlines() if v.startswith("Version:")), "?")
    out = subprocess.run([binary, "version"], capture_output=True, text=True)
    return (re.findall(r"v?\d+\.\d+\.\d+\S*", out.stdout + out.stderr) or ["?"])[0]


class Gateway:
    def __init__(self, args, tmp):
        start, self.port, self.prefix, self.keyed, self.health, self.tokens = GATEWAYS[args.gateway]
        cmd, env = start(tmp, args.binary)
        self.log = tmp / f"{args.gateway}.log"
        t0 = time.perf_counter()
        with open(self.log, "w") as out:
            self.proc = oh.spawn(cmd, env={**os.environ, **env}, stdout=out, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        while not self.ready():
            if self.proc.poll() is not None or time.perf_counter() - t0 > 300:
                sys.exit(f"{args.gateway} did not start:\n{self.log.read_text()[-2000:]}")
            time.sleep(0.2)
        self.startup = time.perf_counter() - t0
        try:
            self.warm()
        except OSError as e:
            sys.exit(f"warm-up call failed ({e}):\n{self.log.read_text(errors='replace')[-3000:]}")
        self.base = self.counted()

    def ready(self):
        try:
            return urllib.request.urlopen(self.health, timeout=1).status == 200
        except OSError:
            return False

    def call(self, stream):
        body = {"model": "gpt-4o-mini", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}],
                "stream": stream}
        headers = {"Content-Type": "application/json"}
        if self.keyed:
            headers["Authorization"] = "Bearer " + os.environ["HB_GATEWAY_KEY"]
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}{self.prefix}/v1/chat/completions",
                                     json.dumps(body).encode(), headers)
        urllib.request.urlopen(req, timeout=30).read()

    def warm(self):
        """Calls from 8 threads for 5 s, JSON and SSE, so every worker has loaded its code before the measured run,
        as the agents are measured only once attached."""
        import threading

        def calls(i):
            end = time.perf_counter() + 5
            while time.perf_counter() < end:
                self.call(i % 2 == 1)

        threads = [threading.Thread(target=calls, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

    def counted(self):
        """Tokens the gateway's metrics count so far, or None when it exports none."""
        if not self.tokens:
            return None
        url, series = self.tokens
        text = urllib.request.urlopen(url, timeout=10).read().decode()
        return sum(float(v) for v in re.findall(series, text, re.M))

    def pids(self):
        """The gateway and every process it started: its session."""
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
                anon += int(re.search(r"^RssAnon:\s+(\d+)", Path(f"/proc/{pid}/status").read_text(), re.M)[1])
                pss += int(re.search(r"^Pss:\s+(\d+)", Path(f"/proc/{pid}/smaps_rollup").read_text(), re.M)[1])
            except (OSError, TypeError):
                pass
        return anon / 1024, pss / 1024, len(self.pids())

    def stop(self):
        os.killpg(self.proc.pid, signal.SIGTERM)
        try:
            self.proc.wait(30)
        except subprocess.TimeoutExpired:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait(30)


def run_case(args, tmp, case, via, seed, duration=None):
    rate, share = CASES[case]
    rate = args.rate if rate is None else rate
    gw = Gateway(args, tmp) if via else None
    c0 = gw.cpu() if gw else 0
    if gw:
        os.environ["HB_GATEWAY_PREFIX"] = gw.prefix
    outs = [tmp / f"client-{w}.json" for w in range(args.workers)]
    procs = [oh.spawn([sys.executable, oh.__file__, "client", str(seed * 100 + w), str(rate / args.workers),
                       str(duration or args.duration), str(share), str(o), "0", *([str(gw.port)] if gw else [])])
             for w, o in enumerate(outs)]
    for p in procs:
        if p.wait():
            tail = gw.log.read_text(errors="replace")[-3000:] if gw else ""
            sys.exit(f"client failed in case {case}\n{tail}")
    res = [json.loads(o.read_text()) for o in outs]
    lat = [x for r in res for x in r["lat"]]
    n = len(lat)
    row = {"n": n, "rps": n / max(r["wall"] for r in res), "p50": statistics.median(lat) * 1e3,
           "p99": statistics.quantiles(lat, n=100)[98] * 1e3, "sent": sum(r["tokens"] for r in res)}
    if gw:
        time.sleep(2)  # metrics catch up with the last calls
        counted = gw.counted()
        row |= {"cpu": (gw.cpu() - c0) / n * 1000, "mem": gw.mem(), "startup": gw.startup,
                "recorded": None if counted is None else counted - gw.base}
        gw.stop()
    return row


def report(args, rows, ver):
    med = lambda case, on, key: statistics.median(r[key] for r in rows[case, on])
    name = NAMES[args.gateway]
    env = oh.environment(argparse.Namespace(agent=None, obi=False, agentsight=False, workers=args.workers,
                                            rate=args.rate, duration=args.duration, reps=args.reps, seed=args.seed,
                                            client_delay=0))
    lines = [f"# {name} gateway ({time.strftime('%Y-%m-%d')})", "", *env[:3],
             f"- {name} {ver}, set up by `{GATEWAYS[args.gateway][0].__name__}()` in gateway.py; it verifies the mock's "
             "certificate", "",
             f"## Application impact (off: straight to the mock, on: through {name})", "",
             "| case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |", "|---|---|---|---|---|---|---|"]
    for case in args.cases:
        cell = lambda key: f"{med(case, False, key):.2f} | {med(case, True, key):.2f}"
        lines.append(f"| {case} | {cell('rps')} | {cell('p50')} | {cell('p99')} |")
    lines += ["", "## Gateway cost (the gateway and every process it starts)", "",
              "| case | CPU s / 1k req | RSS anon MB | PSS MB | processes | tokens counted / returned |",
              "|---|---|---|---|---|---|"]
    for case in args.cases:
        mem = [statistics.median(r["mem"][i] for r in rows[case, True]) for i in range(3)]
        got = [r["recorded"] for r in rows[case, True]]
        sent = sum(r["sent"] for r in rows[case, True])
        acc = "not exported" if None in got else f"{sum(got) / sent:.2%}"
        lines.append(f"| {case} | {med(case, True, 'cpu'):.3f} | {mem[0]:.1f} | {mem[1]:.1f} | {mem[2]:.0f} | {acc} |")
    start = statistics.median(r["startup"] for c in args.cases for r in rows[c, True])
    lines += ["", f"Start to its health check answering: {start:.1f} s (median)"]
    return "\n".join(lines) + "\n"


def trust_ca(tmp):
    """A CA for this run and a mock certificate for 127.0.0.1 signed by it; the CA joins the system store."""
    ssl = lambda *a: subprocess.run(["openssl", *map(str, a)], check=True, capture_output=True)
    # The extensions a real CA and server certificate carry: Python 3.13+ verifies strictly and refuses others.
    ssl("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-subj",
        "/CN=hb-bench-mock-ca", "-days", "2", "-keyout", tmp / "ca.key", "-out", tmp / "ca.pem",
        "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    ssl("req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-subj", "/CN=bench",
        "-keyout", tmp / "key.pem", "-out", tmp / "cert.csr")
    (tmp / "cert.ext").write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
                                  "extendedKeyUsage=serverAuth\nsubjectKeyIdentifier=hash\n"
                                  "authorityKeyIdentifier=keyid:always\nsubjectAltName=IP:127.0.0.1,DNS:127.0.0.1,DNS:api.openai.com\n")
    ssl("x509", "-req", "-in", tmp / "cert.csr", "-CA", tmp / "ca.pem", "-CAkey", tmp / "ca.key", "-CAcreateserial",
        "-days", "2", "-extfile", tmp / "cert.ext", "-out", tmp / "cert.pem")
    SYSTEM_CA.write_text((tmp / "ca.pem").read_text())
    subprocess.run(["update-ca-certificates"], check=True, capture_output=True)


def untrust_ca():
    SYSTEM_CA.unlink(missing_ok=True)
    subprocess.run(["update-ca-certificates", "--fresh"], capture_output=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gateway", required=True, choices=list(GATEWAYS))
    ap.add_argument("--binary", required=True, help="the litellm command, or the bifrost-http / aigw binary")
    ap.add_argument("--version", help="the gateway's version for the report, when the binary does not print it")
    ap.add_argument("--rate", type=float, default=200)
    ap.add_argument("--duration", type=float, default=30)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--seed", type=int, default=93)
    ap.add_argument("--cases", nargs="+", choices=list(CASES), default=list(CASES))
    ap.add_argument("--out")
    args = ap.parse_args()
    if os.geteuid() != 0:
        sys.exit("run as root: the mock's CA joins the system certificate store for the run")
    for port in (oh.PORT, GATEWAYS[args.gateway][1]):
        if not socket.socket().connect_ex(("127.0.0.1", port)):
            sys.exit(f"port {port} is already in use")
    ver = args.version or version(args.gateway, args.binary)
    os.environ.pop("HB_GATEWAY_KEY", None)
    if GATEWAYS[args.gateway][3]:  # the clients send it; LiteLLM refuses to start without one
        os.environ["HB_GATEWAY_KEY"] = "sk-" + os.urandom(32).hex()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    with tempfile.TemporaryDirectory(prefix="hb-gateway-") as d:
        tmp = Path(d)
        rows = {}
        try:
            trust_ca(tmp)
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
                try:  # a gateway leads its own session, workers included
                    os.killpg(p.pid, signal.SIGKILL)
                except OSError:
                    p.kill()
                p.wait()
            untrust_ca()
    md = report(args, rows, ver)
    print(md)
    if args.out:
        Path(args.out).write_text(md)


if __name__ == "__main__":
    main()
