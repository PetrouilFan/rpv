#!/bin/bash
#
# RPV Quick Start Script
# This script provides a menu-driven interface to manage the RPV system
#
# Usage: ./rpv-quickstart.sh
#

set +e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

menu_option() {
    echo -e "${BLUE}${NC} $1"
}

menu_title() {
    echo ""
    echo -e "${GREEN}========================================${NC}"
    echo -e "${GREEN}  $1${NC}"
    echo -e "${GREEN}========================================${NC}"
    echo ""
}

info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

success() {
    echo -e "${GREEN}[OK]${NC} $1"
}

warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# ---- Main Menu ----
show_main_menu() {
    menu_title "RPV Quick Start"
    echo "Select an option:"
    echo ""
    menu_option "1)" "Run Ground Station (this PC)"
    menu_option "2)" "Setup Pi Hotspot (10.0.0.59)"
    menu_option "3)" "Start Camera on Pi (10.0.0.59)"
    menu_option "4)" "Run Full System Test"
    menu_option "5)" "Run Diagnostics"
    menu_option "6)" "View System Status"
    menu_option "7)" "SSH into Pi (10.0.0.59)"
    menu_option "8)" "Edit Configuration"
    menu_option "9)" "View Logs"
    menu_option "0)" "Exit"
    echo ""
    read -p "Select option: " choice
    
    case $choice in
        1) run_ground_station ;;
        2) setup_pi_hotspot ;;
        3) start_pi_camera ;;
        4) run_full_test ;;
        5) run_diagnostics ;;
        6) show_status ;;
        7) ssh_to_pi ;;
        8) edit_config ;;
        9) view_logs ;;
        0) exit 0 ;;
        *) error "Invalid option" ; sleep 1 ;;
    esac
}

# ---- Option 1: Run Ground Station ----
run_ground_station() {
    menu_title "Run Ground Station"
    info "Starting RPV ground station..."
    echo ""
    ./run-ground.sh
}

# ---- Option 2: Setup Pi Hotspot ----
setup_pi_hotspot() {
    menu_title "Setup Pi Hotspot"
    info "Copying hotspot setup script to Pi..."
    
    if scp -o ConnectTimeout=5 setup-pi-hotspot.sh petrouil@10.0.0.59:/tmp/ 2>&1; then
        success "Script copied to Pi"
        info "Running hotspot setup on Pi..."
        echo ""
        ssh -t petrouil@10.0.0.59 'sudo bash /tmp/setup-pi-hotspot.sh'
    else
        error "Cannot connect to Pi"
        info "Please ensure the Pi is powered on and connected to the network"
        info "Manual setup: scp setup-pi-hotspot.sh petrouil@10.0.0.59:/tmp/"
        info "Then SSH in and run: sudo bash /tmp/setup-pi-hotspot.sh"
    fi
}

# ---- Option 3: Start Pi Camera ----
start_pi_camera() {
    menu_title "Start Camera on Pi"
    info "Starting RPV camera on Pi..."
    echo ""
    ssh -t petrouil@10.0.0.59 'cd ~/rpv && sudo ./run-cam.sh'
}

# ---- Option 4: Full System Test ----
run_full_test() {
    menu_title "Full System Test"
    
    info "Step 1: Checking Pi connectivity..."
    if ping -c 1 -W 2 10.0.0.59 > /dev/null 2>&1; then
        success "Pi is reachable"
    else
        error "Pi is not reachable"
        warn "Please ensure the Pi is powered on"
        return 1
    fi
    
    info "Step 2: Checking hotspot status on Pi..."
    if ssh -o ConnectTimeout=2 petrouil@10.0.0.59 'pgrep -x hostapd > /dev/null' 2>/dev/null; then
        success "Hotspot is running on Pi"
    else
        warn "Hotspot is not running on Pi"
        info "Run option 2 to set it up"
    fi
    
    info "Step 3: Checking ground station config..."
    if [ -f ~/.config/rpv/ground.toml ]; then
        success "Ground station config exists"
        interface=$(grep '^interface' ~/.config/rpv/ground.toml | head -1 | cut -d'"' -f2)
        info "  Interface: $interface"
    else
        warn "Ground station config not found"
    fi
    
    info "Step 4: Checking binaries..."
    if [ -f ./target/release/rpv-ground ]; then
        success "rpv-ground binary exists"
    else
        warn "rpv-ground binary not found"
        info "Run: cargo build --release"
    fi
    
    echo ""
    info "To start the full system:"
    info "  1. Ensure hotspot is running on Pi (option 2)"
    info "  2. Run ground station (option 1)"
    info "  3. Start camera on Pi (option 3)"
}

