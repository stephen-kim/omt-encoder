#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LCD_DRIVER="${LCD_DRIVER:-LCD35-show}"
SKIP_LCD="${SKIP_LCD:-0}"
INSTALL_DIR="${INSTALL_DIR:-/opt/omtcapture-rs}"
SKIP_DEPS="${SKIP_DEPS:-0}"
LCD_MARKER="/var/lib/omt-encode/lcd_installed"
CMDLINE_TWEAK="${CMDLINE_TWEAK:-1}"
CMDLINE_REMOVE_SPLASH="${CMDLINE_REMOVE_SPLASH:-0}"

CMDLINE_FILE="/boot/firmware/cmdline.txt"
if [[ ! -f "$CMDLINE_FILE" && -f /boot/cmdline.txt ]]; then
  CMDLINE_FILE="/boot/cmdline.txt"
fi

ensure_cmdline_flags() {
  if [[ "$CMDLINE_TWEAK" != "1" || ! -f "$CMDLINE_FILE" ]]; then
    return
  fi
  local cmdline
  cmdline="$(cat "$CMDLINE_FILE")"
  if [[ "$cmdline" != *"fbcon=map:0"* ]]; then
    cmdline="${cmdline} fbcon=map:0"
  fi
  if [[ "$cmdline" != *"logo.nologo"* ]]; then
    cmdline="${cmdline} logo.nologo"
  fi
  if [[ "$CMDLINE_REMOVE_SPLASH" = "1" ]]; then
    cmdline="${cmdline// splash/}"
  fi
  echo "$cmdline" | sudo tee "$CMDLINE_FILE" >/dev/null
}

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "This installer is intended for Linux (Raspberry Pi / Orange Pi)."
  exit 1
fi

# ── Clean up previous installations ──────────────────────────────────────────
echo "Cleaning up previous installations..."

# Remove legacy C# omtcapture service and files
if systemctl is-active --quiet omtcapture 2>/dev/null; then
  echo "  Stopping C# omtcapture service..."
  sudo systemctl stop omtcapture
fi
if systemctl is-enabled --quiet omtcapture 2>/dev/null; then
  echo "  Disabling C# omtcapture service..."
  sudo systemctl disable omtcapture
fi
if [[ -f /etc/systemd/system/omtcapture.service ]]; then
  echo "  Removing C# service file..."
  sudo rm -f /etc/systemd/system/omtcapture.service
  sudo systemctl daemon-reload
fi
if [[ -d /opt/omtcapture ]]; then
  echo "  Removing C# install directory (/opt/omtcapture)..."
  sudo rm -rf /opt/omtcapture
fi

# Stop existing Rust service before reinstall
if systemctl is-active --quiet omtcapture-rs 2>/dev/null; then
  echo "  Stopping existing omtcapture-rs service..."
  sudo systemctl stop omtcapture-rs
fi

# ── Install dependencies ─────────────────────────────────────────────────────
if [[ "$SKIP_DEPS" != "1" ]]; then
  echo "Installing dependencies..."
  sudo apt update
  sudo apt install -y git build-essential clang pkg-config ffmpeg alsa-utils libasound2-dev avahi-daemon avahi-utils
  sudo systemctl enable avahi-daemon >/dev/null 2>&1 || true
  sudo systemctl start avahi-daemon >/dev/null 2>&1 || true
fi

# ── LCD-show ─────────────────────────────────────────────────────────────────
if [[ "$SKIP_LCD" != "1" ]]; then
  if sudo test -f "$LCD_MARKER"; then
    echo "LCD-show already installed (marker found: $LCD_MARKER). Skipping."
  else
    if [[ -x "$ROOT_DIR/LCD-show/$LCD_DRIVER" ]]; then
      echo "Running LCD-show installer ($LCD_DRIVER). This may reboot the device."
      sudo mkdir -p "$(dirname "$LCD_MARKER")"
      sudo touch "$LCD_MARKER"
      ensure_cmdline_flags
      pushd "$ROOT_DIR/LCD-show" >/dev/null
      sudo "./$LCD_DRIVER" || true
      popd >/dev/null
      echo "LCD step complete. Reboot may happen; re-run this script to continue build steps."
      exit 0
    else
      echo "LCD driver script not found: $ROOT_DIR/LCD-show/$LCD_DRIVER"
      echo "Set LCD_DRIVER to another script name or set SKIP_LCD=1 to skip."
    fi
  fi
fi

ensure_cmdline_flags

# ── Build ────────────────────────────────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust toolchain not found. Installing via rustup..."
  export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
  export RUSTUP_INIT_SKIP_PATH_CHECK=1
  curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "Building omtcapture (release)..."
cd "$ROOT_DIR"
cargo build --release -p omtcapture

# ── Install ──────────────────────────────────────────────────────────────────
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

# ── Verify ───────────────────────────────────────────────────────────────────
echo "Running post-install checks..."
sleep 1
if ! systemctl is-active --quiet omtcapture-rs; then
  echo "ERROR: omtcapture-rs service is not active."
  sudo systemctl status omtcapture-rs --no-pager || true
  exit 1
fi

if ! sudo ss -lntp | grep -q ":6400 "; then
  echo "ERROR: port 6400 is not listening."
  sudo ss -lntp || true
  exit 1
fi

if ! sudo ss -lntp | grep -q ":8080 "; then
  echo "WARN: web UI port 8080 is not listening."
fi

if command -v avahi-browse >/dev/null 2>&1; then
  if ! timeout 3 avahi-browse -rt _omt._tcp >/tmp/omt_avahi_check.txt 2>/dev/null; then
    echo "WARN: avahi browse timed out."
  fi
  if ! grep -q "_omt._tcp" /tmp/omt_avahi_check.txt 2>/dev/null; then
    echo "WARN: _omt._tcp mDNS service not discovered yet."
  fi
fi

cat <<MESSAGE

Install complete.
- Binary: $INSTALL_DIR/omtcapture
- Config: $INSTALL_DIR/config.json
- Service: sudo systemctl status omtcapture-rs
- Logs: journalctl -u omtcapture-rs -f
MESSAGE
