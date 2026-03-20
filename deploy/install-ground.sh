#!/bin/bash
set -e
cp rpv-net-setup.sh /usr/local/bin/rpv-net-setup.sh
chmod +x /usr/local/bin/rpv-net-setup.sh
cp rpv-wpa.conf /etc/wpa_supplicant/rpv-wpa.conf
mkdir -p ~/.config/autostart
cp ground/rpv-ground.desktop ~/.config/autostart/
echo "Done. On next login, rpv-ground will auto-start in the desktop session."
