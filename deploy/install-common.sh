#!/bin/bash
#
# RPV Installer Common Library
# Shared functions for install-cam.sh and install-ground.sh
#

set -e

# ── Colors ──
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# ── Logging ──
log_info()  { echo -e "${BLUE}[INFO]${NC} $*"; }
log_ok()    { echo -e "${GREEN}[OK]${NC} $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*" >&2; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# Verbosity control
VERBOSE=0
QUIET=0

log_verbose() {
    if [ $VERBOSE -ge 1 ]; then
        echo "[VERBOSE] $*"
    fi
}

log_debug() {
    if [ $VERBOSE -ge 2 ]; then
        echo "[DEBUG] $*"
    fi
}

# ── Global State ──
RPV_INSTALL_ROOT="/opt/rpv"
RPV_STATE_DIR="/var/lib/rpv"
RPV_BACKUP_DIR="/var/backups/rpv"
INSTALL_MODE="system"   # "system" or "user"
PREFIX="/usr/local"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR=""           # Set per-mode
SERVICE_MODE="systemd"  # systemd (system) or user
DRY_RUN=0
FORCE=0
FRESH=0
UPDATE_ONLY=0
STATUS_ONLY=0
UNINSTALL=0
BRANCH="master"
RPV_IFACE=""
RPV_SSID="rpv-link"
RPV_CHANNEL=6
RPV_AP_IP="192.168.50.1"
RPV_STA_IP="192.168.50.100"

# Tracking for rollback
ROLLBACK_FILES=()
ROLLBACK_SERVICES=()
ROLLBACK_PROCESSES=()

# ── OS / Distribution Detection ──
OS=""
OS_VERSION=""
PACKAGE_MANAGER=""
ARCH=""
PKG_UPDATE_CMD=""
PKG_INSTALL_CMD=""
PKG_INSTALL_OPTS=""

detect_os() {
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        OS="$ID"
        OS_VERSION="$VERSION_ID"
    elif [ -f /etc/lsb-release ]; then
        . /etc/lsb-release
        OS="$DISTRIB_ID"
        OS_VERSION="$DISTRIB_RELEASE"
    else
        OS="unknown"
    fi

    case "$OS" in
        debian|ubuntu|raspbian)
            PACKAGE_MANAGER="apt"
            PKG_UPDATE_CMD="apt-get update -qq"
            PKG_INSTALL_CMD="apt-get install -y -qq"
            PKG_INSTALL_OPTS=""
            ;;
        arch|manjaro)
            PACKAGE_MANAGER="pacman"
            PKG_UPDATE_CMD="pacman -Sy --noconfirm"
            PKG_INSTALL_CMD="pacman -S --needed --noconfirm"
            PKG_INSTALL_OPTS=""
            ;;
        *)
            log_warn "Unsupported OS: $OS (may work but package install skipped)"
            PACKAGE_MANAGER=""
            ;;
    esac

    ARCH=$(uname -m)
    log_info "Detected: $OS $OS_VERSION ($ARCH)"
    log_verbose "Package manager: ${PACKAGE_MANAGER:-none}"
}

# ── Sudo Detection ──
need_sudo() {
    if [ "$INSTALL_MODE" = "system" ] && [ "$(id -u)" -ne 0 ]; then
        log_error "System install requires sudo. Use --user for user-level install."
        exit 1
    fi
}

run_sudo() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

