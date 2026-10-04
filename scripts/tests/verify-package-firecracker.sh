#!/usr/bin/env bash
# Offline packaging contract tests; fake identity CLIs are not VM qualification.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
package="$root/scripts/release/package-firecracker.sh"
[[ -f "$package" ]] || { echo 'FAIL: independent Firecracker packager is absent' >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/assets" "$work/out" "$work/relocated" "$work/unrelated"
cat > "$work/assets/server" <<'SH'
#!/bin/sh
test "$1" = --component-info-json || exit 1
printf '%s\n' '{"backend":"firecracker","dragonball":false,"firecracker":true,"debug_assertions":false,"architecture":"x86_64","os":"linux","version":"test","git_commit":"1111111"}'
SH
cat > "$work/assets/firecracker" <<'SH'
#!/bin/sh
test "$1" = --version || exit 1
printf '%s\n' 'Firecracker v1.15.1-test'
SH
cp "$work/assets/server" "$work/assets/daemon"
chmod 755 "$work/assets/"{server,firecracker,daemon}
for asset in kernel tools license notice; do printf 'fixture %s\n' "$asset" > "$work/assets/$asset"; done
jq -n --arg dir "$work/assets" '{
  version:"m5-test", source_revision:"1111111111111111111111111111111111111111",
  firecracker_version:"1.15.1-test",
  kernel_version:"6.1.175",tools_version:"0.1.0",
  sources:{node:"https://github.com/kvcache-ai/AgentENV",firecracker:"https://github.com/kvcache-ai/firecracker",kernel:"https://kernel.org",tools:"fixture"},
  assets:(["server","firecracker","daemon","kernel","tools"] | map({key:.,value:{path:($dir+"/"+.)}}) | from_entries),
  licenses:(["node","firecracker","kernel","tools"] | map({key:.,value:{license:($dir+"/license"),notice:($dir+"/notice")}}) | from_entries)
}' > "$work/input.json"
for asset in server firecracker daemon kernel tools; do
    digest=$(sha256sum "$work/assets/$asset" | cut -d' ' -f1)
    jq --arg key "$asset" --arg digest "$digest" '.assets[$key].sha256=$digest' "$work/input.json" > "$work/next.json"
    mv "$work/next.json" "$work/input.json"
done
bash "$package" "$work/input.json" "$work/out/candidate.tar.gz"
if bash "$package" "$work/input.json" "$work/out/candidate.tar.gz"; then echo 'FAIL: overwrote output'; exit 1; fi
for expression in 'del(.assets.kernel)' 'del(.tools_version)' 'del(.sources.node)' 'del(.licenses.firecracker)' '.assets.firecracker.sha256="0000000000000000000000000000000000000000000000000000000000000000"' '.firecracker_version="wrong"'; do
    jq "$expression" "$work/input.json" > "$work/invalid.json"
    if bash "$package" "$work/invalid.json" "$work/out/invalid.tar.gz"; then echo "FAIL: accepted $expression"; exit 1; fi
    test ! -e "$work/out/invalid.tar.gz"
done
tar -xzf "$work/out/candidate.tar.gz" -C "$work/relocated"
bundle="$work/relocated/agentenv-firecracker"
(cd "$work/unrelated"; bash "$bundle/verify.sh")
test "$(find "$bundle/bin" -type f | wc -l)" -eq 3
test ! -e "$bundle/bin/aenv-dragonball" && test ! -e "$bundle/bin/reply-fault"
for file in "$bundle"/licenses/*/LICENSE "$bundle"/licenses/*/NOTICE; do test -s "$file"; done
jq -e '.candidate == true and .backend == "firecracker" and .firecracker_version == "1.15.1-test"' "$bundle/manifest.json" >/dev/null
test "$(awk '/^\[/{kernel=($0=="[kernel]")} kernel && $1=="version" {gsub(/"/,"",$3); print $3}' "$bundle/config/firecracker.toml")" = '6.1.175'
while IFS='=' read -r key value; do
    case "$key" in *binary_path*|*image_path*|*drive_path*)
        relative=$(printf '%s' "$value" | tr -d '" ')
        test -s "$bundle/config/$relative";;
    esac
done < "$bundle/config/firecracker.toml"
printf tamper >> "$bundle/assets/vmlinux.bin"
if bash "$bundle/verify.sh"; then echo 'FAIL: accepted corrupted bundle'; exit 1; fi
cp "$work/assets/server" "$work/server.original"
for change in 's/"dragonball":false/"dragonball":true/' 's/"debug_assertions":false/"debug_assertions":true/' 's/"architecture":"x86_64"/"architecture":"aarch64"/' 's/"git_commit":"1111111"/"git_commit":"2222222"/'; do
    sed "$change" "$work/server.original" > "$work/assets/server"
    digest=$(sha256sum "$work/assets/server" | cut -d' ' -f1)
    jq --arg digest "$digest" '.assets.server.sha256=$digest' "$work/input.json" > "$work/invalid.json"
    if bash "$package" "$work/invalid.json" "$work/out/invalid.tar.gz"; then echo "FAIL: accepted server $change"; exit 1; fi
done
echo 'PASS: Firecracker packaging prerequisites, identity, no-overwrite, relocation and tamper contracts'
