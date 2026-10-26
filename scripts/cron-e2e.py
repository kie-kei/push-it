#!/usr/bin/env python3
"""Run a manifest cron job against a real WASM guest using temporary state."""
import io
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/pushit-controller"


def build_fixture(root):
    fixture = root / "fixture"
    shutil.copytree(ROOT / "examples/hello/fn", fixture,
                    ignore=shutil.ignore_patterns("target"))
    subprocess.run(["cargo", "component", "build", "--manifest-path", str(fixture / "Cargo.toml"),
                    "--release", "--target", "wasm32-wasip1", "--locked", "--offline"],
                   cwd=ROOT, check=True, timeout=600)
    wasm = fixture / "target/wasm32-wasip1/release/hello_fn.wasm"
    assert wasm.is_file(), f"fixture wasm missing: {wasm}"
    return wasm


def request(port, path, method="GET", body=None, headers=None):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=body, method=method,
                                 headers={"Host": "hub.localhost", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as response:
        return response.code, response.read()


def deploy(port, wasm):
    manifest = 'name: cron-smoke\ncron:\n  - name: tick\n    schedule: "* * * * *"\n    path: /_fn/log\n'
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w:gz") as tar:
        tar.add(wasm, arcname="app.wasm")
    boundary = "pushit-cron-smoke"
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"manifest\"\r\n\r\n{manifest}\r\n"
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"archive\"; filename=\"app.tar.gz\"\r\n"
            "Content-Type: application/gzip\r\n\r\n").encode() + archive.getvalue() + f"\r\n--{boundary}--\r\n".encode()
    status, data = request(port, "/api/apps/cron-smoke/deploy", "POST", body,
                           headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert status == 200, (status, data)


def start(data):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_")}
    env.update(PUSHIT_DATA=str(data), PUSHIT_BIND=f"127.0.0.1:{port}", PUSHIT_DEV="1",
               PUSHIT_CRON_TICK_SECS="1", PUSHIT_SESSION_KEY="test-only-not-for-production-0123456789")
    log = open(data / "controller.log", "wb")
    proc = subprocess.Popen([str(BIN)], cwd=ROOT, env=env, stdout=log, stderr=log)
    try:
        for _ in range(100):
            if proc.poll() is not None:
                raise AssertionError((data / "controller.log").read_text())
            try:
                status, _ = request(port, "/api/apps")
                assert status == 200, status
                return proc, port, log
            except OSError:
                time.sleep(0.1)
        raise AssertionError("controller startup timed out")
    except BaseException:
        stop(proc, log)
        raise


def stop(proc, log):
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    log.close()


def main():
    subprocess.run(["cargo", "build", "-p", "pushit-controller", "--locked"],
                   cwd=ROOT, check=True, timeout=600)
    with tempfile.TemporaryDirectory(prefix="pushit-cron-") as tmp:
        root = Path(tmp)
        wasm = build_fixture(root)
        data = root / "data"
        data.mkdir()
        proc, port, log = start(data)
        try:
            deploy(port, wasm)
            deadline = time.monotonic() + 75
            logs = b""
            while time.monotonic() < deadline:
                assert proc.poll() is None, (data / "controller.log").read_text()
                status, logs = request(port, "/api/apps/cron-smoke/logs")
                assert status == 200, (status, logs)
                if (b"hello from wasi:logging" in logs
                        and b"cron 'tick' GET /_fn/log -> 200" in logs):
                    break
                time.sleep(0.25)
            else:
                raise AssertionError(f"cron did not produce guest and scheduler logs: {logs!r}\n"
                                     + (data / "controller.log").read_text())
        finally:
            stop(proc, log)
    print("PASS: manifest cron invoked real WASM guest and recorded guest + cron 'tick' logs")


if __name__ == "__main__":
    main()
