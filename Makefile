TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

# Rust settings
CARGO = cargo
RUSTFLAGS = -C force-frame-pointers=yes

# Default: Build and capture flamegraph
.PHONY: all
all: build flamegraph

# Build with profiling profile
.PHONY: build
build:
	$(CARGO) build --profile profiling 

# Capture flamegraph
.PHONY: flamegraph
flamegraph: build
	taskset -c 2-5 cargo flamegraph --profile profiling -p pulsebeam --bin pulsebeam

brew-deps:
	brew install git-cliff axodotdev/tap/cargo-dist

cargo-deps:
	cargo install cargo-smart-release --features allow-emoji
	cargo install flamegraph

release:
	cargo smart-release --execute

# Clean build artifacts and flamegraph data
.PHONY: clean
clean:
	$(CARGO) clean
	rm -f perf.data
	rm -f flamegraph.svg

