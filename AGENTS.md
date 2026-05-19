# AGENTS.md — omt-encoder

## What is this?
Rust-based OMT stream encoder for Raspberry Pi 5 / Orange Pi 5 Plus. Captures video (V4L2 HDMI RX or USB capture), encodes (VMX1), mixes audio (ALSA), and streams over TCP. Web UI + mDNS discovery + SPI LCD preview.

## Build
```bash
cargo check             # macOS check
cargo build --release -p omtencoder   # full build
```

## Install / deploy
```bash
git clone --recurse-submodules https://github.com/stephen-kim/omt-encoder.git ~/omt-encoder
cd ~/omt-encoder
./build_and_install_service.sh
```

- For Orange Pi 5 Plus HDMI RX, use an Orange Pi 5 Plus Armbian image with RK3588 HDMI RX kernel/DTB support. The tested image line is `Armbian_25.8.1_Orangepi5-plus_*_current_6.12.43_*` from `https://armbian.lv.auroradev.org/archive/orangepi5-plus/archive/`.
- If the repo was cloned without `--recurse-submodules`, run `git submodule update --init --recursive` before building.
- The systemd service is `omtencoder`; the installed binary is `/opt/omtencoder/omtencoder`.
- `build_and_install_service.sh` installs/loads Rust via rustup when needed and updates submodules before `cargo build --release -p omtencoder`.
- On Orange Pi RK3588 systems, the installer can add the `rk3588-hdmirx.dtbo` U-Boot overlay when HDMI RX is not visible. Set `REBOOT_AFTER_HDMIRX_CHANGE=1` to reboot automatically after changing the overlay.
- At process startup, `device_autodetect` repairs stale video/HDMI audio config entries by selecting a real V4L2 capture device and HDMI ALSA capture device when available.

## Architecture
- Video pipeline: V4L2 capture → VMX encode → broadcast to clients
- Audio pipeline: ALSA capture (HDMI + TRS) → mix → broadcast
- Server: OMTServer (libomtnet) handles client connections + subscriptions
- Web: Axum REST API + embedded HTML UI
- Discovery: avahi-publish-service for mDNS

## Key files
- `omtencoder/src/main.rs` — startup, pipeline management, settings watch
- `omtencoder/src/video_pipeline.rs` — V4L2 capture + VMX encode
- `omtencoder/src/audio_pipeline.rs` — ALSA capture + mixing
- `omtencoder/src/device_autodetect.rs` — startup V4L2/ALSA device validation + autodetect
- `omtencoder/src/send_coordinator.rs` — frame queuing + priority
- `omtencoder/src/web_server.rs` — Axum API
- `omtencoder/src/settings.rs` — JSON/XML config
- `omtencoder/src/discovery.rs` — mDNS publish

## Submodules
- `libomtnet/` → github.com/stephen-kim/libomtnet-rs
- `libvmx/` → github.com/stephen-kim/libvmx
