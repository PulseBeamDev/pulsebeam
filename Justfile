set shell := ["bash", "-euc"]

default:
    @just --list

# Prepare local JavaScript packages in direct-dependency order.
prepare:
    just --justfile apps/meet/Justfile prepare

# Run every static workspace gate.
check:
    just prepare
    cargo check
    cargo fmt --all --check
    cargo clippy --all-targets --workspace --features pulsebeam/sim
    just --justfile agents/pulsebeam-agent-web/Justfile check
    just --justfile agents/react/Justfile check
    just --justfile apps/meet/Justfile check
    just --fmt --check
    @for file in agents/pulsebeam-agent-native/Justfile agents/pulsebeam-agent-web/Justfile agents/react/Justfile apps/meet/Justfile crates/pulsebeam/Justfile crates/pulsebeam-ebpf/Justfile crates/pulsebeam-simulator/Justfile crates/pulsebeam-testdata/Justfile tools/Justfile; do just --justfile "$file" --fmt --check; done

fix:
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged --all-targets --workspace --features pulsebeam/sim
    just --justfile agents/pulsebeam-agent-web/Justfile fix
    just --justfile agents/react/Justfile fix
    just --justfile apps/meet/Justfile fix

# Run workspace unit tests and deterministic simulation plans.
test:
    just prepare
    cargo test --workspace --exclude pulsebeam-simulator --features pulsebeam/sim
    cargo nextest run --cargo-profile sim -p pulsebeam-simulator --no-fail-fast
    just --justfile agents/pulsebeam-agent-web/Justfile test
    just --justfile agents/react/Justfile test

# Build browser fixtures in package ownership order, then run every Rust-owned
# BiDi contract serially. This intentionally fails when Chrome is unavailable.
browser: prepare
    just --justfile agents/pulsebeam-agent-web/Justfile browser-fixture
    just --justfile agents/react/Justfile browser-fixture
    just --justfile agents/pulsebeam-agent-web/Justfile browser-run

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
