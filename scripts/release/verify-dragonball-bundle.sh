#!/usr/bin/env bash
set -euo pipefail
bundle=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$bundle"
sha256sum --strict --check SHA256SUMS
jq -e '.schema_version == 1 and .candidate == true and .backend == "dragonball" and .platform == "linux/amd64"' manifest.json >/dev/null
test ! -e bin/firecracker && test ! -e bin/reply-fault
test "$(find bin -type f | wc -l)" -eq 3
test "$(bin/server --component-info-json | jq -cS .)" = "$(jq -cS .node manifest.json)"
test "$(bin/aenv-dragonball --runtime --version-json | jq -cS .)" = "$(jq -cS .runner manifest.json)"
