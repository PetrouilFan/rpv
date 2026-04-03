#!/bin/bash
set -e

IFACE="${RPV_IFACE:-wlan1}"
SSID="${RPV_SSID:-rpv-link}"

# ── Force disconnect: kill anything using the interface ──
pkill wpa_supplicant 2>/dev/null || true
systemctl stop NetworkManager 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
sleep 0.5

# ── Bring interface up before connecting ──
ip link set "$IFACE" up
sleep 0.5

# ── Connect to open AP ──
iw dev "$IFACE" connect "$SSID"

# ── Wait for association (up to 10s) ──
for i in $(seq 1 20); do
    LINK_STATE=$(iw dev "$IFACE" link 2>/dev/null | grep -c "Connected" || true)
    if [ "$LINK_STATE" -gt 0 ]; then
        break
    fi
    sleep 0.5
done

# ── Set static IP ──
ip addr add 10.42.0.2/24 dev "$IFACE"

# ── Performance tuning ──
iw dev "$IFACE" set power_save off 2>/dev/null || true
sysctl -w net.core.rmem_max=8388608 2>/dev/null || true
sysctl -w net.core.wmem_max=8388608 2>/dev/null || true

for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
    echo performance > "$gov" 2>/dev/null || true
done

echo "Connected to AP '$SSID' on $IFACE, IP 10.42.0.2/24"
