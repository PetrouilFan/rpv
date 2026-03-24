#!/bin/bash
set -e

IFACE="wlan1"   # RTL8821AU

# Idempotent teardown
pkill hostapd 2>/dev/null || true
pkill wpa_supplicant 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
sleep 0.5

ip link set "$IFACE" up
ip addr add 192.168.100.1/24 dev "$IFACE"

# Launch hostapd in background
hostapd -B /etc/hostapd/rpv-hostapd.conf
sleep 1

# Disable power save — critical for latency
iw dev "$IFACE" set power_save off

# Reduce TX queue for lower latency
ip link set "$IFACE" txqueuelen 500

# Set CPU governor to performance
for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
    echo performance > "$gov" 2>/dev/null || true
done
