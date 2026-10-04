#!/usr/bin/env bash
# Explicit local candidate only. No downloads, release uploads or default changes.
set -euo pipefail
[[ $# == 2 ]] || { echo "usage: $0 <qualified-inputs.json> <new-output.tar.gz>" >&2; exit 2; }
inputs=$(realpath "$1")
output_dir=$(cd "$(dirname "$2")" && pwd)
output="$output_dir/$(basename "$2")"
[[ ! -e "$output" && ! -L "$output" ]] || { echo "output already exists: $output" >&2; exit 1; }
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
for command in jq sha256sum tar install; do command -v "$command" >/dev/null; done
jq -e '
  (.version | type == "string" and test("^[A-Za-z0-9._+-]+$")) and
  (.source_revision | test("^[0-9a-f]{40}$")) and
  (.firecracker_version | type == "string" and test("^[A-Za-z0-9._+-]+$")) and
  (.kernel_version | type == "string" and test("^[A-Za-z0-9._+-]+$")) and
  (.tools_version | type == "string" and test("^[A-Za-z0-9._+-]+$")) and
  ([.sources.node,.sources.firecracker,.sources.kernel,.sources.tools] | all(.[]; type == "string" and length > 0))
' "$inputs" >/dev/null
work=$(mktemp -d "$output_dir/.firecracker-package.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
bundle="$work/agentenv-firecracker"
mkdir -p "$bundle/bin" "$bundle/assets" "$bundle/config" "$bundle/licenses"
for asset in server firecracker daemon kernel tools; do
    path=$(jq -er --arg key "$asset" '.assets[$key].path | select(type == "string" and startswith("/"))' "$inputs")
    expected=$(jq -er --arg key "$asset" '.assets[$key].sha256 | select(test("^[0-9a-f]{64}$"))' "$inputs")
    [[ -f "$path" && -s "$path" ]] || { echo "missing asset: $asset" >&2; exit 1; }
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    [[ "$actual" == "$expected" ]] || { echo "asset digest mismatch: $asset" >&2; exit 1; }
    mode=0644
    case "$asset" in
        server) destination=bin/server; mode=0755;;
        firecracker) destination=bin/firecracker; mode=0755;;
        daemon) destination=bin/uvm-ublk-daemon; mode=0755;;
        kernel) destination=assets/vmlinux.bin;;
        tools) destination=assets/tools.ext4;;
    esac
    if [[ "$mode" == 0755 ]]; then test -x "$path"; fi
    install -m "$mode" "$path" "$bundle/$destination"
    [[ "$(sha256sum "$bundle/$destination" | cut -d' ' -f1)" == "$expected" ]] || { echo 'asset changed while staging' >&2; exit 1; }
done
"$bundle/bin/server" --component-info-json > "$work/server.json"
jq -e '.backend == "firecracker" and .dragonball == false and .firecracker == true and .debug_assertions == false and .architecture == "x86_64" and .os == "linux"' "$work/server.json" >/dev/null
source_revision=$(jq -r .source_revision "$inputs")
node_commit=$(jq -er '.git_commit | select(test("^[0-9a-f]{7,40}$"))' "$work/server.json")
[[ "$source_revision" == "$node_commit"* ]] || { echo 'node source revision mismatch' >&2; exit 1; }
firecracker_version=$(jq -r .firecracker_version "$inputs")
"$bundle/bin/firecracker" --version > "$work/firecracker-version.txt"
[[ "$(head -n 1 "$work/firecracker-version.txt")" == "Firecracker v$firecracker_version" ]] || { echo "Firecracker version mismatch" >&2; exit 1; }
for component in node firecracker kernel tools; do
    mkdir "$bundle/licenses/$component"
    for kind in license notice; do
        path=$(jq -er --arg component "$component" --arg kind "$kind" '.licenses[$component][$kind] | select(type == "string" and startswith("/"))' "$inputs")
        [[ -f "$path" && -s "$path" ]] || { echo "missing $component $kind" >&2; exit 1; }
        destination=LICENSE
        if [[ "$kind" == notice ]]; then destination=NOTICE; fi
        install -m 0644 "$path" "$bundle/licenses/$component/$destination"
    done
done
cp "$bundle/licenses/node/LICENSE" "$bundle/LICENSE"
{
    printf '%s\n' 'AgentENV Firecracker candidate — not a published release.'
    jq -r '.sources | to_entries[] | "\(.key): \(.value)"' "$inputs"
    printf '%s\n' 'Component license and notice texts are under licenses/.'
} > "$bundle/NOTICE"
tools_version=$(jq -r .tools_version "$inputs")
kernel_version=$(jq -r .kernel_version "$inputs")
sed -e "s/@TOOLS_VERSION@/$tools_version/g" -e "s/@KERNEL_VERSION@/$kernel_version/g" "$root/config/firecracker.toml" > "$bundle/config/firecracker.toml"
install -m 0755 "$root/scripts/release/verify-firecracker-bundle.sh" "$bundle/verify.sh"
jq -n --slurpfile input "$inputs" --slurpfile server "$work/server.json" '
  $input[0] as $i | {
    schema_version:1,candidate:true,backend:"firecracker",platform:"linux/amd64",
    version:$i.version,source_revision:$i.source_revision,firecracker_version:$i.firecracker_version,
    kernel_version:$i.kernel_version,tools_version:$i.tools_version,sources:$i.sources,
    node:$server[0],
    asset_digests:($i.assets | with_entries(.value = .value.sha256))
  }
' > "$bundle/manifest.json"
(
    cd "$bundle"
    find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum > SHA256SUMS
    bash ./verify.sh
)
tar -czf "$work/candidate.tar.gz" -C "$work" agentenv-firecracker
# Same filesystem hard-link publication is atomic and refuses an existing name.
ln "$work/candidate.tar.gz" "$output"
sha256sum "$output"
