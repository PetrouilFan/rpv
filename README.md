# rpv

Rust FPV system for Raspberry Pi. Low-latency H.264 video over raw 802.11 monitor mode with Reed-Solomon FEC, MAVLink FC integration, RC control, and a real-time OSD.

## Architecture

```
┌─────────────┐    raw 802.11     ┌──────────────┐
│  rpv-cam    │ ──── air ──────→  │  rpv-ground   │
│  (Pi 5)     │ ←──── air ─────  │  (Pi 5)       │
│             │                   │               │
│ ffmpeg      │  Video (RS 1+1)   │ VideoReceiver │
│ → NAL frag  │  Telemetry (JSON) │ → libavcodec  │
│ → FEC encode│  RC (16ch @ 50Hz) │ → egui/wgpu   │
│ → 802.11 TX │  Heartbeat (10Hz) │ → OSD         │
│             │                   │               │
│ FC serial   │                   │ RC joystick   │
│ (MAVLink)   │                   │ Heartbeat     │
└─────────────┘                   └──────────────┘
```

## Components

### rpv-cam

Camera sender. Single binary targeting Pi 5.

Features:
- Captures H.264 via `ffmpeg` (configurable resolution, framerate, bitrate)
- Fragments NALUs, RS FEC encodes, streams over raw 802.11
- Receives RC commands from ground → writes to FC via MAVLink (or file fallback)
- Sends FC telemetry, camera status, and heartbeats back to ground
- RC failsafe: releases override after 500ms (FC path) or clears file after 2s (file path)

### rpv-ground

Ground station (RPi 5 + HDMI). Single binary with GPU YUV→RGB via wgpu shader.

Features:
- RS 4+2 FEC reassembly with out-of-order tolerance and stall recovery
- H.264 decode via libavcodec (FFmpeg)
- Fullscreen OSD: link status, FPS, RSSI, battery, speed, altitude, heading, GPS, mode
- RC transmitter (50Hz, deadline-based scheduling, jitter tracking)
- Heartbeat-based link state machine (Searching → Connected → SignalLost / NoCamera)
- Link state file at `/tmp/rpv_link_status` for external tooling

## L2 Protocol

All communication uses raw 802.11 broadcast frames in monitor mode. No IP stack.

### L2 Header (8 bytes)

```
[0..2]  Magic: 0x52 0x50 ("RP")
[2]     Drone ID: u8 — filters frames from other swarms
[3]     Payload Type:
           0x01 = Video (RS-encoded shards)
           0x02 = Telemetry (JSON)
           0x03 = RC commands
           0x04 = Heartbeat
[4..8]  Sequence number (u32 LE)
[8..]   Payload
```

### Video

Each video block is a Reed-Solomon 4+2 group. Each shard packet has a 16-byte video header:

```
[4B block_seq][1B shard_idx][1B total_shards][1B data_shards][1B pad][2B*4 shard_lens]
```

Data shards carry NALU fragments prefixed with a 2-byte fragment index (u16 LE). The `shard_lens` array tells the receiver the original data size (before FEC padding) so it can trim reconstructed shards.

### RC Commands

Ground sends 16 channels at 50Hz. Payload:

```
[4B channel_count][N × 2B channel_values LE]
```

Channel values: 1000–2000 (PWM us), 1500 = neutral, 1000 = throttle min. MAVLink `RC_CHANNELS_OVERRIDE` forwards channels 1–8 to the FC. Channels 9–16 are logged as a warning (not forwarded — MAVLink v1 limit).

### Telemetry

Camera sends JSON telemetry at 5Hz. Fields:

```json
{"lat": 0.0, "lon": 0.0, "alt": 0.0, "heading": 0.0, "speed": 0.0,
 "satellites": 0, "battery_v": 0.0, "battery_pct": 0, "mode": "UNKNOWN",
 "armed": false, "camera_ok": true}
```

### Heartbeat

Both sides send heartbeats at 10Hz. Payload: `[7B "rpv-bea"][4B seq][8B timestamp_ms]`. The ground heartbeat monitor (500ms timeout) is the **sole authority** for SignalLost transitions. Telemetry and video cannot override it.

## Configuration

### Camera (`~/.config/rpv/cam.toml`)

Auto-generated on first run. All fields optional (defaults shown):

```toml
interface    = "wlan1"      # WiFi interface (must support monitor mode)
drone_id     = 0            # Filters frames by swarm ID
fc_port      = "/dev/ttyAMA0"
fc_baud      = 115200
video_width  = 960
video_height = 540
framerate    = 30           # Frames per second (higher = lower latency, lower quality)
bitrate      = 3000000      # H.264 bitrate in bps
```

### Ground (`~/.config/rpv/ground.toml`)

```toml
interface    = "wlan1"
drone_id     = 0            # Must match camera
video_width  = 960
video_height = 540
```

## Performance Tuning

### Lower latency (at the cost of quality)

```toml
# cam.toml
framerate = 60    # 16ms frame time vs 33ms — ~17ms saved
bitrate = 5000000 # compensate for lower per-frame bits
```

### Higher quality (at the cost of latency)

```toml
framerate = 24    # 42ms frame time
bitrate = 4000000
```

### Network channel

Set frequency via environment variable before deploy scripts run:

