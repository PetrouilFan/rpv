#!/bin/bash
set -e

IFACE="wlan1"   # RTL8821AU

# Idempotent teardown
pkill wpa_supplicant 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
sleep 0.5

ip link set "$IFACE" up
wpa_supplicant -B -i "$IFACE" -c /etc/wpa_supplicant/rpv-wpa.conf -P /run/rpv-wpa.pid
sleep 2
dhclient "$IFACE" 2>/dev/null || ip addr add 192.168.100.2/24 dev "$IFACE"
iw dev "$IFACE" set power_save off
ip link set "$IFACE" txqueuelen 500

# Set CPU governor to performance
for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
    echo performance > "$gov" 2>/dev/null || true
done
