#!/bin/bash
#
# RPV Camera Node Installer/Updater
# Installs and configures rpv-cam on Raspberry Pi
#
# Usage: sudo ./install-cam.sh [OPTIONS]
#

set -e

# Get script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RPV_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Source common library
source "$SCRIPT_DIR/../deploy/install-common.sh" || {
    echo "ERROR: Cannot source install-common.sh"
    exit 1
}

# ── Camera-specific Configuration ──
CAM_CRATES=("rpv-cam")
CAM_PACKAGES=(
    "build-essential"
    "git"
    "libavcodec-dev"
    "libavformat-dev"
    "libavutil-dev"
    "libswscale-dev"
    "libssl-dev"
    "pkg-config"
    "cmake"
    "hostapd"
    "dnsmasq"
    "iw"
    "iproute2"
    "rfkill"
    "proot"  # for hostapd without root (optional)
    "firmware-ath9k-htc"  # for AR9271 WiFi adapter support
)

# Parse command-line arguments
parse_args() {
    while [[ $# -gt 0 ]]; do
        case $1 in
            --system)
                INSTALL_MODE="system"
                PREFIX="/usr/local"
                BIN_DIR="$PREFIX/bin"
                CONFIG_DIR="/etc/rpv"
                shift
                ;;
            --user)
                # Camera requires system mode for AP setup (needs root)
                log_error "Camera node requires system-wide installation (needs root for AP/network setup)"
                log_error "Use: sudo $0 --system"
                exit 1
                ;;
            --branch)
                BRANCH="$2"
                shift 2
                ;;
            --prefix)
                PREFIX="$2"
                BIN_DIR="$PREFIX/bin"
                shift 2
                ;;
            --config-dir)
                CONFIG_DIR="$2"
                shift 2
                ;;
            --dry-run)
                DRY_RUN=1
                shift
                ;;
            --force)
                FORCE=1
                shift
                ;;
            --fresh)
                FRESH=1
                shift
                ;;
            --update-only)
                UPDATE_ONLY=1
                shift
                ;;
            --status)
                STATUS_ONLY=1
                shift
                ;;
            --uninstall)
                UNINSTALL=1
                shift
                ;;
            --verbose)
                VERBOSE=$((VERBOSE + 1))
                shift
                ;;
            --quiet)
                QUIET=1
                shift
                ;;
            --help|-h)
                show_help
                exit 0
                ;;
            *)
                log_error "Unknown option: $1"
                show_help
                exit 1
                ;;
        esac
    done

    # Set config dir if not specified
    if [ -z "$CONFIG_DIR" ]; then
        CONFIG_DIR="/etc/rpv"
    fi
}

# ── Pre-flight Checks ──
preflight_checks() {
    log_info "Running pre-flight checks..."

    local errors=0

    # Check architecture
    case "$ARCH" in
        aarch64|arm64)
            log_ok "Architecture: $ARCH (Raspberry Pi compatible)"
            ;;
        *)
            log_warn "Architecture: $ARCH (may not be optimal for Raspberry Pi)"
            ;;
    esac

    # Check for sudo if system install
    if [ "$INSTALL_MODE" = "system" ] && [ "$(id -u)" -ne 0 ]; then
        log_error "System install requires sudo"
        return 1
    fi

    # Check disk space (need at least 500MB for build + install)
    local available_kb
    available_kb=$(df "$PREFIX" | awk 'NR==2 {print $4}')
    if [ "$available_kb" -lt 500000 ]; then  # ~500MB in KB
        log_warn "Low disk space: ${available_kb}KB available"
    else
        log_ok "Disk space: ${available_kb}KB available"
    fi

    # Check for conflicting rpv-cam process
    if pgrep -f "rpv-cam" >/dev/null 2>&1; then
        log_warn "rpv-cam is currently running"
        if [ "$FORCE" -eq 0 ] && [ "$UPDATE_ONLY" -eq 0 ]; then
            log_error "Stop rpv-cam before installing (or use --force)"
            return 1
        fi
    fi

    # Check for correct WiFi adapter (if not update-only)
    if [ "$UPDATE_ONLY" -eq 0 ] && [ "$UNINSTALL" -eq 0 ]; then
        log_info "Checking WiFi adapter availability..."

        # Try to detect AP-capable adapter
        if [ -z "$RPV_IFACE" ]; then
            RPV_IFACE=$(detect_wifi_adapter "ap") || {
                log_warn "No AP-capable WiFi adapter detected"
                log_warn "RPV camera needs a WiFi adapter that supports AP mode"
                log_warn "Recommended: AR9271 (ath9k_htc) or RTL8821AU (rtl8xxxu)"
                if [ "$FORCE" -eq 0 ]; then
                    log_error "Aborting. Use --force to continue anyway"
                    return 1
                fi
            }
        fi

        if [ -n "$RPV_IFACE" ]; then
            log_ok "Will use WiFi interface: $RPV_IFACE"
        fi
    fi

    return 0
}

