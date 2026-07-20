# --- Configuration ---
HOST_IP      ?= 127.0.0.1

# Traffic Control Defaults
RATE         ?= 10mbit
LATENCY      ?= 0ms
LOSS         ?= 0%

# Execution Defaults
SERVER_BIN   ?= ./target/release/pulsebeam
SERVER_ARGS  ?= --dev
CLIENT_BIN   ?= ./target/release/pulsebeam-cli
CLIENT_ARGS  ?= --api-url http://$(HOST_IP):7070 bench \
  --rooms 1 \
  --max-rooms 120 \
  --users-per-room 4 \
  --arrival-rate 10 \
  --join-spread-secs 30 \
  --session-duration 7200 \
  --drain-duration 7200 \
  --fixed-session

all: help

.PHONY: run-server run-client net-apply net-clear net-limit net-congested-wifi net-mobile-1bar help

# --- Application Execution Engine ---

run-server:
	@echo "🚀 Launching SFU Server on local host..."
	@echo "   Command: $(SERVER_BIN) $(SERVER_ARGS)"
	@echo "────────────────────────────────────────────────────────"
	@$(SERVER_BIN) $(SERVER_ARGS)

run-client:
	@echo "🔥 Launching Benchmark Client..."
	@echo "   Command: $(CLIENT_BIN) $(CLIENT_ARGS)"
	@echo "────────────────────────────────────────────────────────"
	@$(CLIENT_BIN) $(CLIENT_ARGS)

# --- Localhost Traffic Control Simulation ---

net-apply: net-clear
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Applying Localhost Bandwidth & Latency Limits:"
	@echo "  • Interface: loopback (lo)"
	@echo "  • Bandwidth: $(RATE)"
	@echo "  • Delay:     $(LATENCY)"
	@echo "  • Loss:      $(LOSS)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@sudo tc qdisc add dev lo root netem rate $(RATE) delay $(LATENCY) loss $(LOSS)

net-clear:
	@sudo tc qdisc del dev lo root 2>/dev/null || true
	@echo "✅ Removed traffic control limits from localhost."

# --- Presets & Custom Limiters ---

net-limit:
	@$(MAKE) net-apply RATE="$(RATE)" LATENCY="$(LATENCY)" LOSS="$(LOSS)"

net-congested-wifi:
	@$(MAKE) net-apply RATE="10mbit" LATENCY="25ms" LOSS="0.3%"

net-mobile-1bar:
	@$(MAKE) net-apply RATE="3mbit" LATENCY="45ms" LOSS="0.5%"

help:
	@echo "Localhost Traffic Control Manual"
	@echo ""
	@echo "1. Run Services (Separate Terminals):"
	@echo "   make run-server          Run SFU binary on localhost"
	@echo "   make run-client          Run benchmark tool on localhost"
	@echo ""
	@echo "2. Bandwidth & Network Simulation:"
	@echo "   make net-limit RATE=5mbit"
	@echo "   make net-limit RATE=2mbit LATENCY=30ms LOSS=1%"
	@echo "   make net-congested-wifi"
	@echo "   make net-mobile-1bar"
	@echo "   make net-clear           Reset loopback interface to normal"
