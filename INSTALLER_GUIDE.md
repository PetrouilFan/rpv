# RPV Installation Guide

## Quick Start

### Camera Node (Raspberry Pi)

```bash
# Clone repository
git clone https://github.com/yourname/rpv.git
cd rpv

# Build (requires Rust)
cargo build --release --bin rpv-cam

# Install (system-wide, needs sudo)
sudo deploy/install-cam.sh --system

# Or user-only (no sudo)
deploy/install-cam.sh --user
```

### Ground Station (Desktop PC)

```bash
# Build
cargo build --release --bin rpv-ground

# Install (user-level recommended)
./deploy/install-ground.sh --user

# Connect to camera's WiFi
./rpv-connect.sh  # helper script created by installer
```

## Installer Scripts

### `deploy/install-cam.sh` — Camera Node (Raspberry Pi)

Installs and configures the camera node as an access point.

**Features:**
- Auto-detects AP-capable WiFi adapter (prefers AR9271/RTL8821AU)
- Sets up hostapd + dnsmasq for hotspot
- Installs systemd service (system or user)
- Configures `~/.config/rpv/cam.toml`
- Creates helper scripts
- Health checks post-install

**Usage:**
```bash
# System install (default, needs sudo)
sudo deploy/install-cam.sh

# User-only install (no sudo)
./deploy/install-cam.sh --user

# Update from git and restart
./deploy/install-cam.sh --update-only

# Check status
./deploy/install-cam.sh --status

# Uninstall
sudo deploy/install-cam.sh --uninstall

# Install specific branch
./deploy/install-cam.sh --branch feature/improvements

# Fresh reinstall (wipes config)
./deploy/install-cam.sh --fresh
```

**Options:**
```
  --system              System-wide install to /usr/local (default)
  --user                User-only install to ~/.local (no sudo)
  --branch BRANCH       Git branch/tag to install (default: master)
  --prefix PATH         Install prefix (default: /usr/local or ~/.local)
  --config-dir DIR      Config directory (default: ~/.config/rpv)
  --dry-run             Show actions without making changes
  --force               Overwrite existing files without prompting
  --fresh               Remove all existing state, reinstall from scratch
  --update-only         Pull git updates and rebuild (skip config/services)
  --status              Show installation status and exit
  --uninstall           Remove RPV completely
  --verbose             Show detailed output
  --quiet               Only show errors
  --help                Show this help
```

**Environment Variables:**
```bash
RPV_IFACE="wlan1"        # WiFi interface for AP (auto-detected if unset)
RPV_SSID="rpv-link"      # Hotspot SSID
RPV_CHANNEL=6           # WiFi channel (1-14)
RPV_AP_IP="192.168.50.1" # AP static IP
RPV_AP_IP="10.42.0.1"   # Alternative subnet
```

### `deploy/install-ground.sh` — Ground Station (Desktop PC)

Installs and configures the ground station with desktop integration.

**Features:**
- Installs binary to ~/.local/bin or /usr/local/bin
- Creates desktop autostart entry (user mode)
- Sets up udev rules for gamepad access
- Creates `rpv-connect.sh` helper to join camera's AP
- Installs systemd service (system or user)
- Configures `~/.config/rpv/ground.toml`

**Usage:**
```bash
# User install (recommended, no sudo)
./deploy/install-ground.sh --user

# System install
sudo ./deploy/install-ground.sh --system

# Update
./deploy/install-ground.sh --update-only --user

# Check status
./deploy/install-ground.sh --status --user
```

Same options as camera installer.

## Post-Installation

### After Camera Install

1. **Verify AP is running:**
```bash
sudo systemctl status rpv-cam
iw dev wlan1 scan | grep rpv-link
ip addr show wlan1  # should have 192.168.50.1
```

2. **Check logs:**
```bash
journalctl -u rpv-cam -f
```