# ── Platform-Specific Package Name Mapping ──
# Some packages have different names across distributions
get_package_name() {
    local generic_name="$1"
    local os="$2"

    case "$generic_name" in
        build-essential)
            case "$os" in
                debian|ubuntu|raspbian) echo "build-essential" ;;
                arch|manjaro) echo "base-devel" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        iproute2)
            case "$os" in
                debian|ubuntu|raspbian) echo "iproute2" ;;
                arch|manjaro) echo "iproute2" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        libavcodec-dev)
            case "$os" in
                debian|ubuntu|raspbian) echo "libavcodec-dev" ;;
                arch|manjaro) echo "ffmpeg" ;;  # ffmpeg provides all codec dev files
                *) echo "$generic_name" ;;
            esac
            ;;
        libavformat-dev)
            case "$os" in
                debian|ubuntu|raspbian) echo "libavformat-dev" ;;
                arch|manjaro) echo "ffmpeg" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        libavutil-dev)
            case "$os" in
                debian|ubuntu|raspbian) echo "libavutil-dev" ;;
                arch|manjaro) echo "ffmpeg" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        libswscale-dev)
            case "$os" in
                debian|ubuntu|raspbian) echo "libswscale-dev" ;;
                arch|manjaro) echo "ffmpeg" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        libssl-dev)
            case "$os" in
                debian|ubuntu|raspbian) echo "libssl-dev" ;;
                arch|manjaro) echo "openssl" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        pkg-config)
            case "$os" in
                debian|ubuntu|raspbian) echo "pkg-config" ;;
                arch|manjaro) echo "pkgconf" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        cmake)
            case "$os" in
                debian|ubuntu|raspbian) echo "cmake" ;;
                arch|manjaro) echo "cmake" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        hostapd)
            case "$os" in
                debian|ubuntu|raspbian) echo "hostapd" ;;
                arch|manjaro) echo "hostapd" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        dnsmasq)
            case "$os" in
                debian|ubuntu|raspbian) echo "dnsmasq" ;;
                arch|manjaro) echo "dnsmasq" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        iw)
            case "$os" in
                debian|ubuntu|raspbian) echo "iw" ;;
                arch|manjaro) echo "iw" ;;
                *) echo "$generic_name" ;;
            esac
            ;;
        *)
            # Unknown mapping, return as-is
            echo "$generic_name"
            ;;
    esac
}

# ── Git Operations ──
git_ensure_repo() {
    local repo_dir="$1"
    local branch="$2"

    if [ ! -d "$repo_dir/.git" ]; then
        log_error "Not a git repository: $repo_dir"
        log_error "Clone the repository first: git clone <repo> $repo_dir"
        return 1
    fi

    pushd "$repo_dir" > /dev/null

    # Check for local changes
    if ! git diff-index --quiet HEAD --; then
        log_warn "Local changes detected in repository"
        if [ "$FORCE" -eq 0 ]; then
            log_error "Aborting: Repository has uncommitted changes"
            log_error "Commit, stash, or discard changes, or use --force to overwrite"
            popd > /dev/null
            return 1
        else
            log_warn "--force: discarding local changes"
            git reset --hard
            git clean -fd
        fi
    fi

    # Fetch all remotes
    log_info "Fetching from origin..."
    git fetch origin

    # Check if branch exists
    if ! git show-ref --verify --quiet "refs/remotes/origin/$branch"; then
        log_error "Branch '$branch' does not exist in remote"
        log_error "Available branches:"
        git branch -r
        popd > /dev/null
        return 1
    fi

    # Check if we need to pull
    local local_head remote_head
    local_head=$(git rev-parse HEAD)
    remote_head=$(git rev-parse "origin/$branch")

    if [ "$local_head" = "$remote_head" ]; then
        log_ok "Already at latest commit on branch '$branch'"
        popd > /dev/null
        return 0
    fi

    log_info "Updates available. Pulling changes..."

    # Ensure we're on the desired branch
    if ! git checkout "$branch" 2>/dev/null; then
        # Branch doesn't exist locally, create tracking branch
        log_info "Creating local tracking branch for '$branch'"
        git checkout -b "$branch" "origin/$branch" || {
            log_error "Failed to checkout branch '$branch'"
            popd > /dev/null
            return 1
        }
    fi

    # Pull changes
    git pull origin "$branch" || {
        log_error "Failed to pull from origin"
        popd > /dev/null
        return 1
    }

    # Update submodules if any
    if [ -f ".gitmodules" ]; then
        log_info "Updating submodules..."
        git submodule update --init --recursive
    fi

    popd > /dev/null
    log_ok "Repository updated to branch '$branch' (commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown))"
}

