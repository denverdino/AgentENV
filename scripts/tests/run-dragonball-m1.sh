#!/usr/bin/env bash
# Qualified M1 guests reuse the isolated, capability-delegating M0 wrapper.
# Cargo and test processes remain non-root; no service or existing state is changed.
set -euo pipefail
[[ $(id -u) != 0 ]] || { echo 'M1 qualification must run as a non-root user' >&2; exit 1; }
[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || {
    echo 'M1 qualification requires x86_64 Linux' >&2; exit 1;
}
[[ -n ${AENV_CONFIG_PATH:-} && -r "$AENV_CONFIG_PATH" ]] || {
    echo 'M1 qualification requires an explicit readable AENV_CONFIG_PATH' >&2; exit 1;
}
[[ -n ${AENV_DRAGONBALL_M0_ASSETS:-} && -s "$AENV_DRAGONBALL_M0_ASSETS" ]] || {
    echo 'M1 qualification requires prepared local kernel/tools/image/runner assets' >&2; exit 1;
}
for device in /dev/kvm /dev/ublk-control; do
    [[ -r "$device" && -w "$device" ]] || { echo "M1 qualification cannot access $device" >&2; exit 1; }
done
for command in cc debugfs ip nsenter unshare; do
    command -v "$command" >/dev/null || { echo "M1 qualification requires $command" >&2; exit 1; }
done
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
exec bash "$repo_root/scripts/tests/run-dragonball-m0.sh" "$@"
