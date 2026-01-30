#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LCD_DRIVER="${LCD_DRIVER:-LCD35-show}"
SKIP_LCD="${SKIP_LCD:-0}"
INSTALL_DIR="${INSTALL_DIR:-/opt/omtcapture}"
LCD_MARKER="/var/lib/omt-encode/lcd_installed"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "This installer is intended for Raspberry Pi OS (Linux)."
  exit 1
fi

echo "Updating packages and installing dependencies..."
sudo apt update
sudo apt install -y git clang ffmpeg alsa-utils libasound2

if ! command -v dotnet >/dev/null 2>&1; then
  echo "Installing .NET 8 SDK..."
  curl -sSL https://dot.net/v1/dotnet-install.sh | bash /dev/stdin --channel 8.0
  export DOTNET_ROOT="$HOME/.dotnet"
  export PATH="$PATH:$DOTNET_ROOT"
fi

if [[ "$SKIP_LCD" != "1" ]]; then
  if sudo test -f "$LCD_MARKER"; then
    echo "LCD-show already installed (marker found: $LCD_MARKER). Skipping."
  else
    if [[ -x "$ROOT_DIR/LCD-show/$LCD_DRIVER" ]]; then
      echo "Running LCD-show installer ($LCD_DRIVER). This may reboot the device."
      sudo mkdir -p "$(dirname "$LCD_MARKER")"
      sudo touch "$LCD_MARKER"
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

echo "Building libvmx..."
cd "$ROOT_DIR/libvmx/build"
chmod 755 buildlinuxarm64.sh
./buildlinuxarm64.sh

echo "Building libomtnet..."
cd "$ROOT_DIR/libomtnet/build"
chmod 755 buildall.sh
./buildall.sh

echo "Building omtcapture..."
cd "$ROOT_DIR/omtcapture/build"
chmod 755 buildlinuxarm64.sh
./buildlinuxarm64.sh

sudo mkdir -p "$INSTALL_DIR"
sudo cp "$ROOT_DIR/omtcapture/build/arm64/"* "$INSTALL_DIR/"
sudo cp "$ROOT_DIR/omtcapture/omtcapture.service" /etc/systemd/system/omtcapture.service

sudo systemctl daemon-reload
sudo systemctl enable omtcapture

cat <<MESSAGE

Install complete.
- Config: $INSTALL_DIR/config.xml
- Service start: sudo systemctl start omtcapture
- Logs: journalctl -u omtcapture -f
MESSAGE
