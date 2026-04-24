# RPV Multi-Branch Consolidation - Summary

## Overview
Successfully consolidated all feature branches into a unified codebase with proper Cargo feature flags.

## Changes Made

### 1. Workspace Cargo.toml
- Added workspace package defaults
- Maintained resolver = "2"

### 2. rpv-proto (No changes needed)
- Already contained shared protocol code
- L2 header, raw socket utilities, UDP socket, discovery

### 3. rpv-cam Cargo.toml
- Added optional features:
  - `raw-sock`: Raw 802.11 monitor mode
  - `udp-transport`: UDP/IP transport
  - `csi-cam`: Raspberry Pi Camera support (enables ffmpeg-sys-next)
  - `usb-cam`: USB webcam support (enables ffmpeg-sys-next)
  - `gamepad`: Gamepad input support
- Default features: all except gamepad

### 4. rpv-ground Cargo.toml
- Added optional features:
  - `raw-sock`: Raw 802.11 monitor mode
  - `udp-transport`: UDP/IP transport
  - `gamepad`: Gamepad/Joystick input (enables evdev)
- Default features: all
- Fixed evdev dependency to be properly enabled by gamepad feature

### 5. rpv-ground/src/rc/joystick.rs
- Rewrote gamepad input handling with proper conditional compilation
- Uses `#[cfg(feature = "gamepad")]` for gamepad-specific code
- Fallback to safe defaults when gamepad not available
- Fixed ArcSwap usage and borrow checker issues
- Properly imports evdev types only when gamepad feature is enabled

## Architecture

### Core Protocol (rpv-proto)
- L2 header: 8 bytes (Magic + Drone ID + Payload Type + Sequence)
- Payload types: VIDEO, TELEMETRY, RC, HEARTBEAT, MAVLINK
- Reed-Solomon 4+2 FEC for video
- Raw socket utilities for monitor mode
- UDP socket for IP transport

### Transport Modes
1. **Raw 802.11 Monitor Mode** (default)
   - Direct WiFi frame injection
   - Requires monitor-mode capable adapter
   - Lowest latency

2. **UDP/IP Transport**
   - Works over standard networks
   - Discovery protocol (port 9002)
   - Data port 9001
   - Easier for development/testing

### Video Sources
- CSI Camera (rpicam-vid) - Raspberry Pi
- USB Webcam (ffmpeg) - Any UVC device
- Configurable via camera_type parameter

### Input Methods
- MAVLink over UART (FC telemetry/RC)
- Gamepad/Joystick (evdev, optional)
- File-based RC fallback

## Build Instructions

### Default Build (all features)
```bash
cargo build --release --workspace
```

### Minimal Build (no gamepad)
```bash
cargo build --release --workspace --no-default-features --features "raw-sock,udp-transport,csi-cam,usb-cam"
```

### Without Raw Socket (UDP only)
```bash
cargo build --release --workspace --no-default-features --features "udp-transport,csi-cam,usb-cam,gamepad"
```

## Testing

Both binaries compile successfully:
- `rpv-cam`: Camera transmitter
- `rpv-ground`: Ground station with OSD

### Known Warnings
- Dead code warnings for unused constants (NAL_START_CODE, etc.)
- These are from the existing codebase and don't affect functionality

## Feature Matrix

| Feature | Raw Socket | UDP | CSI Cam | USB Cam | Gamepad | Status |
|---------|-----------|-----|---------|---------|---------|--------|
| Production | ✓ | ✓ | ✓ | ✓ | Optional | ✅ |
| Development | ✓ | ✓ | ✓ | ✓ | Optional | ✅ |
| Testing | ✗ | ✓ | ✓ | ✓ | Optional | ✅ |

## Next Steps

1. Test on actual hardware (Pi 5 with monitor-mode WiFi)
2. Verify video streaming latency
3. Test telemetry/RC functionality
4. Update deployment scripts if needed
5. Add CI/CD configuration for feature builds

## Files Modified

- Cargo.toml (workspace)
- rpv-cam/Cargo.toml
- rpv-ground/Cargo.toml
- rpv-ground/src/rc/joystick.rs

## Files Unchanged

- All protocol code (rpv-proto) - already consolidated
- Main application logic - already consolidated in feat/udp-transport-hwdec
- Video decoder/receiver - already consolidated
- Configuration system - already consolidated
- Deploy scripts - already consolidated

## Notes

- The feat/udp-transport-hwdec branch already contained most consolidated features
- Main work was adding proper Cargo feature flags and fixing conditional compilation
- Gamepad support is optional and properly isolated with cfg flags
- Both raw socket and UDP transport can be enabled simultaneously
- Video source selection (CSI vs USB) is configurable at runtime via config file