# ---- Option 5: Run Diagnostics ----
run_diagnostics() {
    menu_title "Run Diagnostics"
    ./diagnose.sh
}

# ---- Option 6: Show Status ----
show_status() {
    menu_title "System Status"
    
    echo "=== Local System ==="
    echo "  WiFi Interface: $(nmcli -t -f DEVICE,STATE device 2>/dev/null | grep wifi || echo 'N/A')"
    echo "  Current SSID:   $(nmcli -t -f active,ssid dev wifi 2>/dev/null | grep '^yes' | cut -d: -f2 || echo 'N/A')"
    echo "  Current IP:     $(ip -4 addr show dev $(nmcli -t -f DEVICE,STATE device 2>/dev/null | grep wifi | cut -d: -f1 | head -1) 2>/dev/null | grep inet | awk '{print $2}' | cut -d/ -f1 || echo 'N/A')"
    echo ""
    
    echo "=== Pi (10.0.0.59) ==="
    if ping -c 1 -W 1 10.0.0.59 > /dev/null 2>&1; then
        echo "  Status:         Online"
        ssh -o ConnectTimeout=2 petrouil@10.0.0.59 '
            echo "  Hostname:       $(hostname)"
            echo "  wlan0 IP:       $(ip -4 addr show wlan0 2>/dev/null | grep inet | awk "{print \$2}")"
            if pgrep -x rpv-cam > /dev/null; then echo "  rpv-cam:        Running"; else echo "  rpv-cam:        Not running"; fi
        ' 2>/dev/null || echo "  SSH:            Failed"
    else
        echo "  Status:         Offline"
    fi
    echo ""
    
    echo "=== RPV Ground Station ==="
    if pgrep -x rpv-ground > /dev/null; then
        echo "  Status:         Running"
        ps -p $(pgrep -x rpv-ground) -o pid,etime --no-headers 2>/dev/null | sed 's/^/  /'
    else
        echo "  Status:         Not running"
    fi
    echo ""
    
    echo "=== Link Status ==="
    if [ -f /tmp/rpv_link_status ]; then
        echo "  Status:         $(cat /tmp/rpv_link_status)"
    else
        echo "  Status:         Unknown"
    fi
}

# ---- Option 7: SSH to Pi ----
ssh_to_pi() {
    menu_title "SSH to Pi (10.0.0.59)"
    info "Connecting to Pi..."
    echo ""
    ssh petrouil@10.0.0.59
}

# ---- Option 8: Edit Configuration ----
edit_config() {
    menu_title "Edit Configuration"
    echo "Select config to edit:"
    echo "  1) Ground station (~/.config/rpv/ground.toml)"
    echo "  2) Camera on Pi (~/rpv/.config/rpv/cam.toml)"
    echo "  3) Both"
    echo ""
    read -p "Select option: " choice
    
    case $choice in
        1)
            ${EDITOR:-nano} ~/.config/rpv/ground.toml
            ;;
        2)
            ssh petrouil@10.0.0.59 '${EDITOR:-nano} ~/rpv/.config/rpv/cam.toml'
            ;;
        3)
            ${EDITOR:-nano} ~/.config/rpv/ground.toml
            ssh petrouil@10.0.0.59 '${EDITOR:-nano} ~/rpv/.config/rpv/cam.toml'
            ;;
    esac
}

# ---- Option 9: View Logs ----
view_logs() {
    menu_title "View Logs"
    echo "Select log to view:"
    echo "  1) Link status (/tmp/rpv_link_status)"
    echo "  2) Ground station (journalctl)"
    echo "  3) Pi system logs (via SSH)"
    echo "  4) Pi rpv-cam output"
    echo ""
    read -p "Select option: " choice
    
    case $choice in
        1)
            echo "=== Link Status ==="
            cat /tmp/rpv_link_status 2>/dev/null || echo "No status file"
            ;;
        2)
            sudo journalctl -u rpv-ground -f 2>/dev/null || echo "No systemd service"
            ;;
        3)
            ssh petrouil@10.0.0.59 'sudo journalctl -n 100 2>/dev/null || dmesg | tail -100'
            ;;
        4)
            ssh petrouil@10.0.0.59 'sudo journalctl -u rpv-cam -n 100 2>/dev/null || echo "No systemd service"'
            ;;
    esac
}

# ---- Main Loop ----
while true; do
    show_main_menu
done
