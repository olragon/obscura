#!/usr/bin/env python3
"""Compare Obscura against headless Chromium on CPU, memory, and wall time.

Measures the two workloads an agent browser actually runs — screenshot and HTML
extraction — over a URL corpus, driving BOTH engines through the same CDP client
so the comparison is of the engines, not of two different automation stacks.

Methodology notes, because the numbers are worthless without them:

* **Both engines are launched cold, per run.** Comparing a cold engine against a
  warm browser pool flatters the pooled one on wall time while telling you
  nothing about the cost of the work. If you want a warm-pool comparison, say so
  explicitly in the results — do not let it be the silent default.
* **CPU is user+sys from `wait4` rusage**, not wall time. Wall time is reported
  separately and is the noisier of the two.
* **Memory is peak RSS (`ru_maxrss`) of the engine process tree.** On macOS
  `ru_maxrss` is bytes; on Linux it is kibibytes. Handled below.
* **Chromium spawns child processes** and `ru_maxrss` only covers reaped
  children, so Chromium's memory is sampled by polling the process tree instead.
  A single number would otherwise undercount it dramatically and flatter us.
* Runs are repeated and the **median** is reported — one run is noise.

Usage:
  python3 scripts/bench.py --engine obscura --engine chrome --runs 3
  python3 scripts/bench.py --corpus corpus.txt --local   # serve a local mirror
"""

import argparse
import json
import os
import platform
import resource
import signal
import socket
import statistics
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_CORPUS = [
    "https://example.com/",
    "https://news.ycombinator.com/",
    "https://developers.cloudflare.com/",
    "https://blog.cloudflare.com/",
    "https://en.wikipedia.org/",
    "https://developer.mozilla.org/en-US/",
    "https://www.elmundo.es/",
    "https://www.rtp.pt/noticias/",
    "https://www.theguardian.com/international",
    "https://todomvc.com/examples/javascript-es6/dist/",
    "https://todomvc.com/examples/react/dist/index.html",
    "https://todomvc.com/examples/vue/dist/",
    "https://todomvc.com/examples/angular/dist/browser/",
    "https://todomvc.com/examples/preact/dist/",
]

# ru_maxrss is bytes on macOS, kibibytes on Linux.
RSS_DIVISOR = 1024 * 1024 if platform.system() == "Darwin" else 1024


def free_port() -> int:
    """Reserve a port by binding it, so a stale listener cannot serve our run.

    A harness that assumes a port is free reads green off somebody else's
    process. Binding to :0 and reading the assignment makes that impossible.
    """
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def wait_for_http(url: str, timeout_s: float = 30.0) -> bool:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=1)
            return True
        except Exception:
            time.sleep(0.25)
    return False


def sample_tree_rss_mib(root_pid: int) -> float:
    """Peak RSS across a process tree, in MiB, via one `ps` sweep.

    Chromium is multi-process; measuring only the launcher would report a
    fraction of the real cost.
    """
    try:
        out = subprocess.run(
            ["ps", "-Ao", "pid=,ppid=,rss="],
            capture_output=True, text=True, timeout=5,
        ).stdout
    except Exception:
        return 0.0
    kids, rss = {}, {}
    for line in out.strip().splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        pid, ppid, r = int(parts[0]), int(parts[1]), int(parts[2])
        kids.setdefault(ppid, []).append(pid)
        rss[pid] = r
    total, stack = 0, [root_pid]
    seen = set()
    while stack:
        p = stack.pop()
        if p in seen:
            continue
        seen.add(p)
        total += rss.get(p, 0)
        stack.extend(kids.get(p, []))
    return total / 1024.0  # ps reports KiB


