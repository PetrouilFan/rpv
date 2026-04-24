#!/bin/bash
set -e

# ── Configuration ──
IFACE="${RPV_IFACE:-}"
HOSTAPD_PID="/tmp/rpv-hostapd.pid"
DNSMASQ_PID="/tmp/rpv-dnsmasq.pid"
IFACE_FILE="/tmp/rpv-iface"

# ── Logging ──
log()  { echo "[RPV-CAM-TEARDOWN] $*"; }
warn() { echo "[RPV-CAM-TEARDOWN] WARN: $*" >&2; }

# ── Detect any WiFi adapter ──
detect_wifi() {
    local iface=""
    for dev in /sys/class/net/*; do
        local name
        name=$(basename "$dev")
        if [ -d "$dev/wireless" ]; then
            iface="$name"
            break
        fi
    done
    echo "$iface"
}

# ── Main ──
main() {
    log "=== RPV Camera Network Teardown ==="

    # Detect interface: env var > saved file > auto-detect
    if [ -z "$IFACE" ]; then
        if [ -f "$IFACE_FILE" ]; then
            IFACE=$(cat "$IFACE_FILE" 2>/dev/null || true)
        fi
        if [ -z "$IFACE" ]; then
            IFACE=$(detect_wifi)
        fi
    fi

    if [ -n "$IFACE" ]; then
        log "Using interface: $IFACE"
    else
        warn "No WiFi interface found, proceeding with process cleanup only"
    fi

    # Kill hostapd
    if [ -f "$HOSTAPD_PID" ]; then
        local pid
        pid=$(cat "$HOSTAPD_PID" 2>/dev/null || true)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            log "Stopping hostapd (PID $pid)"
            kill "$pid" 2>/dev/null || true
            sleep 0.5
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f "$HOSTAPD_PID"
    fi
    pkill -f "hostapd.*$IFACE" 2>/dev/null || true

    # Kill dnsmasq
    if [ -f "$DNSMASQ_PID" ]; then
        local pid
        pid=$(cat "$DNSMASQ_PID" 2>/dev/null || true)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            log "Stopping dnsmasq (PID $pid)"
            kill "$pid" 2>/dev/null || true
            sleep 0.3
        fi
        rm -f "$DNSMASQ_PID"
    fi
    pkill -f "dnsmasq.*$IFACE" 2>/dev/null || true

    # Flush IP and bring down
    if [ -n "$IFACE" ]; then
        log "Flushing IP and bringing down $IFACE"
        ip addr flush dev "$IFACE" 2>/dev/null || true
        ip link set "$IFACE" down 2>/dev/null || true
    fi

    # Clean up config files and interface marker
    rm -f /tmp/rpv-hostapd.conf /tmp/rpv-dnsmasq.conf /tmp/rpv-hostapd.log "$IFACE_FILE"

    # Restart NetworkManager to restore interface management
    if command -v systemctl &>/dev/null; then
        log "Restarting NetworkManager"
        systemctl restart NetworkManager 2>/dev/null || true
    fi

    log "=== Teardown Complete ==="
}

main "$@"