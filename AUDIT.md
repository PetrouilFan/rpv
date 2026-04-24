# RPV Codebase — Complete Audit

## Definite Bugs

### `fc.rs` — MAVLink accumulator permanently stalls on a single bad byte
`fc.rs:161` — When the parse loop gets `consumed == 0` (parse error), it immediately `break`s without discarding any bytes. The next outer-loop iteration appends new serial bytes and retries from the same invalid head byte. Junk at position 0 of `acc` will defeat all parsing until enough data physically overflows past it. Fix: drain 1 byte before break.

### `fc.rs` — `acc` has no size cap
`fc.rs:91` — Under sustained serial noise the accumulator grows without bound — no `acc.truncate()`, no max length check. On a Pi 5 with limited RAM this can cause OOM.

### `fc.rs` — `write_mavlink` silently drops serial write errors
`fc.rs:282` — `let _ = port.write_all(...)`. If the FC USB disconnects mid-flight, the writer thread silently drops every subsequent RC override without triggering failsafe or logging.

### `fc.rs` — Hardcoded `target_system = 1`
`fc.rs:209,217` — `channels_to_override(..., 1)` and `zero_override(1)` hardcode system ID 1. Multi-vehicle setups or Cube autopilots with different system IDs will silently ignore all RC overrides.

### `rawsock.rs` (cam) — `setsockopt` return values ignored
`rpv-cam/src/rawsock.rs:68-97` — `SO_RCVTIMEO`, `SO_SNDBUF`, `SO_RCVBUF` calls are all unchecked. Buffer tuning may silently fail.

### `rawsock.rs` (ground) — `setsockopt` return values ignored
`rpv-ground/src/rawsock.rs:67-97` — Same issue on ground side.

### `rawsock.rs` (both) — Radiotap RSSI offset is wrong on real hardware
`rpv-cam/src/rawsock.rs:328-332`, `rpv-ground/src/rawsock.rs:365-370` — `parse_radiotap_rssi` accumulates field sizes naively but the Radiotap spec mandates each field be aligned to its natural size (TSFT=8-byte align, CHANNEL=4-byte align, etc.). Without alignment padding, every calculated byte offset after TSFT is wrong. RSSI will read garbage on virtually all real hardware.

### `rawsock.rs` (both) — Extended Radiotap present bitmasks are ignored
`rpv-cam/src/rawsock.rs:296`, `rpv-ground/src/rawsock.rs:357` — Only reads the first 4-byte `it_present` word. Radiotap bit 31 signals that another word follows. Nearly all modern mac80211 drivers (Atheros, mt76, brcmfmac) emit 2–3 present words. Field offsets are wrong whenever an extended bitmask is present.

### `rawsock.rs` (both) — LLC/SNAP 8-byte skip skips 8 bytes but LLC/SNAP is only 6
`rpv-cam/src/rawsock.rs:270`, `rpv-ground/src/rawsock.rs:333` — `&after_80211[8..]` skips 8 bytes after detecting LLC/SNAP header (`AA AA 03`). Standard LLC/SNAP is 6 bytes (DSAP, SSAP, Control, OUI[3]) or 8 bytes (with 2-byte EtherType). The code skips 8 unconditionally which is correct for 8-byte LLC+SNAP+EtherType, but the comment says "8-byte header" without explaining the EtherType, which is confusing and could break if a driver emits 6-byte LLC.

### `receiver.rs` — `check_stall` is called redundantly
`rpv-ground/src/video/receiver.rs:112,289` — `check_stall` is called both at `try_recv` Empty and at the bottom of the loop. The second call is redundant because `last_decode_time` is reset by the first call if it fires. Not a correctness bug but adds unnecessary overhead per iteration.

### `receiver.rs` — `blocks` HashMap grows unboundedly during stalls
`rpv-ground/src/video/receiver.rs:183-189` — `blocks` is pruned to retain only blocks within 256 of `max_seq`, but during stalls, future-block arrivals from out-of-order delivery accumulate. The retain only runs when new blocks arrive, so a long stall with no incoming data causes no cleanup.

### `receiver.rs` — Dedup check doesn't handle wrapping correctly
`rpv-ground/src/video/receiver.rs:174,300` — `processed.contains(&seq)` uses plain equality, but `processed.retain` uses `nb.wrapping_sub(s) < 500`. On u32 wraparound, entries near u32::MAX would fail the retain filter and not be evicted, allowing duplicates on very long flights.

