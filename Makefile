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

LATENCY ?= 100ms
PACKET_LOSS ?= 2%
UPLOAD ?= 1mbit
DOWNLOAD ?= 2mbit
JITTER ?= 10ms
CORRUPTION ?= 0%
REORDER_PROB  := 25%
REORDER_CORR  := 50%


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
net-setup:
	# needed for setting up ingress
	modprobe ifb numifbs=1
	# ip link set dev ifb0 up

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
net-apply: net-verify net-clear
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Applying BAD NETWORK simulation to $(IFACE)"
	@echo "  Latency:      $(LATENCY) ± $(JITTER)"
	@echo "  Packet Loss:  $(PACKET_LOSS)"
	@echo "  Reordering:   $(REORDER_PROB) with $(REORDER_CORR) correlation" # <-- New line
	@echo "  Upload:       $(UPLOAD)"
	@echo "  Download:     $(DOWNLOAD)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	# ... your modprobe and ip link setup ...
	@echo "→ Applying egress (upload) rules to $(IFACE)..."
	@sudo tc qdisc add dev $(IFACE) root netem \
		delay $(LATENCY) $(JITTER) \
		loss $(PACKET_LOSS) \
		corrupt $(CORRUPTION) \
		rate $(UPLOAD) \
		reorder $(REORDER_PROB) $(REORDER_CORR) # <-- MODIFIED LINE

	@echo "→ Applying ingress (download) rules to $(IFB_IFACE)..."
	@sudo tc qdisc add dev $(IFB_IFACE) root netem \
		delay $(LATENCY) $(JITTER) \
		loss $(PACKET_LOSS) \
		corrupt $(CORRUPTION) \
		rate $(DOWNLOAD) \
		reorder $(REORDER_PROB) $(REORDER_CORR) # <-- MODIFIED LINE
	@echo ""
	@echo "✓ Bad network simulation is ACTIVE (both directions)"
	@echo "  Run 'make net-clear' to restore normal network"

net-bad:
	@$(MAKE) net-congested-wifi

# Pristine network with minimal issues.
net-good-home-wifi:
	@$(MAKE) net-apply \
		LATENCY="20ms" \
		JITTER="5ms" \
		PACKET_LOSS="0%" \
		UPLOAD="10mbit" \
		DOWNLOAD="50mbit" \
		REORDER_PROB="0.1%" \
		REORDER_CORR="25%"

# Simulates a home network with family members streaming, causing jitter and bandwidth contention.
net-congested-wifi:
	@$(MAKE) net-apply \
		LATENCY="25ms" \
		JITTER="30ms" \
		PACKET_LOSS="0%" \
		UPLOAD="2mbit" \
		DOWNLOAD="10mbit" \
		REORDER_PROB="5%" \
		REORDER_CORR="50%"

# Simulates a stable 4G/5G connection with decent bandwidth but higher base latency than Wi-Fi.
net-stable-mobile:
	@$(MAKE) net-apply \
		LATENCY="40ms" \
		JITTER="15ms" \
		PACKET_LOSS="0%" \
		UPLOAD="8mbit" \
		DOWNLOAD="40mbit" \
		REORDER_PROB="1%" \
		REORDER_CORR="25%"

# Simulates a mobile connection while moving or with a weak signal, causing intermittent loss and high jitter.
net-unstable-mobile:
	@$(MAKE) net-apply \
		LATENCY="70ms" \
		JITTER="50ms" \
		PACKET_LOSS="1%" \
		UPLOAD="1mbit" \
		DOWNLOAD="5mbit" \
		REORDER_PROB="5%" \
		REORDER_CORR="50%"

# A stable but long-distance intercontinental link (e.g., US to Asia).
net-cross-continent:
	@$(MAKE) net-apply \
		LATENCY="140ms" \
		JITTER="20ms" \
		PACKET_LOSS="0%" \
		UPLOAD="10mbit" \
		DOWNLOAD="50mbit" \
		REORDER_PROB="0.5%" \
		REORDER_CORR="50%"

# A true "nightmare" scenario for resilience testing. A very broken connection.
net-unusable:
	@$(MAKE) net-apply \
		LATENCY="100ms" \
		JITTER="50ms" \
		PACKET_LOSS="3%" \
		UPLOAD="500kbit" \
		DOWNLOAD="1mbit" \
		REORDER_PROB="25%" \
		REORDER_CORR="75%"

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
