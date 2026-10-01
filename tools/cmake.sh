#!/usr/bin/env bash
# Cargo's cmake helper finds Ninja through PATH, ignoring CMAKE_MAKE_PROGRAM.
# Both executable paths and their data come from rules_foreign_cc toolchains.
set -euo pipefail
export PATH="$(dirname "$PULSEBEAM_NINJA"):${PATH:-/usr/bin:/bin}"
exec "$PULSEBEAM_CMAKE" "$@"
