#!/bin/bash
ip link set wlan0 down
iw dev wlan0 set type managed
ip link set wlan0 up
wpa_supplicant -B -i wlan0 -c /etc/wpa_supplicant/rpv-wpa.conf
sleep 4
ip addr add 192.168.100.2/24 dev wlan0
