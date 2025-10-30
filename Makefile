# ==============================================================================
# Makefile for PulseBeam Development & Profiling
# ==============================================================================

# --- Configuration ---
# Use `sccache` if available to speed up compilation
SCCACHE := $(shell which sccache)
CARGO_CMD = RUSTC_WRAPPER=$(SCCACHE) cargo

# Binary and directory settings
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

# --- Network Simulation Settings ---
IFACE ?= lo
IFB_IFACE ?= ifb0

# Bad network presets (adjust these for your testing needs)
LATENCY ?= 100ms
PACKET_LOSS ?= 0%
UPLOAD ?= 1mbit
DOWNLOAD ?= 2mbit

# Additional options
JITTER ?= 10ms
CORRUPTION ?= 0%


# --- Targets ---
# Declare all targets as .PHONY to prevent conflicts with file names.
.PHONY: all help dev build release profile flamegraph perf net-bad net-clear deps brew-deps cargo-deps clean


# The default target executed when you just run `make`.
all: build

# A self-documenting help command.
help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Core Targets:"
	@echo "  dev          Build and run the development binary."
	@echo "  build        Build the binary using the 'profiling' profile (default)."
	@echo "  release      Build the final release binary."
	@echo ""
	@echo "Profiling:"
	@echo "  profile      Alias for 'build' to create the profiling binary."
	@echo "  flamegraph   Generate a flamegraph for the profiling binary."
	@echo "  perf         Run perf and generate a hotspot report."
	@echo ""
	@echo "Network Simulation (requires sudo):"
	@echo "  net-bad      Simulate a bad network on interface '$(IFACE)'."
	@echo "  net-clear    Clear all network simulation rules."
	@echo ""
	@echo "Housekeeping:"
	@echo "  deps         Install all required dependencies (brew and cargo)."
	@echo "  clean        Remove build artifacts, perf data, and flamegraphs."


# --- Core Development & Build ---

# Build and run for quick development cycles.
dev:
	$(CARGO_CMD) run -p pulsebeam

# Build with the 'profiling' profile. This is the default for most tasks.
build:
	$(CARGO_CMD) build --profile profiling -p pulsebeam

# Build an optimized release binary.
release:
	$(CARGO_CMD) build --release -p pulsebeam


# --- Profiling ---

# An explicit alias for the default build, used as a dependency.
profile: build

# Generate a flamegraph. Depends on the profiling build.
flamegraph: profile
	taskset -c 2-5 $(CARGO_CMD) flamegraph --profile profiling -p pulsebeam --bin pulsebeam

# Run perf and generate a hotspot report. Depends on the profiling build.
perf: profile
	taskset -c 2-5 perf record --call-graph dwarf $(BINARY)
	hotspot perf.data


# --- Network Simulation ---
net-simple: net-verify net-clear
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Applying SIMPLE BAD NETWORK to $(IFACE) (egress only)"
	@echo "  Latency:      $(LATENCY) ± $(JITTER)"
	@echo "  Packet Loss:  $(PACKET_LOSS)"
	@echo "  Corruption:   $(CORRUPTION)"
	@echo "  Bandwidth:    $(UPLOAD)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@sudo tc qdisc add dev $(IFACE) root netem \
		delay $(LATENCY) $(JITTER) \
		loss $(PACKET_LOSS) \
		corrupt $(CORRUPTION) \
		rate $(UPLOAD)
	@echo "✓ Network simulation active (affects outgoing traffic)"
	@echo "  Run 'make net-clear' to restore normal network"

