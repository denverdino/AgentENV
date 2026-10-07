#!/usr/bin/env bash
# Run on the provisioned Linux host with actual DB/FC binaries, without VMs.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
runner="$root/scripts/tests/run-dragonball-tests.sh"
reject_missing() {
    if env -u "$1" bash "$runner" --platform --check > "$work/output" 2>&1; then
        echo "accepted missing $1" >&2; exit 1
    fi
    grep -F "$1" "$work/output" || { cat "$work/output"; exit 1; }
}
reject_missing AENV_M3_S3_SERVER
reject_missing AENV_M3_REGISTRY_SERVER
reject_missing AENV_TEST_STATE_DIR
reject_missing AENV_CONFIG_PATH
reject_missing AENV_DRAGONBALL_M0_ASSETS
if AENV_DRAGONBALL_SERVER="${AENV_M3_FC_SERVER:?actual FC/both server}" bash "$runner" --platform --check > "$work/output" 2>&1; then
    echo 'accepted Firecracker product for M3' >&2; exit 1
fi
grep -F 'DB-only' "$work/output"
bash "$runner" --platform --check
# A sudo implementation may delete TMPDIR even with -E. The actual namespace
# command must receive the owned disk path, and the outer wrapper must remove it.
bash "$runner" --platform bash -c '
    [[ ${TMPDIR:-} == /var/tmp/db-platform-fixtures.* ]]
    touch "$TMPDIR/probe"
    printf "fixture=%s\n" "$TMPDIR"
' > "$work/namespace"
fixture=$(sed -n 's/^fixture=//p' "$work/namespace")
[[ "$fixture" == /var/tmp/db-platform-fixtures.* && ! -e "$fixture" ]]
echo 'PASS: M3 rejects missing fixtures/state/config/assets and FC product'
