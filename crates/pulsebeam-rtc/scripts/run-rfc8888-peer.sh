#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
(
  cd tests/peers/rfc8888
  go build -mod=readonly -o "$temporary/rfc8888-peer" .
)
PULSEBEAM_RFC8888_PEER="$temporary/rfc8888-peer" \
  cargo test -p pulsebeam-rtc --test rfc8888_peer -- \
    --ignored --exact authenticated_pion_ccfb_reaches_production_feedback
