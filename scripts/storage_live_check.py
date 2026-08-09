#!/usr/bin/env python3
"""Live end-to-end check for localStorage persistence + Playwright storageState.

Unit tests can prove the jar; only this can prove the *wiring* — that page JS
reaches the jar, that the jar reaches disk, and that a second process picks it
up. Every assertion here is a measurement of observable state (a value read back
by page script, a file on disk), never "the call returned".

Two harness rules learned the hard way, enforced below:
  * every port is pre-checked before use — a stale listener from a previous run
    will happily serve the tests and make a broken build read green;
  * the fixture server's bind is asserted, not assumed.

Usage:  python3 scripts/storage_live_check.py [--binary target/debug/obscura]
"""

import argparse
import http.server
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NODE_PATHS = [
    Path.home() / "Documents/erpfit/seo/node_modules",
    Path.home() / "Documents/erpfit/prod-env/node_modules",
]

PAGES = {
    "/set.html": """<!doctype html><title>set</title><script>
      localStorage.setItem('token', 'jwt-abc123');
      localStorage.setItem('second', 'two');
      sessionStorage.setItem('ephemeral', 'nope');
    </script><p>set</p>""",
    "/read.html": """<!doctype html><title>read</title><p>read</p>""",
}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = PAGES.get(self.path.split("?")[0])
        if body is None:
            self.send_error(404)
            return
        raw = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_args):
        pass


def free_port(preferred: int) -> int:
    """Return `preferred` if nothing is listening on it, else fail loudly.

    Not "find any free port": a stale obscura or fixture server on the expected
    port is exactly the failure this guards against, and silently sliding to
    another port would hide it.
    """
    with socket.socket() as s:
        s.settimeout(0.3)
        if s.connect_ex(("127.0.0.1", preferred)) == 0:
            sys.exit(f"FATAL: port {preferred} is already in use — kill the stale listener first")
    return preferred


class Results:
    def __init__(self):
        self.rows = []

    def check(self, name, ok, detail=""):
        self.rows.append((name, bool(ok), detail))
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  — {detail}" if detail else ""))

    @property
    def failed(self):
        return [r for r in self.rows if not r[1]]


def run_fetch(binary, url, storage_dir, expr, timeout=60):
    proc = subprocess.run(
        [
            binary,
            # --storage-dir and --allow-private-network are *global* flags and
            # must precede the subcommand; loopback is SSRF-blocked by default.
            "--allow-private-network",
            "fetch",
            url,
            "--storage-dir",
            str(storage_dir),
            "--eval",
            expr,
            "--quiet",
        ],
        capture_output=True,
        text=True,
        timeout=timeout,
        cwd=ROOT,
    )
    return proc.stdout.strip(), proc.stderr.strip(), proc.returncode


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target/debug/obscura"))
    ap.add_argument("--http-port", type=int, default=8791)
    ap.add_argument("--cdp-port", type=int, default=9333)
    args = ap.parse_args()

    binary = args.binary
    if not Path(binary).exists():
        sys.exit(f"FATAL: {binary} not built — cargo build -p obscura-cli")

    http_port = free_port(args.http_port)
    cdp_port = free_port(args.cdp_port)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", http_port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    # Assert the bind actually took, rather than trusting the constructor.
    with socket.socket() as s:
        s.settimeout(1.0)
        if s.connect_ex(("127.0.0.1", http_port)) != 0:
            sys.exit("FATAL: fixture server did not come up")

    base = f"http://127.0.0.1:{http_port}"
    r = Results()
    tmp = Path(tempfile.mkdtemp(prefix="obscura-storage-live-"))
    profile = tmp / "profile"

    try:
        print("\n== CLI: localStorage survives a process restart ==")
        out, err, rc = run_fetch(binary, f"{base}/set.html", profile, "localStorage.getItem('token')")
        r.check("run 1 writes and reads back in-process", "jwt-abc123" in out, out or err)

        storage_file = profile / "storage.json"
        r.check("storage.json written on exit", storage_file.exists(), str(storage_file))
        if storage_file.exists():
            data = json.loads(storage_file.read_text())
            origins = {e["origin"] for e in data}
            r.check("origin is the page origin", origins == {base}, str(origins))
            names = {i["name"] for e in data for i in e["local_storage"]}
            r.check("both localStorage keys persisted", names == {"token", "second"}, str(names))
            r.check("sessionStorage NOT persisted", "ephemeral" not in names, str(names))

        out, err, rc = run_fetch(binary, f"{base}/read.html", profile, "localStorage.getItem('token')")
        r.check("run 2 (new process) reads the token back", "jwt-abc123" in out, out or err)

        out, _, _ = run_fetch(binary, f"{base}/read.html", profile, "sessionStorage.getItem('ephemeral')")
        r.check("sessionStorage did not survive the restart", "nope" not in out, out)

        print("\n== Origin isolation ==")
        # localhost and 127.0.0.1 are different origins; a shared jar keyed
        # wrongly (or keyed by JS-supplied origin) would leak here.
        out, _, _ = run_fetch(
            binary, f"http://localhost:{http_port}/read.html", profile, "localStorage.getItem('token')"
        )
        r.check("a different origin cannot see the token", "jwt-abc123" not in out, out)

        print("\n== Playwright-shaped storageState round-trip (MCP tools) ==")
        state = mcp_roundtrip(binary, profile, base, r)

        print("\n== CDP: DOMStorage + seeded session (Puppeteer) ==")
        cdp_check(binary, cdp_port, base, r, state)

    finally:
        server.shutdown()
        shutil.rmtree(tmp, ignore_errors=True)

    print("\n" + "=" * 60)
    total = len(r.rows)
    print(f"{total - len(r.failed)}/{total} checks passed")
    for name, _, detail in r.failed:
        print(f"  FAILED: {name} — {detail}")
    return 1 if r.failed else 0


