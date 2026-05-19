# AGENTS.md — omt-encoder

## What is this?
Rust-based OMT stream encoder for Raspberry Pi 5. Captures video (V4L2), encodes (VMX1), mixes audio (ALSA), and streams over TCP. Web UI + mDNS discovery + SPI LCD preview.

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

- If the repo was cloned without `--recurse-submodules`, run `git submodule update --init --recursive` before building.
- The systemd service is `omtencoder`; the installed binary is `/opt/omtencoder/omtencoder`.
- `build_and_install_service.sh` installs/loads Rust via rustup when needed and updates submodules before `cargo build --release -p omtencoder`.

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
- `omtencoder/src/send_coordinator.rs` — frame queuing + priority
- `omtencoder/src/web_server.rs` — Axum API
- `omtencoder/src/settings.rs` — JSON/XML config
- `omtencoder/src/discovery.rs` — mDNS publish

## Submodules
- `libomtnet/` → github.com/stephen-kim/libomtnet-rs
- `libvmx/` → github.com/stephen-kim/libvmx
