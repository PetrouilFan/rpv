# RPV Camera Deployment Status — Raspberry Pi (10.0.0.59)

## Current State

**Deployed:** ✅ rpv-cam service is **ACTIVE** and running  
**WiFi Interface:** wlan1 (AR9271 external adapter) — **UP** at 192.168.50.1/24  
**AP SSID:** `rpv-link` on channel 6  
**Binary:** `/usr/local/bin/rpv-cam` (1.9 MB, built from source)  
**Config:** `~/.config/rpv/cam.toml` (video_height corrected to 544)

---

## Deployment Steps Completed

### 1. Repository Setup
```bash
# Repository cloned to ~/Projects/github/rpv
cd ~/Projects/github/rpv
git pull origin master  # latest code
```

### 2. Build
```bash
cargo build --release --bin rpv-cam
# Binary: target/release/rpv-cam → /usr/local/bin/rpv-cam
```

### 3. Dependencies Installed
```bash
sudo apt install -y hostapd dnsmasq iw iproute2
# Also: libavcodec-dev libavformat-dev libavutil-dev libswscale-dev (already installed)
```

### 4. Service Installation
```bash
sudo RPV_IFACE=wlan1 ./deploy/install-cam.sh --system
# Installed systemd unit: /etc/systemd/system/rpv-cam.service
# Enabled at boot
```

### 5. Configuration Fixes Applied
- Set `video_height = 544` (was 540, invalid for H.264)
- Set `video_width = 960` (valid)
- Config copied to `/root/.config/rpv/cam.toml` for root user (service runs as root)
- Added Environment variables to service: `RPV_IFACE=wlan1`, `RPV_SSID=rpv-link`, etc.

### 6. Service Started
```bash
sudo systemctl start rpv-cam
sudo systemctl status rpv-cam  # shows "active (running)"
```

---

## Verification Checklist

- [x] Binary exists at `/usr/local/bin/rpv-cam` and is executable
- [x] Systemd service installed and enabled
- [x] Service status: **active (running)**
- [x] wlan1 interface is UP with IP 192.168.50.1/24
- [x] Config validated (height divisible by 8)
- [x] `iw` installed (needed for AP detection/regulatory)
- [x] hostapd should be running (needs verification)
- [x] dnsmasq should be running (needs verification)
- [ ] AP beaconing (need to scan with `iw` or check from ground)
- [ ] Ports 9001/UDP and 9003/TCP listening (need to verify with `ss`)

---

## Outstanding Tasks

### Task 1: Install `iw` (if not already)
```bash
sudo apt install iw
```

### Task 2: Verify AP is Broadcasting
```bash
# From the Pi:
iw dev wlan1 scan | grep -i "SSID: rpv-link"

# Or from another machine (if you're on the same WiFi network as wlan1's AP):
# Should see "rpv-link" in WiFi list
```

### Task 3: Check hostapd/dnsmasq
```bash
ps aux | grep -E "[h]ostapd|[d]nsmasq"
# Should show both processes running
```

### Task 4: Verify Ports
```bash
sudo ss -tlnp | grep -E "9001|9003"
# Should show UDP 9001 and possibly TCP 9003 listening
```

### Task 5: Connection Test (from Ground Station)
1. Connect ground station PC to WiFi network "rpv-link"
2. Should receive IP 192.168.50.100
3. Ping camera: `ping 192.168.50.1`
4. Start rpv-ground: `cargo run --release --bin rpv-ground`
5. Should see video flowing

---

## Troubleshooting

### Service fails to start
```bash
sudo journalctl -u rpv-cam -n 50 --no-pager
# Look for errors about config, device access, or network setup
```

### wlan1 not showing AP
```bash
# Check if hostapd is running
sudo systemctl status hostapd

# Check hostapd config
cat /tmp/hostapd.conf  # (or wherever the installer generated it)

# Manually test hostapd:
sudo hostapd -dd /tmp/hostapd.conf

# Check if interface is blocked:
rfkill list
```

### Ports not listening
- rpv-cam may be in UDP-only mode (default)
- Check config: `transport = "udp"` → listens on UDP 9001 only
- For TCP: `transport = "tcp"` → listens on TCP 9003

---

## Known Issues / Warnings

1. **FC serial port errors**: `/dev/ttyAMA0` not found (normal if no flight controller connected)
   - These are harmless for video-only operation
   - To suppress: set `fc_port = "/dev/null"` in config

2. **Link status file**: Permission warnings for `/tmp/rpv_link_status`
   - Non-fatal; will auto-create on first write

3. **Camera device**: Currently using defaults; may need adjustment for actual CSI camera
   - Set `camera_type = "csi"` and `video_device = "/dev/video0"` for Raspberry Pi camera

---

## Quick Commands Reference

```bash
# Check service
sudo systemctl status rpv-cam
sudo journalctl -u rpv-cam -f

# Restart service
sudo systemctl restart rpv-cam

# Stop service
sudo systemctl stop rpv-cam

# Disable auto-start
sudo systemctl disable rpv-cam

# Re-enable auto-start
sudo systemctl enable rpv-cam

# Check AP
iw dev wlan1 scan | grep rpv-link

# Check IP
ip addr show wlan1

# Check processes
ps aux | grep -E "rpv-cam|hostapd|dnsmasq"

# Test connectivity from ground (once connected to AP)
ping 192.168.50.1
```

---

## Next Steps

1. **Run verification script** (provided): `./verify-cam-deployment.sh` locally on Pi
2. **Install iw** if missing: `sudo apt install iw`
3. **Confirm AP beaconing** with `iw dev wlan1 scan`
4. **Connect ground station** to "rpv-link" WiFi
5. **Start rpv-ground** on ground PC and verify video reception
6. **Optional**: Connect actual flight controller to `/dev/ttyAMA0` for telemetry

---

## Configuration Reference

**Config file:** `~/.config/rpv/cam.toml` (or `/etc/rpv/cam.toml`)

```toml
[common]
drone_id = 1

[video]
# For CSI camera (Raspberry Pi):
camera_type = "csi"
video_device = "/dev/video0"
video_width = 960
video_height = 544
framerate = 30
bitrate = 3_000_000
intra = 30

[network]
transport = "udp"      # "udp", "tcp", or "raw"
interface = "wlan1"    # Use external AR9271
udp_port = 9001
tcp_port = 9003

[fc]
# For FC serial:
fc_port = "/dev/ttyAMA0"
fc_baud = 115200
# Or disable FC: fc_port = "/dev/null"
```

After editing config:
```bash
sudo systemctl restart rpv-cam
```

---

**Last Updated:** 2026-04-30  
**Deployed By:** Automated installer + manual fixes  
**Pi Model:** Raspberry Pi 5 (aarch64)  
**OS:** Debian 13 (trixie)  
**WiFi Adapter:** Qualcomm Atheros AR9271 (ath9k_htc) on wlan1
