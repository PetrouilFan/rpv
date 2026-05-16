# WiFi Adapter Status Report - RPV System

## System Information
- **System**: Arch Linux (x86_64) on Intel NUC (not Raspberry Pi)
- **Kernel**: 6.19.14-arch1-1
- **Date**: 2026-04-30

## External WiFi Adapter Detection

### AR9271 Adapter Status
✅ **DETECTED**: Qualcomm Atheros Communications AR9271 802.11n
- **USB Location**: Bus 003 Device 010
- **Driver**: ath9k_htc (loaded and active)
- **MAC Address**: 9A:DE:F2:39:11:F5
- **Interface Name**: wlp40s0f3u4
- **PHY**: phy1

### Driver Modules Loaded
```
ath9k_htc             131072  0  ← AR9271 driver
ath9k_common           53248  1 ath9k_htc
ath9k_hw              577536  2 ath9k_htc,ath9k_common
ath                    40960  3 ath9k_htc,ath9k_common,ath9k_hw
mac80211             1728512  2 ath9k_htc,ath9k_hw
cfg80211             1470464  4 ath9k_htc,ath9k_common,ath,mac80211
```

## Interface Analysis

### Current Wireless Interface
- **Name**: wlp40s0f3u4
- **Type**: WiFi (802.11)
- **State**: DORMANT (disconnected)
- **Mode**: monitor
- **Channel**: 1 (2412 MHz)
- **TX Power**: 20.00 dBm (100 mW)

### Interface Type Comparison
- **wlan0**: ❌ NOT PRESENT (no built-in WiFi on this system)
- **wlan1**: ❌ NOT PRESENT (naming convention not used)
- **wlp40s0f3u4**: ✅ PRESENT (external AR9271 adapter)

**Note**: This is an x86_64 system, not a Raspberry Pi. The interface naming follows predictable network interface naming (wlp40s0f3u4 = wireless, PCI slot 40s0f3u4) rather than the traditional wlan0/wlan1 convention.

## Supported Interface Modes
The AR9271 adapter (ath9k_htc driver) supports all required modes:
- ✅ **IBSS** (Ad-hoc)
- ✅ **managed** (Client mode)
- ✅ **AP** (Access Point mode)
- ✅ **AP/VLAN** (AP with VLAN)
- ✅ **monitor** (Monitor mode for packet injection)
- ✅ **mesh point**
- ✅ **P2P-client**
- ✅ **P2P-GO**

## Current Configuration Issues

### Issue 1: Interface in Monitor Mode
The wlp40s0f3u4 interface is currently in **monitor mode** (type: monitor). For RPV hotspot operation, it needs to be in **AP mode**.

### Issue 2: No IP Address Assigned
The interface has no IP address configured. For the RPV hotspot, it needs:
- IP: 192.168.50.1/24

### Issue 3: Not Connected to NetworkManager
NetworkManager shows the device as "disconnected" with no active connection.

## RPV Hotspot Configuration Requirements

Based on `setup-pi-hotspot.sh` and `README_RPV.md`:

### Required Configuration
1. **Interface Mode**: AP (not monitor)
2. **SSID**: rpv-link
3. **Channel**: 6 (2437 MHz)
4. **IP Address**: 192.168.50.1/24
5. **DHCP Range**: 192.168.50.100 - 192.168.50.101
6. **TX Power**: 20 dBm (100 mW)
7. **No Encryption**: RPV handles its own security

### Network Architecture
```
Ground Station (PC)                          Raspberry Pi (Camera)
 wlp40s0f3u2 (AR9271)                          wlan1 (RTL8821AU)
 192.168.50.100                               192.168.50.1
    │                                              │
    └─────────────── WiFi: rpv-link ───────────────┘
            Channel 6, No encryption
```

**Note**: On this system (ground station), wlp40s0f3u4 is the AR9271 adapter that should connect to the Pi's hotspot.

## Verification Commands

### Check Adapter Detection
```bash
lsusb | grep "0cf3:9271"              # Should show AR9271
lsmod | grep ath9k_htc                # Should show driver loaded
```

### Check Interface Status
```bash
iw dev                               # List wireless interfaces
iw dev wlp40s0f3u4 info              # Detailed interface info
ip link show wlp40s0f3u4             # Network interface status
```

### Check Supported Modes
```bash
iw phy phy1 info | grep "Supported interface modes"
```

### Check RF Kill Status
```bash
rfkill list                          # Should show "no" for both soft/hard blocked
```

## Correct Adapter Identification

✅ **CORRECT**: The AR9271 external adapter (wlp40s0f3u4) IS being used
❌ **NOT PRESENT**: No built-in WiFi adapter exists on this system

**Conclusion**: Since this is an x86_64 system (not Raspberry Pi), there is no "built-in Pi WiFi" to conflict with. The only WiFi adapter present is the external AR9271 device, which is correctly detected and using the ath9k_htc driver.

## Next Steps for RPV Operation

### On This System (Ground Station):
1. Connect to Pi's hotspot:
   ```bash
   nmcli device wifi connect rpv-link
   ```

2. Verify connection:
   ```bash
   ip addr show wlp40s0f3u4    # Should show 192.168.50.100
   ping 192.168.50.1           # Should reach Pi
   ```

3. Start ground station:
   ```bash
   ./target/release/rpv-ground
   ```

### On Raspberry Pi (10.0.0.59):
1. Run hotspot setup:
   ```bash
   sudo ./setup-pi-hotspot.sh
   ```

2. Verify hotspot is running:
   ```bash
   sudo systemctl status hostapd
   ip addr show wlan1          # Should show 192.168.50.1/24
   ```

## Summary

| Item | Status | Details |
|------|--------|---------|
| External AR9271 Adapter | ✅ Detected | Bus 003 Device 010 |
| Driver (ath9k_htc) | ✅ Loaded | Version 1.4.0 firmware |
| Interface Name | ✅ wlp40s0f3u4 | x86_64 naming convention |
| wlan0 (built-in) | ❌ Not present | No built-in WiFi on this system |
| wlan1 (external) | ❌ Not present | Uses wlp40s0f3u4 instead |
| Monitor Mode | ✅ Active | Currently in monitor mode |
| AP Mode | ❌ Not active | Needs configuration for hotspot |
| RF Kill Status | ✅ Unblocked | Both soft/hard: no |
| Supported Modes | ✅ All present | AP, managed, monitor, etc. |

**The correct WiFi adapter (AR9271) IS being used. There is no conflicting built-in adapter on this system.**
