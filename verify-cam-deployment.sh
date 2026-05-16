#!/bin/bash
# RPV Camera Post-Deployment Verification Script
# Run on the Raspberry Pi after installation

set -e

echo "=== RPV Camera Deployment Verification ==="
echo ""

echo "1. Service status:"
sudo systemctl status rpv-cam --no-pager -l
echo ""

echo "2. Recent logs (last 30 lines):"
sudo journalctl -u rpv-cam -n 30 --no-pager
echo ""

echo "3. WiFi interfaces:"
ip link show | grep -E "wlan|wlp"
echo ""

echo "4. wlan1 status:"
ip link show wlan1 2>/dev/null || echo "wlan1 NOT FOUND"
echo ""

echo "5. USB devices (look for AR9271):"
lsusb | grep -i "0cf3:9271\|atheros\|realtek" || echo "No AR9271 detected"
echo ""

echo "6. hostapd process:"
ps aux | grep -E "[h]ostapd" || echo "hostapd not running"
echo ""

echo "7. dnsmasq process:"
ps aux | grep -E "[d]nsmasq" || echo "dnsmasq not running"
echo ""

echo "8. rpv-cam process:"
ps aux | grep -E "[r]pv-cam" || echo "rpv-cam not running"
echo ""

echo "9. Listening ports (9001 UDP, 9003 TCP):"
sudo ss -tlnp | grep -E "9001|9003" || echo "No listening ports"
echo ""

echo "10. IP address on wlan1:"
ip addr show wlan1 2>/dev/null | grep inet || echo "No IP assigned"
echo ""

echo "11. AP beacon check (scan for SSID):"
if command -v iw &>/dev/null; then
    iw dev wlan1 scan 2>/dev/null | grep -i "SSID: rpv-link" || echo "SSID not found in scan"
else
    echo "iw not installed - install with: sudo apt install iw"
fi
echo ""

echo "12. Config file locations:"
echo "  System: /etc/rpv/cam.toml"
echo "  User (petrouil): /home/petrouil/.config/rpv/cam.toml"
echo "  User (root): /root/.config/rpv/cam.toml"
echo ""
echo "Actual config being used (check process env):"
cat /proc/$(pgrep -f rpv-cam)/environ 2>/dev/null | tr '\0' '\n' | grep -i "config\|home" || echo "Cannot read process env"
echo ""

echo "13. Config content (if exists):"
for f in "/etc/rpv/cam.toml" "/root/.config/rpv/cam.toml" "/home/petrouil/.config/rpv/cam.toml"; do
    if [ -f "$f" ]; then
        echo "--- $f ---"
        cat "$f"
        echo ""
    fi
done

echo "=== End of verification ==="
