#!/bin/bash
set -e

IFACE="${RPV_IFACE:-wlan1}"
SSID="${RPV_SSID:-rpv-link}"
CHANNEL="${RPV_CHANNEL:-6}"

# ── Force disconnect: kill anything using the interface ──
pkill wpa_supplicant 2>/dev/null || true
systemctl stop NetworkManager 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
sleep 0.5

# ── Kill any existing hostapd/dnsmasq ──
pkill hostapd 2>/dev/null || true
pkill dnsmasq 2>/dev/null || true
sleep 0.3

# ── Generate hostapd config (open network) ──
HOSTAPD_CONF="/tmp/rpv-hostapd.conf"
cat > "$HOSTAPD_CONF" <<EOF
interface=$IFACE
driver=nl80211
ssid=$SSID
hw_mode=g
channel=$CHANNEL
wmm_enabled=0
macaddr_acl=0
auth_algs=1
ignore_broadcast_ssid=0
EOF

# ── Start hostapd ──
hostapd "$HOSTAPD_CONF" -B
sleep 1

# ── Set static IP ──
ip addr add 10.42.0.1/24 dev "$IFACE"
ip link set "$IFACE" up

# ── Start dnsmasq for DHCP ──
DNSMASQ_CONF="/tmp/rpv-dnsmasq.conf"
cat > "$DNSMASQ_CONF" <<EOF
interface=$IFACE
dhcp-range=10.42.0.10,10.42.0.50,12h
dhcp-option=3,10.42.0.1
dhcp-option=6,8.8.8.8
EOF

dnsmasq -C "$DNSMASQ_CONF" --no-daemon &
sleep 0.5

# ── Performance tuning ──
iw dev "$IFACE" set power_save off 2>/dev/null || true
sysctl -w net.core.rmem_max=8388608 2>/dev/null || true
sysctl -w net.core.wmem_max=8388608 2>/dev/null || true

for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
    echo performance > "$gov" 2>/dev/null || true
done

echo "AP ready: $IFACE -> $SSID (ch $CHANNEL), IP 10.42.0.1/24"
