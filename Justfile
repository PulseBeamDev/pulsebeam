set shell := ["bash", "-euc"]

test_owners := "agents/pulsebeam-agent-core agents/pulsebeam-agent-native agents/pulsebeam-agent-web agents/react apps/meet crates/pulsebeam crates/pulsebeam-cli crates/pulsebeam-core crates/pulsebeam-proto crates/pulsebeam-routing crates/pulsebeam-rtc crates/pulsebeam-runtime crates/pulsebeam-simulator crates/pulsebeam-testdata docs tools"

default:
    @just --list

# Prepare local JavaScript packages in direct-dependency order.
prepare:
    just --justfile apps/meet/Justfile prepare
    just --justfile docs/Justfile prepare

# Run every static workspace gate.
check:
    just prepare
    cargo check
    cargo fmt --all --check
    cargo clippy --all-targets --workspace --features pulsebeam/sim
    just --justfile agents/pulsebeam-agent-web/Justfile check
    just --justfile agents/react/Justfile check
    just --justfile apps/meet/Justfile check
    just --justfile docs/Justfile check
    just --fmt --check
    @for owner in {{ test_owners }}; do just --justfile "$owner/Justfile" --fmt --check; done

fix:
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged --all-targets --workspace --features pulsebeam/sim
    just --justfile agents/pulsebeam-agent-web/Justfile fix
    just --justfile agents/react/Justfile fix
    just --justfile apps/meet/Justfile fix

# Run every owner fast gate concurrently, bounded to the machine's CPU count.
test-fast:
    #!/usr/bin/env bash
    set -euo pipefail
    printf '%s\n' {{ test_owners }} | xargs -P "${JUST_TEST_JOBS:-$(nproc)}" -I{} just --justfile "{}/Justfile" test-fast

# Run independent slow owners concurrently. RTC and Web remain one ordered
# chain because RTC provisions the browser cache consumed by Web.
test-slow:
    #!/usr/bin/env bash
    set -euo pipefail
    just --justfile crates/pulsebeam-rtc/Justfile test-slow &
    browser_pid=$!
    just --justfile crates/pulsebeam-simulator/Justfile test-slow &
    sim_pid=$!
    wait "$browser_pid"
    just --justfile agents/pulsebeam-agent-web/Justfile test-slow
    wait "$sim_pid"

# Run all owner test gates.
test: test-fast
    just test-slow

# Build the static Meet export.
meet-build:
    just --justfile apps/meet/Justfile build

# Build local packages and start the Meet development server.
dev:
    just --justfile apps/meet/Justfile dev

# Search deterministic simulation seeds without making the result a merge gate.
sweep seeds="20" from="1" filter="":
    just --justfile crates/pulsebeam-simulator/Justfile sweep "{{ seeds }}" "{{ from }}" "{{ filter }}"

# Build the server with dev, release, or profiling settings.
build profile="dev":
    just --justfile crates/pulsebeam/Justfile build "{{ profile }}"

# Run one repository-owned cargo-dist release stage.
release stage *args:
    just --justfile tools/Justfile "release-{{ stage }}" {{ args }}
