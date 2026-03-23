#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  pi_remote_deploy.sh --host <hostname-or-ip> [options]

Options:
  --host <value>           Raspberry Pi host (required)
  --user <value>           SSH user (default: pi)
  --port <value>           SSH port (default: 22)
  --repo-path <value>      Remote repository path (default: ~/omt-encode)
  --branch <value>         Git branch to pull (default: current branch)
  --build-script <value>   Script path relative to repo (default: build_and_install_service.sh)
  --service <value>        systemd service to inspect (default: omtcapture)
  --skip-lcd               Run build with SKIP_LCD=1
  --ssh-option <value>     Extra ssh option, repeatable (example: --ssh-option StrictHostKeyChecking=no)
  --tail-lines <value>     Log lines to include in result (default: 80)
  --dry-run                Print resolved plan and exit
  -h, --help               Show help

Examples:
  pi_remote_deploy.sh --host 192.168.0.50 --repo-path ~/cpm-omt-encode --skip-lcd
  pi_remote_deploy.sh --host pi.local --build-script build_and_install_service.sh --service omtcapture-rs
EOF
}

HOST=""
USER_NAME="pi"
PORT="22"
REPO_PATH="~/omt-encode"
BRANCH=""
BUILD_SCRIPT="build_and_install_service.sh"
SERVICE_NAME="omtcapture"
SKIP_LCD="0"
TAIL_LINES="80"
DRY_RUN="0"
SSH_OPTIONS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host)
      HOST="${2:-}"
      shift 2
      ;;
    --user)
      USER_NAME="${2:-}"
      shift 2
      ;;
    --port)
      PORT="${2:-}"
      shift 2
      ;;
    --repo-path)
      REPO_PATH="${2:-}"
      shift 2
      ;;
    --branch)
      BRANCH="${2:-}"
      shift 2
      ;;
    --build-script)
      BUILD_SCRIPT="${2:-}"
      shift 2
      ;;
    --service)
      SERVICE_NAME="${2:-}"
      shift 2
      ;;
    --skip-lcd)
      SKIP_LCD="1"
      shift 1
      ;;
    --ssh-option)
      SSH_OPTIONS+=("${2:-}")
      shift 2
      ;;
    --tail-lines)
      TAIL_LINES="${2:-}"
      shift 2
      ;;
    --dry-run)
      DRY_RUN="1"
      shift 1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ -z "$HOST" ]]; then
  echo "--host is required" >&2
  usage
  exit 1
fi

if [[ ! "$PORT" =~ ^[0-9]+$ ]]; then
  echo "--port must be numeric" >&2
  exit 1
fi

if [[ ! "$TAIL_LINES" =~ ^[0-9]+$ ]]; then
  echo "--tail-lines must be numeric" >&2
  exit 1
fi

SSH_CMD=(ssh -p "$PORT")
if [[ ${#SSH_OPTIONS[@]} -gt 0 ]]; then
  for opt in "${SSH_OPTIONS[@]}"; do
    SSH_CMD+=(-o "$opt")
  done
fi
SSH_CMD+=("${USER_NAME}@${HOST}")

if [[ "$DRY_RUN" == "1" ]]; then
  cat <<EOF
Dry run:
  host: ${HOST}
  user: ${USER_NAME}
  port: ${PORT}
  repo: ${REPO_PATH}
  branch: ${BRANCH:-<current>}
  build script: ${BUILD_SCRIPT}
  service: ${SERVICE_NAME}
  skip lcd: ${SKIP_LCD}
  tail lines: ${TAIL_LINES}
EOF
  exit 0
fi

echo "Checking SSH connectivity..."
"${SSH_CMD[@]}" "echo connected >/dev/null"

echo "Running remote deploy sequence..."
"${SSH_CMD[@]}" bash -s -- "$REPO_PATH" "$BRANCH" "$BUILD_SCRIPT" "$SERVICE_NAME" "$SKIP_LCD" "$TAIL_LINES" <<'REMOTE_SCRIPT'
set -euo pipefail

REPO_PATH="${1:-}"
BRANCH="${2:-}"
BUILD_SCRIPT="${3:-build_and_install_service.sh}"
SERVICE_NAME="${4:-omtcapture}"
SKIP_LCD="${5:-0}"
TAIL_LINES="${6:-80}"

expand_path() {
  local path="$1"
  if [[ "$path" == "~/"* ]]; then
    echo "$HOME/${path#~/}"
  elif [[ "$path" == "~" ]]; then
    echo "$HOME"
  else
    echo "$path"
  fi
}

REPO_DIR="$(expand_path "$REPO_PATH")"

if [[ ! -d "$REPO_DIR/.git" ]]; then
  echo "ERROR: not a git repository: $REPO_DIR" >&2
  exit 1
fi

cd "$REPO_DIR"

echo "== System =="
uname -a || true
echo "User: $(whoami)"
echo "Repo: $REPO_DIR"

echo
echo "== Git update =="
git rev-parse --abbrev-ref HEAD
git fetch --all --prune
if [[ -n "$BRANCH" ]]; then
  git checkout "$BRANCH"
fi
git pull --ff-only
echo "HEAD: $(git rev-parse --short HEAD)"

if [[ ! -f "$BUILD_SCRIPT" ]]; then
  echo "ERROR: build script not found: $REPO_DIR/$BUILD_SCRIPT" >&2
  exit 1
fi

echo
echo "== Build and deploy =="
chmod +x "$BUILD_SCRIPT"
if [[ "$SKIP_LCD" == "1" ]]; then
  SKIP_LCD=1 "./$BUILD_SCRIPT"
else
  "./$BUILD_SCRIPT"
fi

echo
echo "== Service status =="
sudo systemctl status "$SERVICE_NAME" --no-pager || true

echo
echo "== Service logs (tail ${TAIL_LINES}) =="
journalctl -u "$SERVICE_NAME" -n "$TAIL_LINES" --no-pager || true
REMOTE_SCRIPT

echo "Remote deploy finished."
