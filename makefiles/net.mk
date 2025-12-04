IFACE ?= lo

.PHONY: all help \
	net-apply net-clear net-status net-verify \
	net-good-home-wifi net-congested-wifi net-stable-mobile net-lte-1bar \
	net-cross-continent net-unusable

all: help

# net-apply: Applies network simulation with AUTOMATIC handling of localhost double-counting
# Presets should specify TARGET_RTT and jitter as if measuring end-to-end
# This function divides by 2 automatically for localhost since packets traverse rules twice
net-apply: net-clear net-verify
	$(eval HALF_LATENCY=$(shell echo "$(TARGET_RTT)" | sed 's/ms//' | awk '{print $$1/2}'))
	$(eval HALF_UPLOAD_JITTER=$(shell echo "$(UPLOAD_JITTER)" | sed 's/ms.*//' | awk '{print $$1/2}'))
	$(eval HALF_DOWNLOAD_JITTER=$(shell echo "$(DOWNLOAD_JITTER)" | sed 's/ms.*//' | awk '{print $$1/2}'))
	$(eval JITTER_DIST=$(shell echo "$(UPLOAD_JITTER)" | grep -o 'distribution.*' || echo ""))
	
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Applying Network Simulation to $(IFACE)"
	@echo "Target RTT: $(TARGET_RTT)"
	@echo "Applied (halved for localhost): $(HALF_LATENCY)ms × 2 directions"
	@echo "--- Upload (Egress) ---"
	@echo "  Rate:           $(UPLOAD_RATE)"
	@echo "  Latency:        $(HALF_LATENCY)ms ± $(HALF_UPLOAD_JITTER)ms $(JITTER_DIST)"
	@echo "  Packet Loss:    $(UPLOAD_PACKET_LOSS)"
	@echo "--- Download (Ingress via same path) ---"
	@echo "  Rate:           $(DOWNLOAD_RATE)"
	@echo "  Latency:        $(HALF_LATENCY)ms ± $(HALF_DOWNLOAD_JITTER)ms $(JITTER_DIST)"
	@echo "  Packet Loss:    $(DOWNLOAD_PACKET_LOSS)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

	@echo "→ Applying network rules to $(IFACE)..."
	if [ -n "$(JITTER_DIST)" ]; then \
		sudo tc qdisc add dev $(IFACE) root netem \
			rate $(UPLOAD_RATE) \
			delay $(HALF_LATENCY)ms $(HALF_UPLOAD_JITTER)ms $(JITTER_DIST) \
			loss $(UPLOAD_PACKET_LOSS); \
	else \
		sudo tc qdisc add dev $(IFACE) root netem \
			rate $(UPLOAD_RATE) \
			delay $(HALF_LATENCY)ms $(HALF_UPLOAD_JITTER)ms \
			loss $(UPLOAD_PACKET_LOSS); \
	fi

	@echo ""
	@echo "✓ Network simulation is ACTIVE. Run 'make net-clear' to stop."


# --- Simulation Presets ---
# All presets specify target end-to-end RTT - net-apply handles localhost division automatically

# Pristine network with minimal issues.
# Target RTT: ~40ms
net-good-home-wifi:
	@$(MAKE) net-apply \
		TARGET_RTT="40ms" \
		UPLOAD_RATE="25mbit" \
		DOWNLOAD_RATE="100mbit" \
		UPLOAD_JITTER="10ms" \
		DOWNLOAD_JITTER="10ms" \
		UPLOAD_PACKET_LOSS="0%" \
		DOWNLOAD_PACKET_LOSS="0%"

# Simulates a home network with family members streaming, causing jitter and bandwidth contention.
# Target RTT: ~50ms (with jitter pushing it higher)
net-congested-wifi:
	@$(MAKE) net-apply \
		TARGET_RTT="50ms" \
		UPLOAD_RATE="5mbit" \
		DOWNLOAD_RATE="20mbit" \
		UPLOAD_JITTER="40ms" \
		DOWNLOAD_JITTER="80ms" \
		UPLOAD_PACKET_LOSS="0.2%" \
		DOWNLOAD_PACKET_LOSS="0.4%"

# Simulates a stable 4G/5G connection.
# Target RTT: ~80ms
net-mobile-stable:
	@$(MAKE) net-apply \
		TARGET_RTT="80ms" \
		UPLOAD_RATE="8mbit" \
		DOWNLOAD_RATE="40mbit" \
		UPLOAD_JITTER="30ms" \
		DOWNLOAD_JITTER="30ms" \
		UPLOAD_PACKET_LOSS="0%" \
		DOWNLOAD_PACKET_LOSS="0.2%"

