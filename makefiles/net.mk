# --- Configuration ---
SERVER_NS        ?= sfu_space
CLIENT_NS        ?= client_space
BRIDGE_IFACE     ?= test_bridge

# Interface Names (Kept clean of "veth_" prefixes for your Rust binary)
SERVER_VETH_NS   ?= netns_sfu
SERVER_VETH_BR   ?= netns_sfu_br
CLIENT_VETH_NS   ?= netns_client
CLIENT_VETH_BR   ?= netns_client_br

SERVER_IP        ?= 192.168.15.1
CLIENT_IP        ?= 192.168.15.2
SUBNET_MASK      ?= /24

# CPU Optimization Mask (Hexadecimal)
# 'f'  = Cores 0-3 handle background network driver interrupts.
# This leaves your high cores (e.g. 4-7) fully isolated for Thread-Per-Core SFU execution!
SYSTEM_CORE_MASK ?= f

# Execution Defaults (Override these via CLI)
SERVER_BIN  ?= ./target/release/pulsebeam
SERVER_ARGS ?= --dev
CLIENT_BIN  ?= ./target/release/pulsebeam-cli
CLIENT_ARGS ?= --api-url http://$(SERVER_IP):7070 bench \
  --rooms 1 \
  --max-rooms 120 \
  --users-per-room 4 \
  --arrival-rate 10 \
  --join-spread-secs 30 \
  --session-duration 7200 \
  --drain-duration 7200 \
  --fixed-session

# CPU Optimization Mask (Hexadecimal)
# '1' = Bit 0 only (Core 0 handles background network driver interrupts)
SYSTEM_CORE_MASK ?= 1

all: help

.PHONY: ns-setup ns-destroy

ns-setup: ns-destroy
	@echo "========================================================"
	@echo " Building Robust Asynchronous Network Sandbox"
	@echo "========================================================"
	
	@echo "→ Creating network namespaces..."
	@sudo ip netns add $(SERVER_NS)
	@sudo ip netns add $(CLIENT_NS)
	
	@echo "→ Provisioning central Linux Bridge ($(BRIDGE_IFACE))..."
	@sudo ip link add $(BRIDGE_IFACE) type bridge
	@sudo ip link set $(BRIDGE_IFACE) up
	
	@echo "→ Creating decoupled hardware-veth lines..."
	@sudo ip link add $(SERVER_VETH_NS) type veth peer name $(SERVER_VETH_BR)
	@sudo ip link add $(CLIENT_VETH_NS) type veth peer name $(CLIENT_VETH_BR)
	
	@echo "→ Structuring network topologies..."
	@sudo ip link set $(SERVER_VETH_NS) netns $(SERVER_NS)
	@sudo ip link set $(SERVER_VETH_BR) master $(BRIDGE_IFACE)
	@sudo ip link set $(CLIENT_VETH_NS) netns $(CLIENT_NS)
	@sudo ip link set $(CLIENT_VETH_BR) master $(BRIDGE_IFACE)
	
	@echo "→ Activating bridge interfaces..."
	@sudo ip link set $(SERVER_VETH_BR) up
	@sudo ip link set $(CLIENT_VETH_BR) up
	
	@echo "→ Configuring and initializing Server Space [$(SERVER_NS)]..."
	@sudo ip netns exec $(SERVER_NS) ip addr add $(SERVER_IP)$(SUBNET_MASK) dev $(SERVER_VETH_NS)
	@sudo ip netns exec $(SERVER_NS) ip link set $(SERVER_VETH_NS) up
	@sudo ip netns exec $(SERVER_NS) ip link set lo up
	
	@echo "→ Configuring and initializing Client Space [$(CLIENT_NS)]..."
	@sudo ip netns exec $(CLIENT_NS) ip addr add $(CLIENT_IP)$(SUBNET_MASK) dev $(CLIENT_VETH_NS)
	@sudo ip netns exec $(CLIENT_NS) ip link set $(CLIENT_VETH_NS) up
	@sudo ip netns exec $(CLIENT_NS) ip link set lo up
	
	@echo "→ Applying socket tuning and asynchronous core isolation..."
	@sudo sysctl -w net.core.wmem_max=134217728 >/dev/null
	@sudo sysctl -w net.core.rmem_max=134217728 >/dev/null
	@sudo sysctl -w net.core.rps_sock_flow_entries=32768 >/dev/null
	
	@# Direct background RX/TX packet interrupts strictly to Core 0
	@echo "$(SYSTEM_CORE_MASK)" | sudo tee /sys/class/net/$(SERVER_VETH_BR)/queues/rx-0/rps_cpus >/dev/null
	@echo "$(SYSTEM_CORE_MASK)" | sudo tee /sys/class/net/$(CLIENT_VETH_BR)/queues/rx-0/rps_cpus >/dev/null
	
	@# Enable modern low-latency WebRTC offloads on the isolated interfaces
	@sudo ip netns exec $(SERVER_NS) ethtool -K $(SERVER_VETH_NS) gro on rx-gro-list off rx-udp-gro-forwarding on gso on tx-gso-partial on 2>/dev/null || true
	@sudo ip netns exec $(CLIENT_NS) ethtool -K $(CLIENT_VETH_NS) gro on rx-gro-list off rx-udp-gro-forwarding on gso on tx-gso-partial on 2>/dev/null || true
	
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "✅ Robust Sandbox Deployed Successfully!"
	@echo "   • SFU Target IP: $(SERVER_IP)"
	@echo "   • Host Network Interrupt Core: Core 0 (Mask: $(SYSTEM_CORE_MASK))"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