# --- Full Version (Egress + Ingress with IFB) ---
net-bad: net-verify net-clear
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Applying BAD NETWORK simulation to $(IFACE)"
	@echo "  Latency:      $(LATENCY) ± $(JITTER)"
	@echo "  Packet Loss:  $(PACKET_LOSS)"
	@echo "  Corruption:   $(CORRUPTION)"
	@echo "  Upload:       $(UPLOAD)"
	@echo "  Download:     $(DOWNLOAD)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "→ Loading IFB module..."
	@sudo modprobe ifb numifbs=1 || { echo "ERROR: Failed to load ifb module"; exit 1; }
	@sleep 0.5
	@echo "→ Verifying $(IFB_IFACE) exists..."
	@ip link show $(IFB_IFACE) >/dev/null 2>&1 || { \
		echo "ERROR: $(IFB_IFACE) not found after loading module."; \
		echo "Try: sudo ip link add $(IFB_IFACE) type ifb"; \
		exit 1; \
	}
	@echo "→ Activating $(IFB_IFACE)..."
	@sudo ip link set dev $(IFB_IFACE) up
	@echo "→ Redirecting ingress traffic to $(IFB_IFACE)..."
	@sudo tc qdisc add dev $(IFACE) handle ffff: ingress
	@sudo tc filter add dev $(IFACE) parent ffff: protocol all u32 match u32 0 0 \
		action mirred egress redirect dev $(IFB_IFACE)
	@echo "→ Applying egress (upload) rules to $(IFACE)..."
	@sudo tc qdisc add dev $(IFACE) root netem \
		delay $(LATENCY) $(JITTER) \
		loss $(PACKET_LOSS) \
		corrupt $(CORRUPTION) \
		rate $(UPLOAD)
	@echo "→ Applying ingress (download) rules to $(IFB_IFACE)..."
	@sudo tc qdisc add dev $(IFB_IFACE) root netem \
		delay $(LATENCY) $(JITTER) \
		loss $(PACKET_LOSS) \
		corrupt $(CORRUPTION) \
		rate $(DOWNLOAD)
	@echo ""
	@echo "✓ Bad network simulation is ACTIVE (both directions)"
	@echo "  Run 'make net-clear' to restore normal network"

# --- Moderate Network ---
net-moderate:
	@$(MAKE) net-simple LATENCY=50ms PACKET_LOSS=1% JITTER=5ms UPLOAD=5mbit

# --- Extreme Network ---
net-extreme:
	@$(MAKE) net-simple LATENCY=300ms PACKET_LOSS=10% JITTER=50ms UPLOAD=500kbit

# --- Clear All Network Simulation ---
net-clear:
	@echo "→ Clearing network simulation from $(IFACE)..."
	@sudo tc qdisc del dev $(IFACE) root 2>/dev/null || true
	@sudo tc qdisc del dev $(IFACE) ingress 2>/dev/null || true
	@sudo tc qdisc del dev $(IFB_IFACE) root 2>/dev/null || true
	@sudo ip link set dev $(IFB_IFACE) down 2>/dev/null || true
	@echo "✓ Network simulation cleared"

# --- Show Current Network Rules ---
net-status:
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Network Status for $(IFACE)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo ""
	@echo "=== Egress (Outgoing) Rules ==="
	@sudo tc qdisc show dev $(IFACE) 2>/dev/null || echo "  No rules configured"
	@echo ""
	@echo "=== Ingress (Incoming) Rules via $(IFB_IFACE) ==="
	@sudo tc qdisc show dev $(IFB_IFACE) 2>/dev/null || echo "  No rules configured"
	@echo ""
	@echo "=== Filter Rules ==="
	@sudo tc filter show dev $(IFACE) parent ffff: 2>/dev/null || echo "  No filters configured"

# --- Verify Prerequisites ---
net-verify:
	@echo "→ Verifying prerequisites..."
	@command -v tc >/dev/null 2>&1 || { echo "ERROR: 'tc' command not found. Install iproute2."; exit 1; }
	@ip link show $(IFACE) >/dev/null 2>&1 || { echo "ERROR: Interface $(IFACE) not found."; exit 1; }
	@echo "✓ Prerequisites OK"


# --- Housekeeping ---

# A convenience target to install all dependencies.
deps: brew-deps cargo-deps

# Install dependencies from Homebrew.
brew-deps:
	brew install git-cliff axodotdev/tap/cargo-dist

# Install dependencies with Cargo.
cargo-deps:
	$(CARGO_CMD) install cargo-smart-release --features allow-emoji
	$(CARGO_CMD) install flamegraph

# Clean up all build artifacts and generated files.
clean:
	$(CARGO_CMD) clean
	rm -f perf.data flamegraph.svg
