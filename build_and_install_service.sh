#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${INSTALL_DIR:-/opt/omtencoder}"
SKIP_DEPS="${SKIP_DEPS:-0}"
CMDLINE_TWEAK="${CMDLINE_TWEAK:-1}"
CMDLINE_REMOVE_SPLASH="${CMDLINE_REMOVE_SPLASH:-0}"
CONFIGURE_HDMIRX="${CONFIGURE_HDMIRX:-1}"
REBOOT_AFTER_HDMIRX_CHANGE="${REBOOT_AFTER_HDMIRX_CHANGE:-0}"
HDMIRX_REBOOT_REQUIRED=0

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

has_hdmirx_capture_device() {
  if ! command -v v4l2-ctl >/dev/null 2>&1; then
    return 1
  fi
  v4l2-ctl --list-devices 2>/dev/null | grep -qiE "rk_hdmirx|hdmirx|hdmi rx"
}

is_orangepi_rk3588() {
  local model=""
  if [[ -r /proc/device-tree/model ]]; then
    model="$(tr -d '\0' </proc/device-tree/model)"
  fi
  [[ "$model" == *"Orange Pi 5 Plus"* || "$model" == *"RK3588"* || "$model" == *"rk3588"* ]]
}

ensure_orangepi_hdmirx_overlay() {
  if [[ "$CONFIGURE_HDMIRX" != "1" ]]; then
    return
  fi
  if has_hdmirx_capture_device; then
    echo "HDMI RX capture device already present."
    return
  fi
  if ! is_orangepi_rk3588; then
    return
  fi
  if [[ ! -f /etc/default/u-boot ]] || ! command -v u-boot-update >/dev/null 2>&1; then
    echo "WARN: HDMI RX is not visible, and this system does not expose Armbian U-Boot overlay config."
    echo "WARN: Use an Orange Pi 5 Plus Armbian image with RK3588 HDMI RX kernel/DTB support."
    return
  fi

  local overlay="device-tree/rockchip/overlay/rk3588-hdmirx.dtbo"
  if grep -q "rk3588-hdmirx.dtbo" /etc/default/u-boot; then
    echo "HDMI RX overlay is configured, but the device is not visible yet. A reboot may be required."
    return
  fi

  echo "Configuring Orange Pi RK3588 HDMI RX overlay..."
  sudo cp /etc/default/u-boot "/etc/default/u-boot.bak.$(date +%Y%m%d%H%M%S)"
  if grep -q '^U_BOOT_FDT_OVERLAYS=' /etc/default/u-boot; then
    sudo sed -i -E "s|^U_BOOT_FDT_OVERLAYS=\"([^\"]*)\"|U_BOOT_FDT_OVERLAYS=\"\\1 $overlay\"|" /etc/default/u-boot
    sudo sed -i -E "s|^U_BOOT_FDT_OVERLAYS=([^\"].*)|U_BOOT_FDT_OVERLAYS=\"\\1 $overlay\"|" /etc/default/u-boot
    sudo sed -i -E 's|U_BOOT_FDT_OVERLAYS=" +|U_BOOT_FDT_OVERLAYS="|' /etc/default/u-boot
  else
    echo "U_BOOT_FDT_OVERLAYS=\"$overlay\"" | sudo tee -a /etc/default/u-boot >/dev/null
  fi
  sudo u-boot-update
  HDMIRX_REBOOT_REQUIRED=1

  if [[ "$REBOOT_AFTER_HDMIRX_CHANGE" != "1" ]]; then
    echo "HDMI RX overlay was added. Reboot this device before expecting /dev/video0 HDMI input."
  fi
}

