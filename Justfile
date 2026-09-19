set shell := ["bash", "-euc"]

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
    @for file in agents/pulsebeam-agent-core/Justfile agents/pulsebeam-agent-native/Justfile agents/pulsebeam-agent-web/Justfile agents/react/Justfile apps/meet/Justfile crates/pulsebeam/Justfile crates/pulsebeam-cli/Justfile crates/pulsebeam-core/Justfile crates/pulsebeam-proto/Justfile crates/pulsebeam-routing/Justfile crates/pulsebeam-rtc/Justfile crates/pulsebeam-runtime/Justfile crates/pulsebeam-simulator/Justfile crates/pulsebeam-testdata/Justfile docs/Justfile tools/Justfile; do just --justfile "$file" --fmt --check; done

fix:
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged --all-targets --workspace --features pulsebeam/sim
    just --justfile agents/pulsebeam-agent-web/Justfile fix
    just --justfile agents/react/Justfile fix
    just --justfile apps/meet/Justfile fix

# Run every owner fast gate concurrently.
[parallel]
test-fast: _test-fast-agent-core _test-fast-agent-native _test-fast-agent-web _test-fast-react _test-fast-meet _test-fast-pulsebeam _test-fast-cli _test-fast-core _test-fast-proto _test-fast-routing _test-fast-rtc _test-fast-runtime _test-fast-simulator _test-fast-testdata _test-fast-docs _test-fast-tools

_test-fast-agent-core:
    just --justfile agents/pulsebeam-agent-core/Justfile test-fast

_test-fast-agent-native:
    just --justfile agents/pulsebeam-agent-native/Justfile test-fast

_test-fast-agent-web:
    just --justfile agents/pulsebeam-agent-web/Justfile test-fast

_test-fast-react:
    just --justfile agents/react/Justfile test-fast

_test-fast-meet:
    just --justfile apps/meet/Justfile test-fast

_test-fast-pulsebeam:
    just --justfile crates/pulsebeam/Justfile test-fast

_test-fast-cli:
    just --justfile crates/pulsebeam-cli/Justfile test-fast

_test-fast-core:
    just --justfile crates/pulsebeam-core/Justfile test-fast

_test-fast-proto:
    just --justfile crates/pulsebeam-proto/Justfile test-fast

_test-fast-routing:
    just --justfile crates/pulsebeam-routing/Justfile test-fast

_test-fast-rtc:
    just --justfile crates/pulsebeam-rtc/Justfile test-fast

_test-fast-runtime:
    just --justfile crates/pulsebeam-runtime/Justfile test-fast

_test-fast-simulator:
    just --justfile crates/pulsebeam-simulator/Justfile test-fast

_test-fast-testdata:
    just --justfile crates/pulsebeam-testdata/Justfile test-fast

_test-fast-docs:
    just --justfile docs/Justfile test-fast

_test-fast-tools:
    just --justfile tools/Justfile test-fast

# Run independent slow gates concurrently. Browser tests stay in one chain so
# RTC provisions the shared browser cache before the Web-owned runner uses it.
[parallel]
test-slow: _test-slow-browser _test-slow-simulator _test-slow-other

_test-slow-browser:
    just --justfile crates/pulsebeam-rtc/Justfile test-slow
    just --justfile agents/pulsebeam-agent-web/Justfile test-slow

_test-slow-simulator:
    just --justfile crates/pulsebeam-simulator/Justfile test-slow

# Preserve the owner contract even though these are currently no-ops.
_test-slow-other:
    just --justfile agents/pulsebeam-agent-core/Justfile test-slow
    just --justfile agents/pulsebeam-agent-native/Justfile test-slow
    just --justfile agents/react/Justfile test-slow
    just --justfile apps/meet/Justfile test-slow
    just --justfile crates/pulsebeam/Justfile test-slow
    just --justfile crates/pulsebeam-cli/Justfile test-slow
    just --justfile crates/pulsebeam-core/Justfile test-slow
    just --justfile crates/pulsebeam-proto/Justfile test-slow
    just --justfile crates/pulsebeam-routing/Justfile test-slow
    just --justfile crates/pulsebeam-runtime/Justfile test-slow
    just --justfile crates/pulsebeam-testdata/Justfile test-slow
    just --justfile docs/Justfile test-slow
    just --justfile tools/Justfile test-slow

# Run all non-privileged owner test gates.
test: test-fast && test-slow

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
