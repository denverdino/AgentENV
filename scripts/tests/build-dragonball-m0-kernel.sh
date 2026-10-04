#!/usr/bin/env bash
# Non-root, qualification-only kernel: never replaces Firecracker's dependency.
set -euo pipefail
[[ $(id -u) != 0 ]] || { echo 'build the qualification kernel as a non-root user' >&2; exit 1; }
[[ $# == 2 ]] || { echo "usage: $0 <build-root> <output-dir>" >&2; exit 2; }
build_root=$(realpath -m -- "$1")
output_dir=$(realpath -m -- "$2")
[[ "$build_root" != / && "$output_dir" != / ]]
[[ ! -e "$output_dir/vmlinux.bin" ]] || { echo 'refusing to overwrite an existing kernel artifact' >&2; exit 1; }
mkdir -p "$build_root" "$output_dir"
source_dir="$build_root/linux-6.1.175"
archive="$build_root/linux-6.1.175.tar.xz"
if [[ ! -f "$archive" ]]; then
    curl --fail --location --retry 2 --output "$archive" \
        https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.1.175.tar.xz
fi
curl --fail --location --retry 2 --output "$build_root/sha256sums.asc" \
    https://cdn.kernel.org/pub/linux/kernel/v6.x/sha256sums.asc
(cd "$build_root"; grep '  linux-6.1.175.tar.xz$' sha256sums.asc | sha256sum --check --strict)
if [[ ! -d "$source_dir" ]]; then
    mkdir "$source_dir"
    tar -xJf "$archive" --strip-components=1 -C "$source_dir"
fi
curl --fail --location --retry 2 --output "$build_root/firecracker-baseline.config" \
    https://raw.githubusercontent.com/kvcache-ai/firecracker/9fbc98e4dc05d03e1f00902ba4bb796395465099/resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config
kernel_build="$build_root/build"
mkdir -p "$kernel_build"
cp "$build_root/firecracker-baseline.config" "$kernel_build/.config"
"$source_dir/scripts/config" --file "$kernel_build/.config" \
    --enable VIRTIO_MMIO_CMDLINE_DEVICES --enable X86_MPPARSE \
    --set-str LOCALVERSION '-dragonball-m0.1'
make -C "$source_dir" O="$kernel_build" olddefconfig
make -C "$source_dir" O="$kernel_build" -j"${AENV_DRAGONBALL_M0_KERNEL_JOBS:-8}" vmlinux
cp "$kernel_build/vmlinux" "$output_dir/vmlinux.bin"
cp "$kernel_build/.config" "$output_dir/kernel.config"
sha256sum "$output_dir/vmlinux.bin" "$output_dir/kernel.config"