# ── Build rpv-cam ──
build_cam() {
    log_info "Building rpv-cam..."

    local crate_dir="$RPV_ROOT/rpv-cam"

    if [ ! -d "$crate_dir" ]; then
        log_error "rpv-cam directory not found: $crate_dir"
        log_error "Make sure you're in the RPV repository root"
        return 1
    fi

    # Check if we need to update git
    if [ "$UPDATE_ONLY" -eq 1 ]; then
        git_ensure_repo "$RPV_ROOT" "$BRANCH"
    fi

    # Build
    cargo_build "$crate_dir" "release"

    # Verify binary
    local binary_path="$crate_dir/target/release/rpv-cam"
    if [ ! -f "$binary_path" ]; then
        log_error "Binary not built: $binary_path"
        return 1
    fi

    log_ok "Build successful: $binary_path"
}

# ── Install Binary ──
install_cam_binary() {
    log_info "Installing rpv-cam binary..."

    local src="$RPV_ROOT/rpv-cam/target/release/rpv-cam"
    local dest_dir="$BIN_DIR"

    # Check if binary already exists
    if [ -f "$dest_dir/rpv-cam" ] && [ "$FORCE" -eq 0 ] && [ "$UPDATE_ONLY" -eq 0 ]; then
        log_warn "Binary already exists at $dest_dir/rpv-cam"
        log_warn "Use --force to overwrite"
        return 0
    fi

    # Backup existing
    if [ -f "$dest_dir/rpv-cam" ]; then
        backup_existing "$dest_dir/rpv-cam"
    fi

    install_binary "$src" "$dest_dir" "rpv-cam"

    # Verify installation
    if command -v rpv-cam &>/dev/null; then
        log_ok "rpv-cam is available in PATH"
    else
        log_warn "Binary installed but not in PATH: $dest_dir"
        log_warn "Add to PATH: export PATH=\"$dest_dir:\$PATH\""
    fi
}

# ── Install Helper Scripts ──
install_helper_scripts() {
    log_info "Installing helper scripts..."

    local scripts=(
        "rpv-net-setup-pre.sh"
        "rpv-net-teardown.sh"
    )

    for script in "${scripts[@]}"; do
        local src="$RPV_ROOT/deploy/cam/$script"
        local dest="/usr/local/bin/$script"

        if [ ! -f "$src" ]; then
            log_error "Helper script not found: $src"
            return 1
        fi

        # Copy script
        if [ "$DRY_RUN" -eq 1 ]; then
            log_info "[DRY-RUN] Would copy $src -> $dest"
        else
            run_sudo cp "$src" "$dest"
            run_sudo chmod +x "$dest"
            log_ok "Installed: $dest"
        fi
    done
}

# ── Configuration Setup ──
setup_config() {
    log_info "Setting up configuration..."

    ensure_config_dir "$CONFIG_DIR/rpv"
    local config_path="$CONFIG_DIR/rpv/cam.toml"

    if [ "$UPDATE_ONLY" -eq 1 ]; then
        log_info "Skipping config (update-only mode)"
        return 0
    fi

    if [ "$FRESH" -eq 1 ] && [ -f "$config_path" ]; then
        log_warn "Removing old config (fresh install)"
        rm -f "$config_path"
    fi

    # Generate default config
    generate_default_config "rpv-cam" "$config_path"

    # Update config with our network settings
    if [ -n "$RPV_IFACE" ]; then
        # Use toml editor (tomlq) or sed to update interface
        if command -v tomllq &>/dev/null; then
            tomllq set "$config_path" video_device "/dev/video0" 2>/dev/null || true
        else
            # Fallback to sed (basic)
            sed -i "s|^video_device =.*|video_device = \"/dev/video0\"|" "$config_path" 2>/dev/null || true
        fi
    fi

    chmod 600 "$config_path"
    log_ok "Configuration: $config_path"
}

