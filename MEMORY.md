# Memory — omt-encoder

## Architecture
- Video pipeline: V4L2 capture → VMX encode (NEON SIMD) → broadcast to OMT clients
- Audio pipeline: ALSA capture (HDMI + TRS dual input) → mix → broadcast
- Server: OMTServer from libomtnet handles per-client connections, subscriptions, codec/quality negotiation
- Web: Axum REST API with embedded dark theme HTML UI
- Preview: ffmpeg CLI for JPEG snapshot generation (SPI LCD + web preview)
- Discovery: avahi-publish-service for mDNS _omt._tcp

## Key behaviors
- Per-client codec negotiation: client sends `<OMTSettings Quality="..." Codec="H265,H264,VMX1" />`
- Server picks best codec from client's priority list that encoder supports
- Quality hint adjusts VMX profile (LQ/SQ/HQ) at runtime without reconnect
- Encoder can detect HW encoders (h264_rkmpp, hevc_nvenc, etc.) for future H.264/H.265 support

## Key relationships
- Crate was renamed: omtcapture → omtencoder
- Service: omtencoder (was omtcapture-rs)
- Install dir: /opt/omtencoder
- Legacy C# cleanup code kept in build script for migration
