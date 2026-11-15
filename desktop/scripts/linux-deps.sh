#!/bin/sh
# Installs what building the app needs on Debian/Ubuntu: WebKitGTK and the libraries Tauri links.
set -eu
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential curl file pkg-config \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev libxdo-dev
