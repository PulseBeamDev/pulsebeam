include makefiles/net.mk

SCCACHE := $(shell which sccache)
CARGO_CMD = RUSTC_WRAPPER=$(SCCACHE) cargo
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

.PHONY: all help dev build release profile flamegraph perf deps brew-deps cargo-deps clean
all: build

dev:
	$(CARGO_CMD) run -p pulsebeam -- --dev

build:
	$(CARGO_CMD) build

release:
	$(CARGO_CMD) build --verbose --release -p pulsebeam

profile:
	$(CARGO_CMD) build --profile profiling -p pulsebeam

test: test-unit test-sim

test-unit:
	cargo test --workspace --exclude pulsebeam-simulator

test-sim:
	cargo test -p pulsebeam-simulator -- --no-capture

lint:
	cargo fix --allow-dirty && cargo clippy --fix --allow-dirty

flamegraph: profile
	taskset -c 2-5 $(CARGO_CMD) flamegraph --profile profiling -p pulsebeam --bin pulsebeam

perf:
	@# Capture PIDs, replace newlines with commas, and trim the trailing comma
	$(eval PIDS := $(shell pgrep -x pulsebeam | paste -sd "," -))
	@if [ -z "$(PIDS)" ]; then echo "Error: pulsebeam not running"; exit 1; fi; \
	sudo sysctl -w kernel.kptr_restrict=0
	sudo sysctl -w kernel.perf_event_paranoid=-1
	sudo perf record -p $(PIDS) \
		--sample-cpu \
		-e cycles,cache-misses,LLC-load-misses \
		-e sched:sched_switch --switch-events \
		--call-graph fp \
		-m 16M \
    -- sleep 30 
	sudo hotspot perf.data

stats:
	$(eval PIDS := $(shell pgrep -x pulsebeam | paste -sd "," -))
	@if [ -z "$(PIDS)" ]; then echo "Error: pulsebeam not running"; exit 1; fi; \
	perf stat -e cpu_core/L1-dcache-loads/,cpu_core/L1-dcache-load-misses/,cpu_core/LLC-loads/,cpu_core/LLC-load-misses/,cpu_core/instructions/,cpu_core/cpu-cycles/ \
		-p $(PIDS)

deps: deps-brew deps-cargo gh-deps

deps-brew:
	brew install git-cliff axodotdev/tap/cargo-dist

deps-cargo:
	$(CARGO_CMD) install cargo-release cargo-dist git-cliff
	$(CARGO_CMD) install flamegraph cargo-machete

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