# ── Network Setup ──
setup_network() {
    if [ "$UPDATE_ONLY" -eq 1 ]; then
        log_info "Skipping network setup (update-only mode)"
        return 0
    fi

    if [ "$UNINSTALL" -eq 1 ]; then
        # Teardown handled by uninstall
        return 0
    fi

    log_info "Setting up camera AP network..."

    # Determine interface
    local iface="${RPV_IFACE:-}"
    if [ -z "$iface" ]; then
        iface=$(detect_wifi_adapter "ap") || {
            log_error "Cannot detect AP-capable WiFi adapter"
            log_error "Specify with: RPV_IFACE=<interface> $0"
            return 1
        }
    fi

    # Run network setup script
    local net_setup_script="$RPV_ROOT/deploy/cam/rpv-net-setup-pre.sh"
    if [ ! -f "$net_setup_script" ]; then
        log_error "Network setup script not found: $net_setup_script"
        return 1
    fi

    # Export environment variables for the script
    export RPV_IFACE="$iface"
    export RPV_SSID="${RPV_SSID:-rpv-link}"
    export RPV_CHANNEL="${RPV_CHANNEL:-6}"
    export RPV_AP_IP="${RPV_AP_IP:-192.168.50.1}"

    log_info "Running network setup (interface=$iface, SSID=$RPV_SSID, channel=$RPV_CHANNEL)"
    if [ "$DRY_RUN" -eq 1 ]; then
        log_info "[DRY-RUN] Would execute: $net_setup_script"
    else
        if bash "$net_setup_script"; then
            log_ok "Network setup completed"
        else
            log_error "Network setup failed"
            return 1
        fi
    fi

    # Save interface for other services
    echo "$iface" > /tmp/rpv-iface
}

# ── Systemd Service Setup ──
setup_service() {
    if [ "$UPDATE_ONLY" -eq 1 ]; then
        log_info "Skipping service setup (update-only mode)"
        return 0
    fi

    if [ "$UNINSTALL" -eq 1 ]; then
        uninstall_service
        return 0
    fi

    log_info "Setting up systemd service..."

    local service_name="rpv-cam"
    local service_file="$RPV_ROOT/deploy/cam/rpv-cam.service"

    if [ ! -f "$service_file" ]; then
        log_error "Service template not found: $service_file"
        return 1
    fi

    # Customize service file
    local customized_service="/tmp/rpv-cam.service"
    # Replace binary path and optionally the network setup script path
    sed -e "s|/usr/local/bin/rpv-cam|$BIN_DIR/rpv-cam|g" \
        -e "s|/usr/local/bin/rpv-net-setup-pre.sh|/usr/local/bin/rpv-net-setup-pre.sh|g" \
        "$service_file" > "$customized_service"

    if [ "$INSTALL_MODE" = "system" ]; then
        install_systemd_service "$service_name" "$customized_service" "$BIN_DIR/rpv-cam" "yes"
    else
        # For camera, user service is less common but supported
        log_warn "User service for camera may not work without network setup privileges"
        install_user_service "$service_name" "$customized_service"
    fi
}