ns-destroy:
	@echo "→ Tearing down active topologies..."
	@sudo ip netns del $(SERVER_NS) 2>/dev/null || true
	@sudo ip netns del $(CLIENT_NS) 2>/dev/null || true
	@sudo ip link del $(BRIDGE_IFACE) 2>/dev/null || true

# --- Optimization & Offloads ---

net-tune:
	@sudo sysctl -w net.core.wmem_max=134217728 >/dev/null
	@sudo sysctl -w net.core.rmem_max=134217728 >/dev/null
	@sudo ip netns exec $(SERVER_NS) ethtool -K $(SERVER_VETH) gro on rx-gro-list off rx-udp-gro-forwarding on gso on tx-gso-partial on 2>/dev/null || true
	@sudo ip netns exec $(CLIENT_NS) ethtool -K $(CLIENT_VETH) gro on rx-gro-list off rx-udp-gro-forwarding on gso on tx-gso-partial on 2>/dev/null || true

# --- Application Execution Engine ---

.PHONY: run-server run-client

run-server: ns-verify-sfu
	@echo "🚀 Launching SFU Server inside namespace [$(SERVER_NS)]..."
	@echo "   Command: $(SERVER_BIN) $(SERVER_ARGS)"
	@echo "────────────────────────────────────────────────────────"
	@sudo ip netns exec $(SERVER_NS) $(SERVER_BIN) $(SERVER_ARGS)

run-client: ns-verify-client
	@echo "🔥 Launching Benchmark Client inside namespace [$(CLIENT_NS)]..."
	@echo "   Command: $(CLIENT_BIN) $(CLIENT_ARGS)"
	@echo "────────────────────────────────────────────────────────"
	@sudo ip netns exec $(CLIENT_NS) $(CLIENT_BIN) $(CLIENT_ARGS)

# --- Traffic Control Simulation ---

