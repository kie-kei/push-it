#!/usr/bin/env python3
"""Real HTTP/WASM regression: streamed responses and timed-out guests pin versions.
Build workspace binaries first. Uses the pinned Rust toolchain and the example's
locked dependency set to build a temporary fixture; no live deployment is touched.
"""
import concurrent.futures
import importlib.util
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
import queue
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("security_smoke", ROOT / "scripts/security-e2e.py")
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))

# The ordinary example stays unchanged. Build a fixture from its checked-in
# bindings/WIT and lockfile, adding only controlled guest lifetime test routes.
# The gate needs a blocking external host call, so the fixture re-vendors
# wasi:sql (the example itself dropped it together with the built-in sqlite).
GUEST = r'''
        let wait_for_release = || {
            let connection = bindings::wasi::sql::types::Connection::open("gate").unwrap();
            let statement = bindings::wasi::sql::types::Statement::prepare("SELECT 1", &[])
                .unwrap();
            bindings::wasi::sql::readwrite::query(&connection, &statement).unwrap();
        };
        if path == "/_fn/lifecycle-stream" {
            let response = OutgoingResponse::new(Fields::new());
            let body = response.body().unwrap();
            ResponseOutparam::set(out, Ok(response));
            let stream = body.write().unwrap();
            stream.blocking_write_and_flush(b"first").unwrap();
            wait_for_release();
            stream.blocking_write_and_flush(b"last").unwrap();
            drop(stream);
            OutgoingBody::finish(body, None).unwrap();
            return;
        }
        if path == "/_fn/lifecycle-timeout" {
            // The controller times out at 30 seconds, while this host query is
            // deliberately blocked until the test explicitly releases it.
            wait_for_release();
        }
'''


def wait_for(description, check, seconds):
    deadline = time.monotonic() + seconds
    while True:
        if check():
            return
        assert time.monotonic() < deadline, f"timed out: {description}"
        time.sleep(0.05)


def delete_after_guest_finishes(port, version):
    def deleted():
        status, body = smoke.request(port, f"/api/apps/hello/versions/{version}", "DELETE")
        assert status in (204, 409), (status, body)
        return status == 204
    wait_for("guest finishes and releases version", deleted, 15)


def timeout_request(port):
    request = urllib.request.Request(f"http://127.0.0.1:{port}/_fn/lifecycle-timeout",
                                     headers={"Host": "hello.localhost"})
    try:
        with HTTP.open(request, timeout=45) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as response:
        return response.code, response.read()


