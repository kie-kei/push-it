#!/usr/bin/env python3
"""Outgoing-HTTP allowlist smoke with a real guest component.

Patches examples/hello/fn into a fixture whose handler performs an outgoing
HTTP request via wasi:http/outgoing-handler, deploys it under three
capabilities.http_out policies and asserts host-level enforcement:
denied without the grant, allowed only for allowlisted authority hosts,
and unrestricted for ['*'].
"""
import io
import json
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

# Route + helper appended to the example handler: GET /_fn/fetch?to=<authority><path>
# performs an outgoing request and reports its status, or the host error name.
GUEST_ROUTE = '''        if path == "/_fn/fetch" {
            let q = request.path_with_query().unwrap_or_default();
            let to = q.rsplit("to=").next().unwrap_or_default().to_string();
            let payload = match fetch_status(&to) {
                Ok(status) => format!("{{\\"status\\":{status}}}"),
                Err(e) => format!("{{\\"error\\":\\"{e}\\"}}"),
            };
            let response = OutgoingResponse::new(Fields::new());
            let _ = response.set_status_code(200);
            let body = response.body().unwrap();
            ResponseOutparam::set(out, Ok(response));
            let stream = body.write().unwrap();
            stream.blocking_write_and_flush(payload.as_bytes()).unwrap();
            drop(stream);
            OutgoingBody::finish(body, None).unwrap();
            return;
        }
'''

GUEST_FN = '''
fn fetch_status(target: &str) -> Result<u16, String> {
    use bindings::wasi::http::outgoing_handler;
    use bindings::wasi::http::types::{Fields, IncomingBody, Method, OutgoingRequest, Scheme};
    let (authority, path) = target.split_once('/').unwrap_or((target, ""));
    let request = OutgoingRequest::new(Fields::new());
    let _ = request.set_method(&Method::Get);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some(authority));
    let _ = request.set_path_with_query(Some(&format!("/{path}")));
    let future = outgoing_handler::handle(request, None).map_err(|e| format!("{e:?}"))?;
    future.subscribe().block();
    let response = future
        .get()
        .unwrap_or_else(|| Err(()))
        .map_err(|()| "future consumed".to_string())?
        .map_err(|e| format!("{e:?}"))?;
    let status = response.status();
    let body = response.consume().map_err(|e| format!("{e:?}"))?;
    let _ = body.stream().map_err(|e| format!("{e:?}"))?;
    IncomingBody::finish(body);
    Ok(status)
}
'''

WORLD_MARKER = "    import wasi:cli/environment@0.2.0;\n"
WORLD_ADDITION = WORLD_MARKER + "    import wasi:http/outgoing-handler@0.2.0;\n"
ROUTE_MARKER = "        let path = path.split('?').next().unwrap_or(\"/\");\n"
EXPORT_MARKER = "bindings::export!(Component with_types_in bindings);"


def build_fixture(root: Path) -> Path:
    fixture = root / "fixture"
    shutil.copytree(ROOT / "examples/hello/fn", fixture, ignore=shutil.ignore_patterns("target"))
    world = fixture / "wit/world.wit"
    assert world.read_text().count(WORLD_MARKER) == 1, "example world changed; update fixture patch"
    world.write_text(world.read_text().replace(WORLD_MARKER, WORLD_ADDITION))
    source = fixture / "src/lib.rs"
    text = source.read_text()
    assert text.count(ROUTE_MARKER) == 1, "example handler changed; update fixture insertion point"
    text = text.replace(ROUTE_MARKER, ROUTE_MARKER + GUEST_ROUTE)
    assert text.count(EXPORT_MARKER) == 1, "example export marker moved"
    text = text.replace(EXPORT_MARKER, GUEST_FN + "\n" + EXPORT_MARKER)
    source.write_text(text)
    wasm = fixture / "target/wasm32-wasip2/release/hello_fn.wasm"
    # cwd=ROOT keeps the rustup toolchain override and wasm32 targets of the checkout.
    subprocess.run(["cargo", "component", "build", "--manifest-path", str(fixture / "Cargo.toml"),
                    "--release", "--target", "wasm32-wasip2", "--locked", "--offline"],
                   cwd=ROOT, check=True, capture_output=True, timeout=600)
    assert wasm.is_file(), f"fixture wasm missing: {wasm}"
    return wasm


def request(port, path, method="GET", body=None, host="hub.localhost", headers=None):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=body, method=method,
                                 headers={"Host": host, **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=30) as res:
            return res.status, res.read()
    except urllib.error.HTTPError as res:
        return res.code, res.read()


def deploy(port, name, wasm, capabilities):
    manifest = f"name: {name}\ncapabilities:\n  http_out: {capabilities}\n"
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w:gz") as tar:
        tar.add(wasm, arcname="app.wasm")
    boundary = "pushit-http-out-smoke"
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"manifest\"\r\n\r\n{manifest}\r\n"
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"archive\"; filename=\"app.tar.gz\"\r\n"
            "Content-Type: application/gzip\r\n\r\n").encode() + archive.getvalue() + f"\r\n--{boundary}--\r\n".encode()
    status, data = request(port, f"/api/apps/{name}/deploy", "POST", body,
                           headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert status == 200, (status, data)


def start(data):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_")}
    env.update(PUSHIT_DATA=str(data), PUSHIT_BIND=f"127.0.0.1:{port}",
               PUSHIT_DEV="1", PUSHIT_SESSION_KEY="test-only-not-for-production-0123456789")
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


def fetch(port, app, target):
    status, body = request(port, f"/_fn/fetch?to={target}", host=f"{app}.localhost")
    assert status == 200, (status, body)
    return json.loads(body)


def main():
    assert BIN.is_file(), "build workspace binaries first"
    with tempfile.TemporaryDirectory(prefix="pushit-http-out-") as tmp:
        root = Path(tmp)
        wasm = build_fixture(root)
        data = root / "data"
        data.mkdir()
        proc, port, log = start(data)
        try:
            # The controller itself is the outbound target: any HTTP status
            # proves the request left the guest; denial surfaces as an error.
            target = f"127.0.0.1:{port}/api/apps"
            localhost_target = f"localhost:{port}/api/apps"

            deploy(port, "allowlist", wasm, "['127.0.0.1']")
            result = fetch(port, "allowlist", target)
            assert "status" in result and "error" not in result, result
            result = fetch(port, "allowlist", localhost_target)
            assert "denied" in result.get("error", "").lower(), result  # host not in allowlist

            deploy_no_grant(port, "no-http-out", wasm)
            result = fetch(port, "no-http-out", target)
            assert "denied" in result.get("error", "").lower(), result

            deploy(port, "allow-all", wasm, "['*']")
            result = fetch(port, "allow-all", localhost_target)
            assert "status" in result and "error" not in result, result
        finally:
            stop(proc, log)
    print("PASS: outgoing HTTP is denied without a grant and enforced per allowlist host")


def deploy_no_grant(port, name, wasm):
    manifest = f"name: {name}\n"
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w:gz") as tar:
        tar.add(wasm, arcname="app.wasm")
    boundary = "pushit-http-out-smoke"
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"manifest\"\r\n\r\n{manifest}\r\n"
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"archive\"; filename=\"app.tar.gz\"\r\n"
            "Content-Type: application/gzip\r\n\r\n").encode() + archive.getvalue() + f"\r\n--{boundary}--\r\n".encode()
    status, data = request(port, f"/api/apps/{name}/deploy", "POST", body,
                           headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert status == 200, (status, data)


if __name__ == "__main__":
    main()
