include makefiles/net.mk

SCCACHE := $(shell which sccache)
CARGO_CMD = RUSTC_WRAPPER=$(SCCACHE) cargo
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam
SIM := sim
TARGET = pulsebeam
TEST =

.PHONY: all help dev build release profile flamegraph perf deps brew-deps cargo-deps clean build-ebpf test-routing \
	test test-unit test-sim test-sim-seed test-sim-agent-native agent-check agent-test agent-conformance agent-wasm agent-wasm-size agent-browser-test protected-paths
all: build

dev:
	$(CARGO_CMD) run -p pulsebeam -- --dev -i enp0s13f0u1u2

build:
	$(CARGO_CMD) build

release:
	$(CARGO_CMD) build --verbose --release -p pulsebeam

profile:
	$(CARGO_CMD) build --profile profiling -p pulsebeam

run-profile: profile
	$(BINARY)

test: test-unit test-sim

agent-check:
	$(CARGO_CMD) check -p pulsebeam-agent-core --no-default-features
	$(CARGO_CMD) check -p pulsebeam-agent-core --all-features
	$(CARGO_CMD) check -p pulsebeam-agent-native --all-features
	$(CARGO_CMD) check -p pulsebeam-agent-web --no-default-features
	$(CARGO_CMD) check -p pulsebeam-agent-web --all-features

agent-test:
	$(CARGO_CMD) test -p pulsebeam-agent-core --all-targets
	$(CARGO_CMD) test -p pulsebeam-agent-native --all-targets
	$(CARGO_CMD) test -p pulsebeam-agent-web --no-default-features --all-targets
	$(CARGO_CMD) test -p pulsebeam-agent-web --all-features --all-targets

agent-conformance:
	$(CARGO_CMD) test -p pulsebeam-agent-core --test conformance
	$(CARGO_CMD) test -p pulsebeam-agent-native --test conformance
	$(CARGO_CMD) test -p pulsebeam-agent-web --features protocol --test conformance

agent-wasm:
	rustup target add wasm32-unknown-unknown
	$(CARGO_CMD) check -p pulsebeam-agent-core --no-default-features --target wasm32-unknown-unknown
	$(CARGO_CMD) check -p pulsebeam-agent-core --all-features --target wasm32-unknown-unknown
	$(CARGO_CMD) check -p pulsebeam-agent-web --no-default-features --target wasm32-unknown-unknown
	$(CARGO_CMD) check -p pulsebeam-agent-web --no-default-features --features protocol --target wasm32-unknown-unknown
	$(CARGO_CMD) check -p pulsebeam-agent-web --all-features --target wasm32-unknown-unknown

agent-wasm-size:
	scripts/check-agent-wasm-size.sh

test-sim-agent-native:
	$(CARGO_CMD) nextest run --cargo-profile $(SIM) -p pulsebeam-simulator --no-fail-fast agent_native::

agent-browser-test:
	$(CARGO_CMD) test -p pulsebeam-agent-web --target wasm32-unknown-unknown --test browser

protected-paths:
	@test -z "$$(git status --short --untracked-files=all -- pulsebeam-agent)"
	@if [ ! -d ../pulsebeam-js ]; then exit 0; fi; test -z "$$(git -C ../pulsebeam-js status --short --untracked-files=all)"

# `sim` is on because the shaper lives behind it, and the shaper is the authority on what a
# simulated link can carry. Without the feature its tests are not compiled, so they never ran
# here and nothing said so.
test-unit:
	$(CARGO_CMD) test --workspace --exclude pulsebeam-simulator --features pulsebeam/sim -- $(TEST)

# One plan per process, across all cores.
#
# nextest gives each plan its own process, so the shaper's registry, the clock guard and the RNG
# are per-plan already. Wall-clock contention was the one reason to serialise, and the clock is
# virtual now (clock_gettime reads turmoil's simulated time, see sim_clock.rs), so machine load
# cannot change what a plan computes - parallel runs reproduce identically.
#
# No --no-capture here on purpose: nextest forces test-threads=1 under it, to keep live output
# from interleaving, so it is what was serialising the run. nextest still shows a failed plan's
# captured output, which is all a pass/fail run needs. The scoreboard, which does need the live
# [scoreboard] lines, keeps --no-capture in bwe-baseline and pays the serial cost there.
test-sim:
	$(CARGO_CMD) nextest run --cargo-profile $(SIM) -p pulsebeam-simulator --no-fail-fast $(TEST)

# Replay one seed. This is what a sweep failure prints.
test-sim-seed:
	PULSEBEAM_SIM_SEED=$(SEED) $(CARGO_CMD) nextest run --cargo-profile $(SIM) \
		-p pulsebeam-simulator --no-fail-fast $(TEST)

# bpfel-unknown-none has no precompiled std, so it always needs nightly plus
# `-Z build-std=core` — `rustup target add` cannot install it, tier-3 targets
# ship no prebuilt sysroot. pulsebeam-ebpf/build.rs resolves and caches
# bpf-linker, so no separate linker installation is needed.
build-ebpf:
	$(CARGO_CMD) +nightly build -Z build-std=core --target bpfel-unknown-none -p pulsebeam-ebpf --release

test-routing:
	$(CARGO_CMD) test -p pulsebeam-routing -- $(TEST)