### `receiver.rs` — `nal_buf` and `nal_started` are dead code
`rpv-ground/src/video/receiver.rs:101-102` — `nal_buf` and `nal_started` are allocated and passed around but nothing in `run()` ever sets `nal_started = true` or writes to `nal_buf`. They are only cleared. This is dead code left over from a refactor.

### `decoder.rs` — FFI struct layouts hardcoded to FFmpeg 6.x
`rpv-ground/src/video/decoder.rs:27-101` — `AvFrame` and `AvPacket` are manually declared `#[repr(C)]` structs with ~50 hardcoded fields. FFmpeg 7.x removed `_pkt_pts`/`_pkt_dts` and changed several field positions. Any mismatch silently corrupts all decoded frame data.

### `decoder.rs` — `av_parser_parse2` negative return not checked for errors
`rpv-ground/src/video/decoder.rs:293-310` — The return value of `av_parser_parse2` indicates bytes consumed, or negative on error. Negative values are checked (`consumed < 0`) but the code only warns and breaks from the inner while loop — it doesn't reset parser state, so the outer decode_loop continues with a potentially corrupted parser.

### `decoder.rs` — `CString::new("h264").unwrap()` inside infinite loop
`rpv-ground/src/video/decoder.rs:241` — Allocates a new CString every restart iteration. Should be hoisted outside the loop or made a static/const.

### `main_rpi5.rs` (ground) — Startup test packet poisons the VideoReceiver
`rpv-ground/src/main_rpi5.rs:855-858` — `video_payload_tx.try_send(vec![0x52, 0x50, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00])` passes the L2 magic check (0x52 0x50), has payload_type=0x01 (video), and reaches VideoReceiver::run() as a real video payload. It contains no valid FEC shard data and will corrupt block 0 state or cause a log error.

### `main_rpi5.rs` (ground) — Vertex shader produces vertically flipped video
`rpv-ground/src/main_rpi5.rs:42` — `out.uv = vec2<f32>(x * 0.5 + 0.5, y * 0.5 + 0.5)` maps clip NDC y=+1 (screen top) to UV y=1.0 (texture bottom). Since libavcodec NV12 output is top-to-bottom, this flips the image vertically. The y component should be `0.5 - y * 0.5`. If it appears correct it is only because some other path is also flipping, which is fragile.

### `main_rpi5.rs` (ground) — Present mode `Fifo` vs `Mailbox` discrepancy
`rpv-ground/src/main_rpi5.rs:931` uses `Fifo` (vsync-locked). `rpv-ground/src/main.rs:574` uses `Mailbox` (triple-buffered low-latency). Both target Pi 5. No comment explains the difference. `Fifo` adds 1-2 frames of latency.

### `main.rs` (ground) — Battery bar indistinguishable from "unknown" state
`rpv-ground/src/main.rs:323`, `rpv-ground/src/main_rpi5.rs:696` — `Telemetry.battery_pct` is `u32`. When the camera has no FC, it sends `battery_pct: 0` (unknown). The OSD renders this as a red empty bar (0% fill, red color). There's no way for the user to distinguish "battery at 0%" from "no battery data available". Should show a gray bar or "N/A" when battery_pct is 0 and no FC is connected.

### `main.rs` (cam) — `write_link_status` never writes `"disconnected"`
`rpv-cam/src/main.rs:56` — Writes `"connected"` at startup but never `"disconnected"` on exit. After crash or clean exit the status file remains `"connected"` forever.

### `main.rs` (cam) — `.join().ok()` silently swallows thread panics
`rpv-cam/src/main.rs:135-138` — Discards `JoinError`. If the video sender or telemetry thread panics, the process exits silently with no indication of what failed.

### `main_rpi5.rs` (cam) — Same join().ok() issue
`rpv-cam/src/main_rpi5.rs:131-134` — Same pattern.

### `video_tx.rs` — FEC parity computed but never sent
`rpv-cam/src/video_tx.rs:495-498` — The send loop breaks at `i >= DATA_SHARDS`, never sending parity shards. The RS encoder runs (computing parity), but parity is discarded. This is 2× encoding workload for zero FEC benefit. With DATA_SHARDS=1 and PARITY_SHARDS=1, there's no actual FEC — just sending one data shard.

