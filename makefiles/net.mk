IFACE ?= lo
IFB_IFACE ?= ifb0

.PHONY: all help \
	net-apply net-clear net-status net-verify \
	net-good-home-wifi net-congested-wifi net-stable-mobile net-lte-1bar \
	net-unstable-mobile net-cross-continent net-unusable net-high-latency

all: help

net-apply: net-clear net-verify
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Applying ASYMMETRIC Network Simulation to $(IFACE)"
	@echo "--- Upload (Egress on $(IFACE)) ---"
	@echo "  Rate:           $(UPLOAD_RATE)"
	@echo "  One-Way Latency:  $(UPLOAD_LATENCY) ± $(UPLOAD_JITTER)"
	@echo "  Packet Loss:    $(UPLOAD_PACKET_LOSS)"
	@echo "--- Download (Ingress on $(IFB_IFACE)) ---"
	@echo "  Rate:           $(DOWNLOAD_RATE)"
	@echo "  One-Way Latency:  $(DOWNLOAD_LATENCY) ± $(DOWNLOAD_JITTER)"
	@echo "  Packet Loss:    $(DOWNLOAD_PACKET_LOSS)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

	@# 1. Set up the IFB device for ingress shaping
	@echo "→ [1/4] Setting up IFB device for ingress control..."
	@sudo modprobe ifb numifbs=1
	@sudo ip link set dev $(IFB_IFACE) up

	@# 2. Apply egress (upload) rules to the main physical interface
	@echo "→ [2/4] Applying egress (upload) rules to $(IFACE)..."
	@sudo tc qdisc add dev $(IFACE) root netem \
		rate $(UPLOAD_RATE) \
		delay $(UPLOAD_LATENCY) $(UPLOAD_JITTER) \
		loss $(UPLOAD_PACKET_LOSS)

	@# 3. Redirect all ingress traffic from the physical interface to the IFB interface
	@echo "→ [3/4] Redirecting ingress traffic from $(IFACE) to $(IFB_IFACE)..."
	@sudo tc qdisc add dev $(IFACE) ingress
	@sudo tc filter add dev $(IFACE) parent ffff: protocol all u32 match u32 0 0 action mirred egress redirect dev $(IFB_IFACE)

	@# 4. Apply ingress (download) rules to the IFB interface
	@echo "→ [4/4] Applying ingress (download) rules to $(IFB_IFACE)..."
	@sudo tc qdisc add dev $(IFB_IFACE) root netem \
		rate $(DOWNLOAD_RATE) \
		delay $(DOWNLOAD_LATENCY) $(DOWNLOAD_JITTER) \
		loss $(DOWNLOAD_PACKET_LOSS)

	@echo ""
	@echo "✓ Asymmetric network simulation is ACTIVE. Run 'make net-clear' to stop."


# --- Simulation Presets ---
# To simulate a target RTT of X, set UPLOAD_LATENCY and DOWNLOAD_LATENCY to X/2.

# Simulates a home network with family members streaming, causing jitter and bandwidth contention.
# Target RTT: ~50ms
net-congested-wifi:
	@$(MAKE) net-apply \
		UPLOAD_RATE="5mbit" \
		DOWNLOAD_RATE="20mbit" \
		UPLOAD_LATENCY="25ms" \
		DOWNLOAD_LATENCY="25ms" \
		UPLOAD_JITTER="20ms" \
		DOWNLOAD_JITTER="40ms" \
		UPLOAD_PACKET_LOSS="0.1%" \
		DOWNLOAD_PACKET_LOSS="0.2%"

# Pristine network with minimal issues.
# Target RTT: ~40ms
net-good-home-wifi:
	@$(MAKE) net-apply \
		UPLOAD_RATE="25mbit" \
		DOWNLOAD_RATE="100mbit" \
		UPLOAD_LATENCY="20ms" \
		DOWNLOAD_LATENCY="20ms" \
		UPLOAD_JITTER="5ms" \
		DOWNLOAD_JITTER="5ms" \
		UPLOAD_PACKET_LOSS="0%" \
		DOWNLOAD_PACKET_LOSS="0%"