3. **Test:**
- Connect a device (phone/ground station) to "rpv-link" WiFi
- Should get IP 192.168.50.100
- Ping 192.168.50.1 should work

### After Ground Station Install

1. **Connect to camera's AP:**
```bash
# Use helper script (created by installer)
./rpv-connect.sh

# Or manually:
sudo iw dev wlan0 connect rpv-link
sudo ip addr add 192.168.50.100/24 dev wlan0
```

2. **Verify connection:**
```bash
ping 192.168.50.1
iw dev wlan0 link
```

3. **Start ground station:**
```bash
# If using user service (auto-starts on login)
systemctl --user status rpv-ground

# Or manually:
rpv-ground
```

4. **Check logs:**
```bash
journalctl --user -u rpv-ground -f
```

## Update / Redeploy

To update to the latest version:

```bash
# Camera
sudo ./deploy/install-cam.sh --update-only

# Ground
./deploy/install-ground.sh --update-only --user
```

This will:
1. `git fetch` and `git pull` latest changes
2. Rebuild with `cargo build --release`
3. Reinstall binary
4. Restart service

**Note:** Config files are preserved across updates.

## Uninstall

### Camera
```bash
sudo ./deploy/install-cam.sh --uninstall
```

Removes:
- Binary (`/usr/local/bin/rpv-cam`)
- Systemd service (`/etc/systemd/system/rpv-cam.service`)
- Network config (hostapd/dnsmasq)
- State file (`/var/lib/rpv/cam.state`)
- Prompts to remove config (`~/.config/rpv/cam.toml`)

### Ground
```bash
./deploy/install-ground.sh --uninstall --user
```

Removes:
- Binary (`~/.local/bin/rpv-ground`)
- User service (`~/.config/systemd/user/rpv-ground.service`)
- Desktop autostart (`~/.config/autostart/rpv-ground.desktop`)
- Helper script (`~/rpv-connect.sh`)
- Prompts to remove config and udev rules

## Troubleshooting

### "No AP-capable WiFi adapter found"

**Solution:** Use a supported adapter:
- **Recommended:** AR9271 (ath9k_htc driver) — works out of the box
- **Also works:** RTL8821AU (rtl8xxxu driver) — may need firmware
- **Avoid:** Broadcom BCM4345 (brcmfmac) — poor AP support

Force a specific interface:
```bash
sudo RPV_IFACE=wlan1 ./deploy/install-cam.sh
```

### "hostapd failed to start"

Common causes:
1. **Interface in use by NetworkManager:**
   ```bash
   sudo systemctl stop NetworkManager
   # Or configure NM to ignore the interface:
   sudo nmcli dev set wlan1 managed no
   ```

2. **Driver doesn't support AP mode:**
   ```bash
   iw phy phy0 info | grep "Supported interface modes" -A 10
   # Look for "AP" in the list
   ```

3. **Channel not allowed in regulatory domain:**
   ```bash
   sudo iw reg set US  # or your country code
   ```

4. **Another hostapd instance already running:**
   ```bash
   sudo pkill hostapd
   ```

### "dnsmasq failed to start"

Common causes:
1. **Port 53 already in use:**
   ```bash
   sudo systemctl stop systemd-resolved
   # Or use different DNS port in dnsmasq config
   ```

2. **Interface doesn't exist:**
   ```bash
   ip link show wlan1  # verify interface name
   ```

### Build fails: "can't find libavcodec"

Install FFmpeg development libraries:

Debian/Ubuntu/Raspbian:
```bash
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
```

Arch/Manjaro:
```bash
sudo pacman -S ffmpeg
```

### "rpv-cam: command not found" after install

Binary not in PATH. Add to shell config:

```bash
# If installed to /usr/local/bin (system)
# Already in PATH on most systems

# If installed to ~/.local/bin (user)
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

### Ground station: "No display server detected"

RPV ground station needs X11 or Wayland. If running headless/SSH:

```bash
# Option 1: SSH with X forwarding
ssh -X user@ground-pc

