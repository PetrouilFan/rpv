#!/bin/bash
IFACE="${RPV_IFACE:-wlan1}"

pkill hostapd 2>/dev/null || true
pkill dnsmasq 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
rm -f /tmp/rpv-hostapd.conf /tmp/rpv-dnsmasq.conf 2>/dev/null || true
systemctl start NetworkManager 2>/dev/null || true
echo "Interface $IFACE restored"
