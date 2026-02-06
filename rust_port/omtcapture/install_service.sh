#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${INSTALL_DIR:-/opt/omtcapture-rs}"

echo "Building Rust omtcapture (release)..."
cd "$ROOT_DIR"
cargo build --release

if systemctl is-active --quiet omtcapture-rs; then
  echo "Stopping existing omtcapture-rs service..."
  sudo systemctl stop omtcapture-rs
fi

echo "Installing to $INSTALL_DIR"
sudo mkdir -p "$INSTALL_DIR"
sudo cp "$ROOT_DIR/target/release/omtcapture" "$INSTALL_DIR/"

if [[ ! -f "$INSTALL_DIR/config.json" ]]; then
  sudo cp "$ROOT_DIR/omtcapture/config.json" "$INSTALL_DIR/"
fi

sudo cp "$ROOT_DIR/omtcapture/omtcapture-rs.service" /etc/systemd/system/omtcapture-rs.service
sudo systemctl daemon-reload
sudo systemctl enable omtcapture-rs
sudo systemctl restart omtcapture-rs

cat <<MESSAGE

Install complete.
- Binary: $INSTALL_DIR/omtcapture
- Config: $INSTALL_DIR/config.json
- Service: sudo systemctl status omtcapture-rs
MESSAGE
