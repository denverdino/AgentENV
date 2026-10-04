# Selectable hypervisor components

The user-approved goal is to retain Firecracker and Dragonball as optional
components, with compile-time product selection. This supersedes the earlier
Dragonball roadmap's M5 removal of Firecracker.

## Contract

- `hypervisor-firecracker` is the default AgentENV feature and enables the
  optional Firecracker client. `hypervisor-dragonball` enables the Dragonball
  adapter on Linux x86_64. At least one backend must be enabled.
- A Dragonball-only server is built with `-p agentenv --no-default-features
  --features hypervisor-dragonball`. Both features may be enabled for contract
  tests; the product then selects Firecracker. No runtime plugin registry or
  mixed-backend scheduling is added.
- `SelectedSandboxFactory` is the compile-time product factory. Server, API,
  orchestrator and template construction share this selection. Backend-private
  config conversions and CPU policy handling stay inside the adapters.
- Template factories provide a concrete lifecycle + executor implementation;
  the template pipeline must not construct, downcast or probe Firecracker types.
- Captures carry an optional backend-owned recording factory. Repository and
  template publication know only the recording task and its artifact lease.
- Dragonball setup validates an explicit runner, kernel and versioned plain
  ext4 tools image rather than
  installing or silently reusing Firecracker assets. PVM and unqualified CPU
  policies fail before VM allocation. Existing FC configuration remains valid.
- Snapshot backend/ABI identities and cross-backend rejection remain unchanged.
  Compile-time selection does not make FC and DB memory snapshots compatible.

## Verification

Build and lint each single-backend product and the all-features test build.
Check the DB-only dependency tree excludes `firecracker_client`. Exercise the
same template and snapshot publication paths on Linux KVM for both backends,
including recording ownership and incompatible snapshot rejection. Preserve
in-progress M2 changes and report aggregate-suite limitations explicitly.