# Run the suite over many seeds.
#
# The suite is a set of property tests over a simulated network, and a property
# that holds at one seed is not a property. Every scenario otherwise runs at
# DEFAULT_SIM_SEED forever, sampling one ordering of arrivals, losses and
# reorderings and reporting that sample as though it were the space.
#
#   make sim-sweep                    20 seeds from 1
#   make sim-sweep SEEDS=200 FROM=500 200 seeds from 500
#   make sim-sweep TEST=properties::  one module, many seeds
sim-sweep:
	SEEDS=$(SEEDS) FROM=$(FROM) TEST=$(TEST) scripts/seed-sweep.sh

# Regenerate the committed scoreboard. Diff it to see a change's effect on every scenario at
# once, rather than discovering days later that a fix for one wrecked another.
bwe-baseline:
	-$(CARGO_CMD) nextest run --cargo-profile $(SIM) -p pulsebeam-simulator --no-capture --no-fail-fast 2>&1 \
		| python3 scripts/bwe-scoreboard.py > bwe-baseline.txt
	@git --no-pager diff --stat bwe-baseline.txt || true

lint:
	cargo fix --allow-dirty && cargo clippy --fix --allow-dirty && cargo fmt --all
	@$(MAKE) --no-print-directory lint-check

# The gate. Fails rather than warns, which is the whole point: `cargo clippy
# --fix` above leaves everything as a warning, and a warning in a 100-line build
# log is not a gate.
#
# The deny tier lives in [workspace.lints] in Cargo.toml; the architectural
# rules (shared state, ambient clock, blocking) live in clippy.toml with a
# reason string each. See docs/thread-per-core.md.
# `--features pulsebeam/sim` is explicit rather than relied upon. The simulator
# crate pulls it in by feature unification today, so sim-gated code happens to
# be linted; naming it here means that stays true if the simulator ever leaves
# the default members.
lint-check:
	cargo clippy --all-targets --workspace --features pulsebeam/sim

flamegraph: profile
	taskset -c 2-5 $(CARGO_CMD) flamegraph --profile profiling -p pulsebeam --bin pulsebeam

perf-server:
	$(eval PIDS := $(shell pgrep -x $(TARGET) | paste -sd "," -))
	@if [ -z "$(PIDS)" ]; then echo "Error: pulsebeam not running"; exit 1; fi
	sudo sysctl -w kernel.perf_event_max_stack=64
	sudo sysctl -w kernel.kptr_restrict=0
	sudo sysctl -w kernel.perf_event_paranoid=-1
	perf record \
		-p $(PIDS) \
		-g \
		-e cycles \
		--call-graph fp \
		-F 999 \
		-m 256M \
		-o perf.data \
		-- sleep 15
	# @echo "Launching UI..."
	# hotspot perf.data

perf-system:
	sudo sysctl -w kernel.kptr_restrict=0
	sudo sysctl -w kernel.perf_event_paranoid=-1
	
	@echo "========================================================"
	@echo " RECORDING ALL CORES SYSTEM-WIDE"
	@echo " Press [Ctrl + C] the instant you see the rare spike hit!"
	@echo "========================================================"
	
	# We use a massive 128M buffer so data isn't dropped during a long wait
	perf record \
		-a \
		-F 99 \
		-e cycles \
		-g \
		--call-graph fp \
		-m 128M \
		-o perf-system.data


stats:
	$(eval PIDS := $(shell pgrep -x pulsebeam | paste -sd "," -))
	@if [ -z "$(PIDS)" ]; then echo "Error: pulsebeam not running"; exit 1; fi; \
	perf stat -e cpu_core/L1-dcache-loads/ \
		-e cpu_core/L1-dcache-load-misses/ \
    -e cpu_core/L1-dcache-stores/ \
		-e cpu_core/L1-dcache-store-misses/ \
    -e cpu_core/l2_rqsts.miss/ \
    -e cpu_core/LLC-loads/,cpu_core/LLC-load-misses/ \
    -e dtlb-load-misses,dtlb-store-misses \
    -e instructions,cpu-cycles \
    -p $(PIDS) -- sleep 30

deps: deps-brew deps-cargo gh-deps

deps-brew:
	brew install git-cliff axodotdev/tap/cargo-dist

deps-cargo:
	$(CARGO_CMD) install cargo-release cargo-dist git-cliff
	$(CARGO_CMD) install flamegraph cargo-machete

deps-system:
	paru -S rustc-demangle

deps-profile:
	# used for perf record speed up
	$(CARGO_CMD) install addr2line --features=bin
	# https://github.com/flamegraph-rs/flamegraph/issues/74
	sudo cp /usr/bin/addr2line /usr/bin/addr2line-bak
	sudo cp target/release/examples/addr2line /usr/bin/addr2line

deps-gh:
	gh extension install yusukebe/gh-markdown-preview

preview-markdown:
	gh markdown-preview

clean:
	$(CARGO_CMD) clean
	rm -f perf.data flamegraph.svg

chore-release:
	cargo release -v --execute

unused:
	cargo machete

tune: net-tune cpu-tune system-tune

cpu-tune:
	cpupower frequency-set --max 1.8GHz

system-tune:
	sudo grubby --update-kernel=ALL --args="isolcpus=1-4 nohz_full=1-4 rcu_nocbs=1-4"

net-tune:
	sudo sysctl -w net.core.wmem_max=134217728 >/dev/null
	sudo sysctl -w net.core.rmem_max=134217728 >/dev/null
