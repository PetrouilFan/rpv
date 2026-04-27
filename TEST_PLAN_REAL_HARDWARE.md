# RPV Real-Hardware Validation Plan

## Targets
- **Camera**: Raspberry Pi 5 (10.42.0.1) — AP mode on `wlan1`, runs `rpv-cam`
- **Ground**: PC (10.42.0.2) — station mode on `wlp3s0f0u1`, runs `rpv-ground`

## Prerequisites
- Both binaries built with `--release` and current feature flags
- Config files present:
  - Camera: `~/.config/rpv/cam.toml`
  - Ground: `~/.config/rpv/ground.toml`
- `drone_id = 0` in both configs (default)
- Test H.264 file available on ground for `RPV_TEST_VIDEO` (e.g., `/tmp/test-640x480.h264`)

---

## Step 1 — Build and Deploy Binaries

On **ground PC** (this machine):

```bash
# Build both release binaries
cargo build --release -p rpv-cam -p rpv-ground

# Copy camera binary to Pi
scp target/release/rpv-cam petrouil@10.42.0.1:/home/petrouil/rpv/target/release/
```

On **Pi 5** (camera):

```bash
# Verify binary exists and is executable
ls -l /home/petrouil/rpv/target/release/rpv-cam

# (Optional) Verify config
cat ~/.config/rpv/cam.toml
```

---

## Step 2 — Ground Station Loopback Test (sanity check)

**On ground PC** (no WiFi needed):

```bash
# Terminal 1: Start ground in loopback mode
RPV_TEST_VIDEO=/tmp/test-640x480.h264 \
  target/release/rpv-ground --transport udp --peer-addr 127.0.0.1:9001 --drone-id 0
```

In another terminal, inject test frames:

```bash
# Terminal 2: Send test video packets to loopback
cat /tmp/test-640x480.h264 | \
  python3 -c "
import sys, socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', 9001))
while True:
    data = sys.stdin.buffer.read(1400)
    if not data: break
    s.sendto(data, ('127.0.0.1', 9001))
"
```

**Expected** in ground logs:
- `VideoReceiver loop starting`
- `Decoder started`
- `Decoded 1 frames`, `Decoded 2 frames`, … increasing
- No "no frame!" or decode errors
- UI window opens showing test pattern

**Pass criteria**: ≥30 consecutive frames decoded, no errors.

---

## Step 3 — WiFi Test — Test Video Mode

### 3a — Start Camera (Pi) in Test Mode

On **Pi 5**:

```bash
# Ensure AP is up on wlan1
sudo /usr/local/bin/rpv-net-setup-pre.sh

# Wait for AP (check `ip addr show wlan1` shows 10.42.0.1/24)

# Start camera streaming test file over WiFi
RPV_TEST_VIDEO=/tmp/test-640x480.h264 \
  /home/petrouil/rpv/target/release/rpv-cam \
  --camera-type csi \
  --interface wlan1 \
  --drone-id 0 \
  --transport udp
```

**Log output to monitor**: `/tmp/rpv-cam-test.log` (or run with `2>&1 | tee`)

**Expected**:
- `Video sender ready (FEC 4+2, L2 broadcast, device=, type=csi (rpicam-vid))`
- `RX dispatcher starting`
- Periodic: `TX: block_seq=XX shard_idx=Y` and `SEND shard[...]` lines for first 5 blocks

### 3b — Start Ground in Test Mode

On **ground PC**:

```bash
# Connect to Pi's AP
sudo /home/petrouil/Projects/github/rpv/deploy/ground/rpv-net-setup-pre.sh

# Verify IP
ip addr show wlp3s0f0u1  # should show 10.42.0.2/24

# Start ground station
RPV_TEST_VIDEO=/tmp/test-640x480.h264 \
  target/release/rpv-ground \
  --transport udp \
  --peer-addr 10.42.0.1:9001 \
  --drone-id 0 \
  --iface wlp3s0f0u1
```

**Expected ground logs**:
- `Discovery: peer 10.42.0.1:9001 confirmed` (within 1–2 s)
- `VideoReceiver loop starting`
- `Decoder started`
- `Decoded X frames` every few seconds
- No `E/decoder` or `no frame!` errors

**Pass criteria**:
- Ground UI displays test pattern smoothly
- Frame counter climbs steadily (no multi-second stalls)
- No FEC recovery warnings or stall timeouts

---

## Step 4 — Live Camera Test

### 4a — Camera (Pi) — Live CSI

On **Pi 5**:

