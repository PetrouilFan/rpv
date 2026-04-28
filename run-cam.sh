#!/bin/bash
#
# RPV Camera (Pi) Setup and Run Script
# Run this on the Raspberry Pi to start the RPV camera.
#
# This script:
#   1. Optionally sets up the hotspot (if not already running)
#   2. Configures the camera
#   3. Starts rpv-cam
#

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# ---- Configuration ----
HOTSSID="rpv-link"
HOTSPOT_IP="192.168.50.1"
CAMERA_TCP_PORT="9003"

# ---- Helper functions ----
log_info()  { echo "[INFO]  $*"; }
log_ok()    { echo "[OK]    $*"; }
log_warn()  { echo "[WARN]  $*"; }

# ---- 1. Check if hotspot is running ----
log_info "Checking hotspot status..."

# Find external WiFi adapter (for hotspot)
EXT_IFACE=""
for iface in $(ls /sys/class/net/ | grep -E '^wl'); do
    if [ "$iface" != "wlan0" ]; then
        # Check if it's a USB device
        if readlink -f /sys/class/net/$iface/device 2>/dev/null | grep -q 'usb'; then
            EXT_IFACE="$iface"
            break
        fi
    fi
done

if [ -n "$EXT_IFACE" ]; then
    log_info "External WiFi adapter: $EXT_IFACE"
    
    # Check if hostapd is running
    if pgrep -x hostapd > /dev/null; then
        log_ok "hostapd is running"
    else
        log_warn "hostapd is not running"
        log_info "Run ./setup-pi-hotspot.sh to set up the hotspot"
    fi
    
    # Check hotspot interface IP
    if ip addr show $EXT_IFACE 2>/dev/null | grep -q '192.168.50.1'; then
        log_ok "Hotspot IP configured: 192.168.50.1"
    else
        log_warn "Hotspot IP not configured"
    fi
else
    log_warn "No external WiFi adapter found"
fi

# ---- 2. Configure camera ----
log_info "Configuring camera..."

CONFIG_DIR="$HOME/.config/rpv"
mkdir -p "$CONFIG_DIR"

CAM_CONFIG="$CONFIG_DIR/cam.toml"

log_info "Writing camera config to $CAM_CONFIG"

cat > "$CAM_CONFIG" <<EOF
# Network
interface = "wlan0"
drone_id = 1
transport = "tcp"
tcp_port = 9003
udp_port = 9001
ap_ssid = "rpv-link"
ap_channel = 6
video_width = 960
video_height = 540

# Camera settings
video_device = "/dev/video0"
camera_type = "csi"
fc_port = "/dev/ttyAMA0"
fc_baud = 115200
framerate = 30
bitrate = 3000000
intra = 30

# RPV settings (TCP mode - camera connects to ground station)
# For TCP mode, set peer_addr to the ground station's IP on the hotspot
# Example: if ground station gets 192.168.50.100, set:
# peer_addr = "192.168.50.100:9003"
#
# If not set, camera will use discovery to find ground station
# peer_addr = ""
EOF

log_ok "Config saved"

# ---- 3. Show network status ----
log_info "Network status:"
echo "  wlan0 (internal/home): $(ip -4 addr show wlan0 2>/dev/null | grep inet | awk '{print $2}')"
if [ -n "$EXT_IFACE" ]; then
    echo "  $EXT_IFACE (hotspot):   $(ip -4 addr show $EXT_IFACE 2>/dev/null | grep inet | awk '{print $2}')"
fi
echo ""

# ---- 4. Start camera ----
log_info "Starting RPV camera..."

if [ -f "./target/release/rpv-cam" ]; then
    log_ok "Found rpv-cam binary ($(du -h ./target/release/rpv-cam | cut -f1))"
    echo ""
    echo "Starting rpv-cam..."
    echo "Press Ctrl+C to stop"
    echo ""
    sudo ./target/release/rpv-cam
else
    log_warn "rpv-cam binary not found"
    log_info "Building..."
    cargo build --release 2>&1 | tail -20
    
    if [ -f "./target/release/rpv-cam" ]; then
        log_ok "Build successful, starting..."
        sudo ./target/release/rpv-cam
    else
        log_error "Build failed!"
        exit 1
    fi
fi
