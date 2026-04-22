#!/bin/bash
set -e

# ── Configuration ──
IFACE="${RPV_IFACE:-}"
SSID="${RPV_SSID:-rpv-link}"
CHANNEL="${RPV_CHANNEL:-6}"
REGDOMAIN="${RPV_REGDOMAIN:-US}"
AP_IP="${RPV_AP_IP:-10.42.0.1}"
AP_SUBNET="24"
DHCP_START="${RPV_DHCP_START:-10.42.0.10}"
DHCP_END="${RPV_DHCP_END:-10.42.0.50}"
HOSTAPD_PID="/tmp/rpv-hostapd.pid"
DNSMASQ_PID="/tmp/rpv-dnsmasq.pid"
IFACE_FILE="/tmp/rpv-iface"

# ── Logging ──
log()  { echo "[RPV-CAM] $*"; }
warn() { echo "[RPV-CAM] WARN: $*" >&2; }
fail() { echo "[RPV-CAM] FAIL: $*" >&2; exit 1; }

# ── Detect any WiFi adapter that supports AP mode ──
detect_ap_wifi() {
    local iface=""
    for dev in /sys/class/net/*; do
        local name
        name=$(basename "$dev")
        # Must have a wireless entry
        [ -d "$dev/wireless" ] || continue

        # Find the phy for this interface
        local phy
        phy=$(iw dev "$name" info 2>/dev/null | grep wiphy | awk '{print $2}') || continue
        if [ -z "$phy" ]; then
            continue
        fi

        # Check if this phy supports AP mode
        if iw phy"$phy" info 2>/dev/null | grep -q "^\s*AP$"; then
            if [ -n "$iface" ]; then
                warn "Multiple AP-capable adapters found, using first: $iface"
            else
                iface="$name"
            fi
        fi
    done

    # Fallback: any wireless interface (may not support AP)
    if [ -z "$iface" ]; then
        warn "No AP-capable WiFi adapter found, falling back to first wireless interface"
        for dev in /sys/class/net/*; do
            local name
            name=$(basename "$dev")
            if [ -d "$dev/wireless" ]; then
                iface="$name"
                break
            fi
        done
    fi

    if [ -z "$iface" ]; then
        fail "No WiFi adapter found (looking for any interface in /sys/class/net/*/wireless)"
    fi
    echo "$iface"
}

