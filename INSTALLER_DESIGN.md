# RPV Ultimate Installer/Updater Design

## Overview

Create comprehensive, idempotent installer scripts that handle:
- **Fresh installation**: Dependencies, build, config, services
- **Updates**: Git pull, rebuild, restart services
- **Validation**: Hardware checks, configuration verification
- **Rollback**: Ability to undo changes if installation fails
- **Uninstall**: Clean removal of all RPV components

## Architecture

### Two Specialized Scripts
1. **`install-cam.sh`** — Raspberry Pi camera node (AP mode, hostapd, dnsmasq)
2. **`install-ground.sh`** — Ground station (client mode, desktop integration)

### Shared Library (`install-common.sh`)
All common functions factored out:
- OS/distribution detection
- Package manager abstraction (apt/pacman)
- Git operations (clone, pull, checkout tags)
- Cargo build with caching
- Binary installation (/usr/local/bin vs ~/.local/bin)
- Systemd service management
- Configuration merging and validation
- Logging with levels (info/warn/error)
- Health checks
- Rollback state management
- Uninstall cleanup

## Installer Features

### 1. Pre-flight Validation
```
- Check OS (Debian/Ubuntu/Raspbian vs Arch)
- Check architecture (aarch64 vs x86_64)
- Verify sudo privileges (if system install)
- Check disk space (min 500MB)
- Detect hardware:
  * Cam: WiFi adapter with AP mode, CSI camera or /dev/video0
  * Ground: WiFi adapter with station mode, gamepad optional
- Check for conflicting services (NetworkManager, wpa_supplicant)
```

### 2. Dependency Installation
```
System packages (auto-detect package manager):
- build-essential / base-devel (compiler toolchain)
- git, curl, wget
- libavcodec-dev, libavformat-dev, libavutil-dev, libswscale-dev (ffmpeg libs)
- libssl-dev, pkg-config, cmake
- hostapd, dnsmasq (cam only)
- iw, iproute2, rfkill (both)
- systemd, systemd-timesyncd (optional)
- [Optional] docker, if containerized deployment desired

Rust toolchain:
- Check if cargo exists
- If not: install via rustup (user) or apt (system)
- Use existing if present
```

### 3. Repository Management
```
- If repo doesn't exist: git clone <repo> to /opt/rpv or ~/rpv
- If repo exists: git fetch, git checkout <branch>, git pull
- Tag/Branch selection: --branch option (default: master)
- Update strategy:
  * If repo is dirty (uncommitted changes): warn and skip update
  * If upstream changed: pull and rebuild
  * If no changes: skip rebuild
```

### 4. Build Process
```
- cargo fetch --release --locked (if Cargo.lock changed)
- cargo build --release
- Verify binary size > 1MB (sanity check)
- Optional: strip binary (reduce size)
- Optional: create AppImage/flatpak/snap (desktop integration)
- Test binary: rpv-cam --help, rpv-ground --help
```

### 5. Configuration Management
```
Config locations:
- System: /etc/rpv/
- User:   ~/.config/rpv/
- Legacy: /usr/local/etc/rpv/

Config strategy:
1. If fresh install: generate default config from binary
   rpv-cam --generate-config > ~/.config/rpv/cam.toml
2. If config exists:
   a. Backup: cp config config.bak.YYYYMMDD-HHMMSS
   b. Merge: keep existing values, fill in new defaults
   c. Or prompt: overwrite, keep, merge? (with --force auto-keep)
3. Validate: rpv-cam --validate-config (if supported)
4. Set permissions: chmod 600 config (contains secrets)
5. Create symlinks for convenience: ~/rpv.conf -> ~/.config/rpv/cam.toml
```

### 6. Binary Installation
```
Installation targets:
- /usr/local/bin/rpv-cam (system-wide, needs sudo)
- ~/.local/bin/rpv-cam (user-only, no sudo)
- /opt/rpv/bin/rpv-cam (self-contained)

Default: /usr/local/bin for system services, ~/.local/bin for user

Steps:
1. Copy binary to target
2. chmod 755 binary
3. Verify binary runs: rpv-cam --version
4. Create symlink: /usr/bin/rpv-cam -> /usr/local/bin/rpv-cam (optional)
```

