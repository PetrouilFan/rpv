# rpv

Rust FPV system for Raspberry Pi.

## Components

- [**rpv-ground**](rpv-ground/) — Ground station (RPi 5). Receives H.264 over UDP, Reed-Solomon 2+1 FEC reassembly, decodes with ffmpeg, displays fullscreen with egui/wgpu. OSD overlay, RC transmitter, telemetry receiver. Two binaries: `rpv-ground` (default) and `rpv-ground-rpi5`.

- [**rpv-cam**](rpv-cam/) — Camera sender (RPi Zero 2W or Pi 5 + HQ camera). Captures H.264 via rpicam-vid, fragments NALUs, Reed-Solomon 2+1 FEC encoding, streams over UDP. RC receiver, telemetry sender. Two binaries: `rpv-cam` (Zero 2W) and `rpv-cam-rpi5` (Pi 5, binds to wlan1).

## Protocol

| Port | Direction | Content |
|------|-----------|---------|
| UDP 5600 | cam → ground | H.264 video (RS 2+1 FEC, 10-byte header) |
| UDP 5601 | cam → ground | Telemetry (JSON) |
| UDP 5602 | ground → cam | RC commands |
| UDP 5603 | ground → cam | Heartbeat |

## Wire format

Each video block is a Reed-Solomon 2+1 group. Each shard packet header (10 bytes):

```
[4B seq][1B shard_idx][1B total_shards][1B data_shards][1B pad][2B shard_len]
```

Data shards carry NALU fragments prefixed with a 1-byte fragment index.

## Hardware

- **Ground**: Raspberry Pi 5 + HDMI display
- **Camera**: Raspberry Pi Zero 2W or Pi 5 + Raspberry Pi HQ camera (IMX477, CSI)
- **Network**: RTL8821AU USB adapter (wlan1), hostapd AP on cam side

## Build

```bash
cargo build --release -p rpv-ground
cargo build --release -p rpv-cam
```

## Deploy

```bash
# On camera Pi:
sudo deploy/install-cam.sh

# On ground Pi:
sudo deploy/install-ground.sh
```

## License

MIT
