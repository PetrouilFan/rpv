#!/bin/bash
set -e
cp rpv-net-setup.sh /usr/local/bin/rpv-net-setup.sh
chmod +x /usr/local/bin/rpv-net-setup.sh
cp rpv-hostapd.conf /etc/hostapd/rpv-hostapd.conf
cp rpv-cam.service /etc/systemd/system/rpv-cam.service
systemctl daemon-reload
systemctl enable rpv-cam.service
echo "Done. Reboot to activate."
