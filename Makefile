include makefiles/net.mk

SCCACHE := $(shell which sccache)
CARGO_CMD = RUSTC_WRAPPER=$(SCCACHE) cargo
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

.PHONY: all help dev build release profile flamegraph perf deps brew-deps cargo-deps clean
all: build

dev:
	$(CARGO_CMD) run -p pulsebeam

build:
	$(CARGO_CMD) build --profile profiling -p pulsebeam

release:
	$(CARGO_CMD) build --release -p pulsebeam

profile: build

flamegraph: profile
	taskset -c 2-5 $(CARGO_CMD) flamegraph --profile profiling -p pulsebeam --bin pulsebeam

perf: profile
	taskset -c 2-5 perf record --call-graph dwarf $(BINARY)
	hotspot perf.data

deps: brew-deps cargo-deps

brew-deps:
	brew install git-cliff axodotdev/tap/cargo-dist

cargo-deps:
	$(CARGO_CMD) install cargo-smart-release --features allow-emoji
	$(CARGO_CMD) install flamegraph cargo-machete

gh-deps:
	gh extension install yusukebe/gh-markdown-preview

preview-markdown:
	gh markdown-preview

clean:
	$(CARGO_CMD) clean
	rm -f perf.data flamegraph.svg

unused:
	cargo machete
