# Deploy Checklist

## Parameter Presets

### C# stack

- `--build-script build_and_install_service.sh`
- `--service omtcapture`

### Rust stack

- `--build-script build_and_install_service.sh`
- `--service omtcapture-rs`

## Recommended Preflight Checks

- Confirm target host is reachable over SSH.
- Confirm remote path is a valid git repository.
- Confirm user has sudo permission without interactive blocker.
- Use `--skip-lcd` unless LCD driver changes are intended.

## Interpreting Results

- `git pull --ff-only` succeeds: source update is clean.
- build script exits 0: build/install stage succeeded.
- `systemctl status <service>` active: deployment is running.
- `journalctl` output has no repeating error: runtime is stable.

## Common Failure Patterns

- Permission denied (publickey): configure SSH key or SSH options.
- git pull conflict / non-fast-forward: inspect remote branch strategy.
- Build dependency failure: run dependency install on the Pi and retry.
- Service inactive after deploy: inspect full logs with `journalctl -u <service> -e`.
