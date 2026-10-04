#!/usr/bin/env bash
set -euo pipefail
bundle=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$bundle"
sha256sum --strict --check SHA256SUMS
jq -e '.schema_version == 1 and .candidate == true and .backend == "firecracker" and .platform == "linux/amd64"' manifest.json >/dev/null
test ! -e bin/aenv-dragonball && test ! -e bin/reply-fault
test "$(find bin -type f | wc -l)" -eq 3
test "$(bin/server --component-info-json | jq -cS .)" = "$(jq -cS .node manifest.json)"
version=$(bin/firecracker --version)
test "${version%%$'\n'*}" = "Firecracker v$(jq -r .firecracker_version manifest.json)"
