#!/bin/bash
#
# RPV System Diagnostic Script
# Run this to diagnose connectivity and configuration issues
#

set +e  # Don't exit on errors

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_info()  { echo -e "[INFO] $*"; }

echo "=========================================="
echo "  RPV System Diagnostic"
echo "=========================================="
echo ""

# ---- System ----
echo "=== System ==="
log_info "Hostname: $(hostname)"
log_info "Date: $(date)"
echo ""

# ---- Network Interfaces ----
echo "=== Network Interfaces ==="
ip -4 addr show | grep -E 'inet |^[0-9]:' | sed 's/^/  /'
echo ""

# ---- WiFi Interfaces ----
echo "=== WiFi Interfaces ==="
for iface in $(ls /sys/class/net/ 2>/dev/null | grep -E '^wl'); do
    state=$(cat /sys/class/net/$iface/operstate 2>/dev/null)
    echo "  $iface: state=$state"
    ip -4 addr show $iface 2>/dev/null | grep inet | sed 's/^/    /'
    
    # Check if it's USB
    if readlink -f /sys/class/net/$iface/device 2>/dev/null | grep -q 'usb'; then
        echo "    (USB device)"
        lsusb 2>/dev/null | grep -i 'RTL8821\|Realtek\|wifi\|wireless' | sed 's/^/    /'
    fi
done
echo ""

# ---- Hotspot Status ----
echo "=== Hotspot Status ==="
if pgrep -x hostapd > /dev/null 2>&1; then
    log_pass "hostapd is running"
    SSID=$(grep '^ssid=' /etc/hostapd/hostapd.conf 2>/dev/null | cut -d= -f2)
    CH=$(grep '^channel=' /etc/hostapd/hostapd.conf 2>/dev/null | cut -d= -f2)
    log_info "  SSID: $SSID"
    log_info "  Channel: $CH"
else
    log_fail "hostapd is NOT running"
fi

if pgrep -x dnsmasq > /dev/null 2>&1; then
    log_pass "dnsmasq is running"
else
    log_fail "dnsmasq is NOT running"
fi

# Check hotspot interface IP
HOTSPOT_IF=$(grep '^interface=' /etc/hostapd/hostapd.conf 2>/dev/null | cut -d= -f2)
if [ -n "$HOTSPOT_IF" ]; then
    if ip addr show $HOTSPOT_IF 2>/dev/null | grep -q '192.168.50.1'; then
        log_pass "Hotspot IP: 192.168.50.1"
    else
        log_fail "Hotspot IP not configured on $HOTSPOT_IF"
    fi
fi
echo ""

# ---- Connected Clients ----
echo "=== Connected Clients ==="
if [ -n "$HOTSPOT_IF" ]; then
    arp -a 2>/dev/null | grep '192.168.50' | sed 's/^/  /' || log_info "  No clients connected"
fi
echo ""

# ---- RPV Config Files ----
echo "=== RPV Configuration ==="
for config in ground.toml cam.toml; do
    if [ -f "$HOME/.config/rpv/$config" ]; then
        log_pass "$config exists"
        transport=$(grep '^transport' $HOME/.config/rpv/$config | cut -d'"' -f2)
        interface=$(grep '^interface' $HOME/.config/rpv/$config | head -1 | cut -d'"' -f2)
        log_info "  transport=$transport, interface=$interface"
    else
        log_fail "$config NOT found"
    fi
done
echo ""

# ---- RPV Binaries ----
echo "=== RPV Binaries ==="
for bin in rpv-ground rpv-cam; do
    if [ -f "$HOME/rpv/target/release/$bin" ]; then
        size=$(du -h $HOME/rpv/target/release/$bin | cut -f1)
        log_pass "$bin exists ($size)"
    else
        log_fail "$bin NOT found"
    fi
done
echo ""

# ---- RPV Processes ----
echo "=== RPV Processes ==="
for proc in rpv-ground rpv-cam; do
    if pgrep -x $proc > /dev/null 2>&1; then
        log_pass "$proc is running"
        ps -p $(pgrep -x $proc) -o pid,etime,cmd --no-headers 2>/dev/null | sed 's/^/  /'
    else
        log_fail "$proc is NOT running"
    fi
done
echo ""

# ---- Link Status ----
echo "=== Link Status ==="
if [ -f /tmp/rpv_link_status ]; then
    status=$(cat /tmp/rpv_link_status)
    case $status in
        connected)    log_pass "Link: $status" ;;
        connecting)   log_warn "Link: $status" ;;
        *)            log_fail "Link: $status" ;;
    esac
else
    log_fail "Link status file NOT found"
fi
echo ""

# ---- Network Connectivity ----
echo "=== Network Connectivity ==="

# Check if we can reach Pi's hotspot
if ping -c 1 -W 2 192.168.50.1 > /dev/null 2>&1; then
    log_pass "Can reach Pi at 192.168.50.1"
else
    log_fail "Cannot reach Pi at 192.168.50.1"
fi

# Check TCP port
if nc -z -w 2 192.168.50.1 9003 > /dev/null 2>&1; then
    log_pass "TCP port 9003 is open on Pi"
else
    log_fail "TCP port 9003 is NOT reachable on Pi"
fi
echo ""

# ---- SSH Access ----
echo "=== SSH Access ==="
if ssh -o ConnectTimeout=2 -o BatchMode=yes petrouil@10.0.0.59 'echo ok' > /dev/null 2>&1; then
    log_pass "SSH key-based auth to Pi works"
elif ssh -o ConnectTimeout=2 petrouil@10.0.0.59 'echo ok' <<< 'kalhmera' > /dev/null 2>&1; then
    log_pass "SSH password auth to Pi works"
else
    log_fail "SSH to Pi FAILED"
fi
echo ""

# ---- Pi Status ----
echo "=== Pi Status (via SSH) ==="
ssh -o ConnectTimeout=2 petrouil@10.0.0.59 '
    echo "  Pi hostname: $(hostname)"
    echo "  Pi wlan0: $(ip -4 addr show wlan0 2>/dev/null | grep inet | awk "{print \$2}")"
    if pgrep -x rpv-cam > /dev/null; then
        echo "  rpv-cam: RUNNING"
    else
        echo "  rpv-cam: NOT running"
    fi
    if ss -tlnp 2>/dev/null | grep -q ":9003"; then
        echo "  TCP 9003: LISTENING"
    else
        echo "  TCP 9003: NOT listening"
    fi
' 2>/dev/null || log_fail "Cannot SSH to Pi"

echo ""
echo "=========================================="
echo "  Diagnostic Complete"
echo "=========================================="
