# Dragonball M5 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Native execution, no repeated approval prompts.

**Goal:** 完成双后端恢复依赖审计、独立候选打包与干净状态复验。
**Architecture:** 独立只读 example + 后端专属本地候选打包；复用现有构建矩阵与 M4 Linux 测试架。
**Tech Stack:** Rust/RocksDB 0.24、shell、Python 标准库、Linux KVM/ublk。
**Spec:** [M5 设计](./dragonball-m5-design.md)

## Global Constraints

- 保留 FC 和默认选择；不改 PVM/aarch64 路径，不推送/部署/删除历史状态。
- 历史审计限定显式 roots；未知/损坏记录保留，不把空本地目录当作全局退役证明。
- builds/VM 为 aenv，root 只编排；M4 性能/双物理机门槛不随 M5 自动通过。
- 独立新 state/deps/cache；不声称全新 OS 或无法执行的平台已验证。

## Review Focus

- 只读打开数据库却写日志或创建目录，改变历史记录。
- 无 backend 的未知 schema 被误判为可恢复 FC。
- 报告意外输出 env/credentials 或把扫描失败当空数据。
- 候选包接受双 feature/debug server，或覆盖现有输出。
- 干净状态测试偷偷依赖其他 backend 的资产或宣称未跑的平台通过。

### Task 1: 只读暂停记录 inventory

**Files:** Create `examples/recovery_inventory.rs`; tests embedded; final inventory in M5 results.
**Interfaces:** CLI `cargo run --example recovery_inventory -- <existing-records.db>` outputs JSON with count and sanitized record summaries; no repair/delete options. `inventory(&Path) -> Result<Value>`.

- [x] RED tests: missing DB remains absent; real DB valid legacy v1/explicit DB/future schema/corrupt records classified conservatively; bytes unchanged; secret fields omitted.
- [x] Implement read-only RocksDB reader with external temporary log directory; unknowns stay retained and not qualified, white-list identity/version fields.
- [x] Run example tests on Linux as aenv, then read-only bind-mounted historical DB; compare all file hashes before/after and record scoped assets/owner/retention without printing config secrets.
- [x] Commit `feat: add read-only recovery dependency inventory`.

### Task 2: FC 独立候选打包与组件矩阵

**Files:** Create `scripts/release/package-firecracker.sh`, `scripts/release/verify-firecracker-bundle.sh`, `config/firecracker.toml`, `scripts/tests/verify-package-firecracker.sh`; modify `scripts/tests/check-hypervisor-builds.sh`.
**Interfaces:** Same two positional arguments and qualified asset/source/license structure as DB packager; FC assets server/firecracker/daemon/kernel/tools; source_revision and firecracker_version. Output `agentenv-firecracker` tar with manifest/verify/config/checksums.

- [x] RED offline tests for absent packager, wrong identity/digest/revision, missing asset/license, existing output, relocation/tamper.
- [x] Implement backend-specific bundle; verify release FC-only linux/amd64, binary version and all checksums; no DB runner or downloads.
- [x] Run offline tests plus actual Cargo checks FC-only/DB-only/both; assert FC tree lacks runner and DB tree lacks FC client. Package real release inputs and verify relocated DB/FC candidates as aenv.
- [x] Commit `build: package independent Firecracker candidates`.

### Task 3: 干净状态资格与交接

**Files:** Modify `Makefile`, `docs/src/internals/dragonball-m1-m5-roadmap.md`; create `docs/src/internals/dragonball-m5-results.md`.
**Interfaces:** Consumes inventory JSON and verified candidate paths. `test-hypervisor-packaging` runs both offline packaging contracts; current M4 inputs point to relocated candidates.

- [x] Execute single-backend persistence/identity regressions and new example tests; fmt and strict all-target/all-feature Clippy, applicable full Make regression.
- [x] Run actual candidate-backed clean namespace restore/rollback using M4 explicit entrypoints; verify runtime identity, same-ID resume, incompatible record retention and cleanup.
- [x] One final read-only fresh review, one RED→GREEN fix pass for Important/Critical findings; no re-review.
- [x] Record artifact identities, historical retain list, exact checks/ignored/platform limitations and untouched default. Commit `docs: record M5 dual-backend qualification`.