### `video_tx.rs` — `intra` parameter passed but unused
`rpv-cam/src/video_tx.rs:66,98` — `run()` takes `intra: u32` but `gop_s` is always `framerate.to_string()`. The passed `intra` value (10) is logged but not used in the `-g` ffmpeg argument.

### `video_tx.rs` — `fail_count` is `u8`, saturates with no recovery
`rpv-cam/src/video_tx.rs:208` — When `send_with_buf` fails repeatedly, `fail_count` saturates at 255. No circuit breaker to trigger socket restart or signal the outer restart loop.

### `video_tx.rs` — ffmpeg stderr thread handle is dropped
`rpv-cam/src/video_tx.rs:170` — `thread::spawn(move || { ... })` spawns a stderr logger whose handle is immediately dropped. On ffmpeg restart, a new thread is spawned while the old may still block on a closed pipe. Leaks accumulate.

### `video_tx.rs` — Partial shards contain stale data
`rpv-cam/src/video_tx.rs:47-48` — `write_frag` doesn't zero the tail of a slot. If a slot is partially filled, the remainder contains stale data from the previous FEC group since slots are initialized once with zeroes and then overwritten.

### `video_tx.rs` — `last_stats` timing is self-referential
`rpv-cam/src/video_tx.rs:275-278` — `last_stats.elapsed().as_secs()` is used in the log message, then `last_stats` is set to `Instant::now()`. The elapsed time in the log is the elapsed-since-last-stats-reset, which is correct, but the log says "in {}s" which could be confusing since it's measuring a 5-second window.

### `fc.rs` — Comment `// degrees (1e-7)` is misleading
`rpv-cam/src/fc.rs:15-16` — The stored value is already in degrees (after multiplication by 1e-7). The comment implies the field holds raw integer units.

### `telemetry.rs` — `mode` is heap-allocated `String` for a closed set
`rpv-ground/src/telemetry.rs:18` — Flight modes are a small fixed set. Using `String` means allocation + comparison on every update and every frame draw.

### `joystick.rs` — RC channels use `Vec<u16>` instead of `[u16; 16]`
`rpv-ground/src/rc/joystick.rs:167,187,197,248-251` — `channels` is `Arc<Mutex<Vec<u16>>>`. The channels are always 16 elements. Using a heap-allocated Vec for a fixed-size array adds unnecessary allocation and indirection. The `channels.clone()` at line 251 clones the entire Vec on every 20ms send cycle.

---

## Performance Issues

### `main_rpi5.rs` (ground) — `tracing::info!` fires at ~30fps
`rpv-ground/src/main_rpi5.rs:465` — `process_frames` logs with `tracing::info!` on every frame batch. At 30fps this is ~2700 formatted log lines per minute with allocation on the UI thread.

### Both ground stations — 3 mutex acquisitions per frame in `draw_osd`
`rpv-ground/src/main.rs:252,297,412`, `rpv-ground/src/main_rpi5.rs:624,670,785` — `telemetry.lock()`, `rssi.lock()`, and `channels.lock()` are all taken separately inside `draw_osd` on every UI repaint. `rssi` is `Arc<Mutex<Option<i8>>>` which is extreme overhead for a scalar.

### `draw_osd` is duplicated between `main.rs` and `main_rpi5.rs`
`rpv-ground/src/main.rs:250-441`, `rpv-ground/src/main_rpi5.rs:622-813` — Over 180 lines of pixel-position UI code are copied verbatim. Every OSD bug fix must be done twice.

### `fc.rs` — `ardupilot_mode_name` allocates new `String` at 10Hz
`rpv-cam/src/fc.rs:288-318` — Every mode parse calls `String::from("STABILIZE")` etc. These 10-per-second allocations are pointless. Should return `&'static str`.

### `fc.rs` — `SyncSender<Vec<u16>>` for RC channels is heap-per-packet
`rpv-cam/src/fc.rs:51` — RC is at most 16 × u16 (32 bytes). Sending as heap-allocated `Vec<u16>` adds one malloc and free per RC packet. Use `[u16; 8]` or similar.

### `main_rpi5.rs` (ground) — High-priority channel is unbounded
`rpv-ground/src/main_rpi5.rs:78-81` — `crossbeam_channel::unbounded()` for `hp_tx`/`hp_rx`. If video stalls, telemetry/heartbeat packets accumulate without limit.

### `video_tx.rs` — `hp_rx` has no cap on drain
`rpv-cam/src/video_tx.rs:501-505` — The `while let Ok(hp_frame) = hp.try_recv()` loop drains ALL pending high-priority packets before sending each shard. If telemetry backed up, a single shard send can be delayed by hundreds of drained packets.