```bash
RPV_FREQ=2437   # 2.4 GHz ch6 (default, good range)
RPV_FREQ=2412   # 2.4 GHz ch1
RPV_FREQ=2462   # 2.4 GHz ch11 (max power in US)
RPV_FREQ=5805   # 5 GHz (less interference, shorter range)
```

## Hardware

| Component | Camera (sender) | Ground (receiver) |
|-----------|----------------|-------------------|
| Board | Raspberry Pi 5 | Raspberry Pi 5 |
| Camera | Raspberry Pi HQ (IMX477, CSI) | — |
| Display | — | HDMI monitor |
| WiFi | RTL8821AU USB adapter (wlan1) | RTL8821AU USB adapter (wlan1) |

## Project Structure

```
├── rpv-cam/src/
│   ├── main.rs          # Entry point, RX dispatcher, telemetry, heartbeat
│   ├── video_tx.rs      # ffmpeg capture, NAL fragmentation, RS FEC encode
│   ├── fc.rs            # MAVLink serial link (RC override, telemetry parsing)
│   ├── rawsock.rs       # AF_PACKET socket, 802.11 frame build/parse, RSSI
│   ├── link.rs          # L2 header encode/decode (shared with ground)
│   └── config.rs        # TOML config with serde defaults
│
├── rpv-ground/src/
│   ├── main.rs          # egui + wgpu GPU YUV→RGB shader, OSD
│   ├── video/
│   │   ├── receiver.rs  # RS FEC reassembly, NAL reassembly, stall detection
│   │   └── decoder.rs   # libavcodec H.264 decode, NV12 output
│   ├── rc/
│   │   └── joystick.rs  # RC transmitter (50Hz, deadline scheduling)
│   ├── telemetry.rs     # JSON telemetry receiver, link state integration
│   ├── link_state.rs    # Atomic link state machine (Searching/Connected/SignalLost/NoCamera)
│   ├── rawsock.rs       # AF_PACKET socket (same as camera)
│   ├── link.rs          # L2 header (same as camera)
│   └── config.rs        # TOML config
│
├── deploy/
│   ├── install-cam.sh       # Installs systemd service + network scripts
│   ├── install-ground.sh    # Installs desktop autostart + network scripts
│   ├── cam/                 # rpv-cam.service, net scripts
│   └── ground/              # rpv-ground.desktop, rpv-ground.service, net scripts
│
├── Cargo.toml           # Workspace root
└── .github/workflows/ci.yml
```

## Build

Requires FFmpeg development libraries:

```bash
# Debian/Ubuntu
sudo apt install libavcodec-dev libavutil-dev libswscale-dev

cargo build --release -p rpv-ground
cargo build --release -p rpv-cam
```

Binaries: `target/release/rpv-ground`, `target/release/rpv-cam`.

## Deploy

```bash
# On camera Pi:
sudo deploy/install-cam.sh

# On ground Pi:
sudo deploy/install-ground.sh
```

The camera script installs `rpv-cam.service` as a systemd service. The ground script installs a desktop autostart entry that launches `rpv-ground` on login.

Both deploy scripts copy network setup/teardown scripts that configure the RTL8821AU adapter in monitor mode on the configured channel.

## Systemd

### Camera service

```
rpv-cam.service
  ExecStartPre: rpv-net-setup-pre.sh (monitor mode, freq, txpower)
  ExecStart:    rpv-cam
  ExecStopPost: rpv-net-teardown.sh (restore managed mode)
  Restart:      on-failure, 5s delay
  Scheduling:   SCHED_FIFO @ priority 50
```

### Ground service

```
rpv-ground.service
  ExecStartPre: rpv-net-setup-pre.sh
  ExecStart:    rpv-ground
  ExecStopPost: rpv-net-teardown.sh
  Restart:      always, 3s delay
  Scheduling:   SCHED_FIFO @ priority 50
```

## Link State Machine

The ground station tracks link status through a centralized atomic state machine:

```
                    ┌──────────┐
          startup   │          │  camera_ok=false
          ────────→ │ SEARCHING │ ──────────────→ NO_CAMERA
                    │          │ ←─────────────   (from telem)
                    └────┬─────┘  camera_ok=true
                         │
              heartbeat  │  video / telemetry
              restored   │  activity
                         ↓
                    ┌──────────┐
                    │ CONNECTED│
                    └────┬─────┘
                         │
              heartbeat  │
              timeout    │
                         ↓
                    ┌──────────┐
                    │SIGNAL_LOST│ ← only heartbeat can restore
                    └──────────┘
```

Precedence: heartbeat > telemetry > video. Only heartbeat transitions can override SignalLost, preventing races where stale telemetry/video masks a real disconnect.

## Fuzzing

Fuzz testing for critical parsers (L2, radiotap, NAL finder) is set up in the `fuzz/` directory using `cargo-fuzz`.

**Requirements:** Nightly Rust toolchain (`rustup install nightly`)

**Run fuzz targets:**
```bash
cargo +nightly fuzz run l2_parse
cargo +nightly fuzz run radiotap_parse
cargo +nightly fuzz run nal_finder
```

**Targets:**
- `l2_parse` - Fuzzes `L2Header::decode()` in rpv-proto
- `radiotap_parse` - Fuzzes `parse_radiotap_rssi()` and `strip_radiotap()` in rpv-proto
- `nal_finder` - Fuzzes `find_start_code()` in rpv-cam

## License

MIT
