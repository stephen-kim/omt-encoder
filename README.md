# OMT Encoder

Raspberry Pi 5 / Orange Pi 5 Plus에서 HDMI 캡처 영상을 OMT(Open Media Transport)로 인코딩/송출합니다.
HDMI + TRS 오디오 믹싱, SPI/HDMI 프리뷰, 웹 UI 설정을 지원합니다.

## 빠른 시작

### 권장 OS

Orange Pi 5 Plus 내장 HDMI RX를 쓰려면 HDMI RX 드라이버와 RK3588 device-tree overlay가
포함된 Orange Pi 5 Plus용 Armbian 이미지를 사용하세요. 현재 테스트한 조합은:

- `Armbian_25.8.1_Orangepi5-plus_noble_current_6.12.43_minimal.img.xz`
- 같은 archive의 `bookworm_current_6.12.43` 또는 `trixie_current_6.12.43` 계열도 같은
  커널 라인이므로 우선 후보입니다.

다운로드 archive:

```
https://armbian.lv.auroradev.org/archive/orangepi5-plus/archive/
```

일반 Ubuntu 이미지나 Orange Pi 5 Plus가 아닌 보드용 커널에서는 `/dev/video0` 자체가
HDMI RX로 나타나지 않을 수 있습니다. 이 경우 앱 설정으로는 해결할 수 없고, 맞는 커널/DTB가
있는 OS 이미지로 부팅해야 합니다.

부팅 후 HDMI RX가 잡힌 상태는 대략 이렇게 보여야 합니다:

```bash
v4l2-ctl --list-devices
# rk_hdmirx (...):
#     /dev/video0

v4l2-ctl --device /dev/video0 --info
# Device Caps: Video Capture Multiplanar
```

### 설치

```bash
git clone --recurse-submodules https://github.com/stephen-kim/omt-encoder.git ~/omt-encoder
cd ~/omt-encoder
chmod +x build_and_install_service.sh
./build_and_install_service.sh
```

스크립트가 의존성 설치, Rust 툴체인 설치, 빌드, systemd 서비스 등록까지 한 번에 처리합니다.

설치 중 Orange Pi RK3588 계열에서 HDMI RX가 아직 노출되지 않았다면 `rk3588-hdmirx.dtbo`
오버레이를 자동으로 추가하고 재부팅 필요 여부를 안내합니다. 이 자동 설정은
`/etc/default/u-boot`와 `u-boot-update`를 쓰는 Armbian 계열에서만 가능합니다.
재부팅까지 자동으로 하려면:

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
