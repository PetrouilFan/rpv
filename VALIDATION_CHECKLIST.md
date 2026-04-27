# Pre-Flight Checklist — RPV Video & FC Validation

## Config Verification

- [ ] **Pi config** (`~/.config/rpv/cam.toml`):
  - `drone_id = 0` (matches ground)
  - `camera_type = "csi"` for live, or test mode via `RPV_TEST_VIDEO`
  - `interface = "wlan1"` (external WiFi adapter for AP)
  - `transport = "udp"`

- [ ] **Ground config** (`~/.config/rpv/ground.toml`):
  - `drone_id = 0`
  - `interface = "wlp3s0f0u1"` (external USB WiFi adapter)
  - `transport = "udp"`
  - `peer_addr = "10.42.0.1:9001"` (Pi AP IP)

## Binary Build

- [ ] Camera: `cargo build --release -p rpv-cam`
- [ ] Ground: `cargo build --release -p rpv-ground`
- [ ] Binaries are current (`ls -l target/release/rpv-*` shows recent timestamps)

## Network Setup

**On Pi (camera)**:
```bash
sudo /usr/local/bin/rpv-net-setup-pre.sh
ip addr show wlan1   # → should show 10.42.0.1/24
```

**On ground (PC)**:
```bash
sudo /home/petrouil/Projects/github/rpv/deploy/ground/rpv-net-setup-pre.sh
ip addr show wlp3s0f0u1   # → should show 10.42.0.2/24
```

- [ ] Both interfaces UP and have correct IPs
- [ ] `ping 10.42.0.1` from ground succeeds
- [ ] `ping 10.42.0.2` from Pi succeeds

## Test Sequence

### A. Loopback (ground only, no WiFi)

```bash
RPV_TEST_VIDEO=/tmp/test-640x480.h264 \
  target/release/rpv-ground --transport udp --peer-addr 127.0.0.1:9001 --drone-id 0
```

In another terminal:
```bash
cat /tmp/test-640x480.h264 | \
  python3 -c "import sys,socket; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind(('127.0.0.1',9001)); [s.sendto(sys.stdin.buffer.read(1400),('127.0.0.1',9001)) for _ in iter(int,1) if sys.stdin.buffer.read(1400)]"
```

- [ ] UI opens, shows test pattern
- [ ] Logs show: `Decoded 1 frames`, then increasing
- [ ] No `no frame!`, `parse error`, or stall warnings

### B. WiFi — Test Video Mode

**Terminal 1 (Pi)**:
```bash
RPV_TEST_VIDEO=/tmp/test-640x480.h264 \
  /home/petrouil/rpv/target/release/rpv-cam \
  --camera-type csi --interface wlan1 --drone-id 0 --transport udp \
  2>&1 | tee /tmp/rpv-cam-test.log
```

**Wait 2 s**, then **Terminal 2 (ground)**:
```bash
target/release/rpv-ground \
  --transport udp --peer-addr 10.42.0.1:9001 --drone-id 0 --iface wlp3s0f0u1 \
  2>&1 | tee /tmp/ground_wifi.log
```

- [ ] Ground log: `Discovery: peer 10.42.0.1:9001 confirmed` within 2 s
- [ ] Camera log: `SEND shard[0]` lines appear
- [ ] Ground log: `Decoded X frames` increments steadily
- [ ] No `RS: block XX stalled` or `Video frame channel closed`
- [ ] UI smooth, no freezing

### C. WiFi — Live Camera

**Terminal 1 (Pi)**:
```bash
pkill -f rpv-cam
/home/petrouil/rpv/target/release/rpv-cam \
  --camera-type csi --interface wlan1 --drone-id 0 --transport udp \
  --rpicam-options "-b 1000000 -fps 30" \
  2>&1 | tee /tmp/rpv-cam-live.log
```

**Terminal 2 (ground)**:
```bash
pkill -f rpv-ground
target/release/rpv-ground \
  --transport udp --peer-addr 10.42.0.1:9001 --drone-id 0 --iface wlp3s0f0u1 \
  2>&1 | tee /tmp/ground_live.log
```

- [ ] Live video appears in ground UI
- [ ] Frame counter in UI shows 20–30 fps
- [ ] No "no frame!" errors in ground log
- [ ] RSSI value shows (e.g., `SIG: -65 dBm (good)`)
- [ ] Telemetry (GPS, battery) updates if FC connected

### D. Load Test (telemetry burst)

- [ ] With live link running, rapidly move RC sticks on ground
- [ ] Observe camera log: HP drain should be bounded (no long `try_recv` drains blocking shard 0)
- [ ] Video should not stutter or freeze during RC bursts
- [ ] If video hiccups, consider lowering `max_drain_bytes` to 256 or `max_packets` to 1

## Verification of Specific Fixes

### 1. FEC parity transmission
In camera log (`/tmp/rpv-cam-live.log`), for first 5 blocks:
```
SEND shard[4]: ...   # parity shard exists
SEND shard[5]: ...   # parity shard exists
```
If parity shards missing, FEC encode loop broken.

### 2. NAL reassembly persistence
In ground log, look for multi-fragment NAL types:
```
NAL: seq=XX, frag_type=0x01, NAL_type=7 (SPS)
NAL: seq=XX, frag_type=0x02, NAL_type=7
NAL: seq=XX, frag_type=0x03, NAL_type=7
```
And no "incomplete multi-frag NAL interrupted" warnings after initial warm-up.

### 3. Annex-B start codes preserved
No decoder errors like `no frame!` after first 10 frames. First-frame glitch OK; subsequent frames clean.

### 4. FC target_system uses drone_id
If FC connected and `drone_id` set to non-1 (e.g., 2), RC should work. Previously would silently fail.

## Quick Commands for Log Inspection

```bash
# Check FEC parity present
grep "SEND shard\[4\]" /tmp/rpv-cam-live.log | head -3

# Check NAL reassembly
grep "NAL: seq=" /tmp/ground_live.log | head -20

# Check no parser errors
grep "no frame!" /tmp/ground_live.log     # should be empty
grep "stalled" /tmp/ground_live.log       # should be rare/zero

# Check HP drain caps (if logged)
grep "hp_drain" /tmp/rpv-cam-live.log     # optional: add log in code if needed
```

## Rollback

If critical failure:
```bash
# On each machine
pkill -f rpv-(cam|ground)
# Restore previous binaries from backup or rebuild old commit
```

---

## Sign-Off

- [ ] All tests passed
- [ ] Video stable under telemetry load
- [ ] FC RC overrides verified (if applicable)
- [ ] Ready for field deployment
