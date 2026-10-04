# Hypervisor component verification — 2026-10-02

Scope: compile-time selectable Firecracker/Dragonball products in the
`feat/dragonball-m0` worktree. This qualifies the component refactor, not completion
of all historical M2–M5 milestones or release readiness.

## Composition

| Boundary | Implementation |
| --- | --- |
| Server/orchestrator | `SelectedSandboxFactory` selects a backend at compilation |
| VM lifecycle | Existing `SandboxBackend` / `SandboxBackendFactory` contracts |
| Template construction/version identity | `TemplateSandboxFactory` adapters |
| Deferred startup recording | Capture-owned callback and artifact/workspace lease |

Default and both-feature builds select Firecracker. A Dragonball-only product
uses `-p agentenv --no-default-features --features hypervisor-dragonball` and has
no `firecracker_client` dependency. The runner remains a separate executable.
See the [configuration reference](../configuration/reference.md) for commands
and the explicit runner, qualified kernel and versioned plain-ext4 tools inputs.

## Verified on the remote Linux KVM host

Builds and tests ran as `aenv` (UID 999), using isolated state/dependency paths.
Guest checks received only the normal NET_ADMIN/SYS_ADMIN capability set.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check`, `git diff --check`, shell syntax | Passed |
| `scripts/tests/check-hypervisor-builds.sh` | DB-only, FC-only, both-feature builds passed; DB tree excludes FC client |
| Single-backend all-target Clippy, `-D warnings` | Both passed |
| Workspace/all-target/all-feature Clippy, `-D warnings` | Passed |
| Firecracker library tests | 972 passed, 0 failed, 5 ignored |
| Dragonball library tests, including dynamic CPU policy regression | 943 passed, 0 failed, 5 ignored |
| Selected-backend real template/derive/restore and unpublished-recorder shutdown | 2 passed per backend |
| Dragonball M2 startup recording/prefetch/cancellation/shutdown | 5 passed |
| Full `make test` (default Firecracker product) | Exit 0: AgentENV, envd, ublk/OverlayBD and MinIO suites |

The aggregate includes 21 ordinary AgentENV integration tests and 7 MinIO tests.
Its opt-in/ignored hardware cases are not implied to have run; the component and
Dragonball startup cases above were invoked explicitly.

The test host's sudo implementation drops `-E` environment preservation. The
initial aggregate attempt therefore lost the fixture/config paths. The passing
run used the repository capability wrapper to enter UID 999, then
`setpriv --reset-env`, and `make test CAPABILITY_RUNNER=` to avoid a second sudo
hop. The test wrapper asserted UID 999 and exported the isolated config, state,
dependencies and prepared rootfs. No suite or assertion was skipped to obtain
the passing result.

Remote evidence is under
`/var/tmp/agentenv-dragonball-m2-20261002/logs/components-*.log`, notably
`components-make-test-final.log`, `components-dragonball-lib-final.log`,
`components-all-clippy-final.log`, `components-fc-final.log`,
`components-db-final.log`, and `components-db-startup-final.log`.

## Review and regression fixes

One independent read-only review identified four important issues, all addressed:

- Suppress extension hooks and inherited parameters for FC template VMs; a
  rejecting extension reproduced the old failure before the fix.
- Derive DB template versions from the actual runtime identity; changing the
  host envd/tools versions reproduced the previous metadata/envelope conflict.
- Require explicit qualified tools input during DB setup, before provisioning.
- Register both FC recorder callbacks for shutdown, including publication
  cancellation; the previous drain returned before recorder cleanup finished.

A subsequent constructor audit added a failing regression for dynamic CPU
configuration updates: the common DB constructor now rejects unsupported CPU
templates, including the direct fork-construction path.

There were no deferred minor review findings. Historical runner internals,
cross-host/performance qualification and release packaging were not reviewed as
part of this component change. The remaining M2–M5 work is not declared complete.

Temporary sudo authorization and Docker group membership were revoked after
testing. The previously removed `aenv.service` remains inactive; no installed
AgentENV server was restored. Task-specific build caches/logs and the development
worktree are retained for subsequent work.
