#!/usr/bin/env bash
# Validate a functional test profile, then delegate to the shared isolated runner.
set -euo pipefail
profile=${1:-}
case "$profile" in
    --probe|--runtime|--platform) shift ;;
    *) echo "usage: $0 --probe|--runtime|--platform <command> [arguments...]" >&2; exit 2 ;;
esac
[[ $(id -u) != 0 ]] || { echo 'Dragonball tests must run as a non-root user' >&2; exit 1; }
[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || {
    echo 'Dragonball tests require x86_64 Linux' >&2; exit 1;
}
for name in AENV_CONFIG_PATH AENV_DRAGONBALL_M0_ASSETS; do
    [[ -n ${!name:-} && -s ${!name} && -r ${!name} ]] || { echo "requires readable $name" >&2; exit 1; }
done
for device in /dev/kvm /dev/ublk-control; do
    [[ -r "$device" && -w "$device" ]] || { echo "cannot access $device" >&2; exit 1; }
done
for command in cc debugfs ip nsenter unshare; do
    command -v "$command" >/dev/null || { echo "requires $command" >&2; exit 1; }
done
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ "$profile" == --runtime ]]; then
    [[ $(getconf PAGESIZE) == 4096 ]] || { echo 'packed-memory ABI requires 4 KiB host pages' >&2; exit 1; }
    for name in AENV_DRAGONBALL_FAULT_RUNNER AENV_DRAGONBALL_RUNTIME_RUNNER; do
        [[ -n ${!name:-} && -x ${!name} ]] || { echo "requires executable $name" >&2; exit 1; }
    done
    "$AENV_DRAGONBALL_RUNTIME_RUNNER" --runtime --version-json
    sha256sum "$AENV_DRAGONBALL_RUNTIME_RUNNER" "$AENV_DRAGONBALL_FAULT_RUNNER" \
        "$AENV_CONFIG_PATH" "$AENV_DRAGONBALL_M0_ASSETS"
elif [[ "$profile" == --platform ]]; then
    for name in AENV_M3_S3_SERVER AENV_M3_REGISTRY_SERVER AENV_DRAGONBALL_SERVER AENV_UBLK_DAEMON_BINARY_PATH; do
        [[ -n ${!name:-} && -x ${!name} ]] || { echo "requires executable $name" >&2; exit 1; }
    done
    for name in AENV_TEST_STATE_DIR AENV_TEST_DEPS_PATH; do
        [[ -n ${!name:-} && ${!name} == /* && -d ${!name} && -w ${!name} ]] || {
            echo "requires explicit writable directory $name" >&2; exit 1;
        }
    done
    for command in jq sha256sum openssl htpasswd; do
        command -v "$command" >/dev/null || { echo "requires $command" >&2; exit 1; }
    done
    "$AENV_DRAGONBALL_SERVER" --component-info-json | jq -e \
        '.backend == "dragonball" and .dragonball == true and .firecracker == false' >/dev/null || {
        echo 'platform tests require a DB-only server' >&2; exit 1;
    }
fi
if [[ ${1:-} == --check ]]; then
    echo 'Dragonball prerequisites ready (preflight only, not VM qualification)'
    exit 0
fi
[[ $# -gt 0 ]] || { echo 'missing test command' >&2; exit 2; }
if [[ "$profile" == --platform ]]; then
    sha256sum "$AENV_DRAGONBALL_SERVER" "$AENV_UBLK_DAEMON_BINARY_PATH" \
        "$AENV_M3_S3_SERVER" "$AENV_M3_REGISTRY_SERVER" "$AENV_CONFIG_PATH" "$AENV_DRAGONBALL_M0_ASSETS"
    # Cleanup occurs after the disposable mount namespace exits, even on failure.
    fixture_root=$(mktemp -d /var/tmp/db-platform-fixtures.XXXXXXXX)
    trap 'rm -rf --one-file-system -- "$fixture_root"' EXIT
    export TMPDIR="$fixture_root"
    export AENV_HOME_PATH="$AENV_TEST_STATE_DIR/home"
    export AENV_RUNTIME_PATH="$AENV_TEST_STATE_DIR/run"
    export AENV_DEPS_PATH="$AENV_TEST_DEPS_PATH"
    bash "$repo_root/scripts/tests/run-dragonball-isolated.sh" env "TMPDIR=$fixture_root" "$@"
else
    exec bash "$repo_root/scripts/tests/run-dragonball-isolated.sh" "$@"
fi