# ── Cargo Build ──
cargo_build() {
    local crate_dir="$1"
    local release="$2"

    if ! command -v cargo &>/dev/null; then
        log_error "Cargo not found. Install Rust toolchain:"
        log_error "  curl https://sh.rustup.rs -sSf | sh"
        exit 1
    fi

    pushd "$crate_dir" > /dev/null

    log_info "Fetching dependencies..."
    if [ "$release" = "release" ]; then
        cargo fetch --release
    else
        cargo fetch
    fi

    log_info "Building (this may take a few minutes)..."
    local build_start=$(date +%s)

    if [ "$release" = "release" ]; then
        if ! cargo build --release; then
            log_error "Build failed. See errors above."
            popd > /dev/null
            exit 1
        fi
    else
        if ! cargo build; then
            log_error "Build failed. See errors above."
            popd > /dev/null
            exit 1
        fi
    fi

    local build_end=$(date +%s)
    local elapsed=$((build_end - build_start))
    log_ok "Build completed in ${elapsed}s"

    # Verify binary exists
    local binary_path
    if [ "$release" = "release" ]; then
        binary_path="target/release/$(cargo pkg manifest-name 2>/dev/null || echo rpv-cam)"
    else
        binary_path="target/debug/$(cargo pkg manifest-name 2>/dev/null || echo rpv-cam)"
    fi

    if [ ! -f "$binary_path" ]; then
        log_error "Binary not found at $binary_path"
        popd > /dev/null
        exit 1
    fi

    log_ok "Binary: $binary_path ($(du -h "$binary_path" | cut -f1))"
    popd > /dev/null
}

# ── Binary Installation ──
install_binary() {
    local src="$1"
    local dest_dir="$2"
    local binary_name="$3"

    local dest="$dest_dir/$binary_name"

    log_info "Installing binary: $src -> $dest"

    # Create destination directory
    mkdir -p "$dest_dir"

    # Copy binary
    cp "$src" "$dest" || {
        log_error "Failed to copy binary to $dest"
        return 1
    }

    # Set permissions
    chmod 755 "$dest"

    # Verify it runs
    if "$dest" --version &>/dev/null; then
        log_ok "Binary installed and verified"
    else
        log_warn "Binary installed but --version returned non-zero (may be normal)"
    fi

    # Track for rollback
    ROLLBACK_FILES+=("$dest")
}

# ── Configuration Handling ──
ensure_config_dir() {
    local config_dir="$1"
    mkdir -p "$config_dir"
}

generate_default_config() {
    local binary="$1"
    local config_path="$2"

    if [ -f "$config_path" ] && [ "$FORCE" -eq 0 ]; then
        log_warn "Config already exists: $config_path (use --force to overwrite)"
        return 0
    fi

    log_info "Generating default configuration..."
    "$binary" --generate-config > "$config_path" 2>/dev/null || {
        log_warn "Binary doesn't support --generate-config, using template"
        # Fallback: create minimal config
        generate_config_template "$config_path"
    }

    chmod 600 "$config_path"
    log_ok "Config generated: $config_path"
}

backup_existing() {
    local file="$1"
    if [ -f "$file" ]; then
        local backup_dir="$RPV_BACKUP_DIR/$(date +%Y%m%d-%H%M%S)"
        mkdir -p "$backup_dir"
        local backup="$backup_dir/$(basename "$file")"
        cp "$file" "$backup"
        log_info "Backed up: $file -> $backup"
        echo "$backup"
    fi
}

# ── Systemd Service Management ──
install_systemd_service() {
    local service_name="$1"
    local service_file="$2"
    local binary_path="$3"
    local needs_network="$4"  # "yes" or "no"

    log_info "Installing systemd service: $service_name"

    # Check if service already exists
    if [ -f "/etc/systemd/system/$service_name" ] && [ "$FORCE" -eq 0 ]; then
        log_warn "Service already exists: /etc/systemd/system/$service_name"
        log_warn "Use --force to overwrite"
        return 0
    fi

    # Backup existing
    backup_existing "/etc/systemd/system/$service_name"

    # Install service file
    run_sudo cp "$service_file" "/etc/systemd/system/"
    run_sudo chmod 644 "/etc/systemd/system/$service_name"

    # Reload systemd
    run_sudo systemctl daemon-reload

    # Enable service
    run_sudo systemctl enable "$service_name"
    log_ok "Service enabled: $service_name"

    # Start service if not update-only
    if [ "$UPDATE_ONLY" -eq 0 ]; then
        log_info "Starting service..."
        if run_sudo systemctl start "$service_name"; then
            log_ok "Service started"
        else
            log_error "Failed to start service"
            log_error "Check logs: journalctl -u $service_name -n 50"
            return 1
        fi
    fi

    ROLLBACK_SERVICES+=("$service_name")
}