net-apply: net-clear
	$(eval LATENCY=$(shell echo "$(TARGET_RTT)" | sed 's/ms//'))
	$(eval UPLOAD_JITTER_VAL=$(shell echo "$(UPLOAD_JITTER)" | sed 's/ms.*//'))
	$(eval DOWNLOAD_JITTER_VAL=$(shell echo "$(DOWNLOAD_JITTER)" | sed 's/ms.*//'))
	$(eval JITTER_DIST=$(shell echo "$(UPLOAD_JITTER)" | grep -o 'distribution.*' || echo ""))
	
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "Applying Simulation: RTT $(TARGET_RTT)"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@if [ -n "$(JITTER_DIST)" ]; then \
		sudo ip netns exec $(SERVER_NS) tc qdisc add dev $(SERVER_VETH) root netem rate $(UPLOAD_RATE) delay $(LATENCY)ms $(UPLOAD_JITTER_VAL)ms $(JITTER_DIST) loss $(UPLOAD_PACKET_LOSS); \
		sudo ip netns exec $(CLIENT_NS) tc qdisc add dev $(CLIENT_VETH) root netem rate $(DOWNLOAD_RATE) delay $(LATENCY)ms $(DOWNLOAD_JITTER_VAL)ms $(JITTER_DIST) loss $(DOWNLOAD_PACKET_LOSS); \
	else \
		sudo ip netns exec $(SERVER_NS) tc qdisc add dev $(SERVER_VETH) root netem rate $(UPLOAD_RATE) delay $(LATENCY)ms $(UPLOAD_JITTER_VAL)ms loss $(UPLOAD_PACKET_LOSS); \
		sudo ip netns exec $(CLIENT_NS) tc qdisc add dev $(CLIENT_VETH) root netem rate $(DOWNLOAD_RATE) delay $(LATENCY)ms $(DOWNLOAD_JITTER_VAL)ms loss $(DOWNLOAD_PACKET_LOSS); \
	fi

net-clear:
	@sudo ip netns exec $(SERVER_NS) tc qdisc del dev $(SERVER_VETH) root 2>/dev/null || true
	@sudo ip netns exec $(CLIENT_NS) tc qdisc del dev $(CLIENT_VETH) root 2>/dev/null || true

# --- Presets ---
net-regional-server:
	@$(MAKE) net-apply TARGET_RTT="25ms" UPLOAD_RATE="10000mbit" DOWNLOAD_RATE="10000mbit" UPLOAD_JITTER="3ms" DOWNLOAD_JITTER="3ms" UPLOAD_PACKET_LOSS="0%" DOWNLOAD_PACKET_LOSS="0%"

net-congested-wifi:
	@$(MAKE) net-apply TARGET_RTT="50ms" UPLOAD_RATE="5mbit" DOWNLOAD_RATE="20mbit" UPLOAD_JITTER="40ms" DOWNLOAD_JITTER="80ms" UPLOAD_PACKET_LOSS="0.2%" DOWNLOAD_PACKET_LOSS="0.4%"

net-mobile-1bar:
	@$(MAKE) net-apply TARGET_RTT="90ms" UPLOAD_RATE="3mbit" DOWNLOAD_RATE="15mbit" UPLOAD_JITTER="70ms 25% distribution pareto" DOWNLOAD_JITTER="70ms 25% distribution pareto" UPLOAD_PACKET_LOSS="0.15%" DOWNLOAD_PACKET_LOSS="0.15%"

# --- Verifications ---
ns-verify-sfu:
	@ip netns list | grep -q $(SERVER_NS) || { echo "ERROR: Namespace '$(SERVER_NS)' not found. Run 'make ns-setup' first."; exit 1; }
ns-verify-client:
	@ip netns list | grep -q $(CLIENT_NS) || { echo "ERROR: Namespace '$(CLIENT_NS)' not found. Run 'make ns-setup' first."; exit 1; }

help:
	@echo "Namespace Runner Manual"
	@echo ""
	@echo "1. Infrastructure Build:"
	@echo "   make ns-setup             Build & tune namespaces"
	@echo "   make ns-destroy           Destroy network sandboxes"
	@echo ""
	@echo "2. Process Execution (In separate terminals):"
	@echo "   make run-server           Executes the SFU server in its namespace"
	@echo "   make run-client           Executes the load benchmark in its namespace"
	@echo ""
	@echo "3. Profile Swapping:"
	@echo "   make net-regional-server"
	@echo "   make net-congested-wifi"
	@echo "   make net-mobile-1bar"
	@echo ""
	@echo "Modifiers Example:"
	@echo "   make run-server SERVER_BIN=./mediasoup-sfu SERVER_ARGS='--config config.json'"
