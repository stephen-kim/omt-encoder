# OMT Encoder

Raspberry Pi 5 / Orange Pi 5 Plus에서 HDMI 캡처 영상을 OMT(Open Media Transport)로 인코딩/송출합니다.
HDMI + TRS 오디오 믹싱, SPI/HDMI 프리뷰, 웹 UI 설정을 지원합니다.

## 빠른 시작

```bash
git clone --recurse-submodules https://github.com/stephen-kim/omt-encoder.git ~/omt-encoder
cd ~/omt-encoder
chmod +x build_and_install_service.sh
./build_and_install_service.sh
```

스크립트가 의존성 설치, Rust 툴체인 설치, 빌드, systemd 서비스 등록까지 한 번에 처리합니다.

설치 중 Orange Pi RK3588 계열에서 HDMI RX가 아직 노출되지 않았다면 `rk3588-hdmirx.dtbo`
오버레이를 자동으로 추가하고 재부팅 필요 여부를 안내합니다. 재부팅까지 자동으로 하려면:

```bash
REBOOT_AFTER_HDMIRX_CHANGE=1 ./build_and_install_service.sh
```

앱은 시작할 때 현재 설정된 V4L2/ALSA 장치가 유효한지 확인하고, 사용할 수 없는 경우
HDMI RX 또는 USB 캡처 장치를 자동 선택합니다.

## 서비스 관리

```bash
sudo systemctl status omtencoder
journalctl -u omtencoder -f
```

## 웹 설정 UI

```
http://<pi-ip>:8080/
```

영상/오디오/프리뷰 설정을 실시간으로 변경할 수 있습니다.
