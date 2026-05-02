#!/bin/bash
#
# RPV Ground Station Installer/Updater
# Installs and configures rpv-ground on desktop PC
#
# Usage: ./install-ground.sh [OPTIONS]
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

# ── Ground-specific Configuration ──
GROUND_CRATES=("rpv-ground")
GROUND_PACKAGES=(
    "build-essential"
    "git"
    "libavcodec-dev"
    "libavformat-dev"
    "libavutil-dev"
    "libswscale-dev"
    "libssl-dev"
    "pkg-config"
    "cmake"
    "iw"
    "iproute2"
    "rfkill"
    "libx11-dev"
    "libxrandr-dev"
    "libxinerama-dev"
    "libxi-dev"
    "libwayland-dev"
    "libasound2-dev"
    "libudev-dev"
    "evdev"
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
                INSTALL_MODE="user"
                PREFIX="$HOME/.local"
                BIN_DIR="$PREFIX/bin"
                CONFIG_DIR="$HOME/.config/rpv"
                shift
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
        if [ "$INSTALL_MODE" = "system" ]; then
            CONFIG_DIR="/etc/rpv"
        else
            CONFIG_DIR="$HOME/.config/rpv"
        fi
    fi

    # Ground station typically uses user install
    if [ "$INSTALL_MODE" = "system" ] && [ "$(id -u)" -ne 0 ]; then
        log_error "System install requires sudo. Use --user for user-level install."
        exit 1
    fi
}

