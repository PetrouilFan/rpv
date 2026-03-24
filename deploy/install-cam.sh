#!/bin/bash
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
CAMDIR="$DIR/cam"

cp "$CAMDIR/rpv-net-setup-pre.sh" /usr/local/bin/rpv-net-setup-pre.sh
cp "$CAMDIR/rpv-net-teardown.sh" /usr/local/bin/rpv-net-teardown.sh
chmod +x /usr/local/bin/rpv-net-setup-pre.sh /usr/local/bin/rpv-net-teardown.sh

cp "$CAMDIR/rpv-cam.service" /etc/systemd/system/rpv-cam.service
cp "$CAMDIR/rpv-cam-rpi5.service" /etc/systemd/system/rpv-cam-rpi5.service

systemctl daemon-reload
systemctl enable rpv-cam.service
echo "Done. Reboot to activate."
