#!/usr/bin/env bash
# End-to-end smoke test: build example app, deploy to a dev controller,
# verify routing + fn handler + logs + rollback.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# shellcheck disable=SC1090
source ~/.cargo/env

PUSHIT_DATA=/tmp/pushit-e2e
PUSHIT_BIN="$ROOT/target/debug/pushit"
CONTROLLER_BIN="$ROOT/target/debug/pushit-controller"
# Port must be free; override PUSHIT_E2E_PORT if 9100 is taken (e.g. by a
# running docker compose controller).
PUSHIT_E2E_PORT="${PUSHIT_E2E_PORT:-9100}"
SERVER="http://localhost:$PUSHIT_E2E_PORT"
APP=hello
APP_HOST=hello.localhost
CONTROLLER_PID=""

FAILURES=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

cleanup() {
  if [ -n "$CONTROLLER_PID" ] && kill -0 "$CONTROLLER_PID" 2>/dev/null; then
    kill "$CONTROLLER_PID" 2>/dev/null
    sleep 0.5
    # SIGTERM may be swallowed by the tokio runtime — force
    kill -9 "$CONTROLLER_PID" 2>/dev/null
    wait "$CONTROLLER_PID" 2>/dev/null
  fi
}
trap cleanup EXIT

echo "==> reset data dir"
rm -rf "$PUSHIT_DATA"

echo "==> build example wasm component"
if (cd examples/hello/fn && cargo component build --release --target wasm32-wasip2) \
     && ls examples/hello/fn/target/wasm32-wasip2/release/*.wasm >/dev/null 2>&1; then
  pass "example wasm component built"
else
  fail "example wasm component build"
  echo "FAIL: aborting — cannot deploy without the component"
  exit 1
fi

echo "==> build workspace binaries"
if cargo build -p pushit -p pushit-controller; then
  pass "workspace binaries built"
else
  fail "workspace build"
  exit 1
fi

echo "==> write blobstores.yaml"
mkdir -p "$PUSHIT_DATA"
# local kv store is built-in at $PUSHIT_DATA/appdata/<app>/local.kv
printf 'files: fs://%s/blobs\n' "$PUSHIT_DATA" >"$PUSHIT_DATA/blobstores.yaml"

echo "==> start controller"
PUSHIT_DEV=1 PUSHIT_DATA="$PUSHIT_DATA" PUSHIT_BIND="0.0.0.0:$PUSHIT_E2E_PORT" "$CONTROLLER_BIN" >/tmp/pushit-e2e-controller.log 2>&1 &
CONTROLLER_PID=$!

echo "==> wait for :$PUSHIT_E2E_PORT"
READY=0
for _ in $(seq 1 60); do
  # must see OUR controller's startup line — the port may be occupied by
  # another instance (e.g. docker), whose /api would false-positive the curl
  if grep -q "listening on" /tmp/pushit-e2e-controller.log 2>/dev/null; then
    READY=1; break
  fi
  kill -0 "$CONTROLLER_PID" 2>/dev/null || { echo "controller died: $(tail -3 /tmp/pushit-e2e-controller.log)"; break; }
  sleep 1
done
if [ "$READY" -eq 1 ]; then
  pass "controller listening on :$PUSHIT_E2E_PORT"
else
  fail "controller did not start (see /tmp/pushit-e2e-controller.log)"
  tail -20 /tmp/pushit-e2e-controller.log
  exit 1
fi

echo "==> secret set"
if "$PUSHIT_BIN" --dev --server "$SERVER" secret set "$APP" HELLO_SECRET=secret123; then
  pass "secret set"
else
  fail "secret set"
fi

echo "==> deploy"
if (cd examples/hello && "$PUSHIT_BIN" --dev --server "$SERVER" deploy); then
  pass "deploy"
else
  fail "deploy"
fi

echo "==> GET / (static UI)"
STATIC_OUT="$(curl -s -w '\n%{http_code}' -H "Host: $APP_HOST" "$SERVER/" || true)"
if echo "$STATIC_OUT" | grep -q '<title>hello</title>'; then
  pass "index.html served"
else
  fail "index.html served — got: $(echo "$STATIC_OUT" | tail -c 250)"
fi

echo "==> GET /_fn/hello (wasm handler)"
if curl -sf -H "Host: $APP_HOST" "$SERVER/_fn/hello" | grep -q '"msg":"hello"'; then
  pass "fn hello returned JSON"
else
  fail "fn hello returned JSON"
fi

echo "==> GET /_fn/kv (wasi:keyvalue via redb)"
if curl -sf -H "Host: $APP_HOST" "$SERVER/_fn/kv" | grep -q 'hello from kv'; then
  pass "fn kv round-tripped the store"
else
  fail "fn kv round-tripped the store"
fi

echo "==> GET /_fn/log (wasi:logging)"
if curl -sf -H "Host: $APP_HOST" "$SERVER/_fn/log" | grep -q '"ok":true'; then
  pass "fn log returned ok"
else
  fail "fn log returned ok"
fi

echo "==> wasi:logging line in app logs"
sleep 1
if curl -sf -H 'X-Pushit-Dev: 1' "$SERVER/api/apps/$APP/logs?tail=50" | grep -q 'hello from wasi:logging'; then
  pass "logging line in logs"
else
  fail "logging line in logs"
fi

echo "==> GET /_fn/blob (wasi:blobstore roundtrip)"
BLOB_JSON="$(curl -sf -H "Host: $APP_HOST" "$SERVER/_fn/blob" || true)"
BLOB_WRITTEN="$(printf '%s' "$BLOB_JSON" | grep -o '"written":[0-9]*' | cut -d: -f2)"
BLOB_READ="$(printf '%s' "$BLOB_JSON" | grep -o '"read":[0-9]*' | cut -d: -f2)"
if [ -n "$BLOB_WRITTEN" ] && [ "$BLOB_WRITTEN" = "$BLOB_READ" ] && [ "$BLOB_READ" -ge 33554432 ]; then
  pass "fn blob roundtrip (${BLOB_READ} bytes streamed)"
else
  fail "fn blob roundtrip: $BLOB_JSON"
fi

echo "==> logs endpoint"
if curl -sf -H 'X-Pushit-Dev: 1' "$SERVER/api/apps/$APP/logs?tail=10" -o /dev/null; then
  pass "logs endpoint reachable"
else
  fail "logs endpoint"
fi

echo "==> second deploy (create previous revision)"
if (cd examples/hello && "$PUSHIT_BIN" --dev --server "$SERVER" deploy >/dev/null); then
  pass "second deploy"
else
  fail "second deploy"
fi

echo "==> rollback"
if "$PUSHIT_BIN" --dev --server "$SERVER" rollback "$APP"; then
  pass "rollback"
else
  fail "rollback"
fi

echo "==> stop controller"
cleanup
CONTROLLER_PID=""

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "E2E PASSED"
  exit 0
else
  echo "E2E FAILED ($FAILURES failures)"
  exit 1
fi