# Option 2: Force install without display (won't run UI)
./install-ground.sh --user --force

# Option 3: Set virtual display (Xvfb)
Xvfb :0 -screen 0 1920x1080x24 &
export DISPLAY=:0
```

### Service fails to start

Check logs:
```bash
# System service
sudo journalctl -u rpv-cam -n 50 --no-pager

# User service
journalctl --user -u rpv-cam -n 50 --no-pager
```

Common issues:
- Permission denied on config file (should be 600)
- Binary not found at configured path
- Network interface doesn't exist
- Missing dependencies (check build output)

### WiFi adapter not detected

Verify adapter is recognized:
```bash
lsusb | grep -i "0cf3:9271"    # AR9271
lsusb | grep -i "0bda:8176"    # RTL8821AU

# Check driver loaded
lsmod | grep -E "ath9k_htc|rtl8xxxu"
```

If driver missing:

Debian/Raspbian:
```bash
sudo apt install firmware-atheros
sudo modprobe ath9k_htc
```

Arch:
```bash
sudo pacman -S linux-firmware
sudo modprobe ath9k_htc
```

### RP link status file shows "disconnected"

The status file is at `/tmp/rpv_link_status` (or `~/.config/rpv/link_status`).

If it shows "disconnected" even though services are running:
- Check service logs for errors
- Verify WiFi is actually connected (ground) or AP is up (cam)
- Ensure both sides use same transport (TCP/UDP) and ports

## Advanced Usage

### Custom Configuration

After install, edit config:

Camera: `~/.config/rpv/cam.toml`
```toml
[common]
drone_id = 1

[video]
bitrate = 4_000_000      # 4 Mbps
framerate = 30
width = 1280
height = 720
intra = 30

[network]
transport = "udp"        # "udp", "tcp", or "raw"
interface = "wlan1"
peer_address = "10.42.0.2:9001"
```

Ground: `~/.config/rpv/ground.toml`
```toml
[common]
drone_id = 1

[video]
# decoder = "vaapi"      # Hardware acceleration (if available)
width = 1280
height = 720

[network]
transport = "udp"
listen_address = "0.0.0.0:9001"
```

**Important:** After editing config, restart service:
```bash
sudo systemctl restart rpv-cam      # camera
systemctl --user restart rpv-ground # ground
```

### Multiple Drones

Set different `drone_id` on each camera (1-255):
```bash
# Camera 1
RPV_DRONE_ID=1 ./install-cam.sh --user

# Camera 2
RPV_DRONE_ID=2 ./install-cam.sh --user
```

Update config manually:
```toml
[common]
drone_id = 2
```

### Remote Management

Check status remotely:
```bash
ssh pi@10.0.0.59 'systemctl status rpv-cam'
ssh pi@10.0.0.59 'journalctl -u rpv-cam -n 50'
```

Update remotely:
```bash
ssh pi@10.0.0.59 './rpv/deploy/install-cam.sh --update-only'
```

### Using Different WiFi Adapter

If auto-detection picks wrong interface:

```bash
# Check available interfaces
iw dev

# Force specific interface
sudo RPV_IFACE=wlan2 ./deploy/install-cam.sh
```

Interface will be saved to `/tmp/rpv-iface` and used by service.

### Debug Mode

Enable verbose logging:

Camera:
```bash
sudo systemctl edit rpv-cam
# Add:
[Service]
Environment=RUST_LOG=debug
```

Ground:
```bash
systemctl --user edit rpv-ground
# Add:
[Service]
Environment=RUST_LOG=debug
```

Then restart service.

## Architecture

### Installer Components

```
deploy/
├── install-common.sh        # Shared library (functions, OS detection)
├── install-cam.sh           # Camera installer
├── install-ground.sh        # Ground installer
├── cam/
│   ├── rpv-cam.service      # Systemd service template (cam)
│   ├── rpv-net-setup-pre.sh # Network setup (AP mode)
│   └── rpv-net-teardown.sh  # Network cleanup
└── ground/
    ├── rpv-ground.service   # Systemd service template (ground)
    ├── rpv-net-setup-pre.sh # Network setup (client mode)
    └── rpv-net-teardown.sh  # Network cleanup
