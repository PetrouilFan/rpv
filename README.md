# rpv

Rust FPV system for Raspberry Pi.

## Components

- [**rpv-ground**](rpv-ground/) — Ground station (RPi 5). Receives H.264 video over UDP, decodes with ffmpeg, displays fullscreen with egui/Vulkan. OSD overlay, RC transmitter, telemetry receiver. Runs on boot via systemd.

- [**rpv-cam**](rpv-cam/) — Camera sender (RPi Zero 2W + HQ camera). Captures H.264 via rpicam-vid, streams over UDP. RC receiver, telemetry sender. Runs on boot via systemd.

## Protocol

| Port | Direction | Content |
|------|-----------|---------|
| UDP 5600 | cam → ground | H.264 video stream |
| UDP 5601 | cam → ground | Telemetry (JSON) |
| UDP 5602 | ground → cam | RC commands |

## Hardware

- **Ground**: Raspberry Pi 5 + HDMI display
- **Camera**: Raspberry Pi Zero 2W + Raspberry Pi HQ camera (IMX477, CSI)

## Build

```bash
cargo build --release -p rpv-ground   # on ground Pi
cargo build --release -p rpv-cam      # on camera Pi
```

## License

MIT
