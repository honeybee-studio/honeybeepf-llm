"""Runs each case with and without the agent on this host and writes a Markdown report (see docs/benchmark.md)."""
import argparse
import json
import os
import platform
import random
import re
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PORT = 8443
LLM_HOST = "api.openai.com"
OTHER_HOST = "internal.example"
PROMPT_TOKENS = 13
READY = "Active probe registered: llm"
SUCCESS = re.compile(r"LLM SUCCESS .*? Tokens: (\d+)")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
HZ = os.sysconf("SC_CLK_TCK")
# name: (requests/s: None = --rate, 0 = as fast as possible; share of requests that are LLM calls)
CASES = {"llm": (None, 1.0), "non-llm": (None, 0.05), "max": (0, 1.0)}
CHILDREN = []  # killed on exit, so a failed run never leaves a root agent attached


def spawn(cmd, **kw):
    CHILDREN.append(subprocess.Popen(cmd, **kw))
    return CHILDREN[-1]


def mock(cert, key):
    import ssl
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        disable_nagle_algorithm = True  # head and body are separate writes; Nagle would add a 40 ms delayed-ACK wait

        def log_message(self, *_):
            pass

        def reply(self, body, ctype="application/json"):
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            if self.path != "/v1/chat/completions":
                return self.reply(b'{"ok":true}')
            req = json.loads(body)
            k = req["max_tokens"]
            usage = {"prompt_tokens": PROMPT_TOKENS, "completion_tokens": k, "total_tokens": PROMPT_TOKENS + k}
            head = {"id": "chatcmpl-bench", "model": req["model"]}
            if not req.get("stream"):
                msg = {"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}
                return self.reply(json.dumps({**head, "object": "chat.completion", "choices": [msg], "usage": usage}).encode())
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            events = [{"choices": [{"index": 0, "delta": {"content": "ok"}}]},
                      {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                      {"choices": [], "usage": usage}]
            # One write per event, as a streaming provider sends them.
            for data in [json.dumps({**head, "object": "chat.completion.chunk", **e}) for e in events] + ["[DONE]"]:
                line = f"data: {data}\n\n".encode()
                self.wfile.write(b"%x\r\n%s\r\n" % (len(line), line))
            self.wfile.write(b"0\r\n\r\n")

        do_GET = do_POST

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    server.daemon_threads = True
    server.socket = ctx.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


def client(seed, rate, duration, llm_share, out):
    import http.client
    import ssl

    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    conns = {h: http.client.HTTPSConnection("127.0.0.1", PORT, context=ctx) for h in (LLM_HOST, OTHER_HOST)}
    rng = random.Random(seed)
    lat, tokens, llm = [], 0, 0
    t0 = time.perf_counter()
    while (now := time.perf_counter()) - t0 < duration:
        start = now
        if rate:
            # Latency counts from the scheduled start, so a stalled client is not hidden (coordinated omission).
            start = t0 + len(lat) / rate
            if start > now:
                time.sleep(start - now)
        if rng.random() < llm_share:
            k = rng.randint(1, 512)
            req = {"model": "gpt-4o-mini", "max_tokens": k, "messages": [{"role": "user", "content": "hi"}]}
            if rng.random() < 0.5:
                req |= {"stream": True, "stream_options": {"include_usage": True}}
            host, method, path, body = LLM_HOST, "POST", "/v1/chat/completions", json.dumps(req)
            tokens += PROMPT_TOKENS + k
            llm += 1
        elif rng.random() < 0.5:
            host, method, path, body = OTHER_HOST, "GET", "/static", None
        else:
            host, method, path, body = OTHER_HOST, "POST", "/api/items", '{"id":1}'
        conn = conns[host]
        conn.request(method, path, body, {"Host": host, "Content-Type": "application/json"})
        resp = conn.getresponse()
        resp.read()
        if resp.status != 200:
            sys.exit(f"{method} {path}: HTTP {resp.status}; is another server on port {PORT}?")
        lat.append(time.perf_counter() - start)
    Path(out).write_text(json.dumps({"n": len(lat), "llm": llm, "tokens": tokens, "wall": time.perf_counter() - t0, "lat": lat}))


class Agent:
    def __init__(self, binary, log):
        self.log = log
        env = {**os.environ, "BUILTIN_PROBES__LLM": "true", "RUST_LOG": "info"}
        t0 = time.perf_counter()
        with open(log, "w") as out:
            self.proc = spawn([binary], env=env, stdout=out, stderr=subprocess.STDOUT)
        while READY not in self.text():
            if self.proc.poll() is not None or time.perf_counter() - t0 > 120:
                sys.exit(f"agent did not start:\n{self.text()[-2000:]}")
            time.sleep(0.01)
        self.startup = time.perf_counter() - t0

    def text(self):
        return ANSI.sub("", Path(self.log).read_text(errors="replace"))

    def cpu(self):
        f = Path(f"/proc/{self.proc.pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / HZ

    def mem(self):
        pid = self.proc.pid
        status = dict(re.findall(r"^(\w+):\s+(\d+) kB", Path(f"/proc/{pid}/status").read_text(), re.M))
        pss = re.search(r"^Pss:\s+(\d+) kB", Path(f"/proc/{pid}/smaps_rollup").read_text(), re.M)[1]
        maps = {}  # map_id -> memlock, as `bpftool map` shows it
        for fd in os.listdir(f"/proc/{pid}/fd"):
            try:
                if os.readlink(f"/proc/{pid}/fd/{fd}") == "anon_inode:bpf-map":
                    info = dict(re.findall(r"^(\w+):\s+(\S+)", Path(f"/proc/{pid}/fdinfo/{fd}").read_text(), re.M))
                    maps[info["map_id"]] = int(info["memlock"])
            except OSError:
                pass
        mb = lambda kb: int(kb) / 1024
        return mb(status["RssAnon"]), mb(status["RssFile"]), mb(pss), sum(maps.values()) / 2**20

    def drain(self):
        """Waits until the log has been quiet for 2 s, so calls still in the ringbuf are counted."""
        last, since = -1, time.perf_counter()
        while time.perf_counter() - since < 2:
            size = os.path.getsize(self.log)
            if size != last:
                last, since = size, time.perf_counter()
            time.sleep(0.1)

    def stop(self):
        self.proc.send_signal(signal.SIGINT)
        self.proc.wait(30)


def run_case(args, tmp, case, with_agent, seed, duration=None):
    rate, share = CASES[case]
    rate = args.rate if rate is None else rate
    agent = Agent(args.agent, tmp / f"agent-{case}.log") if with_agent else None
    c0 = agent.cpu() if agent else 0
    outs = [tmp / f"client-{w}.json" for w in range(args.workers)]
    procs = [spawn([sys.executable, __file__, "client", str(seed * 100 + w), str(rate / args.workers),
                    str(duration or args.duration), str(share), str(o)]) for w, o in enumerate(outs)]
    for p in procs:
        if p.wait():
            sys.exit(f"client failed in case {case}")
    res = [json.loads(o.read_text()) for o in outs]
    lat = [x for r in res for x in r["lat"]]
    n = len(lat)
    row = {"n": n, "rps": n / max(r["wall"] for r in res), "p50": statistics.median(lat) * 1e3,
           "p99": statistics.quantiles(lat, n=100)[98] * 1e3, "sent": sum(r["tokens"] for r in res)}
    if agent:
        agent.drain()
        cpu, mem = agent.cpu(), agent.mem()
        agent.stop()  # the agent logs its kernel counters on exit too
        text = agent.text()
        row |= {"cpu": (cpu - c0) / n * 1000, "mem": mem, "recorded": sum(int(t) for t in SUCCESS.findall(text)),
                "unparsed": text.count("LLM UNPARSED"), "stats": ([""] + re.findall(r"\[LLM stats\] (.*)", text))[-1]}
    return row


def startup(args, tmp):
    """Agent start to probes attached, with --processes idle processes on the node (1 in 20 with libssl loaded)."""
    for i in range(args.processes):
        spawn([sys.executable, "-c", "import ssl, time; time.sleep(3600)"] if i < args.processes // 20 else ["sleep", "3600"])
    times = []
    for _ in range(args.reps):
        agent = Agent(args.agent, tmp / "agent-startup.log")
        times.append(agent.startup)
        agent.stop()
    return statistics.median(times)


def environment(args):
    cpu = re.search(r"^model name\s*:\s*(.+)$", Path("/proc/cpuinfo").read_text(), re.M)
    mem = int(re.search(r"^MemTotal:\s+(\d+)", Path("/proc/meminfo").read_text(), re.M)[1]) / 2**20
    distro = re.search(r'^PRETTY_NAME="?([^"\n]+)', Path("/etc/os-release").read_text(), re.M)
    agent = subprocess.run(["sha256sum", args.agent], capture_output=True, text=True).stdout[:12] if args.agent else "-"
    return [f"- Kernel: {platform.release()} ({distro[1] if distro else '?'})",
            f"- CPU: {cpu[1] if cpu else platform.machine()}, {os.cpu_count()} vCPU; memory {mem:.1f} GiB",
            f"- Python {platform.python_version()}, {subprocess.run(['openssl', 'version'], capture_output=True, text=True).stdout.strip()}",
            f"- Agent sha256 {agent}…; RUST_LOG=info, no OTLP endpoint",
            f"- {args.workers} client processes, {args.rate} req/s for fixed-rate cases, {args.duration} s per run, "
            f"{args.reps} runs per cell (median), seed {args.seed}"]


def steal_ticks():
    """(steal, total) CPU ticks of the whole machine; steal is time a hypervisor gave to other guests."""
    v = [int(x) for x in Path("/proc/stat").read_text().split("\n", 1)[0].split()[1:9]]
    return v[7], sum(v)


def report(args, rows, start, steal):
    med = lambda case, on, key: statistics.median(r[key] for r in rows[case, on])
    lines = [f"# honeybeepf-llm overhead ({time.strftime('%Y-%m-%d')})", "", *environment(args),
             f"- CPU steal during the runs: {steal:.1%} (a busy VM host makes every number noisier)", "",
             "## Application impact", "",
             "| case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |", "|---|---|---|---|---|---|---|"]
    for case in args.cases:
        on = (case, True) in rows
        cell = lambda key: f"{med(case, False, key):.2f} | " + (f"{med(case, True, key):.2f}" if on else "-")
        lines.append(f"| {case} | {cell('rps')} | {cell('p50')} | {cell('p99')} |")
    if args.agent:
        lines += ["", "## Agent cost", "",
                  "| case | CPU s / 1k req | RSS anon MB | RSS file MB | PSS MB | BPF maps MB | tokens recorded / returned |",
                  "|---|---|---|---|---|---|---|"]
        for case in args.cases:
            mem = [statistics.median(r["mem"][i] for r in rows[case, True]) for i in range(4)]
            sent = sum(r["sent"] for r in rows[case, True])
            acc = f"{sum(r['recorded'] for r in rows[case, True]) / sent:.2%}" if sent else "no LLM calls"
            lines.append(f"| {case} | {med(case, True, 'cpu'):.3f} | " + " | ".join(f"{m:.1f}" for m in mem) + f" | {acc} |")
        lines += ["", "Where calls were lost (per run: `LLM UNPARSED` lines; the agent's kernel counters at exit):", ""]
        lines += [f"- `{case}`: {r['unparsed']} unparsed; {r['stats']}" for case in args.cases for r in rows[case, True]
                  if r["recorded"] < r["sent"]] or ["- none"]
        lines += ["", "## Startup", "", f"Agent start to `{READY}` with {args.processes} idle processes: {start:.2f} s (median)"]
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--agent", help="agent binary; omit to run the baseline only")
    ap.add_argument("--rate", type=float, default=200, help="requests/s for the fixed-rate cases")
    ap.add_argument("--duration", type=float, default=30)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--processes", type=int, default=1000)
    ap.add_argument("--seed", type=int, default=93)
    ap.add_argument("--cases", nargs="+", choices=list(CASES), default=list(CASES))
    ap.add_argument("--out")
    args = ap.parse_args()
    if args.agent and os.geteuid() != 0:
        sys.exit("run as root: the agent loads eBPF programs")
    if not socket.socket().connect_ex(("127.0.0.1", PORT)):
        sys.exit(f"port {PORT} is already in use")
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))  # runs the cleanup below
    with tempfile.TemporaryDirectory(prefix="hb-overhead-") as d:
        tmp = Path(d)
        subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
                        "-subj", "/CN=bench", "-days", "2", "-keyout", tmp / "key.pem", "-out", tmp / "cert.pem"],
                       check=True, capture_output=True)
        rows, s0 = {}, steal_ticks()
        try:
            server = spawn([sys.executable, __file__, "mock", tmp / "cert.pem", tmp / "key.pem"])
            while socket.socket().connect_ex(("127.0.0.1", PORT)):
                if server.poll() is not None:
                    sys.exit("the mock did not start")
                time.sleep(0.05)
            # A discarded pass: the first runs after boot are slower (cold caches, CPU frequency).
            for case in args.cases:
                run_case(args, tmp, case, False, args.seed, 5)
            for rep in range(args.reps):
                # Alternate the order so drift over the run does not favor one side.
                for on in ([False, True] if rep % 2 == 0 else [True, False]) if args.agent else [False]:
                    for case in args.cases:
                        rows.setdefault((case, on), []).append(run_case(args, tmp, case, on, args.seed + rep))
                        print(case, "on" if on else "off", rows[case, on][-1], file=sys.stderr)
            start = startup(args, tmp) if args.agent else None
        finally:
            for p in CHILDREN:
                p.kill()
                p.wait()
        s1 = steal_ticks()
        md = report(args, rows, start, (s1[0] - s0[0]) / max(1, s1[1] - s0[1]))
    print(md)
    if args.out:
        Path(args.out).write_text(md)


if __name__ == "__main__":
    if sys.argv[1:2] == ["mock"]:
        mock(*sys.argv[2:])
    elif sys.argv[1:2] == ["client"]:
        a = sys.argv[2:]
        client(int(a[0]), float(a[1]), float(a[2]), float(a[3]), a[4])
    else:
        main()
