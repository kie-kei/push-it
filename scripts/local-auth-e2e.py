#!/usr/bin/env python3
"""Full local platform smoke. All passwords/data are synthetic and temporary.
Build workspace and examples/hello/fn for wasm32-wasip2 first. Never deploys remotely.
"""
import html
import http.client
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import tempfile
import time
from urllib.parse import urlencode, urlsplit

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / "target/debug/pushit"
CONTROLLER = ROOT / "target/debug/pushit-controller"
WASM = ROOT / "examples/hello/fn/target/wasm32-wasip2/release/hello_fn.wasm"
PASSWORD = "synthetic-local-auth-smoke-password"


def main():
    assert CLI.is_file() and CONTROLLER.is_file() and WASM.is_file(), "build workspace and real WASM fixture first"
    with tempfile.TemporaryDirectory(prefix="pushit-local-auth-") as directory:
        root = Path(directory)
        data, config = root / "data", root / "config"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        server = f"http://hub.localhost:{port}"
        env = {k: v for k, v in os.environ.items() if not k.startswith("PUSHIT_")}
        # Password and bearer credentials must never visit an inherited HTTP proxy.
        env.update(XDG_CONFIG_HOME=str(config), HTTP_PROXY="http://127.0.0.1:1",
                   HTTPS_PROXY="http://127.0.0.1:1", ALL_PROXY="http://127.0.0.1:1",
                   http_proxy="http://127.0.0.1:1", https_proxy="http://127.0.0.1:1",
                   all_proxy="http://127.0.0.1:1", NO_PROXY="", no_proxy="")

        # Invalid modes and mixed deployment profiles fail before opening data.
        rejected = [
            {"PUSHIT_AUTH_MODE": "unknown"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_DEV": "1"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_OIDC_ISSUER": "https://issuer.invalid"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_OIDC_CLIENT_ID": "legacy"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_BASE_DOMAIN": "example.com"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_BIND": "0.0.0.0:9100"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_BIND": "127.0.0.2:9100"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_BIND": "[::]:9100"},
            {"PUSHIT_AUTH_MODE": "local", "PUSHIT_BIND": "127.0.0.1:0"},
            {"PUSHIT_AUTH_MODE": "oidc", "PUSHIT_SESSION_KEY": "synthetic-persistent-session-key-0123456789"},
        ]
        for index, settings in enumerate(rejected):
            invalid_data = root / f"invalid-{index}"
            result = subprocess.run([str(CONTROLLER)], env=dict(env, PUSHIT_DATA=str(invalid_data), **settings), capture_output=True, timeout=5)
            assert result.returncode != 0 and not invalid_data.exists(), settings

        def cli(*args, password=None, success=True):
            result = subprocess.run([str(CLI), "--server", server, *args], env=env,
                                    input=password, text=True, capture_output=True, timeout=60)
            assert (result.returncode == 0) == success, (args, result.returncode, result.stderr)
            assert PASSWORD not in result.stdout + result.stderr
            return result.stdout

        # Explicit stdin only; no password argument or empty initialization.
        cli("auth", "setup", "--data-dir", str(data), "--password-stdin", password="\n", success=False)
        cli("auth", "setup", "--data-dir", str(data), "--password-stdin", password=PASSWORD + "\n")
        cli("auth", "setup", "--data-dir", str(data), "--password-stdin", password=PASSWORD + "\n", success=False)
        cli("profile", "add", "local", "--server", server)
        cli("profile", "use", "local")
        cookies = {}

        def request(host, path, method="GET", body=None, headers=None):
            authority = f"{host}.localhost:{port}"
            fields = {"Host": authority, **(headers or {})}
            if host in cookies and "Cookie" not in fields:
                fields["Cookie"] = "; ".join(cookies[host].values())
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
            connection.request(method, path, body, fields)
            response = connection.getresponse()
            result = (response.status, dict(response.getheaders()), response.read())
            for name, value in response.getheaders():
                if name.lower() == "set-cookie":
                    assert "domain=" not in value.lower()
                    pair = value.split(";", 1)[0]
                    cookies.setdefault(host, {})[pair.split("=", 1)[0]] = pair
            connection.close()
            return result

        log_path = root / "controller.log"
        process = log = None

        def start():
            nonlocal process, log
            settings = dict(env, PUSHIT_AUTH_MODE="local", PUSHIT_DATA=str(data),
                            PUSHIT_BIND=f"127.0.0.1:{port}")
            log = open(log_path, "ab")
            process = subprocess.Popen([str(CONTROLLER)], env=settings, stdout=log, stderr=log)
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                assert process.poll() is None, log_path.read_text()
                try:
                    if request("hub", "/auth/mode")[0] == 200:
                        return
                except OSError:
                    pass
                time.sleep(0.1)
            raise AssertionError("local controller startup timed out")

        def stop():
            nonlocal process, log
            if process is not None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                process = None
            if log is not None:
                log.close()
                log = None

        try:
            start()
            cli("login", "--local", "--password-stdin", password="wrong\n", success=False)
            output = cli("login", "--local", "--password-stdin", password=PASSWORD + "\n")
            assert "token=" not in output and "opening" not in output
            profile = config / "pushit/profiles.toml"
            assert profile.stat().st_mode & 0o777 == 0o600
            # Parse only for assertions; never print the bearer value.
            import tomllib
            token = tomllib.loads(profile.read_text())["profiles"]["local"]["token"]
            assert token not in output
            app = root / "app"
            app.mkdir()
            (app / "dist").mkdir()
            (app / "dist/index.html").write_text("<title>standalone local platform</title>")
            manifest = app / "pushit.yaml"
            manifest.write_text("name: hello\nenv:\n  GREETING: standalone\n")
            deployed = json.loads(cli("deploy", "--manifest", str(manifest), "--wasm", str(WASM), "--dist", str(app / "dist")))
            version = deployed["revision"]
            assert request("hello", "/")[0] == 303
            # Start at an unauthenticated app, enter password only at hub,
            # resume the app challenge without any preexisting hub session.
            host, path = "hello", "/"
            for _ in range(12):
                status, fields, body = request(host, path)
                if status == 200 and host == "hub" and b"type=password" in body:
                    text = body.decode()
                    csrf = html.unescape(re.search(r'name=csrf value="([^"]+)"', text)[1])
                    return_to = html.unescape(re.search(r'name=return_to value="([^"]*)"', text)[1])
                    status, fields, body = request("hub", "/auth/login", "POST",
                        urlencode({"password": PASSWORD, "csrf": csrf, "return_to": return_to}),
                        {"Origin": server, "Content-Type": "application/x-www-form-urlencoded"})
                if status == 200 and host == "hello":
                    assert b"standalone local platform" in body
                    break
                assert status == 303, (host, path.split("?", 1)[0], status)
                destination = urlsplit(fields["location"])
                if destination.netloc:
                    assert destination.port == port and destination.hostname in ("hub.localhost", "hello.localhost")
                    host = destination.hostname.split(".")[0]
                path = destination.path + ("?" + destination.query if destination.query else "")
            else:
                raise AssertionError("browser login handoff did not finish")
            assert request("hello", "/_fn/hello")[0] == 200
            auth_cookie = "; ".join(cookies["hello"].values()) + "; app_preference=ok"
            status, _, body = request("hello", "/_fn/auth-check", headers={
                "Cookie": auth_cookie, "Authorization": "Bearer synthetic-forged-value",
                "X-User-Id": "forged", "X-User-Name": "forged", "X-User-Email": "forged"})
            assert status == 200 and json.loads(body) == {
                "trusted_identity": True, "platform_credentials": False, "app_cookie": True}
            assert version in cli("version", "list", "hello")
            cli("version", "deactivate", "hello")
            assert request("hello", "/")[0] == 404
            cli("version", "activate", "hello", version)
            assert request("hello", "/_fn/hello")[0] == 200
            stop()
            start()
            assert version in cli("version", "list", "hello")
            assert request("hello", "/_fn/hello")[0] == 200
            ids = json.loads(cli("auth", "tokens"))
            assert len(ids) == 1
            cli("auth", "revoke", ids[0]["id"])
            cli("list", success=False)
            stop()
            start()
            cli("list", success=False)
            stored = sqlite3.connect(data / "local-auth.db")
            assert stored.execute("SELECT password FROM owner").fetchone()[0].startswith("$argon2id$")
            assert stored.execute("SELECT count(*) FROM credentials WHERE kind='token'").fetchone()[0] == 0
            stored.close()
            assert PASSWORD not in log_path.read_text() and token not in log_path.read_text()
            print("local auth: CLI setup/login, proxy isolation, real WASM deploy, browser handoff, credential stripping, versions, restart persistence and durable revoke passed")
        finally:
            stop()


if __name__ == "__main__":
    main()
