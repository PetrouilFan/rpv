#!/bin/bash
set -e

# ── Configuration ──
IFACE="${RPV_IFACE:-}"
SSID="${RPV_SSID:-rpv-link}"
REGDOMAIN="${RPV_REGDOMAIN:-US}"
STA_IP="${RPV_STA_IP:-10.42.0.2}"
STA_SUBNET="24"
AP_IP="${RPV_AP_IP:-10.42.0.1}"
IFACE_FILE="/tmp/rpv-iface"

# ── Logging ──
log()  { echo "[RPV-GND] $*"; }
warn() { echo "[RPV-GND] WARN: $*" >&2; }
fail() { echo "[RPV-GND] FAIL: $*" >&2; exit 1; }

# ── Detect any WiFi adapter for station mode ──
detect_sta_wifi() {
    local iface=""
    for dev in /sys/class/net/*; do
        local name
        name=$(basename "$dev")
        if [ -d "$dev/wireless" ]; then
            if [ -n "$iface" ]; then
                warn "Multiple WiFi adapters found, using first: $iface"
            else
                iface="$name"
            fi
        fi
    done

    if [ -z "$iface" ]; then
        fail "No WiFi adapter found (looking for any interface in /sys/class/net/*/wireless)"
    fi
    echo "$iface"
}

# ── Auto-install dependencies ──
install_deps() {
    local missing=()
    for cmd in iw ip nmcli; do
        if ! command -v "$cmd" &>/dev/null; then
            missing+=("$cmd")
        fi
    done

    if [ ${#missing[@]} -gt 0 ]; then
        log "Installing missing dependencies: ${missing[*]}"
        if command -v apt-get &>/dev/null; then
            apt-get update -qq && apt-get install -y -qq "${missing[@]}" >/dev/null 2>&1
        elif command -v pacman &>/dev/null; then
            pacman -Sy --noconfirm "${missing[@]}" >/dev/null 2>&1
        else
            fail "Cannot install dependencies: unsupported package manager. Install: ${missing[*]}"
        fi
    fi
}

# ── Idempotent cleanup: disconnect, flush, restore managed state ──
cleanup() {
    log "Cleaning up stale state on $IFACE..."

    # Disconnect from any AP
    iw dev "$IFACE" disconnect 2>/dev/null || true

    # Kill wpa_supplicant only if it's using this interface
    if pgrep -f "wpa_supplicant.*$IFACE" &>/dev/null; then
        log "Stopping wpa_supplicant for $IFACE"
        pkill -f "wpa_supplicant.*$IFACE" 2>/dev/null || true
        sleep 0.3
    fi

    # Flush IP
    ip addr flush dev "$IFACE" 2>/dev/null || true
    ip link set "$IFACE" down 2>/dev/null || true
    sleep 0.3

    # Restore NetworkManager management first (in case previous run left it unmanaged)
    log "Ensuring NetworkManager manages $IFACE before re-setup"
    nmcli dev set "$IFACE" managed yes 2>/dev/null || true
    sleep 0.3
}

# ── Set regulatory domain ──
set_regdomain() {
    log "Setting regulatory domain to $REGDOMAIN"
    iw reg set "$REGDOMAIN" 2>/dev/null || warn "Failed to set regulatory domain to $REGDOMAIN"
    sleep 0.5
}

# ── Unmanage interface from NetworkManager ──
unmanage_from_nm() {
    log "Setting $IFACE as unmanaged by NetworkManager"
    nmcli dev set "$IFACE" managed no 2>/dev/null || {
        warn "nmcli failed to unmanage $IFACE - will try direct iw commands"
    }
    sleep 0.3
}

# ── Connect to AP ──
connect_to_ap() {
    log "Bringing up $IFACE"
    ip link set "$IFACE" up
    sleep 0.5

    log "Connecting to AP '$SSID' on $IFACE"
    iw dev "$IFACE" connect "$SSID" || {
        local rc=$?
        warn "iw connect returned $rc, may still be associating"
    }

    # Wait for association (up to 10s)
    log "Waiting for association..."
    local connected=false
    for i in $(seq 1 20); do
        if iw dev "$IFACE" link 2>/dev/null | grep -q "Connected"; then
            connected=true
            break
        fi
        sleep 0.5
    done

    if ! $connected; then
        fail "Failed to connect to AP '$SSID' after 10 seconds"
    fi

    local bssid freq
    bssid=$(iw dev "$IFACE" link 2>/dev/null | grep "Connected to" | awk '{print $3}' || true)
    freq=$(iw dev "$IFACE" link 2>/dev/null | grep "freq:" | awk '{print $2}' || true)
    log "Connected to AP (BSSID: $bssid, freq: ${freq}MHz)"
}

# ── Assign static IP ──
assign_ip() {
    log "Assigning $STA_IP/$STA_SUBNET to $IFACE"
    ip addr add "$STA_IP/$STA_SUBNET" dev "$IFACE"
    sleep 0.3
}

# ── Performance tuning ──
tune_performance() {
    log "Disabling power save on $IFACE"
    iw dev "$IFACE" set power_save off 2>/dev/null || warn "Failed to disable power save"

    log "Increasing socket buffer sizes"
    sysctl -w net.core.rmem_max=8388608 2>/dev/null || true
    sysctl -w net.core.wmem_max=8388608 2>/dev/null || true

    log "Setting CPU governor to performance"
    for gov in /sys/devices/system/cpu/*/cpufreq/scaling_governor; do
        echo performance > "$gov" 2>/dev/null || true
    done
}

# ── Health checks ──
health_check() {
    log "Running health checks..."
    local ok=true

    # Check interface is up
    if ! ip link show "$IFACE" | grep -q "state UP"; then
        warn "Interface $IFACE is not UP"
        ok=false
    fi

    # Check IP assignment
    if ! ip addr show "$IFACE" | grep -q "$STA_IP/$STA_SUBNET"; then
        warn "IP $STA_IP/$STA_SUBNET not assigned to $IFACE"
        ok=false
    fi

    # Check AP association
    if ! iw dev "$IFACE" link 2>/dev/null | grep -q "Connected"; then
        warn "Not connected to AP"
        ok=false
    fi

    # Check reachability to camera (ping)
    if ! ping -c 1 -W 2 "$AP_IP" &>/dev/null; then
        warn "Cannot ping camera at $AP_IP (may need a moment for ARP)"
        # Try ARP as fallback
        if ! arp -n | grep "$AP_IP" | grep -v "incomplete" &>/dev/null; then
            warn "No ARP entry for $AP_IP either"
        else
            log "ARP resolved $AP_IP (ping may be filtered)"
        fi
    else
        log "Successfully pinged camera at $AP_IP"
    fi

    if $ok; then
        log "All health checks passed"
    else
        warn "Some health checks failed - review warnings above"
    fi
}

# ── Main ──
main() {
    log "=== RPV Ground Station Network Setup ==="

    # Detect interface
    if [ -z "$IFACE" ]; then
        IFACE=$(detect_sta_wifi)
    fi
    log "Using WiFi interface: $IFACE"

    # Write detected interface for other services to read
    echo "$IFACE" > "$IFACE_FILE"

    # Install dependencies
    install_deps

    # Set regulatory domain
    set_regdomain

    # Cleanup stale state
    cleanup

    # Unmanage from NetworkManager
    unmanage_from_nm

    # Connect to AP
    connect_to_ap

    # Assign IP
    assign_ip

    # Performance tuning
    tune_performance

    # Health checks
    health_check

    log "=== STA Ready: $IFACE -> $SSID, IP $STA_IP/$STA_SUBNET ==="
}

main "$@"