```

### State Tracking

Installation state saved to:
- System: `/var/lib/rpv/cam.state` and `ground.state`
- Keys: `INSTALL_DATE`, `VERSION`, `BRANCH`, `MODE`, `PREFIX`, `IFACE`, `SSID`

Used for:
- Determining if update is needed
- `--status` command
- Rollback on failure

### Idempotency

All installer operations are idempotent:
- Running twice with same args is safe
- Existing configs are preserved (unless `--force` or `--fresh`)
- Services are only installed once
- Duplicate network setup is harmless (cleanup first)

### Update Workflow

```
install.sh --update-only
    ├── git fetch + git pull
    ├── cargo build --release
    ├── Stop service
    ├── Replace binary
    ├── Start service
    └── Show git log
```

### Rollback on Failure

If any step fails:
1. Trap catches error
2. Stops any started services
3. Removes partially installed files
4. Restores backups
5. Exits with error code

**Note:** Config changes are not rolled back (to preserve user data).

## Compatibility

### Supported Distributions

| Distro          | Package Manager | Tested |
|-----------------|-----------------|--------|
| Debian 11/12    | apt             | Yes    |
| Ubuntu 20.04+   | apt             | Yes    |
| Raspbian/Raspberry Pi OS | apt   | Yes    |
| Arch Linux      | pacman          | Yes    |
| Manjaro         | pacman          | Yes    |

May work on:
- Fedora (dnf) — package names differ
- openSUSE (zypper) — not supported
- Alpine (apk) — musl libc, unlikely

### Hardware Requirements

**Camera Node (Raspberry Pi):**
- Raspberry Pi 3B+ or newer (Pi 5 recommended)
- 2GB+ RAM
- USB 3.0 recommended (for AR9271 adapter)
- WiFi adapter with AP mode (AR9271, RTL8821AU)
- CSI camera (Raspberry Pi camera) or USB webcam
- Ethernet optional (for setup)

**Ground Station:**
- x86_64 PC or Raspberry Pi 4/5
- 4GB+ RAM (for decoding 720p@30fps)
- WiFi adapter with station mode (any modern adapter)
- Optional: Gamepad (XInput compatible)
- Optional: GPU for hardware decoding (Intel VAAPI, NVIDIA NVENC)

## Development

### Testing the Installer

```bash
# Dry run (no changes)
./deploy/install-cam.sh --dry-run --user

# Fresh VM test
# 1. Create fresh VM
# 2. Copy RPV repo
# 3. Run installer
# 4. Verify services start
# 5. Check logs
```

### Adding New Dependencies

1. Update `CAM_PACKAGES` or `GROUND_PACKAGES` array in respective script
2. Add package name mapping in `get_package_name()` in `install-common.sh`
3. Test on both Debian and Arch

### Debugging

Enable shell tracing:
```bash
bash -x ./deploy/install-cam.sh --user --verbose 2>&1 | tee install.log
```

Check state file:
```bash
cat /var/lib/rpv/cam.state
```

## Support

If installation fails:
1. Check logs in systemd: `journalctl -u rpv-cam -n 100`
2. Run with `--verbose` for detailed output
3. Check `install.log` if using `bash -x`
4. Open an issue on GitHub with:
   - OS and version
   - Architecture (uname -m)
   - WiFi adapter (`lsusb`, `iwconfig`)
   - Full installer output
   - Relevant log snippets

## Credits

Installer design inspired by:
- Docker's install script
- rustup's installer
- systemd's upstream packaging guidelines
