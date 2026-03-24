#!/bin/bash
set -e

IFACE="${RPV_IFACE:-wlan1}"   # RTL8821AU USB adapter

# Idempotent teardown of any existing WiFi state
pkill hostapd 2>/dev/null || true
pkill wpa_supplicant 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
ip link set "$IFACE" nomacaddr 2>/dev/null || true
sleep 0.5

# Put interface into monitor mode
iw dev "$IFACE" set type monitor
ip link set "$IFACE" up

# Set target frequency — 5GHz channel 36 (5180 MHz)
# Adjust to match your hardware and regulatory domain.
iw dev "$IFACE" set freq 5180

# Disable power save — critical for latency
iw dev "$IFACE" set power_save off 2>/dev/null || true

# Set CPU governor to performance
for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
    echo performance > "$gov" 2>/dev/null || true
done

echo "Monitor mode ready on $IFACE @ 5180 MHz"