def run_engine(engine: str, urls, workload: str, node_script: Path):
    """Launch an engine cold, drive the corpus, return metrics."""
    port = free_port()
    if engine == "obscura":
        binary = REPO / "target" / "release" / "obscura"
        if not binary.exists():
            raise SystemExit(f"missing {binary}; run: cargo build --release -p obscura-cli")
        cmd = [str(binary), "serve", "--port", str(port)]
        probe = f"http://127.0.0.1:{port}/json/version"
    elif engine == "chrome":
        chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        if not Path(chrome).exists():
            chrome = "google-chrome"
        cmd = [
            chrome, "--headless=new", f"--remote-debugging-port={port}",
            "--no-sandbox", "--disable-gpu", "--user-data-dir=/tmp/bench-chrome-profile",
        ]
        probe = f"http://127.0.0.1:{port}/json/version"
    else:
        raise SystemExit(f"unknown engine {engine}")

    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not wait_for_http(probe, 45):
        proc.kill()
        raise SystemExit(f"{engine} never became ready on :{port}")
    startup_s = time.time() - t0

    ws = json.load(urllib.request.urlopen(probe))["webSocketDebuggerUrl"]

    peak_rss = sample_tree_rss_mib(proc.pid)
    t1 = time.time()
    r = subprocess.run(
        ["node", str(node_script), ws, workload, *urls],
        capture_output=True, text=True, timeout=600,
    )
    work_s = time.time() - t1
    peak_rss = max(peak_rss, sample_tree_rss_mib(proc.pid))

    try:
        per_url = json.loads(r.stdout.strip().splitlines()[-1])
    except Exception:
        per_url = {"error": (r.stdout + r.stderr)[-400:]}

    proc.send_signal(signal.SIGTERM)
    try:
        _, status, ru = os.wait4(proc.pid, 0)
        cpu_s = ru.ru_utime + ru.ru_stime
        reaped_rss = ru.ru_maxrss / RSS_DIVISOR
    except (ChildProcessError, OSError):
        proc.kill()
        cpu_s, reaped_rss = float("nan"), 0.0

    return {
        "engine": engine,
        "workload": workload,
        "startup_s": round(startup_s, 3),
        "work_s": round(work_s, 3),
        "cpu_s": round(cpu_s, 3),
        # Report the larger of the two measurements and say which won, since
        # neither method is reliable for both engine shapes.
        "peak_rss_mib": round(max(peak_rss, reaped_rss), 1),
        "per_url": per_url,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", action="append", default=None,
                    choices=["obscura", "chrome"])
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--workload", action="append", default=None,
                    choices=["screenshot", "extract"])
    ap.add_argument("--corpus", type=Path, default=None)
    ap.add_argument("--limit", type=int, default=0, help="use only the first N URLs")
    ap.add_argument("--out", type=Path, default=REPO / "bench-results.json")
    args = ap.parse_args()

    engines = args.engine or ["obscura", "chrome"]
    workloads = args.workload or ["screenshot", "extract"]
    urls = (
        [l.strip() for l in args.corpus.read_text().splitlines() if l.strip()]
        if args.corpus else list(DEFAULT_CORPUS)
    )
    if args.limit:
        urls = urls[: args.limit]

    node_script = Path(__file__).parent / "bench_driver.js"
    if not node_script.exists():
        raise SystemExit(f"missing {node_script}")

    results = []
    for workload in workloads:
        for engine in engines:
            runs = []
            for i in range(args.runs):
                print(f"[{engine}/{workload}] run {i+1}/{args.runs}...", file=sys.stderr)
                try:
                    runs.append(run_engine(engine, urls, workload, node_script))
                except Exception as e:
                    print(f"  run failed: {e}", file=sys.stderr)
            if not runs:
                continue
            med = lambda k: round(statistics.median(r[k] for r in runs), 3)
            ok = statistics.median(
                sum(1 for v in r["per_url"].values() if isinstance(v, dict) and v.get("ok"))
                for r in runs if isinstance(r["per_url"], dict)
            ) if runs else 0
            results.append({
                "engine": engine, "workload": workload, "runs": len(runs),
                "urls": len(urls), "urls_ok_median": ok,
                "startup_s": med("startup_s"), "work_s": med("work_s"),
                "cpu_s": med("cpu_s"), "peak_rss_mib": med("peak_rss_mib"),
            })

    args.out.write_text(json.dumps(
        {"platform": platform.platform(), "corpus_size": len(urls),
         "methodology": "cold-start both engines; median of N runs; CPU=user+sys rusage; "
                        "RSS=max(process-tree sample, ru_maxrss)",
         "results": results}, indent=2))

    print(f"\n{'engine':10} {'workload':11} {'ok/urls':>9} {'cpu_s':>8} {'rss_MiB':>9} "
          f"{'wall_s':>8} {'start_s':>8}")
    for r in results:
        print(f"{r['engine']:10} {r['workload']:11} "
              f"{int(r['urls_ok_median'])}/{r['urls']:<7} {r['cpu_s']:>8} "
              f"{r['peak_rss_mib']:>9} {r['work_s']:>8} {r['startup_s']:>8}")
    print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