# ── Auto-install dependencies ──
install_deps() {
    local missing=()
    for cmd in hostapd dnsmasq iw ip sysctl; do
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

# ── Idempotent cleanup: kill stale processes, flush interface ──
cleanup() {
    log "Cleaning up stale state..."

    # Kill hostapd by PID file
    if [ -f "$HOSTAPD_PID" ]; then
        local pid
        pid=$(cat "$HOSTAPD_PID" 2>/dev/null || true)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            log "Stopping existing hostapd (PID $pid)"
            kill "$pid" 2>/dev/null || true
            sleep 0.5
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f "$HOSTAPD_PID"
    fi

    # Kill dnsmasq by PID file
    if [ -f "$DNSMASQ_PID" ]; then
        local pid
        pid=$(cat "$DNSMASQ_PID" 2>/dev/null || true)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            log "Stopping existing dnsmasq (PID $pid)"
            kill "$pid" 2>/dev/null || true
            sleep 0.3
        fi
        rm -f "$DNSMASQ_PID"
    fi

    # Kill any remaining hostapd/dnsmasq bound to this interface
    pkill -f "hostapd.*$IFACE" 2>/dev/null || true
    pkill -f "dnsmasq.*$IFACE" 2>/dev/null || true
    sleep 0.3

    # Stop NetworkManager to release interface (safe on Pi - SSH uses eth0)
    if command -v systemctl &>/dev/null; then
        log "Stopping NetworkManager to release $IFACE"
        systemctl stop NetworkManager 2>/dev/null || true
        sleep 0.5
    fi

    # Kill wpa_supplicant if running
    pkill -f "wpa_supplicant.*$IFACE" 2>/dev/null || true
    sleep 0.3

    # Flush IP and bring down
    ip addr flush dev "$IFACE" 2>/dev/null || true
    ip link set "$IFACE" down 2>/dev/null || true
    sleep 0.3
}

# ── Set regulatory domain ──
set_regdomain() {
    if command -v iw &>/dev/null; then
        log "Setting regulatory domain to $REGDOMAIN"
        iw reg set "$REGDOMAIN" 2>/dev/null || warn "Failed to set regulatory domain to $REGDOMAIN"
        sleep 0.5
    fi
}

# ── Configure and start hostapd ──
start_hostapd() {
    local conf="/tmp/rpv-hostapd.conf"

    # Detect band from channel number for hw_mode
    local hw_mode="g"
    if [ "$CHANNEL" -gt 14 ] 2>/dev/null; then
        hw_mode="a"
    fi

    log "Generating hostapd config for $IFACE (SSID=$SSID, channel=$CHANNEL, hw_mode=$hw_mode)"
    cat > "$conf" <<EOF
interface=$IFACE
driver=nl80211
ssid=$SSID
hw_mode=$hw_mode
channel=$CHANNEL
wmm_enabled=0
macaddr_acl=0
auth_algs=1
ignore_broadcast_ssid=0
beacon_int=100
dtim_period=2
EOF

    log "Starting hostapd..."
    hostapd "$conf" -B -P "$HOSTAPD_PID" >/tmp/rpv-hostapd.log 2>&1 || {
        local rc=$?
        log "hostapd log:"
        cat /tmp/rpv-hostapd.log >&2
        fail "hostapd failed to start (exit code $rc)"
    }
    sleep 1

    # Verify hostapd is running
    if [ -f "$HOSTAPD_PID" ] && kill -0 "$(cat "$HOSTAPD_PID")" 2>/dev/null; then
        log "hostapd started (PID $(cat "$HOSTAPD_PID"))"
    else
        fail "hostapd process not found after start"
    fi
}

# ── Assign static IP and bring interface up ──
assign_ip() {
    log "Assigning $AP_IP/$AP_SUBNET to $IFACE"
    ip link set "$IFACE" up
    ip addr add "$AP_IP/$AP_SUBNET" dev "$IFACE"
    sleep 0.5
}

# ── Start dnsmasq bound to interface ──
start_dnsmasq() {
    local conf="/tmp/rpv-dnsmasq.conf"

    log "Generating dnsmasq config for $IFACE"
    cat > "$conf" <<EOF
interface=$IFACE
bind-interfaces
listen-address=$AP_IP
dhcp-range=$DHCP_START,$DHCP_END,12h
dhcp-option=3,$AP_IP
dhcp-option=6,8.8.8.8
no-resolv
no-poll
log-dhcp
EOF

    log "Starting dnsmasq..."
    dnsmasq -C "$conf" --pid-file="$DNSMASQ_PID" || {
        local rc=$?
        fail "dnsmasq failed to start (exit code $rc)"
    }
    sleep 0.5

    if [ -f "$DNSMASQ_PID" ] && kill -0 "$(cat "$DNSMASQ_PID")" 2>/dev/null; then
        log "dnsmasq started (PID $(cat "$DNSMASQ_PID"))"
    else
        fail "dnsmasq process not found after start"
    fi
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

    # Check interface exists and is up
    if ! ip link show "$IFACE" | grep -q "state UP"; then
        warn "Interface $IFACE is not UP"
        ok=false
    fi

    # Check IP assignment
    if ! ip addr show "$IFACE" | grep -q "$AP_IP/$AP_SUBNET"; then
        warn "IP $AP_IP/$AP_SUBNET not assigned to $IFACE"
        ok=false
    fi

    # Check hostapd is running
    if [ -f "$HOSTAPD_PID" ] && ! kill -0 "$(cat "$HOSTAPD_PID")" 2>/dev/null; then
        warn "hostapd is not running"
        ok=false
    fi

    # Check dnsmasq is running
    if [ -f "$DNSMASQ_PID" ] && ! kill -0 "$(cat "$DNSMASQ_PID")" 2>/dev/null; then
        warn "dnsmasq is not running"
        ok=false
    fi

    # Check AP is beaconing (scan for our own SSID)
    if ! iw dev "$IFACE" scan 2>/dev/null | grep -q "SSID: $SSID"; then
        warn "AP SSID '$SSID' not found in scan (may take a few seconds to appear)"
    fi

    if $ok; then
        log "All health checks passed"
    else
        warn "Some health checks failed - review warnings above"
    fi
}

# ── Main ──
main() {
    log "=== RPV Camera Network Setup ==="

    # Detect interface
    if [ -z "$IFACE" ]; then
        IFACE=$(detect_ap_wifi)
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

    # Start AP stack
    start_hostapd
    assign_ip
    start_dnsmasq

    # Performance tuning
    tune_performance

    # Health checks
    health_check

    log "=== AP Ready: $IFACE -> $SSID (ch $CHANNEL), IP $AP_IP/$AP_SUBNET ==="
}

main "$@"