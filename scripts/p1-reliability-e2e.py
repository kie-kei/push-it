#!/usr/bin/env python3
"""Roadmap P1 reliability smoke: deploy failure injection + restart consistency.

Every failing deploy (bad archive / bad manifest / invalid component / missing
secret / malformed capabilities) must be rejected without disturbing the
serving revision. Rollback is verified on two distinct revisions (twice, and
with an invalid target), and the active revision must survive controller
restarts and still match `status` and what is actually served.
"""
import io
import json
import os
from pathlib import Path
import socket
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


def archive(marker=None, wasm=WASM, with_wasm=True):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        if with_wasm:
            tar.add(wasm, arcname="app.wasm")
        if marker is not None:
            data = f"<title>{marker}</title>".encode()
            entry = tarfile.TarInfo("dist/index.html")
            entry.size = len(data)
            tar.addfile(entry, io.BytesIO(data))
    return buf.getvalue()


def deploy(port, name, manifest, body, expect=200):
    boundary = "pushit-p1-smoke"
    payload = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"manifest\"\r\n\r\n{manifest}\r\n"
               f"--{boundary}\r\nContent-Disposition: form-data; name=\"archive\"; filename=\"app.tar.gz\"\r\n"
               "Content-Type: application/gzip\r\n\r\n").encode() + body + f"\r\n--{boundary}--\r\n".encode()
    status, data = request(port, f"/api/apps/{name}/deploy", "POST", payload,
                           headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert status == expect, (status, data)
    return json.loads(data)["revision"] if status == 200 else data


def start(data):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_")}
    env.update(PUSHIT_DATA=str(data), PUSHIT_BIND=f"127.0.0.1:{port}",
               PUSHIT_DEV="1", PUSHIT_SESSION_KEY="test-only-not-for-production-0123456789")
    log = open(data / "controller.log", "ab")
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


def serving(port, revision, env_value, marker):
    """The active revision must match status AND what is actually served."""
    detail = json.loads(request(port, "/api/apps/hello")[1])
    active = [r for r in detail["revisions"] if r["active"]]
    assert [r["id"] for r in active] == [revision], (revision, detail["revisions"])
    assert request(port, "/", host="hello.localhost")[1].decode().count(marker) == 1
    assert request(port, "/_fn/hello", host="hello.localhost")[0] == 200
    assert request(port, "/config.js", host="hello.localhost")[1] == \
        f'window.__ENV__ = {{"VERSION":"{env_value}"}};\n'.encode()


def revisions(port):
    return [v["id"] for v in json.loads(request(port, "/api/apps/hello/versions")[1])]


def main():
    assert BIN.is_file() and WASM.is_file(), "build workspace binaries and the wasm32-wasip2 example first"
    with tempfile.TemporaryDirectory(prefix="pushit-p1-") as tmp:
        data = Path(tmp)
        data.mkdir(exist_ok=True)
        proc, port, log = start(data)
        try:
            manifest_a = "name: hello\nenv:\n  VERSION: A\nsecrets: [KEY]\ncapabilities:\n  kv: [local]\n"
            request(port, "/api/apps/hello/secrets/KEY", "PUT", b"secret-value")
            a = deploy(port, "hello", manifest_a, archive("revision-A"))
            serving(port, a, "A", "revision-A")

            # --- failure injection: every bad deploy is rejected and leaves A serving ---
            (Path(tmp) / "fake.wasm").write_bytes(os.urandom(2048))
            bad_deploys = [
                ("archive without app.wasm", manifest_a, archive("no-wasm", with_wasm=False)),
                ("invalid manifest yaml", "name: hello\nenv: [", archive("x")),
                ("manifest name mismatch", "name: other\n", archive("x")),
                ("manifest without name", "env: {}\n", archive("x")),
                ("random bytes instead of a component", manifest_a,
                 archive("bad-component", wasm=Path(tmp) / "fake.wasm")),
                ("declared secret never set", "name: hello\nsecrets: [NEVER_SET]\n", archive("x")),
                ("empty http_out allowlist", "name: hello\ncapabilities:\n  http_out: []\n", archive("x")),
            ]
            versions_before = revisions(port)
            for description, manifest, body in bad_deploys:
                deploy(port, "hello", manifest, body, expect=400)
                serving(port, a, "A", "revision-A")
            assert revisions(port) == versions_before, "a rejected deploy created a version"

            # --- rollback between two distinct revisions, verified twice ---
            manifest_b = "name: hello\nenv:\n  VERSION: B\nsecrets: [KEY]\ncapabilities:\n  kv: [local]\n"
            b = deploy(port, "hello", manifest_b, archive("revision-B"))
            assert a != b
            serving(port, b, "B", "revision-B")
            status, body = request(port, "/api/apps/hello/rollback", "POST", b"{}")
            assert status == 200 and json.loads(body)["revision"] == a, (status, body)
            serving(port, a, "A", "revision-A")  # first confirmation
            serving(port, a, "A", "revision-A")  # repeated check stays consistent
            status, body = request(port, "/api/apps/hello/rollback", "POST",
                                   json.dumps({"revision": b}).encode())
            assert status == 200, (status, body)
            serving(port, b, "B", "revision-B")
            # invalid rollback target changes nothing
            status, body = request(port, "/api/apps/hello/rollback", "POST",
                                   json.dumps({"revision": "rDEADBEEF"}).encode())
            assert status == 404, (status, body)
            serving(port, b, "B", "revision-B")

            # --- restart consistency: active revision survives restarts, then deploys still work ---
            for _ in range(2):
                stop(proc, log)
                proc, port, log = start(data)
                serving(port, b, "B", "revision-B")
                assert set(revisions(port)) == {a, b}
            manifest_c = "name: hello\nenv:\n  VERSION: C\nsecrets: [KEY]\ncapabilities:\n  kv: [local]\n"
            c = deploy(port, "hello", manifest_c, archive("revision-C"))
            assert c not in (a, b)
            serving(port, c, "C", "revision-C")
        finally:
            stop(proc, log)
    print("PASS: rejected deploys preserve the serving revision; rollback and restart consistency verified")


if __name__ == "__main__":
    main()