### `main_rpi5.rs` (ground) — Video payload channel bounded at 64
`rpv-ground/src/main_rpi5.rs:850` — The rpi5 variant uses `bounded(64)` while `main.rs` uses `bounded(1024)`. At 30fps with ~1400B shards, 64 is only ~2 frames of buffer. The 1024 variant is ~34 frames, which is excessive for real-time video.

### `receiver.rs` — `ReedSolomon::new` called on every cold start
`rpv-ground/src/video/receiver.rs:89` — RS tables are recomputed every time VideoReceiver starts. Should be created once at construction.

### `receiver.rs` — Unnecessary Vec allocations in reconstruct
`rpv-ground/src/video/receiver.rs:320,335` — Allocates `padded = vec![0u8; max_size]` per missing shard per reconstruct call, plus clones all data shards. Could reuse buffers.

### `video_tx.rs` — Per-row texture upload in slow path
`rpv-ground/src/main_rpi5.rs:288-353` — When stride != width, uploads row by row with individual `write_texture` calls. Could stage into a contiguous buffer first.

### `link.rs` (both) — `L2Header::encode` is dead code
`rpv-cam/src/link.rs:31-37`, `rpv-ground/src/link.rs:31-37` — Marked `#[allow(dead_code)]` and "legacy API". Only `encode_into` is used.

### `rawsock.rs` (ground) — `send()` is dead code
`rpv-ground/src/rawsock.rs:185` — Marked `#[allow(dead_code)]`. Only `send_with_buf` is used.

### `rawsock.rs` (ground) — `recv_strip_headers` is dead code
`rpv-ground/src/rawsock.rs:314-317` — Marked `#[allow(dead_code)]`. Only `recv_extract` is used in the actual RX dispatcher.

### `rawsock.rs` (ground) — BPF filter is a no-op
`rpv-ground/src/rawsock.rs:102-181` — `try_attach_bpf_filter` exists but is never called. The one filter defined is `ret #0xffff` (accept all), making the entire function pointless. It's also never invoked.

### `rawsock.rs` (ground) — Redundant `poll()` before `recv()`
`rpv-ground/src/rawsock.rs:230-246` — Uses `poll()` with 100ms timeout, then `recv()` with the socket's own SO_RCVTIMEO (100ms). This doubles the effective timeout to ~200ms. The cam side uses SO_RCVTIMEO alone.

### `link_state.rs` — `SeqCst` ordering everywhere
`rpv-ground/src/link_state.rs:72,78,80,88,90,98,100,108,110,117,119,126,128` — All AtomicU8 operations use `Ordering::SeqCst`. On ARM (the target hardware), SeqCst emits a full `dmb ish` barrier. `Acquire`/`Release` pairs are sufficient for a state flag.

### `joystick.rs` — channels Vec cloned every 20ms
`rpv-ground/src/rc/joystick.rs:248-251` — `locked.clone()` clones the entire 16-element Vec every 20ms (50Hz). With a fixed-size array behind the mutex this wouldn't be needed.

---

## Architecture Issues

### `link.rs` is duplicated
`rpv-cam/src/link.rs` and `rpv-ground/src/link.rs` are identical (70 lines each). Should be a shared `rpv-common` crate.

### `rawsock.rs` is partially duplicated
`rpv-cam/src/rawsock.rs` (340 lines) and `rpv-ground/src/rawsock.rs` (375 lines) share `strip_radiotap`, `ieee80211_hdr_len`, `recv_extract`, `recv_strip_headers`, `parse_radiotap_rssi`, `build_data_frame_header`, `RADIOTAP` constant, and struct layout. Only `RawSocket::recv` and `RawSocket::new` differ (cam uses SO_RCVTIMEO, ground uses poll+recv).

### `config.rs` is partially duplicated
`rpv-cam/src/config.rs` (103 lines) and `rpv-ground/src/config.rs` (66 lines) share the same load/save/config_path pattern. Could be a shared config module with platform-specific fields.

### Two entry points per crate (Pi Zero removal needed)
`rpv-cam/src/main.rs` (399 lines) and `rpv-cam/src/main_rpi5.rs` (352 lines) are near-duplicates. `rpv-ground/src/main.rs` (759 lines) and `rpv-ground/src/main_rpi5.rs` (1126 lines) are also near-duplicates. Since only Pi 5 is supported, `main.rs` (Pi Zero) should be deleted and `main_rpi5.rs` renamed to `main.rs`.

