#!/bin/bash
set -e
cp rpv-net-setup.sh /usr/local/bin/rpv-net-setup.sh
chmod +x /usr/local/bin/rpv-net-setup.sh
cp rpv-wpa.conf /etc/wpa_supplicant/rpv-wpa.conf
cp rpv-ground.service /etc/systemd/system/rpv-ground.service
systemctl daemon-reload
systemctl enable rpv-ground.service
echo "Done. Reboot to activate."
