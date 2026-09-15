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
    @for file in agents/pulsebeam-agent-core/Justfile agents/pulsebeam-agent-native/Justfile agents/pulsebeam-agent-web/Justfile agents/react/Justfile apps/meet/Justfile crates/pulsebeam/Justfile crates/pulsebeam-cli/Justfile crates/pulsebeam-core/Justfile crates/pulsebeam-ebpf/Justfile crates/pulsebeam-proto/Justfile crates/pulsebeam-routing/Justfile crates/pulsebeam-rtc/Justfile crates/pulsebeam-runtime/Justfile crates/pulsebeam-simulator/Justfile crates/pulsebeam-testdata/Justfile docs/Justfile tools/Justfile; do just --justfile "$file" --fmt --check; done

fix:
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged --all-targets --workspace --features pulsebeam/sim
    just --justfile agents/pulsebeam-agent-web/Justfile fix
    just --justfile agents/react/Justfile fix
    just --justfile apps/meet/Justfile fix

# Run every owner fast gate.
test-fast:
    just --justfile agents/pulsebeam-agent-core/Justfile test-fast
    just --justfile agents/pulsebeam-agent-native/Justfile test-fast
    just --justfile agents/pulsebeam-agent-web/Justfile test-fast
    just --justfile agents/react/Justfile test-fast
    just --justfile apps/meet/Justfile test-fast
    just --justfile crates/pulsebeam/Justfile test-fast
    just --justfile crates/pulsebeam-cli/Justfile test-fast
    just --justfile crates/pulsebeam-core/Justfile test-fast
    just --justfile crates/pulsebeam-ebpf/Justfile test-fast
    just --justfile crates/pulsebeam-proto/Justfile test-fast
    just --justfile crates/pulsebeam-routing/Justfile test-fast
    just --justfile crates/pulsebeam-rtc/Justfile test-fast
    just --justfile crates/pulsebeam-runtime/Justfile test-fast
    just --justfile crates/pulsebeam-simulator/Justfile test-fast
    just --justfile crates/pulsebeam-testdata/Justfile test-fast
    just --justfile docs/Justfile test-fast
    just --justfile tools/Justfile test-fast

# Run every owner slow gate. RTC precedes Web to provision the shared browser cache.
test-slow:
    just --justfile agents/pulsebeam-agent-core/Justfile test-slow
    just --justfile agents/pulsebeam-agent-native/Justfile test-slow
    just --justfile agents/react/Justfile test-slow
    just --justfile apps/meet/Justfile test-slow
    just --justfile crates/pulsebeam/Justfile test-slow
    just --justfile crates/pulsebeam-cli/Justfile test-slow
    just --justfile crates/pulsebeam-core/Justfile test-slow
    just --justfile crates/pulsebeam-ebpf/Justfile test-slow
    just --justfile crates/pulsebeam-proto/Justfile test-slow
    just --justfile crates/pulsebeam-routing/Justfile test-slow
    just --justfile crates/pulsebeam-rtc/Justfile test-slow
    just --justfile agents/pulsebeam-agent-web/Justfile test-slow
    just --justfile crates/pulsebeam-runtime/Justfile test-slow
    just --justfile crates/pulsebeam-simulator/Justfile test-slow
    just --justfile crates/pulsebeam-testdata/Justfile test-slow
    just --justfile docs/Justfile test-slow
    just --justfile tools/Justfile test-slow

# Run all non-privileged owner test gates, with fast gates before slow gates.
test: test-fast test-slow

# Build the static Meet export.
meet-build:
    just --justfile apps/meet/Justfile build

# Build local packages and start the Meet development server.
dev:
    just --justfile apps/meet/Justfile dev

# Build, load, and attach the eBPF steering programs.
ebpf:
    just --justfile crates/pulsebeam-ebpf/Justfile ci

# Search deterministic simulation seeds without making the result a merge gate.
sweep seeds="20" from="1" filter="":
    just --justfile crates/pulsebeam-simulator/Justfile sweep "{{ seeds }}" "{{ from }}" "{{ filter }}"

# Build the server with dev, release, or profiling settings.
build profile="dev":
    just --justfile crates/pulsebeam/Justfile build "{{ profile }}"

# Run one repository-owned cargo-dist release stage.
release stage *args:
    just --justfile tools/Justfile "release-{{ stage }}" {{ args }}
