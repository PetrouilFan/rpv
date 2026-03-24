#!/bin/bash
set -e

IFACE="${RPV_IFACE:-wlan1}"   # RTL8821AU USB adapter

# Prevent NetworkManager from reclaiming the interface
NM_CONF="/etc/NetworkManager/conf.d/99-rpv.conf"
if [ -d "$(dirname "$NM_CONF")" ]; then
    IFACE_MAC=$(cat "/sys/class/net/$IFACE/address" 2>/dev/null || echo "")
    if [ -n "$IFACE_MAC" ]; then
        mkdir -p "$(dirname "$NM_CONF")"
        echo -e "[keyfile]\nunmanaged-devices=mac:$IFACE_MAC" > "$NM_CONF"
        systemctl reload NetworkManager 2>/dev/null || true
    fi
fi

# Idempotent teardown of any existing WiFi state
pkill wpa_supplicant 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
sleep 0.5

# Put interface into monitor mode
iw dev "$IFACE" set type monitor
ip link set "$IFACE" up

# Set target frequency — must match camera's frequency exactly
FREQ="${RPV_FREQ:-5180}"
iw dev "$IFACE" set freq "$FREQ"

# Disable power save — critical for latency
iw dev "$IFACE" set power_save off 2>/dev/null || true

# Set CPU governor to performance
for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
    echo performance > "$gov" 2>/dev/null || true
done

echo "Monitor mode ready on $IFACE @ 5180 MHz"