### 7. Systemd Service Setup (Cam)
```
System service (needs sudo):
- Template: deploy/cam/rpv-cam.service
- Substitutions:
  * ExecStart=/usr/local/bin/rpv-cam
  * ExecStartPre=/usr/local/bin/rpv-net-setup-pre.sh
  * User=rpv (create system user) or root (current)
- Install: cp service to /etc/systemd/system/
- Enable: systemctl enable rpv-cam
- Start: systemctl start rpv-cam
- Status: systemctl status rpv-cam

User service (no sudo):
- Template: deploy/cam/rpv-cam.service [User] section omitted
- Install: cp to ~/.config/systemd/user/
- Enable: systemctl --user enable rpv-cam
- Start: systemctl --user start rpv-cam
- Requires lingering: loginctl enable-linger $USER (for non-login)
```

### 8. Network Configuration (Camera Only)
```
AP Mode Setup:
1. Detect AP-capable WiFi adapter (ath9k_htc preferred)
2. Kill interfering processes (NetworkManager, wpa_supplicant)
3. Create hostapd config:
   - Interface: <detected adapter>
   - SSID: rpv-link (configurable)
   - Channel: 6
   - No encryption (RPV handles security)
4. Create dnsmasq config:
   - Interface: same adapter
   - DHCP range: 192.168.50.100-101
   - Gateway: 192.168.50.1
5. Assign static IP to adapter: 192.168.50.1/24
6. Start hostapd and dnsmasq
7. Verify AP is beaconing (iw dev <iface> scan)
8. Configure iptables for NAT (optional internet sharing)

Rollback:
- Stop hostapd/dnsmasq
 - Restore NetworkManager
- Remove static IP
```

### 9. Desktop Integration (Ground Only)
```
- Create .desktop file in ~/.config/autostart/
- Set WAYLAND_DISPLAY/XDG_RUNTIME_DIR
- Create xorg.conf for X11 mode
- Add udev rule for gamepad: /etc/udev/rules.d/99-rpv-gamepad.rules
- Reload udev
```

### 10. Health Checks
```
After installation:
- Binary runs: rpv-cam --version
- Config valid: rpv-cam --check-config
- Service status: systemctl is-active rpv-cam
- Network: ping 192.168.50.1 (cam) or 192.168.50.100 (ground)
- Ports listening: ss -tlnp | grep 9001/9003
- Logs: journalctl -u rpv-cam -n 20 (no errors)
- Hardware: v4l2-ctl --list-devices, iw dev <iface> info
```

### 11. Update Mechanism
```
Update detection:
1. Check if git repo exists: [ -d .git ]
2. Fetch remote: git fetch origin
3. Check if HEAD differs: git rev-parse HEAD != git rev-parse @{u}
4. If different:
   a. Stop services (systemctl stop rpv-cam)
   b. git pull origin <branch>
   c. cargo update (if Cargo.lock changed)
   d. cargo build --release
   e. Restart services (systemctl start rpv-cam)
   f. Show git log --oneline -5
5. If same: echo "Already up to date"
```

### 12. Uninstall
```
- Stop services: systemctl stop/disable rpv-cam
- Remove service files: /etc/systemd/system/rpv-cam.service
- Remove binaries: /usr/local/bin/rpv-cam, /opt/rpv/*
- Remove configs: ~/.config/rpv/ (ask before delete)
- Remove network config: hostapd.conf, dnsmasq.conf
- Restore NetworkManager: nmcli dev set <iface> managed yes
- Clean up: systemctl daemon-reload
- Optionally remove Rust toolchain (no, may be used elsewhere)
```

## Script Interface

### Common Flags
```
  --branch BRANCH       Git branch/tag to install (default: master)
  --prefix PATH         Install prefix (/usr/local, ~/.local, /opt)
  --user               User-level install (no sudo needed)
  --system             System-wide install (needs sudo)
  --config-dir PATH    Config directory override
  --binary-dir PATH    Binary directory override
  --no-service         Don't install systemd service
  --no-network         Skip network configuration (cam only)
  --dry-run            Show what would be done, don't modify
  --verbose            Show detailed output
  --quiet              Only errors
  --force              Overwrite existing configs without prompt
  --fresh              Wipe all state and reinstall from scratch
  --update-only        Only pull and rebuild, skip config/service
  --status             Show installation status, exit
  --uninstall          Remove RPV completely
  --help               Show help
```

