#!/bin/bash
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"

cp "$DIR/ground/rpv-net-setup-pre.sh" /usr/local/bin/rpv-net-setup-pre.sh
cp "$DIR/ground/rpv-net-teardown.sh" /usr/local/bin/rpv-net-teardown.sh
chmod +x /usr/local/bin/rpv-net-setup-pre.sh /usr/local/bin/rpv-net-teardown.sh

cp "$DIR/ground/rpv-wpa.conf" /etc/wpa_supplicant/rpv-wpa.conf
mkdir -p ~/.config/autostart
cp "$DIR/ground/rpv-ground.desktop" ~/.config/autostart/
echo "Done. On next login, rpv-ground will auto-start in the desktop session."
