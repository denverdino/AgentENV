#!/usr/bin/env bash
# Run from the repository root with the qualification toolchain/environment.
set -euo pipefail
cargo check -p agentenv --lib --bins --no-default-features --features hypervisor-dragonball
db_tree=$(cargo tree -p agentenv --no-default-features --features hypervisor-dragonball --edges normal)
if [[ "$db_tree" == *firecracker_client* ]]; then
    echo 'Dragonball-only product unexpectedly depends on firecracker_client' >&2
    exit 1
fi
cargo check -p agentenv --lib --bins --no-default-features --features hypervisor-firecracker
fc_tree=$(cargo tree -p agentenv --no-default-features --features hypervisor-firecracker --edges normal)
if [[ "$fc_tree" == *agentenv-dragonball-runner* ]]; then
    echo 'Firecracker-only product unexpectedly depends on the Dragonball runner' >&2
    exit 1
fi
cargo check -p agentenv --lib --bins --all-features
