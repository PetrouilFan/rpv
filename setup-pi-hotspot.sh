#!/bin/bash
#
# RPV Pi Hotspot Setup Script
# Run this on the Raspberry Pi (10.0.0.59) to set up a WiFi AP
# using the external WiFi adapter (RTL8821AU) for RPV communication.
#
# This script:
#   1. Installs hostapd and dnsmasq
#   2. Configures the external WiFi adapter as an AP
#   3. Sets up DHCP/DNS for the hotspot
#   4. Enables IP forwarding and NAT (so Pi can still reach internet)
#   5. Ensures SSH is accessible via the hotspot interface
#
# The internal wlan0 stays connected to the home network (SmartHome).
# The external WiFi adapter (wlan1/phy1) becomes the RPV hotspot.
#

set -e

echo "=== RPV Pi Hotspot Setup ==="

# ---- 1. Identify the external WiFi adapter ----
echo "[1/7] Identifying external WiFi adapter..."

# Get the physical device for each interface
INT_WLAN_PHY=$(readlink -f /sys/class/net/wlan0/device/phy80211 2>/dev/null | grep -o 'phy[0-9]*' || true)
EXT_WLAN_IFACE=""
EXT_WLAN_PHY=""

# Look for USB WiFi adapters (RTL8821AU typically shows up as wlan1, wlan2, etc.)
for iface in $(ls /sys/class/net/ | grep -E '^wl' | grep -v wlan0); do
    phy=$(readlink -f /sys/class/net/${iface}/device/phy80211 2>/dev/null | grep -o 'phy[0-9]*' || true)
    if [ -n "$phy" ] && [ "$phy" != "$INT_WLAN_PHY" ]; then
        EXT_WLAN_IFACE="$iface"
        EXT_WLAN_PHY="$phy"
        break
    fi
done

# Also check USB devices
USB_WIFI=$(lsusb 2>/dev/null | grep -i 'RTL8821AU\|Realtek' | head -1 || true)

if [ -z "$EXT_WLAN_IFACE" ]; then
    echo "WARNING: Could not auto-detect external WiFi adapter."
    echo "USB WiFi info: ${USB_WIFI:-none}"
    echo "Available wireless interfaces:"
    ls /sys/class/net/ | grep -E '^wl' || true
    echo ""
    echo "Please manually set EXT_WLAN_IFACE below."
    # Try common names
    for try_iface in wlan1 wlan2 wlx00c0ca000001; do
        if [ -d "/sys/class/net/$try_iface" ]; then
            EXT_WLAN_IFACE="$try_iface"
            echo "Trying $try_iface..."
            break
        fi
    done
fi

if [ -z "$EXT_WLAN_IFACE" ]; then
    echo "ERROR: No external WiFi adapter found!"
    echo "Make sure the RTL8821AU driver is loaded (88XXau kernel module)."
    exit 1
fi

echo "  Internal WiFi: wlan0 (phy${INT_WLAN_PHY})"
echo "  External WiFi: ${EXT_WLAN_IFACE} (phy${EXT_WLAN_PHY})"
echo "  USB WiFi:      ${USB_WIFI:-N/A}"

# ---- 2. Install hostapd and dnsmasq ----
echo "[2/7] Installing hostapd and dnsmasq..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq hostapd dnsmasq iw rfkill 2>&1 | tail -5

# ---- 3. Stop services before configuring ----
echo "[3/7] Stopping existing services..."
systemctl stop hostapd 2>/dev/null || true
systemctl stop dnsmasq 2>/dev/null || true

# ---- 4. Configure hostapd ----
echo "[4/7] Configuring hostapd..."
cat > /etc/hostapd/hostapd.conf <<EOF
# RPV Hotspot Configuration
interface=${EXT_WLAN_IFACE}
driver=nl80211

# Network
ssid=rpv-link
hw_mode=g
channel=6
wmm_enabled=0

# No encryption (RPV handles its own security)
auth_algs=1
wpa=0

# Beacon interval (100ms for faster discovery)
beacon_int=100

# Allow hidden SSID (not really hidden, but don't broadcast excessively)
ignore_broadcast_ssid=0

# DTIM period
dtim_period=2

# Max stations (just the ground station)
max_num_sta=2

# RTS/CTS threshold (lower for better reliability)
rts_threshold=2347
fragm_threshold=2346

# TX power (in mBm, ~20dBm = 100mW is typical)
txpower=20

# Country code (affects allowed channels)
country_code=US
ieee80211d=1
ieee80211h=0
EOF

# Point hostapd to the config
echo 'DAEMON_CONF="/etc/hostapd/hostapd.conf"' > /etc/default/hostapd

