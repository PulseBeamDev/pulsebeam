#!/usr/bin/env bash
set -euo pipefail

browser=${BROWSER:-chromium}
driver_url=${WEBDRIVER_URL:-http://127.0.0.1:9515}
harness_dir=target/browser-harness
port=${PULSEBEAM_AGENT_HARNESS_PORT:-39080}

cargo build --target wasm32-unknown-unknown -p pulsebeam-agent-web --features browser-test-harness
rm -rf "$harness_dir"
mkdir -p "$harness_dir/pkg"
wasm-bindgen --target web --out-dir "$harness_dir/pkg" target/wasm32-unknown-unknown/debug/pulsebeam_agent_web.wasm
cp pulsebeam-agent-web/browser-test-harness.html "$harness_dir/index.html"
python3 -m http.server "$port" --directory "$harness_dir" >/tmp/pulsebeam-agent-harness.log 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT
BROWSER="$browser" WEBDRIVER_URL="$driver_url" PULSEBEAM_AGENT_HARNESS_URL="http://127.0.0.1:$port/" cargo test -p pulsebeam-agent-browser-test -- --ignored
