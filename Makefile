TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

# Rust and perf settings
CARGO = cargo
RUSTFLAGS = -C force-frame-pointers=yes
PERF = perf
PERF_RECORD_FLAGS = -F 4999 --call-graph dwarf

# Default: Build and capture perf data
.PHONY: all
all: build profile

# Build with profiling profile
.PHONY: build
build:
	$(CARGO) build --profile profiling 

# Capture perf data
.PHONY: profile
profile: $(BINARY)
	sudo $(PERF) record $(PERF_RECORD_FLAGS) -- $(BINARY)

brew-deps:
	brew install git-cliff axodotdev/tap/cargo-dist

cargo-deps:
	cargo install cargo-smart-release --features allow-emoji

release:
	cargo smart-release --execute

# Clean build artifacts and perf data
.PHONY: clean
clean:
	$(CARGO) clean
	rm -f perf.data
