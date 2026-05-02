#!/bin/bash
#
# RPV Ground Station Setup Script
# Run this on the ground station PC to connect to the Pi hotspot
# and start the RPV ground station.
#
# This script:
#   1. Finds the correct external WiFi interface
#   2. Connects to the 'rpv-link' hotspot
#   3. Waits for connection and verifies IP
#   4. Sets up monitor mode if needed (for raw mode)
#   5. Starts rpv-ground
#

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# ---- Configuration ----
HOTSSID="rpv-link"
HOTSPOT_IP="192.168.50.1"      # Pi's hotspot IP
CAMERA_TCP_PORT="9003"          # Camera TCP port
EXPECTED_SUBNET="192.168.50"
MAX_WAIT=60

# ---- Helper functions ----
log_info()  { echo "[INFO]  $*"; }
log_warn()  { echo "[WARN]  $*"; }
log_error() { echo "[ERROR] $*"; }
log_ok()    { echo "[OK]    $*"; }

# ---- 1. Find external WiFi interface ----
log_info "Step 1: Finding external WiFi adapter..."

# Get list of wireless interfaces (excluding internal/known ones)
WIFI_IFACES=()
for iface in $(ls /sys/class/net/ 2>/dev/null | grep -E '^wlan|^wl'); do
    # Skip virtual interfaces
    if [ -d "/sys/class/net/$iface/device" ]; then
        # Check if it's a USB device (external)
        if readlink -f /sys/class/net/$iface/device 2>/dev/null | grep -q 'usb'; then
            WIFI_IFACES+=("$iface")
            log_info "  Found USB WiFi adapter: $iface"
        elif [ "$iface" != "wlp3s0f0u1" ] && [ "$iface" != "wlan0" ]; then
            # Likely external (not typical internal name)
            WIFI_IFACES+=("$iface")
            log_info "  Found WiFi adapter: $iface"
        fi
    fi
done

# Also check with nmcli
NM_WIFI=$(nmcli -t -f NAME,DEVICE device 2>/dev/null | grep wifi | cut -d: -f2)
for iface in $NM_WIFI; do
    if [[ ! " ${WIFI_IFACES[@]} " =~ " ${iface} " ]]; then
        WIFI_IFACES+=("$iface")
    fi
done