print_capture_device_summary() {
  echo "Detected V4L2 capture devices:"
  if ! command -v v4l2-ctl >/dev/null 2>&1; then
    echo "  v4l2-ctl not installed."
    return
  fi

  local found=0
  while IFS= read -r dev; do
    [[ -n "$dev" ]] || continue
    local info
    info="$(v4l2-ctl --device "$dev" --info 2>/dev/null || true)"
    if [[ "$info" == *"Device Caps"* && "$info" == *"Video Capture"* && "$info" != *"Memory-to-Memory"* && "$info" != *"Metadata Capture"* ]]; then
      local card
      card="$(printf "%s\n" "$info" | sed -n 's/^[[:space:]]*Card type[[:space:]]*:[[:space:]]*//p' | head -n1)"
      echo "  $dev ${card:+($card)}"
      found=1
    fi
  done < <(find /dev -maxdepth 1 -type c -name 'video*' | sort -V)

  if [[ "$found" = "0" ]]; then
    echo "  none"
    echo "  WARN: no usable video capture device is visible."
  fi
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
if systemctl is-active --quiet omtencoder 2>/dev/null; then
  echo "  Stopping existing omtencoder service..."
  sudo systemctl stop omtencoder
fi

# ── Install dependencies ─────────────────────────────────────────────────────
if [[ "$SKIP_DEPS" != "1" ]]; then
  echo "Installing dependencies..."
  sudo apt update
  sudo apt install -y git curl build-essential clang pkg-config ffmpeg v4l-utils alsa-utils libasound2-dev libdrm-dev libdrm-tests avahi-daemon avahi-utils gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-bad
  sudo systemctl enable avahi-daemon >/dev/null 2>&1 || true
  sudo systemctl start avahi-daemon >/dev/null 2>&1 || true
fi

ensure_orangepi_hdmirx_overlay
ensure_cmdline_flags

# ── Build ────────────────────────────────────────────────────────────────────
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust toolchain not found. Installing via rustup..."
  export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  export RUSTUP_INIT_SKIP_PATH_CHECK=1
  curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
  export PATH="$CARGO_HOME/bin:$PATH"
fi

echo "Building omtencoder (release)..."
cd "$ROOT_DIR"
echo "Updating git submodules..."
git submodule update --init --recursive
cargo build --release -p omtencoder

# ── Kernel tuning (TCP send buffers for glitch-free audio streaming) ───────
SYSCTL_CONF="/etc/sysctl.d/99-omt.conf"
echo "Applying kernel TCP tuning ($SYSCTL_CONF)..."
cat <<SYSCTL | sudo tee "$SYSCTL_CONF" >/dev/null
net.core.wmem_max = 4194304
net.core.wmem_default = 4194304
net.ipv4.tcp_wmem = 4096 262144 4194304
SYSCTL
sudo sysctl -p "$SYSCTL_CONF" >/dev/null 2>&1 || true

# ── Install ──────────────────────────────────────────────────────────────────
echo "Installing to $INSTALL_DIR"
sudo mkdir -p "$INSTALL_DIR"
sudo cp "$ROOT_DIR/target/release/omtencoder" "$INSTALL_DIR/"

if [[ ! -f "$INSTALL_DIR/config.json" ]]; then
  sudo cp "$ROOT_DIR/omtencoder/config.json" "$INSTALL_DIR/"
fi

sudo cp "$ROOT_DIR/omtencoder/omtencoder.service" /etc/systemd/system/omtencoder.service
sudo systemctl daemon-reload
sudo systemctl enable omtencoder
sudo systemctl restart omtencoder

# ── Verify ───────────────────────────────────────────────────────────────────
echo "Running post-install checks..."
sleep 1
if ! systemctl is-active --quiet omtencoder; then
  echo "ERROR: omtencoder service is not active."
  sudo systemctl status omtencoder --no-pager || true
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

print_capture_device_summary

cat <<MESSAGE

Install complete.
- Binary: $INSTALL_DIR/omtencoder
- Config: $INSTALL_DIR/config.json
- Service: sudo systemctl status omtencoder
- Logs: journalctl -u omtencoder -f
MESSAGE

if [[ "$HDMIRX_REBOOT_REQUIRED" = "1" && "$REBOOT_AFTER_HDMIRX_CHANGE" = "1" ]]; then
  echo "Rebooting to apply HDMI RX overlay..."
  sudo reboot
fi
