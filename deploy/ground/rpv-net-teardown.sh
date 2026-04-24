#!/bin/bash
set -e

# ── Configuration ──
IFACE="${RPV_IFACE:-}"
IFACE_FILE="/tmp/rpv-iface"

# ── Logging ──
log()  { echo "[RPV-GND-TEARDOWN] $*"; }
warn() { echo "[RPV-GND-TEARDOWN] WARN: $*" >&2; }

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
    log "=== RPV Ground Station Network Teardown ==="

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
        warn "No WiFi interface found, proceeding with best-effort cleanup"
    fi

    # Disconnect from AP
    if [ -n "$IFACE" ]; then
        log "Disconnecting $IFACE from AP"
        iw dev "$IFACE" disconnect 2>/dev/null || true
    fi

    # Kill wpa_supplicant for this interface
    if [ -n "$IFACE" ]; then
        pkill -f "wpa_supplicant.*$IFACE" 2>/dev/null || true
    fi

    # Flush IP and bring down
    if [ -n "$IFACE" ]; then
        log "Flushing IP and bringing down $IFACE"
        ip addr flush dev "$IFACE" 2>/dev/null || true
        ip link set "$IFACE" down 2>/dev/null || true
    fi

    # Restore NetworkManager management
    if [ -n "$IFACE" ]; then
        log "Restoring NetworkManager management for $IFACE"
        nmcli dev set "$IFACE" managed yes 2>/dev/null || {
            warn "nmcli failed - NetworkManager may need manual restart"
        }
    fi

    # Let NetworkManager re-scan
    if command -v nmcli &>/dev/null; then
        log "Triggering NetworkManager re-scan"
        nmcli dev wifi rescan 2>/dev/null || true
    fi

    # Clean up interface marker
    rm -f "$IFACE_FILE"

    log "=== Teardown Complete ==="
}

main "$@"