# Simulates a stable 4G/5G connection.
# Target RTT: ~80ms
net-stable-mobile:
	@$(MAKE) net-apply \
		UPLOAD_RATE="8mbit" \
		DOWNLOAD_RATE="40mbit" \
		UPLOAD_LATENCY="40ms" \
		DOWNLOAD_LATENCY="40ms" \
		UPLOAD_JITTER="15ms" \
		DOWNLOAD_JITTER="15ms" \
		UPLOAD_PACKET_LOSS="0%" \
		DOWNLOAD_PACKET_LOSS="0.1%"

# Simulates a very weak and unstable LTE connection.
# Min RTT: ~80ms, Avg RTT: ~160ms+, MDEV: ~75ms
net-lte-1bar:
	@$(MAKE) net-apply \
		UPLOAD_RATE="1mbit" \
		DOWNLOAD_RATE="4mbit" \
		UPLOAD_LATENCY="40ms" \
		DOWNLOAD_LATENCY="40ms" \
		UPLOAD_JITTER="160ms" \
		DOWNLOAD_JITTER="160ms" \
		UPLOAD_PACKET_LOSS="0%" \
		DOWNLOAD_PACKET_LOSS="0%"

# A stable but long-distance intercontinental link.
# Target RTT: ~140ms
net-cross-continent:
	@$(MAKE) net-apply \
		UPLOAD_RATE="10mbit" \
		DOWNLOAD_RATE="50mbit" \
		UPLOAD_LATENCY="70ms" \
		DOWNLOAD_LATENCY="70ms" \
		UPLOAD_JITTER="10ms" \
		DOWNLOAD_JITTER="20ms" \
		UPLOAD_PACKET_LOSS="0.1%" \
		DOWNLOAD_PACKET_LOSS="0.2%"

# A true "nightmare" scenario for resilience testing.
# Target RTT: ~300ms
net-unusable:
	@$(MAKE) net-apply \
		UPLOAD_RATE="500kbit" \
		DOWNLOAD_RATE="1mbit" \
		UPLOAD_LATENCY="150ms" \
		DOWNLOAD_LATENCY="150ms" \
		UPLOAD_JITTER="100ms" \
		DOWNLOAD_JITTER="150ms" \
		UPLOAD_PACKET_LOSS="2.5%" \
		DOWNLOAD_PACKET_LOSS="3.0%"


# --- Control and Status ---

net-clear:
	@echo "→ Clearing all network simulation rules..."
	@sudo tc qdisc del dev $(IFACE) root 2>/dev/null || true
	@sudo tc qdisc del dev $(IFACE) ingress 2>/dev/null || true
	@sudo tc qdisc del dev $(IFB_IFACE) root 2>/dev/null || true
	@sudo ip link set dev $(IFB_IFACE) down 2>/dev/null || true
	@echo "✓ Network simulation cleared."

net-status:
	@echo "━━━━━━━━━━━━━━━━━━ Network Status ━━━━━━━━━━━━━━━━━━"
	@echo "Interface: $(IFACE), Ingress via: $(IFB_IFACE)"
	@echo "────────────────────────────────────────────────────────"
	@echo "--- Egress (Upload) rules on $(IFACE) ---"
	@tc qdisc show dev $(IFACE) | sed 's/^/  /'
	@echo ""
	@echo "--- Ingress (Download) rules on $(IFB_IFACE) ---"
	@tc qdisc show dev $(IFB_IFACE) | sed 's/^/  /'
	@echo ""
	@echo "--- Redirection filter on $(IFACE) ---"
	@tc filter show dev $(IFACE) parent ffff: | sed 's/^/  /'
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

net-verify:
	@command -v tc >/dev/null 2>&1 || { echo "ERROR: 'tc' command not found. Please install 'iproute2'."; exit 1; }
	@ip link show $(IFACE) >/dev/null 2>&1 || { echo "ERROR: Interface '$(IFACE)' not found. Please verify the name."; exit 1; }
