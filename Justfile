set shell := ["bash", "-euc"]
set tempdir := "/tmp"

default:
    @just --list

# Run every static workspace gate.
check:
    scripts/check-repository-layout.sh
    cargo check
    cargo fmt --all --check
    cargo clippy --all-targets --workspace --features pulsebeam/sim
    just --fmt --check
    @for file in agents/pulsebeam-agent-web/Justfile crates/pulsebeam/Justfile crates/pulsebeam-ebpf/Justfile crates/pulsebeam-simulator/Justfile crates/pulsebeam-testdata/Justfile tools/Justfile; do just --justfile "$file" --fmt --check; done

# Run workspace unit tests and deterministic simulation plans.
test:
    cargo test --workspace --exclude pulsebeam-simulator --features pulsebeam/sim
    cargo nextest run --cargo-profile sim -p pulsebeam-simulator --no-fail-fast

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