# ── Pre-flight Checks ──
preflight_checks() {
    log_info "Running pre-flight checks..."

    local errors=0

    # Check architecture
    log_info "Architecture: $ARCH"

    # Check for sudo if system install
    if [ "$INSTALL_MODE" = "system" ] && [ "$(id -u)" -ne 0 ]; then
        log_error "System install requires sudo"
        return 1
    fi

    # Check disk space
    local available_kb
    available_kb=$(df "$PREFIX" | awk 'NR==2 {print $4}')
    if [ "$available_kb" -lt 500000 ]; then
        log_warn "Low disk space: ${available_kb}KB available"
    else
        log_ok "Disk space: ${available_kb}KB available"
    fi

    # Check for X11/Wayland availability
    log_info "Checking display server..."
    local display_type
    display_type=$(detect_display_server)

    if [ "$display_type" != "unknown" ]; then
        log_ok "Display server: $display_type"
    else
        log_warn "No active display server detected"
        log_info "RPV ground station requires X11 or Wayland to run"
        log_info "Options:"
        log_info "  1. Run on physical console ( graphical session )"
        log_info "  2. SSH with X forwarding: ssh -X host"
        log_info "  3. Use Xvfb for virtual display"
        if [ "$FORCE" -eq 0 ]; then
            log_warn "Use --force to install anyway"
            # Don't fail yet, allow user to proceed with --force
        fi
    fi

    # Check for conflicting rpv-ground process
    if pgrep -f "rpv-ground" >/dev/null 2>&1; then
        log_warn "rpv-ground is currently running"
        if [ "$FORCE" -eq 0 ] && [ "$UPDATE_ONLY" -eq 0 ]; then
            log_error "Stop rpv-ground before installing (or use --force)"
            return 1
        fi
    fi

    # Check for WiFi adapter
    if [ "$UPDATE_ONLY" -eq 0 ] && [ "$UNINSTALL" -eq 0 ]; then
        log_info "Checking WiFi adapter availability..."

        if [ -z "$RPV_IFACE" ]; then
            RPV_IFACE=$(detect_wifi_adapter "sta") || {
                log_warn "No WiFi adapter detected for station mode"
                log_warn "Ground station needs WiFi to connect to camera's AP"
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

# ── Build rpv-ground ──
build_ground() {
    log_info "Building rpv-ground..."

    local crate_dir="$RPV_ROOT/rpv-ground"

    if [ ! -d "$crate_dir" ]; then
        log_error "rpv-ground directory not found: $crate_dir"
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
    local binary_path="$crate_dir/target/release/rpv-ground"
    if [ ! -f "$binary_path" ]; then
        log_error "Binary not built: $binary_path"
        return 1
    fi

    log_ok "Build successful: $binary_path"
}

# ── Install Binary ──
install_ground_binary() {
    log_info "Installing rpv-ground binary..."

    local src="$RPV_ROOT/rpv-ground/target/release/rpv-ground"
    local dest_dir="$BIN_DIR"

    if [ -f "$dest_dir/rpv-ground" ] && [ "$FORCE" -eq 0 ] && [ "$UPDATE_ONLY" -eq 0 ]; then
        log_warn "Binary already exists at $dest_dir/rpv-ground"
        log_warn "Use --force to overwrite"
        return 0
    fi

    if [ -f "$dest_dir/rpv-ground" ]; then
        backup_existing "$dest_dir/rpv-ground"
    fi

    install_binary "$src" "$dest_dir" "rpv-ground"

    if command -v rpv-ground &>/dev/null; then
        log_ok "rpv-ground is available in PATH"
    else
        log_warn "Binary installed but not in PATH: $dest_dir"
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
        local src="$RPV_ROOT/deploy/ground/$script"
        local dest="/usr/local/bin/$script"

        if [ ! -f "$src" ]; then
            log_error "Helper script not found: $src"
            return 1
        fi

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
    local config_path="$CONFIG_DIR/rpv/ground.toml"

    if [ "$UPDATE_ONLY" -eq 1 ]; then
        log_info "Skipping config (update-only mode)"
        return 0
    fi

    if [ "$FRESH" -eq 1 ] && [ -f "$config_path" ]; then
        log_warn "Removing old config (fresh install)"
        rm -f "$config_path"
    fi

    # Generate default config
    generate_default_config "rpv-ground" "$config_path"

    chmod 600 "$config_path"
    log_ok "Configuration: $config_path"
}

# ── Display Server Detection ──
detect_display_server() {
    # Check for Wayland
    if [ -n "$WAYLAND_DISPLAY" ]; then
        echo "wayland"
        return 0
    fi

    # Check for X11
    if [ -n "$DISPLAY" ]; then
        echo "x11"
        return 0
    fi

    # Check for running compositors
    if pgrep -x "weston" >/dev/null 2>&1; then
        echo "wayland"
        return 0
    fi

    if pgrep -x "gnome-shell" >/dev/null 2>&1; then
        echo "wayland"
        return 0
    fi

    if pgrep -x "kwin_wayland" >/dev/null 2>&1; then
        echo "wayland"
        return 0
    fi

    # Check for X session
    if [ -n "$XDG_SESSION_TYPE" ]; then
        echo "$XDG_SESSION_TYPE"
        return 0
    fi

    # Default: unknown
    echo "unknown"
}

# ── Desktop Integration ──
setup_desktop_integration() {
    if [ "$UPDATE_ONLY" -eq 1 ]; then
        return 0
    fi

    if [ "$UNINSTALL" -eq 1 ]; then
        # Remove desktop file
        rm -f "$HOME/.config/autostart/rpv-ground.desktop"
        return 0
    fi

    # Only for user installs
    if [ "$INSTALL_MODE" = "user" ]; then
        log_info "Setting up desktop autostart..."

        local desktop_file="$HOME/.config/autostart/rpv-ground.desktop"
        mkdir -p "$HOME/.config/autostart"

        # Detect display server
        local display_type
        display_type=$(detect_display_server)

        cat > "$desktop_file" <<EOF
[Desktop Entry]
Type=Application
Name=RPV Ground Station
Exec=env RUST_LOG=info $BIN_DIR/rpv-ground
Terminal=false
Categories=Application;Game;
X-GNOME-Autostart-enabled=true
EOF

        # Add Wayland-specific settings
        if [ "$display_type" = "wayland" ]; then
            cat >> "$desktop_file" <<EOF
StartupWMClass=rpv-ground
EOF
        fi

        chmod 644 "$desktop_file"
        log_ok "Desktop autostart configured ($display_type)"
    fi
}

# ── Network Setup (Ground Client) ──
setup_client_network() {
    if [ "$UPDATE_ONLY" -eq 1 ]; then
        log_info "Skipping network setup (update-only mode)"
        return 0
    fi

    log_info "Ground station network setup..."
    log_info "NOTE: Ground station is expected to connect manually to camera's AP"
    log_info "AP SSID: ${RPV_SSID:-rpv-link}"
    log_info "Expected IP: 192.168.50.100/24"

    # Provide a helper script
    local connect_script="$HOME/rpv-connect.sh"
    cat > "$connect_script" <<EOF
#!/bin/bash
# Connect to RPV camera hotspot
SSID="${RPV_SSID:-rpv-link}"
IFACE="${RPV_IFACE:-}"

if [ -z "$IFACE" ]; then
    # Auto-detect
    for dev in /sys/class/net/*/wireless; do
        IFACE=\$(basename \$(dirname "\$dev"))
        break
    done
fi

if [ -z "$IFACE" ]; then
    echo "ERROR: No WiFi adapter found"
    exit 1
fi

echo "Connecting \$IFACE to \$SSID..."
sudo iw dev "\$IFACE" connect "\$SSID"
sudo ip addr add 192.168.50.100/24 dev "\$IFACE"
echo "Done. Ping 192.168.50.1 to verify."
EOF

    chmod +x "$connect_script"
    log_ok "Created connection helper: $connect_script"
    log_info "Run: $connect_script"
}

# ── udev Rules for Gamepad ──
setup_udev_rules() {
    if [ "$UPDATE_ONLY" -eq 1 ]; then
        return 0
    fi

    if [ "$INSTALL_MODE" = "user" ]; then
        log_info "User install: skipping udev rules (requires sudo)"
        log_info "To enable gamepad without sudo, add udev rules manually:"
        echo "  sudo tee /etc/udev/rules.d/99-rpv-gamepad.rules <<'RULE'"
        echo 'KERNEL=="event*", SUBSYSTEM=="input", TAG+="uaccess"'
        echo "RULE"
        return 0
    fi

    log_info "Setting up udev rules for gamepad..."

    local udev_file="/etc/udev/rules.d/99-rpv-gamepad.rules"

    if [ -f "$udev_file" ] && [ "$FORCE" -eq 0 ]; then
        log_warn "Udev rules already exist"
        return 0
    fi

    if [ -f "$udev_file" ]; then
        backup_existing "$udev_file"
    fi

    cat > /tmp/99-rpv-gamepad.rules <<'RULE'
# RPV Ground Station - Gamepad access without sudo
KERNEL=="event*", SUBSYSTEM=="input", TAG+="uaccess"
RULE

    run_sudo cp /tmp/99-rpv-gamepad.rules "$udev_file"
    run_sudo udevadm control --reload-rules
    run_sudo udevadm trigger

    log_ok "Udev rules installed"
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

    local service_name="rpv-ground"
    local x11_service_file="$RPV_ROOT/rpv-ground/rpv-x11.service"
    local weston_service_file="$RPV_ROOT/rpv-ground/rpv-westonsession.service"
    local main_service_file="$RPV_ROOT/deploy/ground/rpv-ground.service"
    local customized_x11="/tmp/rpv-x11.service"
    local customized_weston="/tmp/rpv-weston.service"
    local customized_main="/tmp/rpv-ground.service"

    # Check which display server to use
    local display_type
    display_type=$(detect_display_server)
    log_info "Detected display server: $display_type (system mode)"

    # Prepare app directory for config files
    local app_dir="$PREFIX/rpv-ground"
    mkdir -p "$app_dir"

    # Copy configuration files (weston.ini only — xorg.conf removed)
    if [ -f "$RPV_ROOT/rpv-ground/weston.ini" ]; then
        cp "$RPV_ROOT/rpv-ground/weston.ini" "$app_dir/"
        log_ok "Weston config installed to $app_dir"
    fi

    # Create start script
    if [ -f "$RPV_ROOT/rpv-ground/start.sh" ]; then
        sed "s|/opt/rpv-ground|$app_dir|g" "$RPV_ROOT/rpv-ground/start.sh" > "$app_dir/start.sh"
        chmod +x "$app_dir/start.sh"
        log_ok "Start script installed to $app_dir/start.sh"
    fi

    # Customize service files
    if [ "$display_type" = "wayland" ] && [ -f "$weston_service_file" ]; then
        # Use Weston service for Wayland
        sed -e "s|/opt/rpv-ground|$app_dir|g" \
            "$weston_service_file" > "$customized_weston"
        # Also fix main service to depend on weston
        sed -e "s|After=network.target rpv-x11.service|After=network.target rpv-westonsession.service|g" \
            -e "s|Requires=rpv-x11.service|Requires=rpv-westonsession.service|g" \
            "$main_service_file" > "$customized_main"
        # Update start script path
        sed -i "s|/opt/rpv-ground/start.sh|$app_dir/start.sh|g" "$customized_main"
    else
        # Use X11 (default fallback)
        display_type="x11"
        if [ -f "$x11_service_file" ]; then
            # Replace install prefix (no longer references xorg.conf)
            sed -e "s|/opt/rpv-ground|$app_dir|g" \
                "$x11_service_file" > "$customized_x11"
        fi
        # Ensure main service uses X11
        sed -e "s|After=network.target rpv-westonsession.service|After=network.target rpv-x11.service|g" \
            -e "s|Requires=rpv-westonsession.service|Requires=rpv-x11.service|g" \
            "$main_service_file" > "$customized_main" 2>/dev/null || \
            cp "$main_service_file" "$customized_main"
        # Update start script path
        sed -i "s|/opt/rpv-ground/start.sh|$app_dir/start.sh|g" "$customized_main" 2>/dev/null || true
    fi

    # Also update desktop file if creating one
    local desktop_file="$HOME/.config/autostart/rpv-ground.desktop"
    if [ -f "$desktop_file" ]; then
        sed -i "s|/usr/local/bin/rpv-ground|$BIN_DIR/rpv-ground|g" "$desktop_file"
    fi

    if [ "$INSTALL_MODE" = "system" ]; then
        # System installation
        log_info "Installing systemd services (using $display_type)"

        # Install compositor service (X11 or Weston)
        if [ "$display_type" = "wayland" ] && [ -f "$customized_weston" ]; then
            run_sudo cp "$customized_weston" /etc/systemd/system/rpv-westonsession.service
            run_sudo chmod 644 /etc/systemd/system/rpv-westonsession.service
            ROLLBACK_SERVICES+=("rpv-westonsession.service")
        else
            run_sudo cp "$customized_x11" /etc/systemd/system/rpv-x11.service
            run_sudo chmod 644 /etc/systemd/system/rpv-x11.service
            ROLLBACK_SERVICES+=("rpv-x11.service")
        fi

        # Install main service
        run_sudo cp "$customized_main" /etc/systemd/system/rpv-ground.service
        run_sudo chmod 644 /etc/systemd/system/rpv-ground.service

        run_sudo systemctl daemon-reload

        # Enable services
        if [ "$display_type" = "wayland" ] && [ -f "$customized_weston" ]; then
            run_sudo systemctl enable rpv-westonsession.service
        else
            run_sudo systemctl enable rpv-x11.service
        fi
        run_sudo systemctl enable rpv-ground.service

        log_ok "Systemd services installed (using $display_type)"

        # Start services if not update-only
        if [ "$UPDATE_ONLY" -eq 0 ]; then
            log_info "Starting services..."
            if [ "$display_type" = "wayland" ] && [ -f "$customized_weston" ]; then
                run_sudo systemctl start rpv-westonsession.service || log_warn "Weston service start failed"
            else
                run_sudo systemctl start rpv-x11.service || log_warn "X11 service start failed"
            fi
            sleep 2
            run_sudo systemctl start rpv-ground.service || {
                log_error "Failed to start rpv-ground service"
                log_error "Check: sudo journalctl -u rpv-ground -n 50"
                return 1
            }
            log_ok "Services started"
        fi
    else
        # User installation - create local copies
        local user_app_dir="$HOME/.local/share/rpv-ground"
        mkdir -p "$user_app_dir"

        # Copy config files (weston.ini)
        cp "$RPV_ROOT/rpv-ground/weston.ini" "$user_app_dir/" 2>/dev/null || true

        # Create start script
        if [ -f "$RPV_ROOT/rpv-ground/start.sh" ]; then
            sed "s|/opt/rpv-ground|$user_app_dir|g" "$RPV_ROOT/rpv-ground/start.sh" > "$user_app_dir/start.sh"
            chmod +x "$user_app_dir/start.sh"
        fi

        # Create wrapper script for user service
        local wrapper_script="$HOME/.local/bin/rpv-ground-wrapper"
        cat > "$wrapper_script" <<EOF
#!/bin/bash
export XDG_RUNTIME_DIR=\$XDG_RUNTIME_DIR
export DISPLAY=\$DISPLAY
export WAYLAND_DISPLAY=\$WAYLAND_DISPLAY
exec "$user_app_dir/start.sh"
EOF
        chmod +x "$wrapper_script"

        # Use simplified service for user mode
        local user_service="/tmp/rpv-ground-user.service"
        cat > "$user_service" <<EOF
[Unit]
Description=RPV Ground Station (User)
After=graphical-session.target

[Service]
Type=simple
ExecStart=$wrapper_script
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info
Environment=XDG_RUNTIME_DIR=%t
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF

        install_user_service "rpv-ground" "$user_service"
        log_ok "User service installed (display: $display_type)"
    fi
}

uninstall_service() {
    log_info "Stopping and removing services..."

    if [ "$INSTALL_MODE" = "system" ]; then
        # Stop all possible services
        run_sudo systemctl stop rpv-westonsession.service 2>/dev/null || true
        run_sudo systemctl disable rpv-westonsession.service 2>/dev/null || true
        run_sudo rm -f /etc/systemd/system/rpv-westonsession.service

        run_sudo systemctl stop rpv-x11.service 2>/dev/null || true
        run_sudo systemctl disable rpv-x11.service 2>/dev/null || true
        run_sudo rm -f /etc/systemd/system/rpv-x11.service

        run_sudo systemctl stop rpv-ground.service 2>/dev/null || true
        run_sudo systemctl disable rpv-ground.service 2>/dev/null || true
        run_sudo rm -f /etc/systemd/system/rpv-ground.service

        run_sudo systemctl daemon-reload
    else
        systemctl --user stop rpv-ground 2>/dev/null || true
        systemctl --user disable rpv-ground 2>/dev/null || true
        rm -f "$HOME/.config/systemd/user/rpv-ground.service"
        systemctl --user daemon-reload
    fi

    log_ok "Services removed"
}

# ── Health Checks ──
run_health_checks() {
    log_info "Running post-install health checks..."

    local errors=0

    # Check binary
    if [ -x "$BIN_DIR/rpv-ground" ]; then
        log_ok "Binary installed: $BIN_DIR/rpv-ground"
    else
        log_error "Binary not found or not executable"
        ((errors++))
    fi

    # Check config
    local config_path="$CONFIG_DIR/rpv/ground.toml"
    if [ -f "$config_path" ]; then
        log_ok "Config exists: $config_path"
    else
        log_warn "Config not found: $config_path"
    fi

    # Check service
    if [ "$INSTALL_MODE" = "system" ]; then
        local service_active=0
        if systemctl is-active --quiet rpv-ground.service; then
            log_ok "Service is running"
            service_active=1
        else
            log_warn "Service is not running"
            ((errors++))
        fi

        # Check compositor service
        if systemctl is-active --quiet rpv-x11.service 2>/dev/null; then
            log_ok "X11 service running"
        elif systemctl is-active --quiet rpv-westonsession.service 2>/dev/null; then
            log_ok "Weston service running"
        else
            log_info "No compositor service active (may be using host compositor)"
        fi
    else
        if systemctl --user is-active --quiet rpv-ground; then
            log_ok "User service is running"
        else
            log_warn "User service is not running"
            ((errors++))
        fi
    fi

    # Check display server
    local display_type
    display_type=$(detect_display_server)

    if [ "$display_type" != "unknown" ]; then
        log_ok "Display server: $display_type"
    else
        log_warn "No display server detected"
        log_info "Ground station requires X11 or Wayland to be running"
        ((errors++))
    fi

    # Check WiFi
    local iface
    iface=$(cat /tmp/rpv-iface 2>/dev/null || echo "")
    if [ -n "$iface" ]; then
        if iw dev "$iface" link 2>/dev/null | grep -q "Connected"; then
            log_ok "WiFi connected to camera"
        else
            log_info "WiFi not connected (use: rpv-connect.sh or nmcli)"
            # Not a hard error
        fi
    fi

    if [ $errors -eq 0 ]; then
        log_ok "All health checks passed"
    else
        log_warn "Some health checks failed (${errors} error(s))"
    fi

    return $errors
}

# ── Show Status ──
show_status() {
    log_info "=== RPV Ground Station Installation Status ==="

    echo ""
    echo "Installation mode: $INSTALL_MODE"
    echo "Install prefix: $PREFIX"
    echo "Config dir: $CONFIG_DIR"
    echo "Branch: $BRANCH"
    echo ""

    if [ -f "$RPV_STATE_DIR/ground.state" ]; then
        echo "Installation state:"
        cat "$RPV_STATE_DIR/ground.state"
        echo ""
    fi

    echo "Binary:"
    if [ -x "$BIN_DIR/rpv-ground" ]; then
        echo "  Status: INSTALLED"
        "$BIN_DIR/rpv-ground" --version 2>&1 || true
    else
        echo "  Status: NOT INSTALLED"
    fi
    echo ""

    echo "Configuration:"
    if [ -f "$CONFIG_DIR/rpv/ground.toml" ]; then
        echo "  Status: EXISTS"
        echo "  Path: $CONFIG_DIR/rpv/ground.toml"
    else
        echo "  Status: NOT FOUND"
    fi
    echo ""

    echo "Display server: $(detect_display_server)"
    echo ""

    echo "Services:"
    if [ "$INSTALL_MODE" = "system" ]; then
        for svc in rpv-x11.service rpv-westonsession.service rpv-ground.service; do
            if systemctl list-unit-files 2>/dev/null | grep -q "$svc"; then
                echo "  $svc: INSTALLED"
                echo "    Enabled: $(systemctl is-enabled $svc 2>/dev/null || echo 'no')"
                echo "    Active: $(systemctl is-active $svc 2>/dev/null || echo 'inactive')"
            else
                echo "  $svc: NOT INSTALLED"
            fi
        done
    else
        if [ -f "$HOME/.config/systemd/user/rpv-ground.service" ]; then
            echo "  rpv-ground (user): INSTALLED"
            echo "    Enabled: $(systemctl --user is-enabled rpv-ground 2>/dev/null || echo 'no')"
            echo "    Active: $(systemctl --user is-active rpv-ground 2>/dev/null || echo 'inactive')"
        else
            echo "  rpv-ground (user): NOT INSTALLED"
        fi
    fi
    echo ""

    echo "Desktop integration:"
    if [ -f "$HOME/.config/autostart/rpv-ground.desktop" ]; then
        echo "  Autostart: ENABLED"
    else
        echo "  Autostart: NOT CONFIGURED"
    fi
    echo ""

    echo "WiFi Interface:"
    if [ -f /tmp/rpv-iface ]; then
        echo "  Configured: $(cat /tmp/rpv-iface)"
    else
        echo "  Not configured"
    fi
    echo ""

    echo "Connection status:"
    local iface
    iface=$(cat /tmp/rpv-iface 2>/dev/null || echo "")
    if [ -n "$iface" ] && iw dev "$iface" link 2>/dev/null | grep -q "Connected"; then
        echo "  Connected to: $(iw dev "$iface" link 2>/dev/null | grep "Connected to" | awk '{print $3}')"
        echo "  Frequency: $(iw dev "$iface" link 2>/dev/null | grep "freq:" | awk '{print $2}') MHz"
    else
        echo "  Not connected"
    fi
    echo ""
}

# ── Uninstall ──
uninstall_ground() {
    log_info "Uninstalling rpv-ground..."

    # Stop service
    run_sudo systemctl stop rpv-x11.service 2>/dev/null || true
    run_sudo systemctl disable rpv-x11.service 2>/dev/null || true
    run_sudo systemctl stop rpv-ground.service 2>/dev/null || true
    run_sudo systemctl disable rpv-ground.service 2>/dev/null || true

    systemctl --user stop rpv-ground 2>/dev/null || true
    systemctl --user disable rpv-ground 2>/dev/null || true

    # Remove service files
    run_sudo rm -f /etc/systemd/system/rpv-x11.service
    run_sudo rm -f /etc/systemd/system/rpv-ground.service
    run_sudo systemctl daemon-reload

    rm -f "$HOME/.config/systemd/user/rpv-ground.service"
    systemctl --user daemon-reload

    # Remove binary
    if [ -f "$BIN_DIR/rpv-ground" ]; then
        log_info "Removing binary: $BIN_DIR/rpv-ground"
        rm -f "$BIN_DIR/rpv-ground"
    fi

    # Remove desktop file
    rm -f "$HOME/.config/autostart/rpv-ground.desktop"

    # Ask about config
    local config_path="$CONFIG_DIR/rpv/ground.toml"
    if [ -f "$config_path" ]; then
        read -p "Remove configuration $config_path? (y/N): " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            log_info "Removing config: $config_path"
            rm -f "$config_path"
        fi
    fi

    # Remove helper scripts
    rm -f "$HOME/rpv-connect.sh" 2>/dev/null || true

    # Remove udev rules (if installed)
    if [ -f /etc/udev/rules.d/99-rpv-gamepad.rules ]; then
        read -p "Remove udev rules? (y/N): " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            run_sudo rm -f /etc/udev/rules.d/99-rpv-gamepad.rules
            run_sudo udevadm control --reload-rules
        fi
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
        uninstall_ground
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
        install_packages "${GROUND_PACKAGES[@]}" || exit 1

        # 2. Build
        log_info "=== Step 2: Building rpv-ground ==="
        build_ground || exit 1

        # 3. Install binary
        log_info "=== Step 3: Installing binary ==="
        install_ground_binary || exit 1

        # 3.5 Install helper scripts
        log_info "=== Step 3.5: Installing helper scripts ==="
        install_helper_scripts || exit 1

        # 4. Configuration
        log_info "=== Step 4: Setting up configuration ==="
        setup_config || exit 1

        # 5. Desktop integration
        log_info "=== Step 5: Desktop integration ==="
        setup_desktop_integration || exit 1

        # 6. udev rules
        log_info "=== Step 6: Setting up udev rules ==="
        setup_udev_rules || exit 1

        # 7. Systemd service
        log_info "=== Step 7: Installing systemd service ==="
        setup_service || exit 1

        # 8. Network helper
        log_info "=== Step 8: Network setup helper ==="
        setup_client_network || exit 1

        # 9. Health checks
        log_info "=== Step 9: Running health checks ==="
        run_health_checks || log_warn "Some checks failed"

        # 10. Save state
        mkdir -p "$RPV_STATE_DIR"
        cat > "$RPV_STATE_DIR/ground.state" <<EOF
INSTALL_DATE=$(date -Iseconds)
VERSION=$(cd "$RPV_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo "unknown")
BRANCH=$BRANCH
MODE=$INSTALL_MODE
PREFIX=$PREFIX
BIN_DIR=$BIN_DIR
CONFIG_DIR=$CONFIG_DIR
IFACE=${RPV_IFACE:-auto-detected}
EOF

        log_ok "=== Installation complete ==="
        echo ""
        echo "Next steps:"
        echo "  1. Connect to camera AP: rpv-connect.sh"
        echo "  2. Check service status: systemctl status rpv-ground"
        echo "  3. View logs: journalctl -u rpv-ground -f"
        echo "  4. Test: rpv-ground --help"
        echo ""
    else
        # Update only
        log_info "=== Updating rpv-ground ==="

        # Stop service
        if [ "$INSTALL_MODE" = "system" ]; then
            run_sudo systemctl stop rpv-ground.service 2>/dev/null || true
            run_sudo systemctl stop rpv-x11.service 2>/dev/null || true
        else
            systemctl --user stop rpv-ground 2>/dev/null || true
        fi

        # Pull and rebuild
        git_ensure_repo "$RPV_ROOT" "$BRANCH"
        build_ground

        # Reinstall binary
        install_ground_binary

        # Restart service
        if [ "$INSTALL_MODE" = "system" ]; then
            run_sudo systemctl start rpv-ground.service
        else
            systemctl --user start rpv-ground
        fi

        log_ok "Update complete"
    fi
}

# Run main function
main "$@"
