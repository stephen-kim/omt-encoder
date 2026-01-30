# OMT Encoder (LCD-show + omtcapture) for Raspberry Pi 5

이 리포지토리는 Raspberry Pi 5에서 USB 캡처 카드 영상을 OMT(Open Media Transport)로 송출하고,
HDMI 입력 오디오 + TRS 아날로그 입력을 믹싱하여 OMT 오디오로 함께 전송하도록 확장한 포크입니다.
LCD-show를 자동 설치하고 SPI/HDMI 화면 미리보기까지 지원합니다.

테스트 환경:
- Raspberry Pi 5
- 캡처 카드: Cam Link 4K (HDMI 입력)
- USB Audio Device (TRS 아날로그 입력)
- LCD-show: `LCD35-show`

## 빠른 빌드 + 서비스 등록 (스크립트)

사전 요구사항을 모두 설치한 뒤, 아래 스크립트 하나로 빌드와 서비스 등록까지 진행합니다.

```bash
cd ~/cpm-omt-encode
chmod +x build_and_install_service.sh
./build_and_install_service.sh
```

스크립트는 `apt update`, 필수 패키지 설치, dotnet 8 설치까지 수행한 뒤
`/opt/omtcapture`로 설치하고 `omtcapture` systemd 서비스를 활성화합니다.

LCD-show 보드가 다른 경우:

```bash
LCD_DRIVER=LCD7C-show ./build_and_install_service.sh
```

LCD 설치를 건너뛰려면:

```bash
SKIP_LCD=1 ./build_and_install_service.sh
```

LCD 설치를 다시 실행하려면:

```bash
sudo rm -f /var/lib/omt-encode/lcd_installed
```

HDMI 해상도를 강제로 480x320으로 고정하고 싶다면:

```bash
LCD_HDMI_FORCE=1 ./build_and_install_service.sh
```

> LCD-show 설치는 재부팅이 발생할 수 있습니다. 재부팅 후 스크립트를 다시 실행하세요.

## 요구사항

- Raspberry Pi 5 (기본 OS)
- USB 3.0 UVC 캡처 장치 (UYVY, YUY2, NV12 지원)
- dotnet 8
- clang
- git
- ffmpeg
- alsa-utils (arecord/aplay)
- libasound2
- libomtnet, libvmx

## 설치 및 빌드 (수동)

1. 패키지 목록 업데이트

```bash
sudo apt update
```

2. dotnet 8 설치

```bash
curl -sSL https://dot.net/v1/dotnet-install.sh | bash /dev/stdin --channel 8.0

echo 'export DOTNET_ROOT=$HOME/.dotnet' >> ~/.bashrc
echo 'export PATH=$PATH:$HOME/.dotnet' >> ~/.bashrc
source ~/.bashrc
```

3. clang + ffmpeg + ALSA 유틸 설치

```bash
sudo apt install clang ffmpeg alsa-utils libasound2
```

4. 소스 코드 구조

```
~/omt-encode/libvmx
~/omt-encode/libomtnet
~/omt-encode/omtcapture
~/omt-encode/LCD-show
```

5. libvmx 빌드

```bash
cd ~/cpm-omt-encode/libvmx/build
chmod 755 buildlinuxarm64.sh
./buildlinuxarm64.sh
```

6. libomtnet 빌드

```bash
cd ~/cpm-omt-encode/libomtnet/build
chmod 755 buildall.sh
./buildall.sh
```

7. omtcapture 빌드

```bash
cd ~/cpm-omt-encode/omtcapture/build
chmod 755 buildlinuxarm64.sh
./buildlinuxarm64.sh
```

8. LCD-show 설치 (LCD35 기준)

```bash
cd ~/cpm-omt-encode/LCD-show
sudo ./LCD35-show
```

9. 실행

```bash
~/cpm-omt-encode/omtcapture/build/arm64/omtcapture
```

## 서비스로 등록 (선택)

```bash
sudo mkdir /opt/omtcapture
sudo cp ~/cpm-omt-encode/omtcapture/build/arm64/* /opt/omtcapture/
sudo cp ~/cpm-omt-encode/omtcapture/omtcapture.service /etc/systemd/system/

sudo systemctl daemon-reload
sudo systemctl enable omtcapture
sudo systemctl start omtcapture
sudo systemctl status omtcapture
```

## 웹 설정 UI (포트 8080)

웹 UI 접속:

```
http://<pi-ip>:8080/
```

설정 가능 항목:
- 오디오 입력 선택 (HDMI / TRS / 둘 다 / 끔)
- 모니터링 출력 장치 선택 (aplay -D)
- SPI/HDMI 프리뷰 출력 on/off 및 framebuffer 선택 (복수 선택 가능)
- 영상 입력 장치/해상도/프레임레이트/코덱 변경
- 장치 목록(arecord/aplay, /dev/video*, /dev/fb*) 확인

영상 설정은 즉시 적용되며 캡처 파이프라인만 자동 재시작됩니다.
웹 포트 변경은 서비스 재시작이 필요합니다.

## 오디오 믹싱/모니터링

기본 설정은 HDMI 입력 + TRS 입력을 합성하여 OMT 오디오를 생성합니다.
모니터 출력이 켜져 있으면 같은 믹스가 지정된 출력 장치로 재생됩니다.

기본 장치 매핑 예시:
- HDMI 입력 (Cam Link 4K): `hw:3,0`
- TRS 입력 (USB Audio Device): `hw:2,0`

장치 목록 확인:

```bash
arecord -l
aplay -l
```

## 프리뷰 출력 (SPI/HDMI)

ffmpeg를 이용해 `/dev/fb*` framebuffer로 프리뷰를 출력합니다.
LCD 쇼가 사용하는 framebuffer 해상도에 맞게 웹 UI에서 preview 설정을 맞춰주세요.

HDMI에서 OS UI와 충돌을 피하려면 부팅을 콘솔 모드로 바꾸는 것을 권장합니다.

```bash
sudo raspi-config
```

`1 System Options` → `S5 Boot / Auto Login` → `B1 Console` (또는 `B2 Console Autologin`)

## 설정 파일

`config.xml`은 실행 시 자동 생성되며 웹 UI에서만 관리됩니다.
직접 편집할 필요는 없습니다.

- 실행 시: `~/cpm-omt-encode/omtcapture/build/arm64/config.xml`
- 서비스 실행 시: `/opt/omtcapture/config.xml`

## 문제 해결

- 소리가 안 나면 `libasound2` 설치 여부 확인
- 장치 인식이 안 되면 `arecord -l` / `aplay -l` 출력 확인
- 프리뷰가 안 나오면 `/dev/fb0` 존재 여부 및 해상도 확인
