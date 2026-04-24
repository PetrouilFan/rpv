#!/bin/bash
set -e

APP_DIR="/opt/rpv-ground"
CONFIG_DIR="/root/.config/rpv"

mkdir -p "$CONFIG_DIR"
mkdir -p "/run/user/0"
chmod 755 /run/user/0

if [ ! -f "$CONFIG_DIR/ground.toml" ]; then
    if [ -f "$APP_DIR/ground.toml" ]; then
        cp "$APP_DIR/ground.toml" "$CONFIG_DIR/"
    fi
fi

exec "$APP_DIR/rpv-ground"