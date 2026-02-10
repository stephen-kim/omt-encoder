---
name: pi-remote-deploy
description: SSH로 Raspberry Pi에 접속해 원격 저장소를 최신화(git pull)하고 빌드/설치 스크립트를 실행한 뒤 systemd 서비스 상태와 로그를 확인해 배포 결과를 보고할 때 사용한다. "라즈베리파이 배포해줘", "원격에서 pull 후 빌드/재시작 확인", "omtcapture 서비스 상태 점검" 같은 요청에서 트리거한다.
---

# Pi Remote Deploy

## Overview

Raspberry Pi 원격 배포를 반복 가능하게 수행한다.  
`scripts/pi_remote_deploy.sh`로 SSH 접속, `git pull`, 빌드/디플로이, 서비스 상태/로그 수집을 한 번에 실행한다.

## Workflow

1. 배포 대상과 배포 모드(C# 또는 Rust)를 결정한다.
2. `scripts/pi_remote_deploy.sh`를 적절한 파라미터로 실행한다.
3. 출력에서 다음을 확인해 결과를 요약 보고한다.
  - `git pull` 성공 여부 및 HEAD 커밋
  - 빌드/설치 스크립트 성공 여부
  - 대상 systemd 서비스 active 상태 여부
  - 최근 로그에 에러가 있는지

## Commands

기본(C# `omtcapture`) 배포:

```bash
skills/pi-remote-deploy/scripts/pi_remote_deploy.sh \
  --host <pi-host-or-ip> \
  --user pi \
  --repo-path ~/omt-encode \
  --build-script build_and_install_service.sh \
  --service omtcapture \
  --skip-lcd
```

Rust(`omtcapture-rs`) 배포:

```bash
skills/pi-remote-deploy/scripts/pi_remote_deploy.sh \
  --host <pi-host-or-ip> \
  --user pi \
  --repo-path ~/omt-encode \
  --build-script rust_port/build_and_install_service.sh \
  --service omtcapture-rs \
  --skip-lcd
```

브랜치 지정 배포:

```bash
skills/pi-remote-deploy/scripts/pi_remote_deploy.sh \
  --host <pi-host-or-ip> \
  --branch main
```

## Reporting Format

결과를 아래 포맷으로 짧게 보고한다.

```text
[Remote Deploy Result]
- Host:
- Repo path:
- Pulled branch/commit:
- Build/deploy:
- Service status:
- Key logs:
- Follow-up action:
```

## Troubleshooting

- SSH 인증 문제 시 키 기반 인증을 우선 사용한다.
- `sudo` 권한이 필요한 스크립트이므로 원격 사용자 권한을 확인한다.
- 상세 옵션/점검 항목은 `references/deploy-checklist.md`를 읽는다.