install_user_service() {
    local service_name="$1"
    local service_file="$2"

    log_info "Installing user systemd service: $service_name"

    local user_service_dir="$HOME/.config/systemd/user"
    mkdir -p "$user_service_dir"

    if [ -f "$user_service_dir/$service_name" ] && [ "$FORCE" -eq 0 ]; then
        log_warn "User service already exists"
        return 0
    fi

    backup_existing "$user_service_dir/$service_name"
    cp "$service_file" "$user_service_dir/"
    chmod 644 "$user_service_dir/$service_name"

    systemctl --user daemon-reload
    systemctl --user enable "$service_name"
    log_ok "User service enabled"

    if [ "$UPDATE_ONLY" -eq 0 ]; then
        systemctl --user start "$service_name" || {
            log_warn "Failed to start user service (may need lingering enabled)"
            log_warn "Enable lingering: loginctl enable-linger $USER"
        }
    fi
}

# ── Network Setup (Camera AP) ──
setup_ap_network() {
    local iface="$1"
    local ssid="$2"
    local channel="$3"
    local ap_ip="$4"

    log_info "Setting up AP on interface: $iface"

    # Check if interface exists
    if ! ip link show "$iface" &>/dev/null; then
        log_error "Interface $iface does not exist"
        return 1
    fi

    # Check AP capability
    if ! iw phy "$(iw dev "$iface" info | grep wiphy | awk '{print $2}')" info 2>/dev/null | grep -q "^\s*AP$"; then
        log_warn "Interface $iface may not support AP mode, attempting anyway..."
    fi

    # Kill interfering processes
    log_info "Stopping NetworkManager..."
    run_sudo systemctl stop NetworkManager 2>/dev/null || true
    run_sudo systemctl stop wpa_supplicant 2>/dev/null || true

    # Cleanup stale state
    run_sudo ip addr flush dev "$iface" 2>/dev/null || true
    run_sudo ip link set "$iface" down 2>/dev/null || true

    # Configure hostapd
    local hostapd_conf="/tmp/rpv-hostapd.conf"
    cat > "$hostapd_conf" <<EOF
interface=$iface
driver=nl80211
ssid=$ssid
hw_mode=g
channel=$channel
ieee80211n=1
wmm_enabled=1
auth_algs=1
wpa=0
EOF

    # Start hostapd
    log_info "Starting hostapd..."
    run_sudo hostapd "$hostapd_conf" -B || {
        log_error "hostapd failed to start"
        log_error "Check: hostapd -dd /tmp/rpv-hostapd.conf"
        return 1
    }

    sleep 1

    # Assign IP
    log_info "Assigning IP $ap_ip/24 to $iface"
    run_sudo ip addr add "$ap_ip/24" dev "$iface"
    run_sudo ip link set "$iface" up

    # Configure dnsmasq
    local dnsmasq_conf="/tmp/rpv-dnsmasq.conf"
    cat > "$dnsmasq_conf" <<EOF
interface=$iface
bind-interfaces
listen-address=$ap_ip
dhcp-range=192.168.50.100,192.168.50.101,24h
dhcp-option=3,$ap_ip
dhcp-option=6,8.8.8.8
no-resolv
no-poll
log-dhcp
EOF

    log_info "Starting dnsmasq..."
    run_sudo dnsmasq -C "$dnsmasq_conf" -x /tmp/rpv-dnsmasq.pid || {
        log_error "dnsmasq failed to start"
        return 1
    }

    # Performance tuning
    run_sudo iw dev "$iface" set power_save off 2>/dev/null || true
    sysctl -w net.core.rmem_max=8388608 2>/dev/null || true
    sysctl -w net.core.wmem_max=8388608 2>/dev/null || true

    # Health check: verify AP is beaconing
    sleep 2
    if iw dev "$iface" scan 2>/dev/null | grep -q "SSID: $ssid"; then
        log_ok "AP is beaconing (SSID: $ssid)"
    else
        log_warn "AP may not be beaconing yet (scan own interface)"
    fi

    log_ok "AP setup complete: $ssid on $iface ($ap_ip)"
}

