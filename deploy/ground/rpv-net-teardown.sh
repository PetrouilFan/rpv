#!/bin/bash
IFACE="${RPV_IFACE:-wlan1}"

iw dev "$IFACE" disconnect 2>/dev/null || true
ip link set "$IFACE" down 2>/dev/null || true
ip addr flush dev "$IFACE" 2>/dev/null || true
systemctl start NetworkManager 2>/dev/null || true
echo "Interface $IFACE restored"