# ── Health Checks ──
run_health_checks() {
    log_info "Running post-install health checks..."

    local errors=0

    # Check binary
    if [ -x "$BIN_DIR/rpv-cam" ]; then
        log_ok "Binary installed: $BIN_DIR/rpv-cam"
    else
        log_error "Binary not found or not executable"
        ((errors++))
    fi

    # Check config
    local config_path="$CONFIG_DIR/rpv/cam.toml"
    if [ -f "$config_path" ]; then
        log_ok "Config exists: $config_path"
    else
        log_warn "Config not found: $config_path"
    fi

    # Check service status
    if [ "$INSTALL_MODE" = "system" ]; then
        if systemctl is-active --quiet rpv-cam; then
            log_ok "Service is running"
        else
            log_warn "Service is not running (check: systemctl status rpv-cam)"
            ((errors++))
        fi
    else
        if systemctl --user is-active --quiet rpv-cam; then
            log_ok "User service is running"
        else
            log_warn "User service is not running"
        fi
    fi

    # Check AP (if applicable)
    local iface
    iface=$(cat /tmp/rpv-iface 2>/dev/null || echo "")
    if [ -n "$iface" ] && iw dev "$iface" scan 2>/dev/null | grep -q "SSID: ${RPV_SSID:-rpv-link}"; then
        log_ok "AP beaconing (SSID: ${RPV_SSID:-rpv-link})"
    else
        log_info "AP check skipped (interface not configured)"
    fi

    if [ $errors -eq 0 ]; then
        log_ok "All health checks passed"
    else
        log_warn "Some health checks failed"
    fi

    return $errors
}

# ── Show Status ──
show_status() {
    log_info "=== RPV Camera Installation Status ==="

    echo ""
    echo "Installation mode: $INSTALL_MODE"
    echo "Install prefix: $PREFIX"
    echo "Config dir: $CONFIG_DIR"
    echo "Branch: $BRANCH"
    echo ""

    if [ -f "$RPV_STATE_DIR/cam.state" ]; then
        echo "Installation state:"
        cat "$RPV_STATE_DIR/cam.state"
        echo ""
    fi

    echo "Binary:"
    if [ -x "$BIN_DIR/rpv-cam" ]; then
        echo "  Status: INSTALLED"
        "$BIN_DIR/rpv-cam" --version 2>&1 || true
    else
        echo "  Status: NOT INSTALLED"
    fi
    echo ""

    echo "Configuration:"
    if [ -f "$CONFIG_DIR/rpv/cam.toml" ]; then
        echo "  Status: EXISTS"
        echo "  Path: $CONFIG_DIR/rpv/cam.toml"
    else
        echo "  Status: NOT FOUND"
    fi
    echo ""

    echo "Service:"
    if [ "$INSTALL_MODE" = "system" ]; then
        if systemctl list-unit-files | grep -q "rpv-cam.service"; then
            echo "  Status: INSTALLED"
            echo "  Enabled: $(systemctl is-enabled rpv-cam 2>/dev/null || echo 'no')"
            echo "  Active: $(systemctl is-active rpv-cam 2>/dev/null || echo 'inactive')"
        else
            echo "  Status: NOT INSTALLED"
        fi
    else
        if [ -f "$HOME/.config/systemd/user/rpv-cam.service" ]; then
            echo "  Status: INSTALLED (user)"
            echo "  Enabled: $(systemctl --user is-enabled rpv-cam 2>/dev/null || echo 'no')"
            echo "  Active: $(systemctl --user is-active rpv-cam 2>/dev/null || echo 'inactive')"
        else
            echo "  Status: NOT INSTALLED"
        fi
    fi
    echo ""

    echo "WiFi Interface:"
    if [ -f /tmp/rpv-iface ]; then
        echo "  Configured: $(cat /tmp/rpv-iface)"
    else
        echo "  Not configured"
    fi
    echo ""

    echo "AP Status:"
    local iface
    iface=$(cat /tmp/rpv-iface 2>/dev/null || echo "")
    if [ -n "$iface" ] && iw dev "$iface" scan 2>/dev/null | grep -q "SSID: ${RPV_SSID:-rpv-link}"; then
        echo "  SSID: ${RPV_SSID:-rpv-link} is beaconing on $iface"
    else
        echo "  AP not detected"
    fi
    echo ""
}

