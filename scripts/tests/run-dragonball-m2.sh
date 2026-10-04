#!/usr/bin/env bash
# M2 is an explicit hardware qualification, never a successful skipped test.
set -euo pipefail
[[ $(id -u) != 0 ]] || { echo 'M2 qualification must run as a non-root user' >&2; exit 1; }
[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || {
    echo 'M2 qualification requires x86_64 Linux' >&2; exit 1;
}
[[ $(getconf PAGESIZE) == 4096 ]] || {
    echo 'M2 packed-memory ABI requires 4 KiB host pages' >&2; exit 1;
}
[[ -n ${AENV_DRAGONBALL_FAULT_RUNNER:-} && -x "$AENV_DRAGONBALL_FAULT_RUNNER" ]] || {
    echo 'M2 requires the qualification-only reply-fault example' >&2; exit 1;
}
[[ -n ${AENV_DRAGONBALL_RUNTIME_RUNNER:-} && -x "$AENV_DRAGONBALL_RUNTIME_RUNNER" ]] || {
    echo 'M2 requires an explicit production runtime runner' >&2; exit 1;
}
[[ -n ${AENV_CONFIG_PATH:-} && -r "$AENV_CONFIG_PATH" ]] || {
    echo 'M2 requires an explicit qualified kernel/tools configuration' >&2; exit 1;
}
[[ -n ${AENV_DRAGONBALL_M0_ASSETS:-} && -s "$AENV_DRAGONBALL_M0_ASSETS" ]] || {
    echo 'M2 requires prepared local guest assets' >&2; exit 1;
}
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
echo 'M2: M1 lifecycle runs FullFileOracle; M2 memory cases explicitly select SharedUblkIncremental.'
echo 'Guest DAMON/kprobe prerequisites are asserted by the startup suite, not silently skipped.'
if git -C "$repo_root" rev-parse --git-dir >/dev/null 2>&1; then
    git -C "$repo_root" rev-parse HEAD
    git -C "$repo_root" diff --stat
else
    echo "Source export: ${AENV_SOURCE_REVISION:?set source revision for an exported tree without .git}"
fi
# Include exported/uncommitted source, not just a misleading clean base SHA.
(cd "$repo_root" && find . -path ./.git -prune -o -path ./target -prune -o -type f -print0 \
    | sort -z | xargs -0 sha256sum | sha256sum)
uname -srmo
"$AENV_DRAGONBALL_RUNTIME_RUNNER" --runtime --version-json
sha256sum "$AENV_DRAGONBALL_RUNTIME_RUNNER" "$AENV_DRAGONBALL_FAULT_RUNNER" \
    "$AENV_CONFIG_PATH" "$AENV_DRAGONBALL_M0_ASSETS"
exec bash "$repo_root/scripts/tests/run-dragonball-m1.sh" "$@"