### WGSL shader embedded as Rust string literal
`rpv-ground/src/main_rpi5.rs:29-67` — 38-line shader embedded as a Rust `&str`. Should be a standalone `.wgsl` file loaded with `include_str!()` for editability, validation, and syntax highlighting.

### Mixed channel types
`fc.rs` uses `std::sync::mpsc` while everything else uses `crossbeam_channel`. Pick one for consistency. crossbeam is preferred for its select!, bounded, and try_recv capabilities.

### Deploy scripts duplicated
`deploy/cam/rpv-net-setup-pre.sh` (49 lines) and `deploy/ground/rpv-net-setup-pre.sh` (46 lines) are near-identical. `deploy/cam/rpv-net-teardown.sh` and `deploy/ground/rpv-net-teardown.sh` are identical. Should be one parametrized script.

### Two systemd service files for cam
`deploy/cam/rpv-cam.service` and `deploy/cam/rpv-cam-rpi5.service` are identical except for the binary name. After Pi Zero removal, only one is needed.

### No shared crate for protocol types
Magic bytes, header constants, payload types, MAX_PAYLOAD, L2Header are all defined independently in both `rpv-cam/src/link.rs` and `rpv-ground/src/link.rs`. A `rpv-proto` crate would eliminate this.

---

## Reliability Issues

### No thread lifecycle management
Detached `thread::spawn` is used everywhere. No join handles are stored for most threads (cam main.rs stores them but swallows errors, ground main.rs prefixes with `_` and never joins). No cancellation, restart, or fault propagation mechanism exists.

### No graceful shutdown coordination
The only shutdown signal is `running.store(false, SeqCst)`. Threads poll this flag with sleep intervals up to 500ms. There's no channel-based shutdown, no `Condvar`, and no mechanism to wait for threads to finish their current work.

### Sleep-based coordination everywhere
`fc.rs:99,108,275`, `video_tx.rs:162,397`, `main.rs:270,275`, `main_rpi5.rs:128,256`, `decoder.rs:474` — Sleep-based polling is used instead of event-driven loops. This adds latency (up to 100ms in some paths) and hides race conditions.

### FC reader has no resync mechanism
`fc.rs:161` — When `consumed == 0`, the code breaks from the inner loop but doesn't attempt to find the next valid MAVLink magic byte. On sustained noise, parsing stalls permanently.

### No watchdog for background threads
If any background thread panics (and the panic is caught by `.join().ok()`), no mechanism exists to restart it or signal the rest of the system.

### `config.rs` — `toml::from_str().unwrap_or_default()` swallows config errors
`rpv-cam/src/config.rs:81`, `rpv-ground/src/config.rs:46` — If the config file has a typo (e.g., `drone_id = "abc"`), the entire config is silently replaced with defaults. No warning is logged.

---

## Code Quality / Maintenance

### `receiver.rs` — Unused variables `nal_buf` and `nal_started`
`rpv-ground/src/video/receiver.rs:101-102` — Allocated but never written to (only cleared and passed to check_stall). Dead code.

### `link_state.rs` — `to_u8` is dead code
`rpv-ground/src/link_state.rs:19-27` — Marked `#[allow(dead_code)]` but never called. Remove or use.

### `rawsock.rs` (ground) — `try_attach_bpf_filter` is dead code
`rpv-ground/src/rawsock.rs:102-181` — 80-line function that is never called. The BPF filter it would attach is a no-op (accept all). Remove entirely.

### `rawsock.rs` (ground) — Redefines `sock_fprog` and `sock_filter`
`rpv-ground/src/rawsock.rs:136-148` — Defines its own `#[repr(C)]` structs for BPF instead of using `libc::sock_fprog` / `libc::sock_filter`. These are available in the libc crate.

### `decoder.rs` — Debug log fires on every first frame but uses wrong condition
`rpv-ground/src/video/decoder.rs:356-361` — `frame_count == 0` triggers the pixel format log, but `frame_count` is incremented AFTER the frame is sent. The log fires on the first decoded frame, which is fine, but the condition is fragile.

### `video_tx.rs` — NAL start code scanning is O(n²)
`rpv-cam/src/video_tx.rs:409` — `for i in 0..data.len().saturating_sub(3)` scans byte-by-byte for start codes. Could use `memchr` or a faster search.

