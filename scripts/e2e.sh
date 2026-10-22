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
SERVER=http://localhost:9100
APP=hello
APP_HOST=hello.localhost
CONTROLLER_PID=""

FAILURES=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

cleanup() {
  if [ -n "$CONTROLLER_PID" ] && kill -0 "$CONTROLLER_PID" 2>/dev/null; then
    kill "$CONTROLLER_PID" 2>/dev/null
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

echo "==> write databases.yaml"
mkdir -p "$PUSHIT_DATA"
printf 'sl: sqlite://%s/hello.db\n' "$PUSHIT_DATA" >"$PUSHIT_DATA/databases.yaml"
# sqlx sqlite won't create the file by default; an empty file is a valid db
touch "$PUSHIT_DATA/hello.db"

echo "==> start controller"
PUSHIT_DEV=1 PUSHIT_DATA="$PUSHIT_DATA" "$CONTROLLER_BIN" >/tmp/pushit-e2e-controller.log 2>&1 &
CONTROLLER_PID=$!

echo "==> wait for :9100"
READY=0
for _ in $(seq 1 60); do
  if curl -sf -o /dev/null "$SERVER/api/apps" -H 'X-Pushit-Dev: 1' || curl -s -o /dev/null -w '%{http_code}' "$SERVER/" | grep -qE '200|401|403|404'; then
    READY=1; break
  fi
  sleep 1
done
if [ "$READY" -eq 1 ]; then
  pass "controller listening on :9100"
else
  fail "controller did not start (see /tmp/pushit-e2e-controller.log)"
  tail -20 /tmp/pushit-e2e-controller.log
  exit 1
fi

echo "==> secret set"
if "$PUSHIT_BIN" --dev secret set "$APP" HELLO_SECRET=secret123; then
  pass "secret set"
else
  fail "secret set"
fi

echo "==> deploy"
if (cd examples/hello && "$PUSHIT_BIN" --dev deploy); then
  pass "deploy"
else
  fail "deploy"
fi

echo "==> GET / (static UI)"
if curl -sf -H "Host: $APP_HOST" "$SERVER/" | grep -q '<title>hello</title>'; then
  pass "index.html served"
else
  fail "index.html served"
fi

echo "==> GET /_fn/hello (wasm handler)"
if curl -sf -H "Host: $APP_HOST" "$SERVER/_fn/hello" | grep -q '"msg":"hello"'; then
  pass "fn hello returned JSON"
else
  fail "fn hello returned JSON"
fi

echo "==> GET /_fn/sql (wasi:sql via sqlite)"
if curl -sf -H "Host: $APP_HOST" "$SERVER/_fn/sql" | grep -q 'hello from sql'; then
  pass "fn sql returned inserted row"
else
  fail "fn sql returned inserted row"
fi

echo "==> logs endpoint"
if curl -sf -H 'X-Pushit-Dev: 1' "$SERVER/api/apps/$APP/logs?tail=10" -o /dev/null; then
  pass "logs endpoint reachable"
else
  fail "logs endpoint"
fi

echo "==> second deploy (create previous revision)"
if (cd examples/hello && "$PUSHIT_BIN" --dev deploy >/dev/null); then
  pass "second deploy"
else
  fail "second deploy"
fi

echo "==> rollback"
if "$PUSHIT_BIN" --dev rollback "$APP"; then
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
