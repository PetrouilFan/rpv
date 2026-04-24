# RPV Repository Status

## ✅ CLEAN AND ORGANIZED

**Last Updated**: 2026-04-24
**Branch**: master
**Status**: Production Ready

---

## Repository Structure

```
rpv/
├── Cargo.toml              # Workspace configuration
├── Cargo.lock              # Dependency lock file
├── README.md               # Main documentation
├── AUDIT.md                # Security audit information
├── CLEANUP_REPORT.md       # Repository cleanup report
│
├── rpv-proto/              # Protocol library (shared)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── config.rs
│       ├── link.rs
│       ├── discovery.rs
│       ├── rawsock_common.rs
│       ├── udpsock.rs
│       └── socket_trait.rs
│
├── rpv-cam/                # Camera transmitter
│   ├── Cargo.toml          # Features: raw-sock, udp-transport, csi-cam, usb-cam
│   └── src/
│       ├── main.rs
│       ├── config.rs
│       ├── video_tx.rs
│       ├── fc.rs
│       └── rawsock.rs
│
├── rpv-ground/             # Ground station
│   ├── Cargo.toml          # Features: raw-sock, udp-transport, gamepad
│   ├── build.rs
│   ├── install.sh
│   └── src/
│       ├── main.rs
│       ├── config.rs
│       ├── link_state.rs
│       ├── rc/joystick.rs
│       ├── telemetry.rs
│       └── video/
│           ├── decoder.rs
│           └── receiver.rs
│
└── deploy/                 # Deployment scripts
    ├── install-cam.sh
    ├── install-ground.sh
    ├── cam/
    │   ├── rpv-net-setup-pre.sh
    │   └── rpv-net-teardown.sh
    └── ground/
        ├── rpv-net-setup-pre.sh
        └── rpv-net-teardown.sh
```

---

## Features

### Transport Modes
- **Raw 802.11 Monitor Mode** (default) - Direct WiFi frame injection
- **UDP/IP Transport** - Works over standard networks

### Video Sources
- **CSI Camera** (rpicam-vid) - Raspberry Pi Camera Module
- **USB Webcam** (ffmpeg) - Any UVC-compatible device

### Input Methods
- **MAVLink over UART** - Flight controller telemetry/RC
- **Gamepad/Joystick** (evdev) - Direct RC input (optional)
- **File-based RC** - Fallback

---

## Build Features

### rpv-cam
- `raw-sock`: Raw 802.11 monitor mode
- `udp-transport`: UDP/IP transport
- `csi-cam`: Raspberry Pi Camera support
- `usb-cam`: USB webcam support
- `gamepad`: Gamepad input (optional)

### rpv-ground
- `raw-sock`: Raw 802.11 monitor mode
- `udp-transport`: UDP/IP transport
- `gamepad`: Gamepad input (enables evdev)

---

## Build Commands

```bash
# Default build (all features)
cargo build --release --workspace

# Development build
cargo build --workspace

# Minimal build (no gamepad)
cargo build --release --workspace \
  --no-default-features \
  --features "raw-sock,udp-transport,csi-cam,usb-cam"

# UDP only (no raw socket)
cargo build --release --workspace \
  --no-default-features \
  --features "udp-transport,csi-cam,usb-cam,gamepad"
```

---

## CI/CD

- **GitHub Actions**: `.github/workflows/ci.yml`
- **Checks**: All targets compile successfully
- **Tests**: cargo check --workspace --all-targets
- **Build**: cargo build --workspace --all-targets

---

## Verification

✅ All source code compiles
✅ No temporary files
✅ Build artifacts cleaned
✅ CI/CD pipeline operational
✅ Production ready

---

## Git History

- `28b41e7` - Add cleanup report
- `37ea056` - Cleanup temporary files
- `1b7b6b8` - Fix CI (ffmpeg libraries)
- `f97fe72` - Merge PR #2
- `0afda57` - Consolidate feature branches
- `783b103` - Add Cargo feature flags

---

## Status: ✅ PRODUCTION READY

The RPV repository is clean, organized, and ready for production use.
