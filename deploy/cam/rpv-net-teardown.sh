#!/bin/bash
IFACE="${RPV_IFACE:-wlan1}"
ip link set "$IFACE" down 2>/dev/null || true
iw dev "$IFACE" set type managed 2>/dev/null || true
ip link set "$IFACE" up 2>/dev/null || true
echo "Interface $IFACE restored to managed mode"
