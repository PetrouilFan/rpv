# RPV System - Quick Start Guide

## Overview

RPV (Raspberry Pi Video) is a low-latency video streaming system for drones/UAVs.
It consists of:
- **Camera (Pi)**: Captures video and streams it over WiFi
- **Ground Station (PC)**: Receives video and displays it

## Network Architecture

The system uses a **WiFi hotspot** created by the Pi's external WiFi adapter:

```
Pi (Camera)                                    Ground Station (PC)
 wlan0 ────────────> Home Network (internet)        
 wlan1 ────────────┐                                 
 192.168.50.1      │  Hotspot: rpv-link              
                   │  Channel: 6                     
                   └───────────────────────────────> wlp40s0f3u2
                    192.168.50.100 (ground station)  
                    TCP port 9003                    
```

The Pi's internal WiFi (wlan0) stays connected to your home network for internet access.
The external WiFi adapter (wlan1) creates a hotspot that the ground station connects to.

## Prerequisites

### On the Pi (10.0.0.59):

The IP address `10.0.0.59` in this guide is an example from a specific setup. Your Pi's IP on the home network may differ. Use `ip addr show eth0` or check your router to find the Pi's actual IP. Alternatively, use mDNS: `ssh petrouil@raspberrypi.local`.

### On the Ground Station (this PC):
- Linux (Arch)
- External WiFi adapter (AR9271-based Alfa AWUS036N)
- SSH access to Pi

## Setup Steps

### 1. On the Pi - Set up the Hotspot

```bash
# SSH into the Pi
ssh petrouil@10.0.0.59

# Copy the setup script
# (or create it manually from setup-pi-hotspot.sh)

# Run the hotspot setup
sudo ./setup-pi-hotspot.sh
```

This will:
- Install hostapd and dnsmasq
- Configure the external WiFi adapter as an AP
- Set up DHCP (192.168.50.100-101)
- Enable IP forwarding and NAT
- Start the hotspot

**Verify:**
```bash
# Check hotspot is running
sudo systemctl status hostapd
sudo systemctl status dnsmasq

# Check IP
ip addr show wlan1  # or whatever the external interface is
# Should show 192.168.50.1/24
```

### 2. On the Pi - Configure and Start the Camera

The camera config is at `~/.config/rpv/cam.toml`:

```toml
interface = "wlan0"          # Internal WiFi (for internet)
drone_id = 1
transport = "tcp"            # TCP mode (more reliable)
tcp_port = 9003              # Ground station connects here
udp_port = 9001
ap_ssid = "rpv-link"
ap_channel = 6
video_width = 960
video_height = 540

video_device = "/dev/video0"
camera_type = "csi"
fc_port = "/dev/ttyAMA0"
fc_baud = 115200
framerate = 30
bitrate = 3000000
intra = 30
```

Start the camera:
```bash
cd ~/rpv
sudo ./target/release/rpv-cam
```

### 3. On the Ground Station - Connect to Hotspot

```bash
# Run the ground station setup script
./run-ground.sh
```

This will:
- Find the external WiFi adapter
- Connect to 'rpv-link' hotspot
- Configure the ground station
- Start rpv-ground

**Or manually:**
```bash
# Connect to hotspot
nmcli device wifi connect rpv-link

# Verify connection
ip addr show wlp40s0f3u2  # Should show 192.168.50.100
ping 192.168.50.1          # Should reach Pi

# Start ground station
sudo ./target/release/rpv-ground
```

### 4. Verify the Connection

**On the Pi:**
```bash
# Check TCP connection
ss -tlnp | grep 9003
# Should show LISTEN on 0.0.0.0:9003

# Check link status
cat /tmp/rpv_link_status
# Should show "connected"
```

**On the Ground Station:**
```bash
# Check link status
cat /tmp/rpv_link_status
# Should show "connected"

# The GUI should show "LINK OK" in green
```

## Troubleshooting

### Hotspot Not Starting

```bash
# Check hostapd logs
sudo journalctl -u hostapd -n 50

# Check if interface is up
ip link show wlan1

# Check for RF kill
rfkill list
sudo rfkill unblock wifi
```

### Camera Not Connecting

```bash
# On Pi, check if rpv-cam is running
ps aux | grep rpv-cam

# Check logs
tail -f /var/log/syslog | grep rpv

# Verify config
cat ~/.config/rpv/cam.toml
```

### Ground Station Shows "Searching"

```bash
# Verify network connectivity
ping 192.168.50.1

# Check TCP port
nc -zv 192.168.50.1 9003

# Check ground station config
cat ~/.config/rpv/ground.toml

# Verify interface
ip addr show wlp40s0f3u2
```

### No Video Display

```bash
# Check if video frames are being received
# Look for "FPS:" in the GUI or logs

# On Pi, check camera
v4l2-ctl --list-devices
ls /dev/video*
```

## SSH Access via Hotspot

Once the hotspot is running and the ground station is connected:

```bash
# From ground station, SSH to Pi via hotspot IP
ssh petrouil@192.168.50.1

# Password: kalhmera
```

The Pi is accessible via:
- **eth0**: 10.0.0.59 (home network)
- **wlan1**: 192.168.50.1 (hotspot)

## Files

- `setup-pi-hotspot.sh` - Pi hotspot setup script
- `run-ground.sh` - Ground station startup script  
- `run-cam.sh` - Camera startup script
- `~/.config/rpv/ground.toml` - Ground station config
- `~/.config/rpv/cam.toml` - Camera config
- `/tmp/rpv_link_status` - Link status file

## Architecture Details

### Transport Modes

- **TCP** (recommended): Ground station listens, camera connects. More reliable, handles reconnection.
- **UDP**: Both use discovery. Lower latency but less reliable.
- **Raw**: Direct 802.11 frame injection (requires monitor mode).

### Ports

- **9003**: TCP video stream (ground station listens)
- **9001**: UDP discovery/streaming
- **14550**: MAVLink downlink (QGC)
- **14551**: MAVLink uplink (QGC)

### CPU Pinning

- **Core 0**: RX dispatcher (SCHED_FIFO priority 50)
- **Core 1**: Video capture/encoding (SCHED_FIFO priority 50)

## Building from Source

```bash
# On both Pi and ground station
cd /home/petrouil/Projects/github/rpv
cargo build --release

# Binaries will be in target/release/
# - rpv-cam (camera)
# - rpv-ground (ground station)
```

## Performance

- **Latency**: ~50-100ms (camera to display)
- **Bandwidth**: ~3-5 Mbps (960x540 @ 30fps)
- **Range**: Depends on WiFi adapter (100-500m typical)
