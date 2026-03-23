#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

git stash --quiet 2>/dev/null || true
git pull --ff-only
git stash pop --quiet 2>/dev/null || true

cargo build --release -p omtcapture

sudo systemctl stop omtcapture-rs 2>/dev/null || true
sudo cp target/release/omtcapture /opt/omtcapture-rs/
sudo systemctl start omtcapture-rs

echo "Done. Checking service..."
sleep 1
sudo systemctl status omtcapture-rs --no-pager -n 5
