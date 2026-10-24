#!/usr/bin/env python3
"""Real nginx HTTPS -> loopback HTTP standalone smoke; no public DNS or credentials.
Needs nginx, openssl, built workspace and examples/hello WASI 0.2 fixture.
NGINX may point at a locally built nginx. Uses a temporary CONNECT tunnel so the
CLI's normal HTTPS stack can reach synthetic DNS names without editing /etc/hosts.
"""
import html
import http.client
import json
import os
from pathlib import Path
import re
import select
import shutil
import socket
import socketserver
import ssl
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlencode, urlsplit

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target"))) / "debug"
WASM = Path(os.environ.get("PUSHIT_TEST_WASM", str(ROOT / "examples/hello/fn/target/wasm32-wasip2/release/hello_fn.wasm")))
PASSWORD = "synthetic-https-proxy-smoke-password"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main():
    nginx = shutil.which(os.environ.get("NGINX", "nginx"))
    assert nginx and shutil.which("openssl"), "install nginx and openssl first"
    assert (BIN / "pushit").is_file() and (BIN / "pushit-controller").is_file() and WASM.is_file(), "build workspace and real WASM fixture first"
    with tempfile.TemporaryDirectory(prefix="pushit-proxy-auth-") as directory:
        root = Path(directory)
        (root / "logs").mkdir()
        data = root / "data"
        backend, public = free_port(), free_port()
        while public == backend:
            public = free_port()
        server = f"https://hub.platform.test:{public}"
        # Neither shell environment nor operator profile contributes credentials.
        env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_") and k.lower() not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")}
        env.update(XDG_CONFIG_HOME=str(root / "config"), NO_PROXY="", no_proxy="")
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=PushIt synthetic test CA", "-keyout", str(root / "ca.key"), "-out", str(root / "ca.crt")], check=True, capture_output=True)
        subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=hub.platform.test", "-keyout", str(root / "tls.key"), "-out", str(root / "tls.csr")], check=True, capture_output=True)
        (root / "ext").write_text("subjectAltName=DNS:hub.platform.test,DNS:*.platform.test,DNS:hub.redirect.test\nextendedKeyUsage=serverAuth\n")
        subprocess.run(["openssl", "x509", "-req", "-in", str(root / "tls.csr"), "-CA", str(root / "ca.crt"), "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-days", "1", "-extfile", str(root / "ext"), "-out", str(root / "tls.crt")], check=True, capture_output=True)
        # A standard HTTPS CONNECT proxy routes only these synthetic names locally.
        # It cannot see TLS request bodies and never disables certificate checks.
        class Tunnel(socketserver.BaseRequestHandler):
            def handle(self):
                request = b""
                while b"\r\n\r\n" not in request and len(request) < 16384:
                    part = self.request.recv(4096)
                    if not part:
                        return
                    request += part
                target = request.split(b" ")[1].decode()
                assert request.startswith(b"CONNECT ") and target in (f"hub.platform.test:{public}", f"hub.redirect.test:{public}")
                with socket.create_connection(("127.0.0.1", public)) as upstream:
                    self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    while True:
                        ready, _, _ = select.select([self.request, upstream], [], [], 30)
                        if not ready:
                            return
                        for source in ready:
                            try:
                                chunk = source.recv(65536)
                            except OSError:
                                return
                            if not chunk:
                                return
                            try:
                                (upstream if source is self.request else self.request).sendall(chunk)
                            except OSError:
                                return
        class TunnelServer(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True
        tunnel = TunnelServer(("127.0.0.1", 0), Tunnel)
        threading.Thread(target=tunnel.serve_forever, daemon=True).start()
        env["HTTPS_PROXY"] = f"http://127.0.0.1:{tunnel.server_address[1]}"
        env["https_proxy"] = env["HTTPS_PROXY"]

        def cli(*args, password=None, success=True, trusted=True, target=server):
            settings = dict(env)
            if trusted:
                settings["SSL_CERT_FILE"] = str(root / "ca.crt")
            result = subprocess.run([str(BIN / "pushit"), "--server", target, *args], env=settings, input=password, text=True, capture_output=True, timeout=60)
            assert (result.returncode == 0) == success, (args, result.returncode, result.stderr)
            assert PASSWORD not in result.stdout + result.stderr
            return result

        valid = dict(PUSHIT_AUTH_MODE="builtin", PUSHIT_BASE_DOMAIN="platform.test",
                     PUSHIT_EXTERNAL_ORIGIN=f"https://platform.test:{public}", PUSHIT_BIND=f"127.0.0.1:{backend}")
        invalid = [
            {"PUSHIT_EXTERNAL_ORIGIN": ""}, {"PUSHIT_EXTERNAL_ORIGIN": "http://platform.test"},
            {"PUSHIT_EXTERNAL_ORIGIN": "https://other.test"}, {"PUSHIT_EXTERNAL_ORIGIN": "https://platform.test/"},
            {"PUSHIT_BASE_DOMAIN": "127.1", "PUSHIT_EXTERNAL_ORIGIN": "https://127.1"},
            {"PUSHIT_BASE_DOMAIN": "localhost", "PUSHIT_EXTERNAL_ORIGIN": "https://localhost"},
            {"PUSHIT_BIND": "0.0.0.0:9100"}, {"PUSHIT_BIND": "[::]:9100"},
            {"PUSHIT_BIND": "localhost:9100"}, {"PUSHIT_BIND": "127.0.0.1:0"},
            {"PUSHIT_DEV": "1"}, {"PUSHIT_OIDC_CLIENT_SECRET": "synthetic-forbidden-setting"},
            {"PUSHIT_AUTH_MODE": "local"},
        ]
        for index, settings in enumerate(invalid):
            invalid_data = root / f"invalid-{index}"
            result = subprocess.run([str(BIN / "pushit-controller")],
                env={**env, **valid, **settings, "PUSHIT_DATA": str(invalid_data)}, capture_output=True, timeout=5)
            assert result.returncode != 0 and not invalid_data.exists(), settings

        cli("auth", "setup", "--data-dir", str(data), "--password-stdin", password=PASSWORD + "\n")
        (root / "nginx.conf").write_text(f"""
worker_processes 1;
pid {root}/nginx.pid;
error_log {root}/nginx-error.log warn;
events {{ worker_connections 64; }}
http {{
  # Exclude query strings (one-use handoff grants), cookies and Authorization.
  log_format safe '$host $request_method $uri $status';
  access_log {root}/nginx-access.log safe;
  client_max_body_size 128m;
  client_body_temp_path {root}/client_body;
  proxy_temp_path {root}/proxy_temp;
  server {{
    listen 127.0.0.1:{public} ssl default_server;
    server_name hub.platform.test *.platform.test;
    ssl_certificate {root}/tls.crt;
    ssl_certificate_key {root}/tls.key;
    location / {{
      proxy_pass http://127.0.0.1:{backend};
      proxy_set_header Host $http_host;
      proxy_set_header Forwarded '';
      proxy_set_header X-Forwarded-Host '';
      proxy_set_header X-Forwarded-Proto '';
      proxy_set_header X-Forwarded-Port '';
      proxy_buffering off;
      proxy_read_timeout 3600s;
    }}
  }}
  server {{
    listen 127.0.0.1:{public} ssl;
    server_name hub.redirect.test;
    ssl_certificate {root}/tls.crt;
    ssl_certificate_key {root}/tls.key;
    location /auth/local/token {{ return 307 https://hub.platform.test:{public}/credential-sink; }}
    location /api/apps {{ return 307 https://hub.platform.test:{public}/credential-sink; }}
  }}
}}
""")
        log = open(root / "controller.log", "wb")
        controller_env = dict(env, PUSHIT_AUTH_MODE="builtin", PUSHIT_DATA=str(data), PUSHIT_BASE_DOMAIN="platform.test", PUSHIT_EXTERNAL_ORIGIN=f"https://platform.test:{public}", PUSHIT_BIND=f"127.0.0.1:{backend}")
        controller = subprocess.Popen([str(BIN / "pushit-controller")], env=controller_env, stdout=log, stderr=log)
        proxy = None
        context = ssl.create_default_context(cafile=str(root / "ca.crt"))
        cookies = {}

        def request(host, path, method="GET", body=None, headers=None):
            authority = f"{host}.platform.test:{public}"
            fields = {"Host": authority, **(headers or {})}
            if host in cookies and "Cookie" not in fields:
                fields["Cookie"] = "; ".join(cookies[host].values())
            raw = socket.create_connection(("127.0.0.1", public), timeout=30)
            connection = http.client.HTTPConnection("127.0.0.1", public, timeout=30)
            connection.sock = context.wrap_socket(raw, server_hostname=f"{host}.platform.test")
            connection.request(method, path, body, fields)
            response = connection.getresponse()
            result = (response.status, dict(response.getheaders()), response.read())
            for name, value in response.getheaders():
                if name.lower() == "set-cookie":
                    assert "domain=" not in value.lower() and "; Secure" in value and value.startswith("__Host-")
                    pair = value.split(";", 1)[0]
                    cookies.setdefault(host, {})[pair.split("=", 1)[0]] = pair
            connection.close()
            return result
        try:
            nginx_env = {k: v for k, v in os.environ.items() if k != "NGINX"}
            subprocess.run([nginx, "-p", str(root), "-c", str(root / "nginx.conf"), "-t"], check=True, capture_output=True, env=nginx_env)
            proxy = subprocess.Popen([nginx, "-p", str(root), "-c", str(root / "nginx.conf"), "-g", "daemon off;"], env=nginx_env)
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                assert controller.poll() is None and proxy.poll() is None
                try:
                    if request("hub", "/auth/mode")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.1)
            else:
                raise AssertionError("proxy/controller startup timed out")
            cli("login", "--builtin", "--password-stdin", password=PASSWORD + "\n", success=False, trusted=False)
            cli("login", "--builtin", "--password-stdin", password=PASSWORD + "\n", success=False, target=f"http://hub.platform.test:{public}")
            cli("login", "--builtin", "--password-stdin", password=PASSWORD + "\n", success=False, target=f"https://hub.redirect.test:{public}")
            cli("profile", "add", "proxy", "--server", server)
            cli("profile", "use", "proxy")
            # Explicit stdin selects the secure owner flow, without a browser/device callback.
            result = cli("login", "--password-stdin", password=PASSWORD + "\n")
            assert "opening" not in result.stdout and "token=" not in result.stdout
            import tomllib
            profile = root / "config/pushit/profiles.toml"
            assert profile.stat().st_mode & 0o777 == 0o600
            token = tomllib.loads(profile.read_text())["profiles"]["proxy"]["token"]
            assert token not in result.stdout + result.stderr
            cli("list", success=False, target=f"https://hub.redirect.test:{public}")
            app = root / "app"
            (app / "dist").mkdir(parents=True)
            (app / "dist/index.html").write_text("<title>standalone HTTPS platform</title>")
            manifest = app / "pushit.yaml"
            manifest.write_text("name: hello\n")
            deployed = json.loads(cli("deploy", "--manifest", str(manifest), "--wasm", str(WASM), "--dist", str(app / "dist")).stdout)
            manifest.write_text("name: sibling\n")
            cli("deploy", "--manifest", str(manifest), "--wasm", str(WASM), "--dist", str(app / "dist"))
            host, path = "hello", "/"
            for _ in range(12):
                status, fields, body = request(host, path)
                if status == 200 and host == "hub" and b"type=password" in body:
                    text = body.decode()
                    csrf = html.unescape(re.search(r'name=csrf value="([^"]+)"', text)[1])
                    return_to = html.unescape(re.search(r'name=return_to value="([^"]*)"', text)[1])
                    status, fields, body = request("hub", "/auth/login", "POST", urlencode({"password": PASSWORD, "csrf": csrf, "return_to": return_to}), {"Origin": server, "Content-Type": "application/x-www-form-urlencoded"})
                if status == 200 and host == "hello":
                    assert b"standalone HTTPS platform" in body
                    break
                assert status == 303, (host, path.split("?", 1)[0], status)
                destination = urlsplit(fields["location"])
                if destination.netloc:
                    assert destination.scheme == "https" and destination.port == public and destination.hostname in ("hub.platform.test", "hello.platform.test")
                    host = destination.hostname.split(".")[0]
                path = destination.path + ("?" + destination.query if destination.query else "")
            else:
                raise AssertionError("HTTPS browser handoff did not finish")
            status, _, body = request("hello", "/_fn/auth-check", headers={"Cookie": "; ".join(cookies["hello"].values()) + "; app_preference=ok", "Authorization": "Bearer synthetic-forged", "X-User-Id": "forged", "X-User-Name": "forged", "X-User-Email": "forged"})
            assert status == 200 and json.loads(body) == {"trusted_identity": True, "platform_credentials": False, "app_cookie": True}, (status, body)
            assert request("hub", "/api/apps", headers={"Cookie": "; ".join(cookies["hello"].values())})[0] == 401
            assert request("sibling", "/", headers={"Cookie": "; ".join(cookies["hello"].values())})[0] == 303
            assert request("hub", "/api/apps", headers={"Host": "hub.platform.test", "X-Forwarded-Host": f"hub.platform.test:{public}"})[0] == 403
            assert request("hub", "/api/apps", headers={"Host": f"evil.test:{public}", "X-Forwarded-Proto": "https"})[0] == 403
            assert request("hub", "/api/secrets/global/TEST", "PUT", "synthetic", {"Origin": f"https://hello.platform.test:{public}", "X-Pushit-CSRF": "1"})[0] == 403
            version = deployed["revision"]
            cli("version", "deactivate", "hello")
            assert request("hello", "/")[0] == 404
            cli("version", "activate", "hello", version)
            assert request("hello", "/_fn/hello")[0] == 200
            controller.terminate()
            controller.wait(timeout=5)
            controller = subprocess.Popen([str(BIN / "pushit-controller")], env=controller_env, stdout=log, stderr=log)
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                assert controller.poll() is None
                try:
                    if request("hub", "/auth/mode")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.1)
            else:
                raise AssertionError("controller restart timed out")
            cli("list")
            assert request("hello", "/_fn/hello")[0] == 200
            ids = json.loads(cli("auth", "tokens").stdout)
            assert len(ids) == 1
            cli("auth", "revoke", ids[0]["id"])
            cli("list", success=False)
            access = (root / "nginx-access.log").read_text()
            assert "/credential-sink" not in access and "?grant=" not in access
            assert PASSWORD not in access and token not in access
            log.flush()
            assert PASSWORD not in (root / "controller.log").read_text() and token not in (root / "controller.log").read_text()
            print("HTTPS nginx auth: trusted TLS, untrusted-certificate/plaintext/credential-redirect rejection, CLI deploy, Secure host-only browser handoff, WASM credential stripping, CSRF, Host validation, sibling separation, version lifecycle, restart persistence and token revocation passed")
        finally:
            for process in (proxy, controller):
                if process is not None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            log.close()
            tunnel.shutdown()
            tunnel.server_close()


if __name__ == "__main__":
    main()
