#!/usr/bin/env bash
# Use the normal capability runner, then isolate its complete network/mount view.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ ${1:-} != --inside ]]; then
    export AENV_DRAGONBALL_M0_PARENT_NETNS="$(readlink /proc/self/ns/net)"
    exec "$repo_root/scripts/run-with-capabilities.sh" \
        unshare --net --mount --propagation private bash "$0" --inside "$@"
fi
shift
[[ "$(readlink /proc/self/ns/net)" != "$AENV_DRAGONBALL_M0_PARENT_NETNS" ]]
# sysfs network entries are tied to the namespace at mount time. Keep the
# manager's /sys/class/net conflict scan inside the disposable namespace too.
mount -t sysfs -o ro,nosuid,nodev,noexec sysfs /sys
ip link set lo up
sysctl -q -w net.ipv4.ip_forward=1
export AENV_DRAGONBALL_M0_ISOLATED=1
exec "$@"