def mcp_roundtrip(binary, profile, base, r):
    """Drive the MCP server over stdio: navigate, export, import into a fresh
    profile, and prove page script sees the imported value."""
    def rpc(proc, method, params, rid):
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
        proc.stdin.flush()
        while True:
            line = proc.stdout.readline()
            if not line:
                return None
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == rid:
                return msg

    def text_of(msg):
        try:
            return msg["result"]["content"][0]["text"]
        except (KeyError, IndexError, TypeError):
            return json.dumps(msg)

    env = dict(os.environ)
    proc = subprocess.Popen(
        [binary, "--allow-private-network", "--storage-dir", str(profile), "mcp"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, cwd=ROOT, env=env,
    )
    state = None
    try:
        rpc(proc, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}}, 1)
        rpc(proc, "tools/call", {"name": "browser_navigate", "arguments": {"url": f"{base}/read.html"}}, 2)
        exported = text_of(rpc(proc, "tools/call", {"name": "browser_storage_state", "arguments": {}}, 3))
        try:
            state = json.loads(exported)
        except json.JSONDecodeError:
            r.check("storage_state exports JSON", False, exported[:200])
            return None
        origins = state.get("origins", [])
        entry = next((o for o in origins if o["origin"] == base), None)
        r.check("export carries the visited origin", entry is not None, str([o.get("origin") for o in origins]))
        if entry:
            shape_ok = all(set(i) >= {"name", "value"} for i in entry["localStorage"])
            r.check("localStorage items use Playwright's {name,value} shape", shape_ok, json.dumps(entry)[:160])
            r.check(
                "token present in export",
                any(i["name"] == "token" and i["value"] == "jwt-abc123" for i in entry["localStorage"]),
                json.dumps(entry)[:160],
            )
        r.check("cookies key present (Playwright shape)", isinstance(state.get("cookies"), list))
    finally:
        proc.terminate()
        proc.wait(timeout=10)

    if state is None:
        return None

    # Import into a *clean* profile and prove page JS reads it.
    fresh = profile.parent / "imported"
    proc = subprocess.Popen(
        [binary, "--allow-private-network", "--storage-dir", str(fresh), "mcp"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, cwd=ROOT,
    )
    try:
        rpc(proc, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}}, 1)
        # Seed BEFORE any navigation — the point of a Rust-native import.
        rpc(proc, "tools/call", {"name": "browser_set_storage_state", "arguments": {"state": state}}, 2)
        rpc(proc, "tools/call", {"name": "browser_navigate", "arguments": {"url": f"{base}/read.html"}}, 3)
        got = text_of(rpc(proc, "tools/call",
                          {"name": "browser_evaluate",
                           "arguments": {"expression": "localStorage.getItem('token')"}}, 4))
        r.check("imported state is visible to page script before login", "jwt-abc123" in got, got[:160])
    finally:
        proc.terminate()
        proc.wait(timeout=10)
    return state


def cdp_check(binary, cdp_port, base, r, state):
    node_path = ":".join(str(p) for p in NODE_PATHS if p.exists())
    if not node_path:
        r.check("puppeteer available", False, "no puppeteer-core found; CDP checks skipped")
        return

    profile = Path(tempfile.mkdtemp(prefix="obscura-cdp-live-"))
    server = subprocess.Popen(
        [binary, "--allow-private-network", "--storage-dir", str(profile),
         "serve", "--port", str(cdp_port)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=ROOT,
    )
    try:
        deadline = time.time() + 30
        up = False
        while time.time() < deadline:
            with socket.socket() as s:
                s.settimeout(0.3)
                if s.connect_ex(("127.0.0.1", cdp_port)) == 0:
                    up = True
                    break
            time.sleep(0.3)
        if not up:
            r.check("CDP server started", False, f"nothing listening on {cdp_port}")
            return

        driver = ROOT / "scripts" / "storage_cdp_driver.js"
        proc = subprocess.run(
            ["node", str(driver), f"http://127.0.0.1:{cdp_port}", base],
            capture_output=True, text=True, timeout=180,
            env={**os.environ, "NODE_PATH": node_path},
        )
        try:
            out = json.loads(proc.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            r.check("CDP driver ran", False, (proc.stdout + proc.stderr)[-300:])
            return
        for name, ok, detail in out.get("checks", []):
            r.check(f"CDP: {name}", ok, detail)
    finally:
        server.terminate()
        try:
            server.wait(timeout=15)
        except subprocess.TimeoutExpired:
            server.kill()
        shutil.rmtree(profile, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