if [ ${#WIFI_IFACES[@]} -eq 0 ]; then
    log_error "No external WiFi adapter found!"
    log_info "Available interfaces:"
    nmcli device 2>/dev/null || ls /sys/class/net/
    exit 1
fi

# Prefer the first external adapter
WIFI_IFACE="${WIFI_IFACES[0]}"
log_ok "Using WiFi interface: $WIFI_IFACE"

# ---- 2. Check current connection ----
log_info "Step 2: Checking current network state..."

CURRENT_SSID=$(nmcli -t -f active,ssid dev wifi 2>/dev/null | grep '^yes' | cut -d: -f2)
if [ "$CURRENT_SSID" = "$HOTSSID" ]; then
    log_ok "Already connected to $HOTSSID"
else
    log_info "Currently connected to: ${CURRENT_SSID:-<none>}"
fi

# Check current IP
CURRENT_IP=$(ip -4 addr show dev "$WIFI_IFACE" 2>/dev/null | grep inet | awk '{print $2}' | cut -d/ -f1)
if [ -n "$CURRENT_IP" ]; then
    log_info "Current IP on $WIFI_IFACE: $CURRENT_IP"
fi

# ---- 3. Connect to hotspot ----
log_info "Step 3: Connecting to hotspot '$HOTSSID'..."

# Check if hotspot is available
if nmcli -t -f SSID dev wifi 2>/dev/null | grep -q "^${HOTSSID}$"; then
    log_ok "Hotspot '$HOTSSID' is visible"
else
    log_warn "Hotspot '$HOTSSID' not found in scan results"
    log_info "Scanning for available networks..."
    nmcli device wifi list 2>/dev/null | head -10
fi

# Try to connect
if nmcli connection show "$HOTSSID" >/dev/null 2>&1; then
    log_info "Connection profile exists, activating..."
    nmcli connection up "$HOTSSID" 2>&1 || log_warn "Failed to activate existing profile"
else
    log_info "Creating new connection profile..."
    nmcli device wifi connect "$HOTSSID" 2>&1 || log_warn "Failed to connect (may need manual intervention)"
fi

# Wait for connection
log_info "Waiting for connection (max ${MAX_WAIT}s)..."
for i in $(seq 1 $MAX_WAIT); do
    CURRENT_SSID=$(nmcli -t -f active,ssid dev wifi 2>/dev/null | grep '^yes' | cut -d: -f2)
    if [ "$CURRENT_SSID" = "$HOTSSID" ]; then
        log_ok "Connected to $HOTSSID"
        break
    fi
    if [ $((i % 10)) -eq 0 ]; then
        log_info "  ... still waiting (${i}s)"
    fi
    sleep 1
done

if [ "$CURRENT_SSID" != "$HOTSSID" ]; then
    log_warn "Could not auto-connect to hotspot"
    log_info "Please manually connect to '$HOTSSID' and re-run this script"
    log_info "Or set a static IP: sudo ip addr add ${EXPECTED_SUBNET}.2/24 dev $WIFI_IFACE"
fi

# ---- 4. Verify IP address ----
log_info "Step 4: Verifying network configuration..."

CURRENT_IP=$(ip -4 addr show dev "$WIFI_IFACE" 2>/dev/null | grep inet | awk '{print $2}' | cut -d/ -f1)
if [ -n "$CURRENT_IP" ]; then
    log_ok "IP address: $CURRENT_IP"
    
    if echo "$CURRENT_IP" | grep -q "^${EXPECTED_SUBNET}"; then
        log_ok "IP is in correct subnet (${EXPECTED_SUBNET}.x)"
    else
        log_warn "IP is not in expected ${EXPECTED_SUBNET}.x subnet"
    fi
else
    log_warn "No IP address assigned"
    log_info "Attempting to set static IP..."
    if sudo ip addr add ${EXPECTED_SUBNET}.2/24 dev "$WIFI_IFACE" 2>/dev/null; then
        log_ok "Static IP set: ${EXPECTED_SUBNET}.2"
    else
        log_error "Failed to set static IP"
    fi
fi

# ---- 5. Test connectivity to Pi ----
log_info "Step 5: Testing connectivity to Pi ($HOTSPOT_IP)..."

if ping -c 1 -W 2 "$HOTSPOT_IP" >/dev/null 2>&1; then
    log_ok "Pi is reachable at $HOTSPOT_IP"
else
    log_warn "Cannot ping Pi at $HOTSPOT_IP"
    log_info "Checking routing..."
    ip route show | grep "$WIFI_IFACE" || true
fi

# Test TCP port
if nc -z -w 2 "$HOTSPOT_IP" "$CAMERA_TCP_PORT" 2>/dev/null; then
    log_ok "TCP port $CAMERA_TCP_PORT is open on Pi"
else
    log_warn "TCP port $CAMERA_TCP_PORT is not reachable"
    log_info "Camera may not be running yet"
fi

# ---- 6. Configure ground station ----
log_info "Step 6: Configuring ground station..."

CONFIG_DIR="$HOME/.config/rpv"
mkdir -p "$CONFIG_DIR"

GROUND_CONFIG="$CONFIG_DIR/ground.toml"

log_info "Writing ground station config to $GROUND_CONFIG"

cat > "$GROUND_CONFIG" <<EOF
interface = "$WIFI_IFACE"
drone_id = 1
transport = "tcp"
tcp_port = 9003
udp_port = 9001
ap_ssid = "rpv-link"
ap_channel = 6
video_width = 960
video_height = 540
gcs_uplink_port = 14551
gcs_downlink_port = 14550
EOF

log_ok "Config saved"

# ---- 7. Start ground station ----
log_info "Step 7: Starting RPV ground station..."

if [ -f "./target/release/rpv-ground" ]; then
    log_ok "Found rpv-ground binary"
    echo ""
    echo "Starting rpv-ground..."
    echo "Press Ctrl+C to stop"
    echo ""
    sudo ./target/release/rpv-ground
else
    log_error "rpv-ground binary not found!"
    log_info "Building..."
    cargo build --release 2>&1 | tail -20
    
    if [ -f "./target/release/rpv-ground" ]; then
        log_ok "Build successful, starting..."
        sudo ./target/release/rpv-ground
    else
        log_error "Build failed!"
        exit 1
    fi
fi
