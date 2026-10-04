#!/usr/bin/env bash
# Explicit platform qualification; missing prerequisites never mean "skipped".
set -euo pipefail
[[ $(id -u) != 0 ]] || { echo 'M3 qualification must run as a non-root user' >&2; exit 1; }
[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || {
    echo 'M3 qualification requires x86_64 Linux' >&2; exit 1;
}
for name in AENV_M3_S3_SERVER AENV_M3_REGISTRY_SERVER AENV_DRAGONBALL_SERVER AENV_UBLK_DAEMON_BINARY_PATH; do
    [[ -n ${!name:-} && -x ${!name} ]] || { echo "M3 requires executable $name" >&2; exit 1; }
done
for name in AENV_TEST_STATE_DIR AENV_TEST_DEPS_PATH; do
    [[ -n ${!name:-} && ${!name} == /* && -d ${!name} && -w ${!name} ]] || {
        echo "M3 requires explicit writable directory $name" >&2; exit 1;
    }
done
for name in AENV_CONFIG_PATH AENV_DRAGONBALL_M0_ASSETS; do
    [[ -n ${!name:-} && -s ${!name} && -r ${!name} ]] || { echo "M3 requires readable $name" >&2; exit 1; }
done
for command in jq sha256sum openssl htpasswd cc debugfs ip nsenter unshare; do
    command -v "$command" >/dev/null || { echo "M3 requires $command" >&2; exit 1; }
done
for device in /dev/kvm /dev/ublk-control; do
    [[ -r "$device" && -w "$device" ]] || { echo "M3 cannot access $device" >&2; exit 1; }
done
"$AENV_DRAGONBALL_SERVER" --component-info-json | jq -e '
    .backend == "dragonball" and .dragonball == true and .firecracker == false
' >/dev/null || { echo 'M3 requires a DB-only server' >&2; exit 1; }
if [[ ${1:-} == --check ]]; then
    echo 'M3 prerequisites ready (preflight only, not VM qualification)'
    exit 0
fi
[[ $# -gt 0 ]] || { echo "usage: $0 <test binary> [arguments...]" >&2; exit 2; }
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
sha256sum "$AENV_DRAGONBALL_SERVER" "$AENV_UBLK_DAEMON_BINARY_PATH" \
    "$AENV_M3_S3_SERVER" "$AENV_M3_REGISTRY_SERVER" "$AENV_CONFIG_PATH" "$AENV_DRAGONBALL_M0_ASSETS"
# Short disk-backed paths avoid UNIX socket limits and large /tmp tmpfs leaks.
# Cleanup is outside the disposable mount namespace: its bind mounts are gone.
fixture_root=$(mktemp -d /var/tmp/db-m3-fixtures.XXXXXXXX)
trap 'rm -rf --one-file-system -- "$fixture_root"' EXIT
export TMPDIR="$fixture_root"
export AENV_HOME_PATH="$AENV_TEST_STATE_DIR/home"
export AENV_RUNTIME_PATH="$AENV_TEST_STATE_DIR/run"
export AENV_DEPS_PATH="$AENV_TEST_DEPS_PATH"
bash "$root/scripts/tests/run-dragonball-m1.sh" env "TMPDIR=$fixture_root" "$@"