# ── Network Setup (Ground Station Client) ──
setup_client_network() {
    local iface="$1"
    local ssid="$2"
    local ip_addr="$3"

    log_info "Connecting $iface to AP '$ssid'..."

    # Unmanage from NetworkManager
    nmcli dev set "$iface" managed no 2>/dev/null || true

    # Bring up interface
    run_sudo ip link set "$iface" up

    # Connect
    log_info "Associating with AP..."
    run_sudo iw dev "$iface" connect "$ssid" || {
        log_warn "iw connect returned error, may still associate"
    }

    # Wait for association (max 10s)
    local attempts=20
    while [ $attempts -gt 0 ]; do
        if iw dev "$iface" link 2>/dev/null | grep -q "Connected"; then
            break
        fi
        sleep 0.5
        ((attempts--))
    done

    if [ $attempts -eq 0 ]; then
        log_error "Failed to connect to AP after 10 seconds"
        return 1
    fi

    # Assign IP
    log_info "Assigning IP $ip_addr/24"
    run_sudo ip addr add "$ip_addr/24" dev "$iface" 2>/dev/null || {
        log_warn "IP may already be assigned"
    }

    # Performance tuning
    run_sudo iw dev "$iface" set power_save off 2>/dev/null || true

    # Verify connectivity
    sleep 1
    if ping -c 1 -W 2 "${RPV_AP_IP:-192.168.50.1}" &>/dev/null; then
        log_ok "Connected to AP, ping OK"
    else
        log_warn "Cannot ping camera (192.168.50.1) - may need ARP resolution"
    fi
}