def main():
    assert smoke.BIN.is_file(), "build workspace binaries first"
    with tempfile.TemporaryDirectory(prefix="pushit-version-lifetime-") as tmp:
        root = Path(tmp)
        fixture = root / "fixture"
        shutil.copytree(ROOT / "examples/hello/fn", fixture, ignore=shutil.ignore_patterns("target"))
        sql_wit = fixture / "wit/deps/sql"
        sql_wit.mkdir()
        for name in ("types.wit", "readwrite.wit"):
            shutil.copy(ROOT / "wit/sql" / name, sql_wit / name)
        world = fixture / "wit/world.wit"
        marker_world = "    import wasi:keyvalue/store@0.2.0-draft;\n"
        assert world.read_text().count(marker_world) == 1
        world.write_text(world.read_text().replace(
            marker_world,
            marker_world
            + "    import wasi:sql/types@0.2.0-draft;\n"
            + "    import wasi:sql/readwrite@0.2.0-draft;\n"))
        toml = fixture / "Cargo.toml"
        marker_toml = '"wasi:keyvalue" = { path = "wit/deps/keyvalue" }'
        assert toml.read_text().count(marker_toml) == 1
        toml.write_text(toml.read_text().replace(
            marker_toml,
            marker_toml + '\n"wasi:sql" = { path = "wit/deps/sql" }'))
        source = fixture / "src/lib.rs"
        marker = '        let path = path.split(\'?\').next().unwrap_or("/");\n'
        text = source.read_text()
        assert text.count(marker) == 1, "example handler changed; update fixture insertion point"
        source.write_text(text.replace(marker, marker + GUEST))
        # cargo-component regenerates bindings.rs for the patched world.
        subprocess.run(["cargo", "component", "build", "--release",
                        "--target", "wasm32-wasip2", "--locked", "--offline"],
                       cwd=fixture, check=True,
                       env={**os.environ, "CARGO_TARGET_DIR": str(root / "target")}, timeout=300)
        smoke.WASM = root / "target/wasm32-wasip2/release/hello_fn.wasm"
        data = root / "data"
        data.mkdir()
        gates = queue.Queue()
        pending = []
        class GateHandler(BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", "0")))
                release = threading.Event()
                pending.append(release)
                gates.put(release)
                if not release.wait(90):
                    self.send_error(504, "fixture release timeout")
                    return
                payload = b'{"n":1}\n'
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                try:
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass
            def log_message(self, *_args):
                pass
        gate_server = ThreadingHTTPServer(("127.0.0.1", 0), GateHandler)
        gate_thread = threading.Thread(target=gate_server.serve_forever, daemon=True)
        gate_thread.start()
        proc = log = None
        try:
            proc, port, log = smoke.start(data, True)
            dsn = f"clickhouse://127.0.0.1:{gate_server.server_port}/"
            assert smoke.request(port, "/api/apps/hello/secrets/GATE_DSN", "PUT", dsn.encode())[0] == 204
            manifest = "name: hello\ncapabilities:\n  sql: [gate]\nsql_bindings:\n  gate: secret:GATE_DSN\n"
            version = smoke.deploy(port, manifest)
            request = urllib.request.Request(f"http://127.0.0.1:{port}/_fn/lifecycle-stream",
                                             headers={"Host": "hello.localhost"})
            with HTTP.open(request, timeout=15) as response:
                # Actual body bytes prove the guest has set response-outparam.
                assert response.read(5) == b"first"
                stream_release = gates.get(timeout=10)
                assert smoke.request(port, "/api/apps/hello/deactivate", "POST")[0] == 204
                status, body = smoke.request(port, f"/api/apps/hello/versions/{version}", "DELETE")
                assert status == 409 and b"in-flight" in body, (status, body)
                assert (data / "apps/hello" / version).exists()
                newer = smoke.deploy(port, manifest)
                assert newer != version
                assert smoke.request(port, "/", host="hello.localhost")[0] == 200
                stream_release.set()
                assert response.read() == b"last"
            delete_after_guest_finishes(port, version)
            assert not (data / "apps/hello" / version).exists()
            assert (data / "apps/hello" / newer).exists()

            # The actual host SQL request is the readiness handshake; its HTTP
            # response is withheld until after deletion has been rejected.
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                result = executor.submit(timeout_request, port)
                timeout_release = gates.get(timeout=10)
                assert smoke.request(port, "/api/apps/hello/deactivate", "POST")[0] == 204
                status, body = result.result(timeout=45)
                assert status == 502 and b"timed out" in body, (status, body)
                status, body = smoke.request(port, f"/api/apps/hello/versions/{newer}", "DELETE")
                assert status == 409 and b"in-flight" in body, (status, body)
                assert (data / "apps/hello" / newer).exists()
                # Zero-active routes remain unavailable even while an older
                # request is still draining its already-pinned guest.
                assert smoke.request(port, "/", host="hello.localhost")[0] == 404
                timeout_release.set()
                delete_after_guest_finishes(port, newer)
                assert not (data / "apps/hello" / newer).exists()
        finally:
            for release in pending:
                release.set()
            if proc is not None:
                smoke.stop(proc, log)
            gate_server.shutdown()
            gate_server.server_close()
            gate_thread.join(timeout=5)
    print("PASS: real streamed HTTP response and timed-out WASM guest retain artifact leases")


if __name__ == "__main__":
    main()
