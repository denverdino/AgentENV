# Selectable Hypervisor Components Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Ship Firecracker-only and Dragonball-only AgentENV products using the existing backend abstraction.

**Architecture:** Compile-time selection at the composition root; concrete adapters own construction, template execution capabilities and recording. Shared lifecycle and publication retain opaque artifact ownership.

**Tech Stack:** Rust, Cargo features, Linux KVM, existing Dragonball runner and ublk.

**Spec:** `docs/src/internals/hypervisor-components-design.md`

## Global Constraints

- Default and both-feature product: Firecracker; Dragonball-only: Linux x86_64/KVM.
- Keep all existing M2 work; do not delete Firecracker or durable test/server data.
- Non-root builds/tests on the authorized remote Linux host; isolated test state.
- No runtime plugin registry, cross-backend memory conversion or mixed scheduler.
- User requested automatic implementation without additional confirmation.

## Review Focus

- Cargo feature unification must not re-enable FC in the documented DB-only build.
- DB-only setup must never download an FC binary or silently use its kernel.
- Template restore must retain source tools identity and suppress extension hooks.
- Delayed recording must retain capture/workspace ownership through publication cancellation.
- Enabling both features must leave the default product and legacy FC snapshots usable.

### Task 1: Compile-time selection and product assembly

**Files:** `Cargo.toml`, `src/sandbox/{mod,selected}.rs`, `src/orchestrator/service.rs`, `src/bin/server.rs`, `src/cfg.rs`, `src/setup/deps.rs`, backend-dependent tests.

**Interfaces:** Produce `SelectedSandboxFactory`, `selected::factory_with_cpu_config(Arc<RwLock<Option<String>>>) -> Result<SelectedSandboxFactory>`, backend pool lifecycle functions and explicit Dragonball asset configuration.

- [x] Write `scripts/tests/check-hypervisor-builds.sh` asserting both single-backend builds succeed and DB-only `cargo tree` contains no FC client; run and observe missing-feature failure.
- [x] Add feature gates, optional dependency and selected factory. Gate backend-specific tests, not shared tests. Validate DB assets before other setup work.
- [x] Run each build and all-features check remotely; expected exit 0, absent FC client from DB dependency graph.

### Task 2: Backend-neutral templates and recording

**Files:** `src/sandbox/backend.rs`, both backend factories/sandboxes/startup modules, `src/template/{runner,builder}.rs`, `src/snapshot/manager.rs`.

**Interfaces:** Produce a template factory capability with associated `Sandbox: SandboxBackend + SandboxExecutor`, and a capture-owned deferred recording callback consumed once by publication.

- [x] Add tests proving recording is invoked once and retains owners; run RED before adding callback support.
- [x] Move concrete construction and runtime version selection behind the template factory capability; preserve existing FC template configuration semantics.
- [x] Route template and ordinary snapshot recording through backend-owned callbacks, retain workspace leases across detached work.
- [x] Run shared template/startup tests and backend-specific real capture/template tests; expected 0 failures.

### Task 3: Regression, documentation and review

**Files:** build script, configuration reference/defaults, Dragonball design and M1–M5 roadmap, integration qualification tests.

**Interfaces:** Consume Tasks 1–2; publish reproducible build commands and backend support limits.

- [x] Verify DB-only, FC-only and all-features formatting/clippy/builds on Linux; run applicable repository tests and report every failure or blocker.
- [x] Update M5 to optional-component qualification, not deletion; document runner packaging and isolated configuration.
- [x] Request one fresh whole-change review; fix important findings with RED→GREEN tests. Preserve unrelated M2 work and evidence.

Verification evidence: [component verification](./hypervisor-components-verification.md).
This plan is complete; the separate historical M2–M5 roadmap is not.
