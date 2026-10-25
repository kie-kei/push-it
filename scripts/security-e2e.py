#!/usr/bin/env python3
"""Local smoke: build workspace + examples/hello/fn for wasm32-wasip2 first.
Uses only temporary data and loopback listeners; never touches a deployment.
"""
import io
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/pushit-controller"
WASM = ROOT / "examples/hello/fn/target/wasm32-wasip2/release/hello_fn.wasm"


def request(port, path, method="GET", body=None, host="hub.localhost", headers=None):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=body, method=method,
                                 headers={"Host": host, **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=30) as res:
            return res.status, res.read()
    except urllib.error.HTTPError as res:
        return res.code, res.read()


def deploy(port, manifest):
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w:gz") as tar:
        tar.add(WASM, arcname="app.wasm")
        data = b"<title>revision smoke</title>"
        entry = tarfile.TarInfo("dist/index.html")
        entry.size = len(data)
        tar.addfile(entry, io.BytesIO(data))
    boundary = "pushit-security-smoke"
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"manifest\"\r\n\r\n{manifest}\r\n"
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"archive\"; filename=\"app.tar.gz\"\r\n"
            "Content-Type: application/gzip\r\n\r\n").encode() + archive.getvalue() + f"\r\n--{boundary}--\r\n".encode()
    status, data = request(port, "/api/apps/hello/deploy", "POST", body,
                           headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert status == 200, (status, data)
    return json.loads(data)["revision"]


def start(data, dev):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_")}
    env.update(PUSHIT_DATA=str(data), PUSHIT_BIND=f"127.0.0.1:{port}",
               PUSHIT_DEV="1" if dev else "0", PUSHIT_SESSION_KEY="test-only-not-for-production-0123456789")
    log = open(data / "controller.log", "wb")
    proc = subprocess.Popen([str(BIN)], env=env, stdout=log, stderr=log)
    for _ in range(100):
        if proc.poll() is not None:
            raise AssertionError((data / "controller.log").read_text())
        try:
            request(port, "/api/apps")
            return proc, port, log
        except OSError:
            time.sleep(0.1)
    proc.kill()
    proc.wait()
    raise AssertionError("controller startup timed out")


def stop(proc, log):
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    log.close()


def main():
    assert BIN.is_file() and WASM.is_file(), "build workspace binaries and wasm32-wasip2 example first"
    # Fail before listening/data creation for missing, empty, short and known keys.
    for key in [None, "", "short", "pushit-dev-session-key"]:
        env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_")}
        if key is not None:
            env["PUSHIT_SESSION_KEY"] = key
        result = subprocess.run([str(BIN)], env=env, capture_output=True, timeout=10)
        assert result.returncode != 0 and b"PUSHIT_SESSION_KEY" in result.stderr
    with tempfile.TemporaryDirectory(prefix="pushit-security-") as tmp:
        data = Path(tmp)
        proc, port, log = start(data, False)
        try:
            for method, path in [("GET", "/api/apps"), ("DELETE", "/api/apps/hello"),
                                 ("PUT", "/api/apps/hello/secrets/KEY"), ("POST", "/api/tokens"),
                                 ("GET", "/auth/login"), ("GET", "/auth/device?port=12345"), ("GET", "/")]:
                assert request(port, path, method, headers={"X-Pushit-Dev": "1"})[0] == 401
        finally:
            stop(proc, log)
        proc, port, log = start(data, True)
        try:
            assert request(port, "/api/apps/hello/secrets/KEY", "PUT", b"before")[0] == 204
            manifest_a = "name: hello\ndomain: first\nenv:\n  VERSION: A\nsecrets: [KEY]\ncapabilities:\n  kv: [local]\n"
            a = deploy(port, manifest_a)
            assert request(port, "/_fn/hello", host="first.localhost")[0] == 200
            assert request(port, "/_fn/kv", host="first.localhost")[0] == 200
            b = deploy(port, "name: hello\ndomain: second\nenv:\n  VERSION: B\n")
            assert a != b
            assert request(port, "/config.js", host="second.localhost")[1] == b'window.__ENV__ = {"VERSION":"B"};\n'
            assert request(port, "/", host="first.localhost")[0] == 404
            assert request(port, "/api/apps/hello/secrets/KEY", "PUT", b"rotated")[0] == 204
            status, body = request(port, "/api/apps/hello/rollback", "POST", json.dumps({"revision": a}).encode())
            assert status == 200, (status, body)
            assert request(port, "/config.js", host="first.localhost")[1] == b'window.__ENV__ = {"VERSION":"A"};\n'
            assert request(port, "/_fn/kv", host="first.localhost")[0] == 200
            assert request(port, "/", host="second.localhost")[0] == 404
            detail = json.loads(request(port, "/api/apps/hello")[1])
            assert detail["manifest"] == manifest_a
            # Exercise public version management through genuine HTTP and CLI.
            with sqlite3.connect(data / "pushit.db") as conn:
                retained_secret = conn.execute("SELECT value_enc FROM secrets WHERE app='hello' AND key='KEY'").fetchone()[0]
            assert (data / "appdata" / "hello" / "local.kv").is_file()
            versions = json.loads(request(port, "/api/apps/hello/versions")[1])
            assert {v["id"] for v in versions} == {a, b}
            assert request(port, f"/api/apps/hello/versions/{a}", "DELETE")[0] == 409
            assert request(port, "/api/apps/hello/deactivate", "POST")[0] == 204
            assert request(port, "/api/apps/hello/deactivate", "POST")[0] == 204
            for host in ["first.localhost", "second.localhost"]:
                for path in ["/", "/config.js", "/_fn/hello", "/_fn/kv"]:
                    assert request(port, path, host=host)[0] == 404, (host, path)
            assert json.loads(request(port, "/api/apps")[1])[0]["status"] == "inactive"
            cli = ROOT / "target/debug/pushit"
            cli_env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_")}
            cli_env.update(HOME=str(data / "cli-home"), XDG_CONFIG_HOME=str(data / "cli-config"))
            def run_cli(*args, ok=True):
                result = subprocess.run([str(cli), "--server", f"http://127.0.0.1:{port}", "--dev", "version", *args],
                                        env=cli_env, capture_output=True, timeout=30)
                assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
                return result
            assert len(json.loads(run_cli("list", "hello").stdout)) == 2
            run_cli("activate", "hello", b)
            assert request(port, "/config.js", host="second.localhost")[1] == b'window.__ENV__ = {"VERSION":"B"};\n'
            run_cli("activate", "hello", a)
            assert request(port, "/_fn/hello", host="first.localhost")[0] == 200
            assert request(port, "/_fn/kv", host="first.localhost")[0] == 200
            run_cli("delete", "hello", a, "--yes", ok=False)
            run_cli("delete", "hello", b, "--yes")
            assert not (data / "apps" / "hello" / b).exists()
            assert (data / "apps" / "hello" / a / "app.wasm").is_file()
            assert (data / "appdata" / "hello" / "local.kv").is_file()
            assert request(port, f"/api/apps/hello/versions/{b}/activate", "POST")[0] == 404
            assert request(port, f"/api/apps/hello/versions/{b}", "DELETE")[0] == 404
            with sqlite3.connect(data / "pushit.db") as conn:
                assert conn.execute("SELECT value_enc FROM secrets WHERE app='hello' AND key='KEY'").fetchone()[0] == retained_secret
            run_cli("deactivate", "hello")
            assert request(port, "/", host="first.localhost")[0] == 404
            run_cli("activate", "hello", a)
            assert request(port, "/_fn/hello", host="first.localhost")[0] == 200
        finally:
            stop(proc, log)
    print("PASS: production startup/auth, real WASM deploy/rollback and HTTP + CLI version lifecycle")


if __name__ == "__main__":
    main()
