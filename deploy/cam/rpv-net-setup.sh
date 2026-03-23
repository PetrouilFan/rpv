#!/bin/bash
ip link set wlan0 down
iw dev wlan0 set type __ap
ip link set wlan0 up
ip addr add 192.168.100.1/24 dev wlan0
hostapd -B /etc/hostapd/rpv-hostapd.conf
sleep 2
# Disable Wi-Fi power management — prevents AP from buffering packets
iw dev wlan0 set power_save off
