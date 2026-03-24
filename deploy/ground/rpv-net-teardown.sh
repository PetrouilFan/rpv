#!/bin/bash
pkill wpa_supplicant 2>/dev/null || true
ip addr flush dev wlan1 2>/dev/null || true