### `video_tx.rs` — Comment contradicts code on FEC
`rpv-cam/src/video_tx.rs:10,496` — Constant says `PARITY_SHARDS = 1` and comment says "parity is not sent", but the RS encode IS called. Then parity is skipped in the send loop. Confusing: why encode at all?

### `fc.rs` — `PeekReader` usage is subtle
`fc.rs:116-156` — The comment at line 153-155 explains that `drop(peek)` doesn't consume the cursor because peek took `&mut cursor`. This is correct but fragile — if the mavlink library changes its PeekReader behavior, the consumed bytes calculation breaks.

### `telemetry.rs` — Unused import
`rpv-ground/src/telemetry.rs:3` — `use std::time::Instant` is imported but `last_telem_time` is declared as `let mut last_telem_time = Instant::now()` at line 68 which uses it. This is actually used. But the timeout check at line 89-91 is a no-op block (empty if body).

### `telemetry.rs` — Timeout check is dead code
`rpv-ground/src/telemetry.rs:89-91` — The timeout arm checks `last_telem_time.elapsed() > timeout` but the body is just a comment saying "No action needed". This is a no-op.

### `joystick.rs` — Hardcoded axis codes
`rpv-ground/src/rc/joystick.rs:104-107` — Uses raw hex constants `0x00`, `0x01`, `0x02`, `0x03` for axis codes. Should use `evdev::AbsoluteAxisCode` constants or at minimum document which axes these are (ABS_X, ABS_Y, ABS_Z, ABS_RZ).

### `joystick.rs` — Button codes are raw hex
`rpv-ground/src/rc/joystick.rs:119-130` — `KeyCode(0x120)` through `KeyCode(0x12b)` are raw gamepad button codes. Should use named constants like `BTN_GAMEPAD`, `BTN_A`, etc.

---

## Test Coverage Gaps

### Only `fc.rs` has tests
`rpv-cam/src/fc.rs:320-515` — Has 4 tests for MAVLink round-trip and accumulation buffer. All other modules have zero tests:
- `rawsock.rs` — No tests for radiotap parsing, header stripping, RSSI extraction
- `link.rs` — No tests for encode/decode round-trip
- `receiver.rs` — No tests for FEC reconstruction, stall detection, wrapping
- `decoder.rs` — No tests (FFI-dependent, but NV12 conversion could be tested)
- `telemetry.rs` — No tests
- `link_state.rs` — No tests for state machine transitions
- `joystick.rs` — No tests for axis_to_rc mapping

### CI doesn't run tests
`.github/workflows/ci.yml:28-32` — Only runs `cargo check` and `cargo build`. No `cargo test` step.

---

## Removal Plan (Pi Zero Only)

### Files to Delete
- `rpv-cam/src/main.rs` (399 lines, Pi Zero entry point)
- `rpv-ground/src/main.rs` (759 lines, Pi Zero entry point)
- `deploy/cam/rpv-cam.service` (Pi Zero systemd unit)

### Files to Rename
- `rpv-cam/src/main_rpi5.rs` → `rpv-cam/src/main.rs`
- `rpv-ground/src/main_rpi5.rs` → `rpv-ground/src/main.rs`
- `deploy/cam/rpv-cam-rpi5.service` → `deploy/cam/rpv-cam.service`

### `Cargo.toml` Changes
- `rpv-cam/Cargo.toml`: Remove `[[bin]]` for `rpv-cam-rpi5`, keep only `rpv-cam`
- `rpv-ground/Cargo.toml`: Remove `[[bin]]` for `rpv-ground-rpi5`, keep only `rpv-ground`

### `decoder.rs` — Remove CPU fallback
- Delete `nv12_to_rgba` (lines 179-220) — only used by the deleted Pi Zero ground main.rs

### `deploy/install-cam.sh` — Update binary name
- Change references from `rpv-cam-rpi5` to `rpv-cam`
- Remove the line copying `rpv-cam.service` (old Pi Zero service)

### Dead code after deletion to verify
- `egui::ColorImage`, `TextureHandle` imports in ground — used in deleted main.rs only, not in main_rpi5.rs
- `check_camera_available()` in cam main.rs — Pi5 variant already has its own version (line 273)
- The `libc::kill` watchdog pattern in cam main.rs — Pi5 variant doesn't have this, consider migrating
