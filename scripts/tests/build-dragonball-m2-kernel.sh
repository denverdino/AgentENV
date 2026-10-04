#!/usr/bin/env bash
# Separate qualification artifact. Never replaces M1 or Firecracker kernels.
set -euo pipefail
[[ $(id -u) != 0 ]] || { echo 'build the qualification kernel as a non-root user' >&2; exit 1; }
[[ $# == 4 ]] || { echo "usage: $0 <linux-6.1.175-source> <m1-kernel.config> <build-dir> <output-dir>" >&2; exit 2; }
source_dir=$(realpath -e -- "$1")
baseline=$(realpath -e -- "$2")
kernel_build=$(realpath -m -- "$3")
output_dir=$(realpath -m -- "$4")
[[ "$kernel_build" != / && "$output_dir" != / && "$kernel_build" != "$source_dir" ]]
[[ $(make -s -C "$source_dir" kernelversion) == 6.1.175 ]]
[[ ! -e "$output_dir/vmlinux.bin" ]] || { echo 'refusing to overwrite an existing kernel artifact' >&2; exit 1; }
mkdir -p "$kernel_build" "$output_dir"
cp "$baseline" "$kernel_build/.config"
"$source_dir/scripts/config" --file "$kernel_build/.config" \
    --enable VIRTIO_MMIO_CMDLINE_DEVICES --enable X86_MPPARSE \
    --enable DAMON_RECLAIM --enable MODULES --enable KPROBES \
    --enable FTRACE --enable KPROBE_EVENTS --enable KALLSYMS_ALL \
    --set-str LOCALVERSION '-dragonball-m2.1'
make -C "$source_dir" O="$kernel_build" olddefconfig
make -C "$source_dir" O="$kernel_build" -j"${AENV_DRAGONBALL_KERNEL_JOBS:-8}" vmlinux
cp "$kernel_build/vmlinux" "$output_dir/vmlinux.bin"
cp "$kernel_build/.config" "$output_dir/kernel.config"
sha256sum "$output_dir/vmlinux.bin" "$output_dir/kernel.config"
