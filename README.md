# OMT Encoder

Raspberry Pi 5에서 USB 캡처 카드 영상을 OMT(Open Media Transport)로 인코딩/송출합니다.
HDMI + TRS 오디오 믹싱, SPI/HDMI 프리뷰, 웹 UI 설정을 지원합니다.

## 빠른 시작

```bash
git clone https://github.com/stephen-kim/omt-encoder.git ~/omt-encoder
cd ~/omt-encoder
chmod +x build_and_install_service.sh
./build_and_install_service.sh
```

LCD 설치를 건너뛰려면:

```bash
SKIP_LCD=1 ./build_and_install_service.sh
```

스크립트가 의존성 설치, Rust 툴체인 설치, 빌드, systemd 서비스 등록까지 한 번에 처리합니다.

## 서비스 관리

```bash
sudo systemctl status omtcapture-rs
journalctl -u omtcapture-rs -f
```

## 웹 설정 UI

```
http://<pi-ip>:8080/
```

영상/오디오/프리뷰 설정을 실시간으로 변경할 수 있습니다.
