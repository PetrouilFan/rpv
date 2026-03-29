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
pkill hostapd 2>/dev/null || true
pkill wpa_supplicant 2>/dev/null || true
# #25: Mask wpa_supplicant so systemd doesn't restart it mid-flight
systemctl mask wpa_supplicant@${IFACE}.service 2>/dev/null || true
systemctl mask wpa_supplicant.service 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
ip link set "$IFACE" nomacaddr 2>/dev/null || true
sleep 0.5

# Put interface into monitor mode
iw dev "$IFACE" set type monitor
ip link set "$IFACE" up

# Set target frequency — 2.4 GHz channel 6 (2437 MHz) for better penetration/range
# Override with RPV_FREQ env var (e.g., RPV_FREQ=2412 for ch1, RPV_FREQ=2462 for ch11)
FREQ="${RPV_FREQ:-2437}"
iw dev "$IFACE" set freq "$FREQ"

# Max out TX power (fixed 3000 = 30 dBm)
iw dev "$IFACE" set txpower fixed 3000 2>/dev/null || true

# Disable power save — critical for latency
iw dev "$IFACE" set power_save off 2>/dev/null || true

# Bypass Linux socket buffer doubling: set hard ceiling to 8 MB
sysctl -w net.core.rmem_max=8388608 2>/dev/null || true
sysctl -w net.core.wmem_max=8388608 2>/dev/null || true

# #4: Set CPU governor to performance (handles pstate driver too)
for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
    echo performance > "$gov" 2>/dev/null || true
done
# Also try intel_pstate path
for gov in /sys/devices/system/cpu/*/cpufreq/energy_performance_preference; do
    echo performance > "$gov" 2>/dev/null || true
done

# #20: For true zero-jitter RF, add isolcpus=0,1 to /boot/firmware/cmdline.txt
# This hides cores 0 and 1 from the kernel scheduler entirely.
echo "Monitor mode ready on $IFACE @ 2437 MHz (2.4 GHz ch6)"
