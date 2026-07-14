include makefiles/net.mk

SCCACHE := $(shell which sccache)
CARGO_CMD = RUSTC_WRAPPER=$(SCCACHE) cargo
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam
SIM := sim
TARGET = pulsebeam
TEST =

.PHONY: all help dev build release profile flamegraph perf deps brew-deps cargo-deps clean
all: build

dev:
	$(CARGO_CMD) run -p pulsebeam --features deep-metrics -- --dev

build:
	$(CARGO_CMD) build

release:
	$(CARGO_CMD) build --verbose --release -p pulsebeam

profile:
	$(CARGO_CMD) build --profile profiling -p pulsebeam

run-profile: profile
	$(BINARY)

test: test-unit test-sim

test-unit:
	$(CARGO_CMD) test --workspace --exclude pulsebeam-simulator --

test-sim:
	$(CARGO_CMD) test --profile $(SIM) -p pulsebeam-simulator -- --no-capture $(TEST)

lint:
	cargo fix --allow-dirty && cargo clippy --fix --allow-dirty && cargo fmt --all

flamegraph: profile
	taskset -c 2-5 $(CARGO_CMD) flamegraph --profile profiling -p pulsebeam --bin pulsebeam

perf-server:
	$(eval PIDS := $(shell pgrep -x $(TARGET) | paste -sd "," -))
	@if [ -z "$(PIDS)" ]; then echo "Error: pulsebeam not running"; exit 1; fi
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
