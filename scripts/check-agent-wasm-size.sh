#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root_dir"

command -v wasm-bindgen >/dev/null || {
    echo "wasm-bindgen CLI is required; install version 0.2.108" >&2
    exit 1
}

read -r wasm_budget gzip_budget < <(python3 - <<'PY'
import json
from pathlib import Path

budget = json.loads(Path("pulsebeam-agent-web/wasm-size-budget.json").read_text())
print(budget["minimal_browser_wasm_bytes"], budget["production_gzipped_wasm_js_bytes"])
PY
)

target_dir=${CARGO_TARGET_DIR:-target}
wasm_dir="$target_dir/agent-wasm"
bindgen_dir="$wasm_dir/bindgen"
rm -rf "$bindgen_dir"
mkdir -p "$bindgen_dir/minimal" "$bindgen_dir/production"

build_and_bindgen() {
    local feature_set=$1
    local output_dir=$2
    cargo build -p pulsebeam-agent-web-size-fixture \
        --no-default-features \
        --features "$feature_set" \
        --profile agent-wasm \
        --target wasm32-unknown-unknown \
        --target-dir "$target_dir"
    local wasm="$target_dir/wasm32-unknown-unknown/agent-wasm/pulsebeam_agent_web_size_fixture.wasm"
    test -f "$wasm"
    wasm-bindgen "$wasm" --target web --out-dir "$output_dir" --no-typescript
}

build_and_bindgen browser "$bindgen_dir/minimal"
build_and_bindgen production "$bindgen_dir/production"

minimal_wasm=$(find "$bindgen_dir/minimal" -maxdepth 1 -name '*_bg.wasm' -print -quit)
production_wasm=$(find "$bindgen_dir/production" -maxdepth 1 -name '*_bg.wasm' -print -quit)
test -f "$minimal_wasm"
test -f "$production_wasm"
minimal_wasm_bytes=$(wc -c < "$minimal_wasm")
production_wasm_bytes=$(wc -c < "$production_wasm")
production_gzip_bytes=$(cat "$production_wasm" "$bindgen_dir/production"/*.js | gzip -9c | wc -c)

printf 'agent web minimal browser WASM: %s bytes (budget %s)\n' "$minimal_wasm_bytes" "$wasm_budget"
printf 'agent web production WASM: %s bytes (informational)\n' "$production_wasm_bytes"
printf 'agent web production gzipped WASM+JS: %s bytes (budget %s)\n' "$production_gzip_bytes" "$gzip_budget"

if (( minimal_wasm_bytes > wasm_budget )); then
    echo "minimal browser WASM size budget exceeded" >&2
    exit 1
fi
if (( production_gzip_bytes > gzip_budget )); then
    echo "gzipped WASM+JS size budget exceeded" >&2
    exit 1
fi
