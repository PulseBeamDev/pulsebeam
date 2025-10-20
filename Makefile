# Directory and binary settings
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

# Rust settings
CARGO = cargo
SCCACHE := $(shell which sccache)

# Development build and run
.PHONY: dev
dev:
	$(CARGO) run -p pulsebeam

# Default target: Build and capture flamegraph
.PHONY: all
all: build flamegraph

# Build with profiling profile
.PHONY: build
build:
	RUSTC_WRAPPER=$(SCCACHE) $(CARGO) build --profile profiling -p pulsebeam

# Capture flamegraph
.PHONY: flamegraph
flamegraph:
	RUSTC_WRAPPER=$(SCCACHE) taskset -c 2-5 cargo flamegraph --profile profiling -p pulsebeam --bin pulsebeam

.PHONY: perf
perf:
	RUSTC_WRAPPER=$(SCCACHE) cargo build --profile profiling -p pulsebeam --bin pulsebeam
	taskset -c 2-5 perf record --call-graph dwarf ./target/profiling/pulsebeam
	hotspot ./perf.data

# Install Homebrew dependencies
.PHONY: brew-deps
brew-deps:
	brew install git-cliff axodotdev/tap/cargo-dist

# Install Cargo dependencies
.PHONY: cargo-deps
cargo-deps:
	RUSTC_WRAPPER=$(SCCACHE) $(CARGO) install cargo-smart-release --features allow-emoji
	RUSTC_WRAPPER=$(SCCACHE) $(CARGO) install flamegraph

# Release with smart-release
.PHONY: release
release:
	RUSTC_WRAPPER=$(SCCACHE) $(CARGO) build --release -p pulsebeam


.PHONY: profile
profile:
	RUSTC_WRAPPER=$(SCCACHE) $(CARGO) build --profile profiling -p pulsebeam

# Clean build artifacts and flamegraph data
.PHONY: clean
clean:
	$(CARGO) clean
	rm -f perf.data flamegraph.svg