### Examples
```bash
# Fresh install (system-wide, default)
sudo ./install-cam.sh

# User-only install (no sudo)
./install-cam.sh --user

# Update from git, rebuild, restart
./install-cam.sh --update-only

# Check status without installing
./install-cam.sh --status

# Uninstall
./install-cam.sh --uninstall

# Install specific branch
./install-ground.sh --branch feature/video-improvements
```

## Implementation Phases

### Phase 1: Core Framework
1. Create `install-common.sh` with:
   - Logging functions
   - OS/arch detection
   - Package manager abstraction
   - Git operations
   - Cargo build wrapper
   - Status tracking (INSTALLED, UPDATED, FAILED)

### Phase 2: Camera Installer
2. Implement `install-cam.sh` using common library:
   - Hardware detection (WiFi AP capability, camera)
   - Hostapd/dnsmasq config generation
   - Network namespace/AP setup
   - Systemd service creation (system & user modes)
   - Health checks specific to cam

### Phase 3: Ground Installer
3. Implement `install-ground.sh`:
   - Hardware detection (WiFi client capability)
   - NetworkManager integration (unmanage interface)
   - Desktop autostart ( .desktop file)
   - Gamepad udev rules
   - User systemd service
   - Health checks (connectivity to cam)

### Phase 4: Testing & Polish
4. Test on both platforms (Pi 5, x86_64)
5. Add rollback on failure
6. Add --dry-run mode
7. Document usage in README
8. Create quickstart: one-liner install command

## Rollback Strategy

Track all changes made during installation:
```
/var/lib/rpv-cam/install.state (or ~/.local/state/rpv/)
Contents:
- INSTALL_DATE=2026-04-30T02:00:00Z
- VERSION=master-abc1234
- FILES_INSTALLED=(
    /usr/local/bin/rpv-cam
    /etc/systemd/system/rpv-cam.service
    ~/.config/rpv/cam.toml
    /etc/hostapd/hostapd.conf
    /etc/dnsmasq.d/rpv.conf
  )
- BACKUPS=(
    /etc/hostapd/hostapd.conf.bak.20260430-020000
    /etc/dnsmasq.d/rpv.conf.bak.20260430-020000
  )
```

On uninstall or failed install:
- Stop services
- Remove installed files
- Restore backups (if uninstall)
- Remove state file

## Configuration Templating

Use environment variable substitution in templates:
```
# hostapd.conf.template
interface=${RPV_IFACE}
ssid=${RPV_SSID}
channel=${RPV_CHANNEL}

# During install, replace with detected values or $RPV_* env vars
# Generate final config with: envsubst < template > /etc/hostapd/hostapd.conf
```

## Validation

Post-install verification:
```bash
# Cam
rpv-cam --validate-config
rpv-cam --test-camera  # Capture 1 sec, verify format
iw dev $IFACE scan | grep -q "$SSID"  # AP beaconing
ss -tlnp | grep -E "9001|9003"  # Ports listening

# Ground
rpv-ground --validate-config
ping -c1 192.168.50.1  # Reach cam
iw dev $IFACE link | grep -q Connected  # WiFi connected
```

## Error Handling

- `set -e` for failures (but trap ERR for cleanup)
- All commands: `cmd || die "message"`
- Network operations: retry 3x with backoff
- Build failures: show cargo error, suggest rustup
- Service start failures: journalctl -u service -n 50
- Timeouts: 30s for network, 300s for build

## Security Considerations

- Config files: chmod 600 (private keys, WiFi might be open but still)
- Binary install: verify checksum if downloading (we build locally)
- Sudo usage: only when absolutely needed, prompt with clear message
- Network isolation: AP network should not bridge to internet (no iptables MASQUERADE by default)
- WiFi security: RPV uses its own crypto layer; AP is open but isolated

## Platform-Specific Notes

### Raspberry Pi (Camera)
- May need firmware updates: `sudo rpi-update` (not auto)
- CPU governor: set to performance
- Disable HDMI (save power) if headless
- Enable 64-bit mode (aarch64)
- Check for correct WiFi adapter (AR9271 vs RTL8821AU)

### Ground Station (x86_64)
- May run as user service (no sudo)
- Desktop autostart integration
- Gamepad udev rules for non-root access
- Wayland/X11 detection
- GPU acceleration (VAAPI, NVENC) optional

## Future Enhancements

- Docker/Podman container support
- Ansible playbook for multi-node deployment
- Web-based configuration UI
- Remote management via SSH tunnel
- Automatic firmware updates for WiFi adapters
- Integration with system updates (apt upgrade hooks)
