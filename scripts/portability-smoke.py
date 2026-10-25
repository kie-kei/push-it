#!/usr/bin/env python3
"""Deploy identical built artifacts through the CLI to two fresh local instances.
Build workspace + examples/hello/fn for wasm32-wasip2 first. No external services.
"""
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
CONTROLLER = ROOT / "target/debug/pushit-controller"
CLI = ROOT / "target/debug/pushit"
WASM = ROOT / "examples/hello/fn/target/wasm32-wasip2/release/hello_fn.wasm"
SECRET = "PORTABILITY_TEST_SECRET"
MANIFEST = ("name: hello\ndomain: portable\nenv:\n  GREETING: portable-hello\n"
            f"secrets: [{SECRET}]\ncapabilities:\n  kv: [local]\n").encode()
UI = b"<!doctype html><title>portable fixture</title><script src='/config.js'></script>\n"
# Ignore ambient proxies too: every request must stay on loopback.
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Instance:
    def __init__(self, root, label):
        self.root = root / label
        self.root.mkdir()
        self.data = self.root / "data"
        self.domain = f"{label}.localhost"
        self.host = f"portable.{self.domain}"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        # Allowlist rather than inheriting user tokens, profiles, proxies or secrets.
        self.env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                    "HOME": str(self.root / "home"),
                    "XDG_CONFIG_HOME": str(self.root / "config"),
                    "XDG_DATA_HOME": str(self.root / "xdg-data"),
                    "XDG_CACHE_HOME": str(self.root / "cache")}
        self.log = None
        self.proc = None

    def request(self, path, host=None):
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}",
                                     headers={"Host": host or f"hub.{self.domain}"})
        try:
            with HTTP.open(req, timeout=30) as res:
                return res.status, res.read()
        except urllib.error.HTTPError as res:
            return res.code, res.read()

    def cli(self, *args, success=True):
        result = subprocess.run([str(CLI), "--dev", "--server",
                                 f"http://127.0.0.1:{self.port}", *map(str, args)],
                                cwd=self.root, env=self.env, capture_output=True, timeout=90)
        assert (result.returncode == 0) == success, result.stdout + result.stderr
        return result.stdout + result.stderr

    def start(self):
        env = dict(self.env, PUSHIT_DEV="1", PUSHIT_DATA=str(self.data),
                   PUSHIT_BASE_DOMAIN=self.domain, PUSHIT_BIND=f"127.0.0.1:{self.port}",
                   PUSHIT_SESSION_KEY=f"test-only-{self.domain}-0123456789abcdef")
        self.log = (self.root / "controller.log").open("wb")
        self.proc = subprocess.Popen([str(CONTROLLER)], cwd=self.root, env=env,
                                     stdout=self.log, stderr=self.log)
        for _ in range(200):
            assert self.proc.poll() is None, (self.root / "controller.log").read_text()
            try:
                status, body = self.request("/api/apps")
                if status == 200 and json.loads(body) == []:
                    return
            except (OSError, ValueError):
                pass
            time.sleep(0.1)
        raise AssertionError(f"controller readiness timed out: {self.domain}")

    def stop(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        if self.log is not None:
            self.log.close()

    def kv_file(self):
        # App-scoped kv store; read-only sanity, never copied or migrated.
        return self.data / "appdata/hello/local.kv"


def main():
    assert all(p.is_file() for p in (CONTROLLER, CLI, WASM)), "build workspace and WASM fixture first"
    with tempfile.TemporaryDirectory(prefix="pushit-portability-") as tmp:
        root = Path(tmp)
        manifest = root / "pushit.yaml"
        manifest.write_bytes(MANIFEST)
        dist = root / "dist"
        dist.mkdir()
        (dist / "index.html").write_bytes(UI)
        # Freeze the once-built inputs before either CLI deploy; no build commands.
        hashes = {p: digest(p) for p in (manifest, WASM, dist / "index.html")}
        a, b = Instance(root, "a"), None
        deploy = ("deploy", "--manifest", manifest, "--wasm", WASM, "--dist", dist)
        try:
            a.start()
            # Allocate B only after A is listening, so the OS cannot reuse A's port.
            b = Instance(root, "b")
            b.start()
            assert a.port != b.port
            for instance in (a, b):
                assert json.loads(instance.cli("list")) == []
                failure = instance.cli(*deploy, success=False)
                assert b"missing secrets" in failure and SECRET.encode() in failure
                assert json.loads(instance.cli("list")) == []
                assert instance.request("/api/apps/hello")[0] == 404
            # Provision A independently; B still cannot deploy with its missing secret.
            a.cli("secret", "set", "hello", f"{SECRET}=synthetic-a-only")
            a.cli(*deploy)
            b.cli(*deploy, success=False)
            assert json.loads(b.cli("list")) == []
            b.cli("secret", "set", "hello", f"{SECRET}=synthetic-b-only")
            b.cli(*deploy)
            expected_config = b'window.__ENV__ = {"GREETING":"portable-hello"};\n'
            for instance, other in ((a, b), (b, a)):
                detail = json.loads(instance.cli("status", "hello"))
                assert detail["manifest"].encode() == MANIFEST
                assert detail["domain"] == instance.host
                revisions = list((instance.data / "apps/hello").iterdir())
                assert len(revisions) == 1
                assert digest(revisions[0] / "app.wasm") == hashes[WASM]
                assert digest(revisions[0] / "dist/index.html") == hashes[dist / "index.html"]
                assert instance.request("/", instance.host) == (200, UI)
                assert instance.request("/config.js", instance.host) == (200, expected_config)
                assert SECRET.encode() not in expected_config
                for secret in (b"synthetic-a-only", b"synthetic-b-only"):
                    assert secret not in instance.request("/config.js", instance.host)[1]
                assert instance.request("/_fn/hello", instance.host) == (200, b'{"msg":"hello","user":null}')
                assert instance.request("/_fn/env", instance.host) == (200, b'{"GREETING":"portable-hello"}')
                assert instance.request("/", other.host)[0] == 404
                assert instance.request("/_fn/hello", other.host)[0] == 404
                assert not instance.kv_file().exists()
                assert instance.request("/_fn/kv", instance.host)[0] == 200
            assert a.kv_file().is_file() and b.kv_file().is_file()
            assert a.request("/_fn/kv", a.host)[0] == 200
            assert (a.data / "age.key").read_bytes() != (b.data / "age.key").read_bytes()
            a.stop()
            assert b.request("/_fn/hello", b.host)[0] == 200
            assert b.request("/config.js", b.host) == (200, expected_config)
            assert b.kv_file().is_file()
            assert all(digest(p) == sha for p, sha in hashes.items())
        finally:
            a.stop()
            if b is not None:
                b.stop()
    print("PASS: identical CLI artifacts/config on two isolated local instances; independent secrets/state")


if __name__ == "__main__":
    main()