# ── Uninstall ──
uninstall_cam() {
    log_info "Uninstalling rpv-cam..."

    # Stop service
    if [ "$INSTALL_MODE" = "system" ]; then
        run_sudo systemctl stop rpv-cam 2>/dev/null || true
        run_sudo systemctl disable rpv-cam 2>/dev/null || true
        run_sudo rm -f /etc/systemd/system/rpv-cam.service
        run_sudo systemctl daemon-reload
    else
        systemctl --user stop rpv-cam 2>/dev/null || true
        systemctl --user disable rpv-cam 2>/dev/null || true
        rm -f "$HOME/.config/systemd/user/rpv-cam.service"
        systemctl --user daemon-reload
    fi

    # Remove binary
    if [ -f "$BIN_DIR/rpv-cam" ]; then
        log_info "Removing binary: $BIN_DIR/rpv-cam"
        rm -f "$BIN_DIR/rpv-cam"
    fi

    # Ask about config
    local config_path="$CONFIG_DIR/rpv/cam.toml"
    if [ -f "$config_path" ]; then
        read -p "Remove configuration $config_path? (y/N): " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            log_info "Removing config: $config_path"
            rm -f "$config_path"
        fi
    fi

    # Teardown network (if we created it)
    local net_teardown="$RPV_ROOT/deploy/cam/rpv-net-teardown.sh"
    if [ -f "$net_teardown" ]; then
        log_info "Running network teardown..."
        bash "$net_teardown" 2>/dev/null || true
    fi

    log_ok "Uninstall complete"
}

# ── Main ──
main() {
    parse_args "$@"

    # Detect OS early
    detect_os

    # Show status and exit if requested
    if [ "$STATUS_ONLY" -eq 1 ]; then
        show_status
        exit 0
    fi

    # Uninstall if requested
    if [ "$UNINSTALL" -eq 1 ]; then
        uninstall_cam
        exit 0
    fi

    # Pre-flight checks
    if [ "$FRESH" -eq 1 ]; then
        log_warn "Fresh install: will remove existing state"
    fi

    preflight_checks || exit 1

    # Installation flow
    if [ "$UPDATE_ONLY" -eq 0 ]; then
        # Full install/update

        # 1. Install dependencies
        log_info "=== Step 1: Installing dependencies ==="
        install_packages "${CAM_PACKAGES[@]}" || exit 1

        # 2. Build
        log_info "=== Step 2: Building rpv-cam ==="
        build_cam || exit 1

        # 3. Install binary
        log_info "=== Step 3: Installing binary ==="
        install_cam_binary || exit 1

        # 3.5 Install helper scripts
        log_info "=== Step 3.5: Installing helper scripts ==="
        install_helper_scripts || exit 1

        # 4. Configuration
        log_info "=== Step 4: Setting up configuration ==="
        setup_config || exit 1

        # 5. Network setup (if not update-only)
        log_info "=== Step 5: Configuring network ==="
        setup_network || exit 1

        # 6. Systemd service
        log_info "=== Step 6: Installing systemd service ==="
        setup_service || exit 1

        # 7. Health checks
        log_info "=== Step 7: Running health checks ==="
        run_health_checks || log_warn "Some checks failed"

        # 8. Save state
        mkdir -p "$RPV_STATE_DIR"
        cat > "$RPV_STATE_DIR/cam.state" <<EOF
INSTALL_DATE=$(date -Iseconds)
VERSION=$(cd "$RPV_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo "unknown")
BRANCH=$BRANCH
MODE=$INSTALL_MODE
PREFIX=$PREFIX
BIN_DIR=$BIN_DIR
CONFIG_DIR=$CONFIG_DIR
IFACE=${RPV_IFACE:-auto-detected}
SSID=${RPV_SSID:-rpv-link}
EOF

        log_ok "=== Installation complete ==="
        echo ""
        echo "Next steps:"
        echo "  1. Check service status: systemctl status rpv-cam"
        echo "  2. View logs: journalctl -u rpv-cam -f"
        echo "  3. Test: rpv-cam --help"
        echo ""
    else
        # Update only
        log_info "=== Updating rpv-cam ==="

        # Stop service
        if [ "$INSTALL_MODE" = "system" ]; then
            run_sudo systemctl stop rpv-cam 2>/dev/null || true
        else
            systemctl --user stop rpv-cam 2>/dev/null || true
        fi

        # Pull and rebuild
        git_ensure_repo "$RPV_ROOT" "$BRANCH"
        build_cam

        # Reinstall binary
        install_cam_binary

        # Restart service
        if [ "$INSTALL_MODE" = "system" ]; then
            run_sudo systemctl start rpv-cam
        else
            systemctl --user start rpv-cam
        fi

        log_ok "Update complete"
    fi
}

# Run main function
main "$@"