# ── WiFi Adapter Detection ──
detect_wifi_adapter() {
    local mode="$1"  # "ap" or "sta"

    for dev in /sys/class/net/*; do
        local name
        name=$(basename "$dev")
        [ -d "$dev/wireless" ] || continue

        local phy
        phy=$(iw dev "$name" info 2>/dev/null | grep wiphy | awk '{print $2}') || continue
        [ -n "$phy" ] || continue

        if [ "$mode" = "ap" ]; then
            if iw phy"$phy" info 2>/dev/null | grep -q "^\s*AP$"; then
                echo "$name"
                return 0
            fi
        else  # sta
            # Any wireless adapter works for station
            echo "$name"
            return 0
        fi
    done

    log_error "No WiFi adapter found for mode: $mode"
    return 1
}

# ── Hardware Validation ──
validate_camera_hardware() {
    # Check for video devices
    if [ ! -e /dev/video0 ]; then
        log_error "No /dev/video0 found"
        return 1
    fi

    # Check camera can capture
    if command -v v4l2-ctl &>/dev/null; then
        if ! v4l2-ctl --list-formats-ext -d /dev/video0 &>/dev/null; then
            log_warn "Cannot query camera formats"
        else
            log_ok "Camera device validated"
        fi
    fi
}

validate_wifi_adapter() {
    local iface="$1"
    local expected_mode="$2"

    if ! iw dev "$iface" info &>/dev/null; then
        log_error "Interface $iface not found or not wireless"
        return 1
    fi

    local mode
    mode=$(iw dev "$iface" info | grep "type" | awk '{print $2}')
    log_info "Interface $iface mode: $mode"

    if [ "$mode" = "$expected_mode" ]; then
        log_ok "Adapter $iface in $mode mode"
    else
        log_warn "Interface $iface is $mode, expected $expected_mode"
    fi
}

# ── Systemd Service Checks ──
service_is_active() {
    systemctl is-active --quiet "$1" 2>/dev/null
}

service_is_enabled() {
    systemctl is-enabled --quiet "$1" 2>/dev/null
}

# ── Health Checks ──
health_check_cam() {
    log_info "Running health checks for rpv-cam..."

    local errors=0

    # Check binary
    if ! command -v rpv-cam &>/dev/null; then
        log_error "rpv-cam binary not found in PATH"
        ((errors++))
    fi

    # Check config
    if [ ! -f "$HOME/.config/rpv/cam.toml" ]; then
        log_warn "Config not found: $HOME/.config/rpv/cam.toml"
    fi

    # Check service
    if systemctl is-active --quiet rpv-cam; then
        log_ok "Service is running"
    else
        log_warn "Service is not running"
        ((errors++))
    fi

    # Check ports (if service running)
    if ss -tlnp 2>/dev/null | grep -q ":9001 "; then
        log_ok "UDP discovery port 9001 listening"
    else
        log_warn "UDP port 9001 not listening"
    fi

    # Check AP (if hostapd configured)
    local iface
    iface=$(cat /tmp/rpv-iface 2>/dev/null || echo wlan1)
    if iw dev "$iface" scan 2>/dev/null | grep -q "SSID: rpv-link"; then
        log_ok "AP is beaconing"
    else
        log_warn "AP SSID not found in scan"
    fi

    return $errors
}

health_check_ground() {
    log_info "Running health checks for rpv-ground..."

    local errors=0

    # Check binary
    if ! command -v rpv-ground &>/dev/null; then
        log_error "rpv-ground binary not found"
        ((errors++))
    fi

    # Check WiFi connection
    local iface
    iface=$(cat /tmp/rpv-iface 2>/dev/null || echo wlan0)
    if iw dev "$iface" link 2>/dev/null | grep -q "Connected"; then
        log_ok "WiFi connected"
    else
        log_warn "WiFi not connected"
        ((errors++))
    fi

    return $errors
}

# ── Rollback ──
rollback() {
    log_error "Installation failed, rolling back..."

    # Stop services
    for svc in "${ROLLBACK_SERVICES[@]}"; do
        run_sudo systemctl stop "$svc" 2>/dev/null || true
        run_sudo systemctl disable "$svc" 2>/dev/null || true
    done

    # Remove installed files
    for file in "${ROLLBACK_FILES[@]}"; do
        if [ -f "$file" ]; then
            rm -f "$file"
            log_info "Removed: $file"
        fi
    done

    log_error "Rollback complete. System should be in pre-install state."
}

# ── Trap for errors ──
trap_err() {
    local lineno=$1
    local cmd="$2"
    log_error "Failed at line $lineno: $cmd"
    if [ "$DRY_RUN" -eq 0 ]; then
        rollback
    fi
    exit 1
}

trap 'trap_err ${LINENO} "$BASH_COMMAND"' ERR

# ── Help ──
show_help() {
    cat <<EOF
RPV Installer/Updater

Usage: $0 [OPTIONS] <cam|ground>

Components:
  cam        Install/update camera node (Raspberry Pi)
  ground     Install/update ground station (desktop PC)

Options:
  --system           System-wide installation (default, needs sudo)
  --user             User-only installation (no sudo)
  --branch BRANCH    Git branch or tag to install (default: master)
  --prefix PATH      Installation prefix (default: /usr/local for system, ~/.local for user)
  --config-dir DIR   Configuration directory (default: ~/.config/rpv)
  --dry-run          Show what would be done without making changes
  --force            Overwrite existing files without prompting
  --fresh            Wipe all state and reinstall from scratch
  --update-only      Pull git updates and rebuild only (skip config/services)
  --status           Show installation status and exit
  --uninstall        Remove RPV installation completely
  --verbose          Show detailed output
  --quiet            Suppress non-error messages
  --help             Show this help

Examples:
  # Fresh system install
  sudo $0 --system cam

  # User-level install
  $0 --user ground

  # Update from git and restart
  $0 --update-only cam

  # Check status
  $0 --status cam

Environment Variables:
  RPV_IFACE          WiFi interface to use (auto-detect if unset)
  RPV_SSID           AP SSID for camera (default: rpv-link)
  RPV_CHANNEL        WiFi channel (default: 6)
  RPV_AP_IP          Camera AP IP (default: 192.168.50.1)
  RPV_STA_IP         Ground station IP (default: 192.168.50.100)

EOF
}

# ── Export common functions ──
# (functions above are automatically available when sourced)
