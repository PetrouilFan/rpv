#!/bin/bash
set -e

APP_DIR="/opt/rpv-ground"
mkdir -p "$APP_DIR"

echo "=== Installing rpv-ground service ==="

cp rpv-ground/target/aarch64-unknown-linux-gnu/release/rpv-ground "$APP_DIR/" 2>/dev/null || \
cp rpv-ground/target/release/rpv-ground "$APP_DIR/" 2>/dev/null || {
    echo "ERROR: Binary not found, run 'cargo build --release' first" >&2
    exit 1
}

cp rpv-ground/xorg.conf "$APP_DIR/"
cp rpv-ground/start.sh "$APP_DIR/"
chmod +x "$APP_DIR/start.sh"

if [ -f rpv-ground/ground.toml ]; then
    cp rpv-ground/ground.toml "$APP_DIR/"
fi

mkdir -p /etc/systemd/system
cp rpv-ground/rpv-x11.service /etc/systemd/system/
cp rpv-ground/rpv-ground.service /etc/systemd/system/

systemctl daemon-reload
systemctl enable rpv-x11.service rpv-ground.service

echo "=== Installation complete ==="
echo "Start with: sudo systemctl start rpv-ground"
echo "Check status: sudo systemctl status rpv-ground"