net-mobile-low-bandwidth:
	@$(MAKE) net-apply \
		TARGET_RTT="160ms" \
		UPLOAD_RATE="1mbit" \
		DOWNLOAD_RATE="10mbit" \
		UPLOAD_JITTER="30ms" \
		DOWNLOAD_JITTER="30ms" \
		UPLOAD_PACKET_LOSS="0%" \
		DOWNLOAD_PACKET_LOSS="0.2%"

# Simulates LTE with 1 bar signal - harsh bufferbloat environment
# Real-world reference: Min ~75ms, Avg ~116ms, Max ~650ms, Loss ~0.3%
# Calibration: TARGET 90ms sets the correct mode/baseline. 
# Jitter 70ms with paretonormal provides the heavy tail without dragging the Min/Avg too far.
# 25% correlation simulates the "clumping" of latency spikes seen in the log.
net-mobile-1bar:
	@$(MAKE) net-apply \
		TARGET_RTT="90ms" \
		UPLOAD_RATE="3mbit" \
		DOWNLOAD_RATE="15mbit" \
		UPLOAD_JITTER="70ms 25% distribution pareto" \
		DOWNLOAD_JITTER="70ms 25% distribution pareto" \
		UPLOAD_PACKET_LOSS="0.15%" \
		DOWNLOAD_PACKET_LOSS="0.15%"  

# A stable but long-distance intercontinental link.
# Target RTT: ~140ms
net-cross-continent:
	@$(MAKE) net-apply \
		TARGET_RTT="140ms" \
		UPLOAD_RATE="10mbit" \
		DOWNLOAD_RATE="50mbit" \
		UPLOAD_JITTER="20ms" \
		DOWNLOAD_JITTER="40ms" \
		UPLOAD_PACKET_LOSS="0.2%" \
		DOWNLOAD_PACKET_LOSS="0.4%"

# A true "nightmare" scenario for resilience testing.
# Target RTT: ~300ms
net-unusable:
	@$(MAKE) net-apply \
		TARGET_RTT="300ms" \
		UPLOAD_RATE="500kbit" \
		DOWNLOAD_RATE="1mbit" \
		UPLOAD_JITTER="200ms" \
		DOWNLOAD_JITTER="300ms" \
		UPLOAD_PACKET_LOSS="5%" \
		DOWNLOAD_PACKET_LOSS="6%"


# --- Control and Status ---

net-clear:
	@echo "→ Clearing all network simulation rules..."
	@sudo tc qdisc del dev $(IFACE) root 2>/dev/null || true
	@echo "✓ Network simulation cleared."

net-status:
	@echo "━━━━━━━━━━━━━━━━━━ Network Status ━━━━━━━━━━━━━━━━━━"
	@echo "Interface: $(IFACE)"
	@echo "────────────────────────────────────────────────────────"
	@tc qdisc show dev $(IFACE) | sed 's/^/  /'
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

net-verify:
	@command -v tc >/dev/null 2>&1 || { echo "ERROR: 'tc' command not found. Please install 'iproute2'."; exit 1; }
	@ip link show $(IFACE) >/dev/null 2>&1 || { echo "ERROR: Interface '$(IFACE)' not found. Please verify the name."; exit 1; }

help:
	@echo "Network Simulation Makefile"
	@echo ""
	@echo "Usage:"
	@echo "  make net-<preset>     Apply a network simulation preset"
	@echo "  make net-clear        Remove all network simulation rules"
	@echo "  make net-status       Show current tc configuration"
	@echo ""
	@echo "Available presets:"
	@echo "  net-good-home-wifi       Pristine home WiFi (~40ms RTT)"
	@echo "  net-congested-wifi       Congested home WiFi (~50ms+ RTT)"
	@echo "  net-stable-mobile        Stable 4G/5G (~80ms RTT)"
	@echo "  net-lte-1bar             LTE 1-bar with bufferbloat (~90ms base, spikes to 600ms+)"
	@echo "  net-cross-continent      Intercontinental link (~140ms RTT)"
	@echo "  net-unusable             Nightmare scenario (~300ms RTT)"
	@echo ""
	@echo "Environment variables:"
	@echo "  IFACE=<n>             Network interface to apply rules to (default: lo)"
	@echo ""
	@echo "Note: All RTT values account for localhost double-traversal automatically"