# ---- 5. Configure dnsmasq (DHCP for hotspot) ----
echo "[5/7] Configuring dnsmasq..."
# Backup existing config
[ -f /etc/dnsmasq.conf ] && mv /etc/dnsmasq.conf /etc/dnsmasq.conf.backup

cat > /etc/dnsmasq.conf <<EOF
# RPV Hotspot DHCP Configuration
interface=${EXT_WLAN_IFACE}

# Don't listen on other interfaces
bind-interfaces

# DHCP range (small range, just for ground station)
dhcp-range=192.168.50.100,192.168.50.101,255.255.255.0,12h

# Gateway (the Pi's hotspot IP)
dhcp-option=3,192.168.50.1

# DNS servers (Cloudflare + Google)
dhcp-option=6,1.1.1.1,8.8.8.8

# Hostname for the Pi (so ground station can find it)
dhcp-option=12,rpv-pi
dhcp-host=set:ground,rpv-pi

# Logging
log-dhcp
log-facility=/var/log/dnsmasq-hotspot.log

# Don't read /etc/resolv.conf
no-resolv

# Static leases (reserve IP for ground station if it identifies itself)
# dhcp-host=aa:bb:cc:dd:ee:ff,192.168.50.100
EOF

# ---- 6. Configure network interfaces ----
echo "[6/7] Configuring network interfaces..."

# Bring up the hotspot interface with static IP
ip link set ${EXT_WLAN_IFACE} down 2>/dev/null || true
ip addr flush dev ${EXT_WLAN_IFACE} 2>/dev/null || true
ip link set ${EXT_WLAN_IFACE} up
ip addr add 192.168.50.1/24 dev ${EXT_WLAN_IFACE}

# Enable IP forwarding (so Pi can route between eth0/wlan0 and hotspot)
echo 1 > /proc/sys/net/ipv4/ip_forward
sysctl -w net.ipv4.ip_forward=1 2>/dev/null || true

# Set up NAT/masquerade so hotspot clients can reach internet via eth0
# (but only if needed - RPV uses its own protocol)
iptables -t nat -A POSTROUTING -o eth0 -j MASQUERADE 2>/dev/null || true
iptables -t nat -A POSTROUTING -o wlan0 -j MASQUERADE 2>/dev/null || true

# Allow forwarding from hotspot to main network
iptables -A FORWARD -i ${EXT_WLAN_IFACE} -o eth0 -j ACCEPT 2>/dev/null || true
iptables -A FORWARD -i ${EXT_WLAN_IFACE} -o wlan0 -j ACCEPT 2>/dev/null || true
iptables -A FORWARD -i eth0 -o ${EXT_WLAN_IFACE} -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || true
iptables -A FORWARD -i wlan0 -o ${EXT_WLAN_IFACE} -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || true

# ---- 7. Start services ----
echo "[7/7] Starting services..."

# Start hostapd
systemctl unmask hostapd 2>/dev/null || true
systemctl start hostapd
sleep 2

# Start dnsmasq
systemctl stop dnsmasq 2>/dev/null || true
systemctl start dnsmasq
sleep 1

# Verify
if ip addr show ${EXT_WLAN_IFACE} | grep -q '192.168.50.1'; then
    echo "  ✓ Hotspot interface configured: 192.168.50.1"
else
    echo "  ✗ Failed to configure hotspot interface"
fi

if pgrep -x hostapd > /dev/null; then
    echo "  ✓ hostapd is running"
else
    echo "  ✗ hostapd is NOT running"
fi

if pgrep -x dnsmasq > /dev/null; then
    echo "  ✓ dnsmasq is running"
else
    echo "  ✗ dnsmasq is NOT running"
fi

# Check if wlan0 (internal) is still connected
if iwconfig wlan0 2>/dev/null | grep -q 'Access Point'; then
    echo "  ✓ Internal WiFi (wlan0) still connected to home network"
fi

echo ""
echo "=== Hotspot Setup Complete ==="
echo ""
echo "Hotspot SSID:    rpv-link"
echo "Hotspot IP:      192.168.50.1"
echo "DHCP Range:      192.168.50.100 - 192.168.50.101"
echo "Channel:         6"
echo ""
echo "To connect from ground station:"
echo "  1. Connect to 'rpv-link' WiFi network"
echo "  2. Ground station will get IP 192.168.50.100"
echo "  3. Pi camera can be reached at 192.168.50.1:9003 (TCP)"
echo ""
echo "To make this persistent across reboots:"
echo "  systemctl enable hostapd"
echo "  systemctl enable dnsmasq"
echo ""
echo "Current connections:"
arp -a 2>/dev/null | grep '192.168.50' || echo "  (none yet)"