```bash
# Stop test mode; use live camera
pkill -f rpv-cam  # if still running

/home/petrouil/rpv/target/release/rpv-cam \
  --camera-type csi \
  --interface wlan1 \
  --drone-id 0 \
  --transport udp \
  --rpicam-options "-b 1000000 -fps 30" \
  2>&1 | tee /tmp/rpv-cam-live.log
```

**Expected camera logs**:
- rpicam-vid spawns with the given options
- `extract_next_nal_cursor()` NAL extraction logs (if DEBUG enabled)
- `HP drain` capped messages visible only if `tracing::info!` present (check code)
- No repeated FEC parity or shard errors

### 4b — Ground (PC) — Live

On **ground PC**:

```bash
pkill -f rpv-ground  # if still running

target/release/rpv-ground \
  --transport udp \
  --peer-addr 10.42.0.1:9001 \
  --drone-id 0 \
  --iface wlp3s0f0u1 \
  2>&1 | tee /tmp/ground_wifi.log
```

**Expected ground logs**:
- Discovery works
- VideoReceiver: `block XX` messages; occasional `fec_recovered` increments are OK
- Decoder: increasing frame count, no errors
- UI: live camera view with OSD overlay (RSSI, battery, GPS if available)

**Pass criteria**:
- Smooth video, fps indicated in UI ~20–30 fps
- No multi-second freezezes or "no frame!" messages
- No repeated stall timeout warnings

---

## Step 5 — Load/Stress Test

While video is running live:

**On ground**: Open a second terminal, watch RSSI and frame rate in UI.

**On Pi**: Generate FEC loss simulation (optional, requires packet drop injection; skip if not applicable).

**Generate telemetry burst**:
If you have a joystick/RC connected to ground, move sticks rapidly to generate RC packets. Watch camera logs for `hp_drain` caps activating (should see bounded sends, not stalls).

**Expected**: Video continues uninterrupted. Minor FEC recovery (1–2%) acceptable.

---

## Step 6 — Verify Fixes are Active

### HP drain cap verification
In camera logs during live run, look for:
```
SEND shard[0]: ...       # first shard — no HP drain
SEND shard[2]: ...       # HP drain capped here
SEND shard[4]: ...
```
If you still see `while let Ok(hp_frame) = hp.try_recv()` draining without bounds, the old code is still running.

### FC target_system verification
If FC is connected, trigger RC override (move sticks). On the FC side (e.g., via Mission Planner), verify RC Ch1–Ch4 respond. If using a non-default system ID (not 1), you should now see correct behavior. Previously it would silently ignore commands.

---

## Troubleshooting

| Symptom | Likely cause | Action |
|---|---|---|
| Ground: `no frame!` or decode errors | Start code stripping regression | Verify `decoder.rs` line 330 uses `nal_start = i` (not `i+3/i+4`) |
| Camera: FEC parity not sent | Parity loop not executed | Verify `send_fec_group_arena()` iterates `i in 0..TOTAL_SHARDS` (6) not `DATA_SHARDS` (4) |
| Receiver: blocks stall/dropped | WiFi interference or HP burst blocking | Check camera logs for unbounded HP drain; if present, code not updated |
| Discovery fails | Peer address mismatch or firewall | Check both sides `--peer-addr` and that UDP port 14550 reachable |
| UI: vertical flip | Shader Y-flip bug | Should already be fixed (d0c64f1). If flipping, check `main_rpi5.rs` UV mapping |

---

## Rollback

If either side misbehaves:
```bash
# On each machine, kill the process and rebuild with previous commit
pkill -f rpv-(cam|ground)
cargo build --release -p rpv-(cam|ground)
# Deploy previous binary from /tmp/backup/ or rebuild from known-good commit
```

---

## Post-Test Checklist

- [ ] Loopback test passed (≥30 frames decoded)
- [ ] WiFi + test video passed (smooth video, no errors)
- [ ] Live CSI camera passed (20–30 fps, no stalls)
- [ ] Telemetry/RC bursts do not interrupt video (HP drain capped)
- [ ] FC RC overrides work with configured `drone_id`
- [ ] No panic or thread-leak warnings in logs
- [ ] Both binaries built with latest fixes confirmed (`git log --oneline -1`)

---

## Notes
- HP drain caps used: max 2 packets, 512 bytes, skip i=0. Tune downward if video still jittery under heavy telemetry.
- FC `target_system` now uses `cfg.common.drone_id`. Ensure both configs match the actual FC system ID (typically 1 for default ArduPilot, but can be changed).
- If using raw 802.11 monitor mode instead of UDP, ensure radiotap parsing handles your adapter's extended present bits (already fixed in `rpv-proto`).
