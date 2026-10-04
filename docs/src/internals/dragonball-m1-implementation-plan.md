# Dragonball M1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付有活跃 I/O 一致性证据的 Dragonball snapshot/pause/resume/fork，并接入现有卷、paused store 和 full-memory OverlayBD；默认后端不切换。

**Architecture:** 节点拥有生命周期、网络/磁盘/卷和产物 lease；独立 runner 拥有 vCPU、设备屏障和 typed upstream state。先扩展固定 fork 的 block/net gate 与 held restore，再通过独立 runtime 协议接入 `SandboxBackend`。M1 完整导出 RAM，恢复时从 OverlayBD materialize 私有普通文件；ublk lazy COW、增量、balloon 和 slot 容量变更由 M2 接续。

**Tech Stack:** Rust 2021、现有 Tokio/Serde、Linux x86_64/KVM、virtio-mmio、RawBlock、用户态 virtio-net、OverlayBD/ublk、原 envd 客户端。

**Spec:** [已确认设计](./dragonball-hypervisor-design.md) §§5–10、12–14；阶段衔接和部署/退役权限见 [M1–M5 路线](./dragonball-m1-m5-roadmap.md)。AgentENV 起点 `2d82c0a`；完整 Kata fork 起点 `bfc470b07aedaeddaf801ff091f5cdb1a12c69bc`。

## Global Constraints

- 对外 E2B API 和已有 sandbox 生命周期语义保持兼容。
- 不迁入 Kata runtime、kata-agent、containerd 或 Kata 的镜像管理链路。
- 第一阶段资格范围为 Linux x86_64 + KVM；aarch64 checkpoint 和 PVM 为后续独立资格项。
- 不承诺 Firecracker 与 Dragonball 的内存/设备快照互读。
- tools `vda`、用户盘 `vdb`、physical extras、volume slots 顺序不变。M1 支持已捕获的相同容量/feature 拓扑；M2 才验收 placeholder→新容量/RO 槽变更，M1 不暴露为完整生产替换。
- recoverable capture 错误返回前源必须 Running；seal/restack 已改变 live storage 或无法确认 worker 状态时 Terminal。发布失败与已完成的 VMM capture 分开。
- fork exclusive 卷为每 child 派生、ro 卷共享；失败 child 回滚自己的分配，不影响源/兄弟。`volume_snapshots` 与 attached drives 分开。
- capture、recording、startup/cleanup 的 owner 不随 caller future 取消释放；runner 确认退出后才释放 backing 和网络。
- committed state 只保存逻辑设备、layer 引用和恢复身份；PID、host address、tap/netns 和 `/dev/ublkbN` 不能进入 durable state。
- M0 probe 协议及 `probe_only` snapshot 保持资格用途；生产 reader 不接受 probe envelope。默认 FC，独立 DB paused roots，禁止 GC 不兼容记录。
- 用完整 fork 的真实提交更新 Cargo pin/lock 和身份测试，不在缓存修改源码、不留下 path patch override、不推上游或默认分支。
- 使用独立 `AENV_TEST_STATE_DIR`、`AENV_TEST_DEPS_PATH`、`AENV_CONFIG_PATH`；非 root 构建/测试。保留旧部署数据与卸载状态。

## Review Focus

1. queue/rate-limit 事件与 capture 同时到达：停止取新 descriptor，但 completion 能完成 used ring；任务 1–2 测试 ack 前/后的真实 ring 与 pending count。
2. 恢复时 device activation 先于 vCPU start：不得在 metadata/backing/限速绑定前写 guest RAM；任务 3、7 测试 held 期间 RAM/盘不变。
3. capture 调用者取消或 response 丢失：owner 继续持有 barrier/backing；未确认操作不重复 Save；任务 4、8 注入并验证清理顺序。
4. shared layer 在一 child 失败时释放：源和成功兄弟仍可读写，最后引用才删除；任务 6、9 验证 actual device/lease 和 source/child 内容。
5. unknown backend/ABI 的 paused record：显示可识别外层 metadata，禁止恢复但不删文件/record；任务 5、10 用旧 FC 与异类记录验证。

## 文件结构和实施顺序

Kata 中路径以下均相对完整 fork 根目录。AgentENV 中新增 `runtime.rs` 与 `runtime_service.rs` 明确区分 M0 probe 和 M1 runtime；不建立通用 hypervisor 插件框架。复用现有 process/metadata/extra-drive helpers，抽取双方确需共享的内存 writer 和 storage seal/restack。

| 任务 | 首次拥有的接口/模块 | 消费方 |
| --- | --- | --- |
| 1–2 | upstream `CaptureGeneration`、block/net worker gate 与 ack | 3–4 |
| 3 | upstream begin/end capture、held load、held state/full RAM export | 4、8 |
| 4 | runtime DTO（含共享 identity）/client/service 和状态机 | 5、7–9 |
| 5 | portable identity 预检、runtime envelope/设备绑定 | 7–10 |
| 6 | 中立 full/FC memory writer 与 storage capture owner | 8–9 |
| 7 | `DragonballSandboxFactory`、fresh/held backend、envd readiness | 8–11 |
| 8–9 | snapshot/pause/fork/卷、owned capture/cleanup | 10–11 |
| 10 | paused store 的 backend/reader 分派和保留 | 11、M3 |
| 11 | 真正负载/故障/重启套件及 M1 证据 | M2 |

接口新增的准确类型如下：`CaptureToken { runner_instance_id: String, generation: u64 }`；`MemoryRegion { guest_base: u64, image_offset: u64, length: u64 }`；`MemoryLayout { page_size: u64, regions: Vec<MemoryRegion>, image_size: u64 }`。M1 不导出 host address。region 按 GPA 升序，image offset 为紧凑连续 RAM 文件偏移，不把 GPA hole 填成 RAM；对齐、checked-add、重叠、数量与实际 machine 全部校验。

`WorkerAck { device_id: String, generation: u64, pending_io: u64, memory_writers: u32, flush_completed: bool }`：block 只有 pending/writers 为 0 且 flush 成功才 ack；net 不要求 block flush，但必须没有正在进行的 RAM/used-ring 写入。报告设备集合/逻辑 ID/顺序与 machine 一致，不接受空报告替代实际设备。

共享 DTO 归 `crates/vmm-protocol/src/runtime.rs` 所有，runner 与节点共同消费；upstream gate 的报告由 runner adapter 转成 DTO，不把 upstream Rust enum 当持久化契约。`SnapshotRestoreIdentity` 中 `snapshot_format_epoch: u16`，设计 §9.1 列出的其余字段均为 `String`；`src/sandbox/restore_identity.rs` 重导出并校验该类型，不另建同名类型或让 runner 依赖 AgentENV 主 crate。

## Task 1：block 队列的真实 drain/hold

**Files:** Kata Create `src/dragonball/crates/dbs_virtio_devices/src/capture.rs`；Modify `src/dragonball/crates/dbs_virtio_devices/src/{lib.rs,block/device.rs,block/handler.rs}`；Test 同模块与 `src/dragonball/tests/minimal-profile-consumer/`。

**Interfaces:** 产生 `CaptureGeneration(pub u64)`、`WorkerAck` 和 `WorkerCaptureControl`。其方法为 `request_hold(generation, deadline: Instant) -> Result<WorkerAck>`、`resume(generation, deadline: Instant) -> Result<()>`。每 block queue 的控制消息通过现有 eventfd/receiver 路径传递；响应 generation 必须匹配，不通过阻塞 worker 所需的同一个设备锁等待 ack。

- [ ] **Step 1：写 `capture_block_drains_taken_requests_before_ack`、`capture_block_preserves_unconsumed_descriptors`、`capture_block_suppresses_queue_and_limiter_while_held`**。实际 queue/pending map：已有请求先完成并更新 used ring、flush；未取 descriptor 在 hold 期间保留；held 后 queue/limiter 不新提交；resume 后继续。
- [ ] **Step 2：在 Linux 非 root fork checkout 运行 `cargo test --manifest-path src/dragonball/Cargo.toml -p dbs-virtio-devices --no-default-features --features virtio-blk capture_block`**。Expected：新增断言在尚无 gate 时失败，记录 RED。
- [ ] **Step 3：接入 queue worker gate**。freeze 只屏蔽新请求，继续处理 END_IO_EVENT；flush 错误不 ack 成功。重复同 generation 的 hold 可确认结果，旧 generation 或 stop 后控制拒绝。补 `capture_block_ack_timeout_is_not_success` 和 `capture_block_resume_kicks_saved_queue`。
- [ ] **Step 4：重跑上述命令及最小 consumer 构建**。Expected：真实 ring、flush、timeout、stale generation 全部通过；无 extra feature 绕过。
- [ ] **Step 5：提交 fork 精确文件集合**，message `feat: add virtio block capture gate`；不更新 AgentENV pin 到未经其他补丁验证的半成品。

## Task 2：net 与完整设备集合的屏障

**Files:** Kata Modify `src/dragonball/crates/dbs_virtio_devices/src/{net.rs,capture.rs,persist.rs}`、`src/dragonball/src/device_manager/{mod.rs,blk_dev_mgr.rs,net_dev_mgr.rs,persist.rs}`。

**Interfaces:** 消费任务 1；产生 `DeviceManager::begin_capture(generation: CaptureGeneration, deadline: Instant) -> Result<Vec<WorkerAck>>`、`end_capture(generation, deadline) -> Result<()>`。按 machine 的 block/net 逻辑 ID 聚合报告；不支持的可写内存设备明确拒绝。

- [ ] **Step 1：写 `capture_net_holds_rx_tx_memory_writes`、`capture_net_keeps_completed_ring_state`、`capture_requires_every_configured_device_ack`**。同时输入 RX/TX 和 queued callback，held 后 RAM 与 used indices 不再变化；缺一个 block/net ack、generation 不同不能成功。
- [ ] **Step 2：运行 `cargo test --manifest-path src/dragonball/Cargo.toml -p dbs-virtio-devices --no-default-features --features virtio-blk,virtio-net capture_` 和 `cargo test --manifest-path src/dragonball/Cargo.toml -p dragonball --no-default-features --features virtio-blk,virtio-net capture_`**。Expected：新 net/集合断言失败，已有 block 测试不退化。
- [ ] **Step 3：实现 net event-loop gate 与聚合**。net callback 与控制 action 同属 VMM loop 时，必须在返回 held 之前关闭后续数据回调；独立 block worker 必须等待 ack。仅丢弃尚未提交 guest queue 的 host RX 暂存帧，完成已消费 descriptor；resume 重启 queue/TAP 通知。任何无法证明一致性的部分 freeze 为 Terminal，不以 sleep 推断成功。
- [ ] **Step 4：重跑并加入 `capture_rejects_unsupported_memory_writer`、`capture_partial_ack_requires_confirmed_release_or_teardown`**。Expected：每个 ack 有生产调用，fake ack 不能代替真实 worker 测试。
- [ ] **Step 5：提交** `feat: coordinate Dragonball device capture barriers`。

## Task 3：VMM 级 held restore 与 state/full memory export

**Files:** Kata Modify `src/dragonball/src/{vm/mod.rs,vcpu/mod.rs,address_space_manager.rs,api/v1/vmm_action.rs,device_manager/persist.rs,snapshot/mod.rs}`；Test 对应模块及 minimal consumer。

**Interfaces:** 消费 1–2；新增 `VmmAction::{BeginCapture,EndCapture,LoadSnapshotHeld,ExportHeldSnapshot}` 和 `VmmData::CaptureReport`。参数：generation、deadline；Load/Export 使用已打开的 state/memory FD 对应路径。`CaptureReport` 包含完整 ack 与 `MemoryLayout`。旧 `SaveMicrovm`/`StartMicroVmFromSnapshot` 行为不改，新增 held API 独立调用。

- [ ] **Step 1：写 `load_held_never_runs_vcpu_or_device`、`export_requires_current_capture_generation`、`capture_full_layout_handles_gpa_hole`**。RAM marker/盘 counters 在 LoadHeld 后不变化；错误 generation 不读写输出；8 GiB、多 region 的 packed file offsets 与 RAM 总量相等。
- [ ] **Step 2：运行 `cargo test --manifest-path src/dragonball/Cargo.toml -p dragonball --no-default-features --features virtio-blk,virtio-net capture_` 和 `cargo test --manifest-path src/dragonball/Cargo.toml -p dragonball --no-default-features --features virtio-blk,virtio-net load_held`**。Expected：缺 held/generation 拒绝时 RED。
- [ ] **Step 3：实现 vCPU pause → device gate → token，及 activation 前 held gate**。state 和 full RAM 导出只在同 token 持有期间执行；禁止 new vCPU 或 block worker 在配置完成前消费队列。恢复前验证 CPUID/MSR/XSAVE 与布局；LoadHeld 后先准备资源，显式 Resume 才放行。用输出 FD 所有权避免覆盖/路径替换。
- [ ] **Step 4：运行上面 tests、原 snapshot roundtrip 与最小/既有扩展 feature consumer**。Expected：新 held、原 API 和 M0 guest 均可验证；host RAM map 元数据与实际映射一致。提交全部主题补丁后推用户 fork topic branch，读回真实 SHA，再更新 AgentENV Cargo pin/lock/build identity；不使用虚构 SHA。
- [ ] **Step 5：提交 fork 和 AgentENV pin**，附实际 feature/revision 与 RED/GREEN。M0 probe snapshots不作为生产兼容输入。

## Task 4：生产 runtime 协议和 runner 状态机

**Files:** Create `crates/vmm-protocol/src/runtime.rs`、`crates/vmm-protocol/tests/runtime_frames.rs`、`crates/dragonball-runner/src/runtime_service.rs`、`crates/dragonball-runner/tests/runtime_service.rs`、`src/sandbox/dragonball/runtime_client.rs`；Modify `crates/vmm-protocol/src/{frame.rs,lib.rs}`、`crates/dragonball-runner/src/{main.rs,vmm.rs}`。

**Interfaces:** runtime protocol version `1`，CLI 显式 `--runtime`，与原 probe mode/DTO 分开。产生共享 `SnapshotRestoreIdentity`、`CaptureToken`、`MemoryLayout` 和 `WorkerAck` DTO。`RuntimeCommand::{Hello,Configure,Start,BeginCapture,ExportHeldSnapshot,EndCapture,LoadHeld,Resume,Status,Stop}`；沿用 request/instance IDs、budget 和 `1_048_576` body 上限。`RuntimePhase::{Unconfigured,Configured,Running,Capturing,Held,LoadedHeld,Stopped,Failed}`；held reply 含 token、布局及 worker ack。`RuntimeClient::request(command: RuntimeCommand, budget: Duration) -> Result<RuntimeReply>`。

- [ ] **Step 1：写 `runtime_rejects_probe_snapshot`、`runtime_rejects_stale_capture_token`、`runtime_capture_response_has_all_worker_acks`、`runtime_timeout_does_not_reissue_export`**；再保留 M0 frames 的 version-2 拒绝、截断、超限、归属测试。
- [ ] **Step 2：运行 `cargo test -p agentenv-vmm-protocol --test runtime_frames` 与 `cargo test -p agentenv-dragonball-runner --test runtime_service`**。Expected：新增运行时契约 RED；不是放宽旧 probe 验证。
- [ ] **Step 3：实现上述 DTO、codec、`RuntimeVmm` adapter 和串行 service**。Hello 返回实际 revision/profile/ABI 和已实现能力；同 token 查询/EndCapture 可确认，未知结果进入 Failed/teardown，不假设 timeout 等于取消。runner 配置不含 registry/OSS/token 明文。
- [ ] **Step 4：重跑 tests 与 `cargo test -p agentenv-vmm-protocol -p agentenv-dragonball-runner --locked`**。Expected：runtime/M0 并存，peer UID、NNP/seccomp、parent-death 与 fd/path 守卫不退化。
- [ ] **Step 5：提交** `feat: add Dragonball held runtime protocol`。

## Task 5：portable identity/envelope 与恢复预检

**Files:** Create `src/sandbox/dragonball/snapshot.rs`、`crates/dragonball-runner/src/runtime_snapshot.rs`、`crates/dragonball-runner/tests/runtime_snapshot.rs`、`src/sandbox/restore_identity.rs`；Modify `src/sandbox/manifest.rs`、`src/snapshot/types/{snapshot.rs,version.rs}`。

**Interfaces:** 消费任务 4 的 `SnapshotRestoreIdentity`，包含设计 §9.1 的 backend/architecture/mode、实际 runner/revision、format epoch、restore ABI、CPU/device/memory layout IDs 和 guest versions。runtime envelope version `1`，包含 identity、logical devices、`MemoryLayout`、opaque upstream state；不接受 `probe_only`。节点 `validate_restore(identity: &SnapshotRestoreIdentity) -> Result<()>` 必须先于资源分配。

- [ ] **Step 1：写 `runtime_envelope_contains_no_node_local_bindings`、`restore_rejects_identity_or_topology_conflict_before_allocation`、`legacy_fc_versions_remain_readable`**。归档 state 不含 PID/address/TAP/netns/device path；记录/manifest/envelope 的 backend/revision/ABI 任一冲突失败；不把 DB version 填进旧 firecracker_version。
- [ ] **Step 2：运行 `cargo test -p agentenv --lib restore_identity` 与 `cargo test -p agentenv-dragonball-runner --test runtime_snapshot`**。Expected：尚无 portable envelope/完整预检时 RED。
- [ ] **Step 3：实现逻辑设备转换和精确 reader/profile 检查**。由实际 machine 构造受检 identity，ABI 绑定 CPU/设备/布局，不仅 epoch。保存已捕获的 tools version；恢复将当前资源注入合法 host config 字段，不强改 config space/negotiated features，不按位置兜底。未知字段/缺设备/路径逃逸/短 memory 均在 VMM action 前拒绝。
- [ ] **Step 4：加入 state/device 逐字段篡改、duplicate IDs、未知 ABI、非法 offset/overflow、缺 tools 的测试，并重跑旧 manifest/version tests**。Expected：完整预检失败不改变 live phase，不发布残缺 snapshot。
- [ ] **Step 5：提交** `feat: persist portable Dragonball restore identity`。

## Task 6：中立内存 writer 与磁盘捕获 owner

**Files:** Create `src/sandbox/memory_snapshot.rs`、`src/sandbox/storage_snapshot.rs`；Modify `firecracker/overlaybd_snapshot.rs` 与其真实调用点；复用 `extra_drive.rs`、`ublk/` 和 `storage/util`，不复制 daemon/repository。

**Interfaces:** `MemorySegment { image_offset: u64, source_offset: u64, length: u64 }`；`async fn write_memory_overlaybd(source: Arc<dyn VirtualFile>, segments: &[MemorySegment], virtual_size: u64, at: &Path, mode: OverlaybdCompactOutput) -> Result<PathBuf>`。FC adapter 转换原 dirty ranges，DB full file 是同 RAM layout 的完整源；storage owner 包含 rootfs/extras/volume seal/restack 结果和 leases。

- [ ] **Step 1：写 `full_memory_layer_matches_packed_ram_bytes`、`full_memory_layer_rejects_bad_layout`、`capture_owner_outlives_publish_future`**。多 region/祖先非零内容对照；输入短读/overlap/unaligned/ENOSPC 拒绝；发布尚未完成时 drop caller 不释放 owner。原 FC range 和 publication-failure tests 保持。
- [ ] **Step 2：运行 `cargo test -p agentenv --lib memory_snapshot` 和 `cargo test -p agentenv --lib overlaybd_snapshot`**。Expected：中立 writer 新契约 RED，FC baseline 输出不变。
- [ ] **Step 3：抽取实际共享 writer/磁盘 seal/restack**。保留 layer budget、compression 区分、runtime upper 重接和 terminal boundary；temporary lower 原子采用，失败不能覆盖 final/durable 文件。full RAM 对照为 packed offsets，不使用 FC 固定 GPA hole 常量。
- [ ] **Step 4：重跑 focused tests 与 FC snapshot/attached-drive Linux 回归**。Expected：FC 没有行为变化，DB full lower 可由现有 OverlayBD reader逐字节读取；归还 device 前必须确认访问它的 runner 已退出。
- [ ] **Step 5：提交** `refactor: share sandbox memory and storage snapshot writers`。

## Task 7：真正 backend 的 fresh/held 启动和 readiness

**Files:** Create `src/sandbox/dragonball/{config.rs,factory.rs,sandbox.rs}`；Modify `mod.rs`、`metadata.rs` 及实际共用 helper；Test `tests/integration/dragonball_m1.rs`。

**Interfaces:** `DragonballRuntimePolicy::from_app_config(config: &AppConfig, runner: PathBuf, kernel: PathBuf) -> Result<Self>` 仅消费真实公共 machine/network/storage/guest 字段，拒绝 FC-only CPU policy。`DragonballSandboxFactory::new(policy: DragonballRuntimePolicy) -> Result<Self>` 实现现有 `SandboxBackendFactory`，`DragonballSandbox` 实现 `SandboxBackend` 和 `SandboxExecutor`。本阶段测试直接传该 factory；M3 才完成 server 的选型/安装配置。

- [ ] **Step 1：写 `dragonball_m1_fresh_ready_preserves_guest_contract`、`dragonball_m1_load_held_does_not_run_before_identity`、`dragonball_m1_unsupported_profile_allocates_nothing`**。真实 envd 与 tools vda/user vdb、USER/workdir/env/mount 验证；held 期间 guest marker不递增，metadata/限速未准备不能 Resume。
- [ ] **Step 2：在授权的非 root capability runner/隔离 namespace 中运行 `cargo test -p agentenv --test integration dragonball_m1 -- --ignored --test-threads=1 --nocapture`**。Expected：新 backend 尚未具备行为时 RED，不能只跑 fake RuntimeVmm。
- [ ] **Step 3：实现 owner-shielded 启动、逻辑盘/TAP、metadata 和 envd**。fork 新 token/hash/instance，resume 旧身份；vCPU放行前 metadata listener就绪，envd重新初始化身份后才 Ready/路由暴露。限速在放行前生效，禁用时清继承限制；扩展 start/patch 失败使操作失败、stop best-effort。
- [ ] **Step 4：加入 cancel-during-allocation、metadata/extension 失败、wrong token/neighbor/deny-all 和 forced runner exit 回归**。Expected：保留 M0 的 fail-closed owner/reap 顺序；无 capability/seccomp 回退。重跑 M0 专项与 FC guest baseline。
- [ ] **Step 5：提交** `feat: add Dragonball sandbox backend startup`。

## Task 8：一致 snapshot/pause/resume 与产物持有

**Files:** Create `src/sandbox/dragonball/capture.rs`；Modify `sandbox.rs/snapshot.rs`、共享 storage capture；Test `tests/integration/dragonball_m1.rs` 和 backend 内部 failure tests。

**Interfaces:** `DragonballPausedState` 实现现有 `PausedSandboxState`；`DragonballCaptureArtifacts` 由 `CapturedSandboxSnapshot` 持有。内部 `async fn capture(&mut self, at: &Path, keep_held: bool) -> SandboxCaptureResult<DragonballCaptureArtifacts>` 消费任务 4 token，5 envelope，6 writer；pause/capture_to_dir held，snapshot 恢复源 Running。resume materialize OverlayBD RAM 为私有普通文件，再 LoadHeld/rebind/Resume。

- [ ] **Step 1：写 `dragonball_m1_capture_under_active_io_is_one_cut`、`dragonball_m1_snapshot_source_continues`、`dragonball_m1_pause_resume_keeps_id_and_ram`**。写入数据/校验和/内存序号可恢复到同切点，无 guest fsfreeze/sleep 作前提；snapshot 返回时源工作负载继续推进。
- [ ] **Step 2：在任务 7 的非 root capability/隔离 runner 下运行 `cargo test -p agentenv --test integration dragonball_m1_capture -- --ignored --test-threads=1 --nocapture`、`cargo test -p agentenv --test integration dragonball_m1_snapshot -- --ignored --test-threads=1 --nocapture`、`cargo test -p agentenv --test integration dragonball_m1_pause -- --ignored --test-threads=1 --nocapture` 及 `cargo test -p agentenv --lib dragonball::`**。Expected：一致性/恢复性断言 RED，保留失败日志和 serial。
- [ ] **Step 3：实现 capture owner 和严格错误分类**。BeginCapture成功后持屏障直到 state、full RAM lower和所有可写盘的 seal/restack可独立引用；pre-mutation失败确认源 Running才 Recoverable；已 restack或状态未知Terminal。caller取消不取消owner；发布失败不杀已经恢复的源；pause persist成功前不冒充Paused。
- [ ] **Step 4：注入 ack timeout、Export超时/丢响应、ENOSPC、restack后失败、caller取消和reap失败**。Expected：Recoverable都实际Running、Terminal统一teardown、未确认退出时所有backing不释放；后续publish/GC不误删live/durable数据。
- [ ] **Step 5：提交** `feat: capture consistent Dragonball sandbox snapshots`。

## Task 9：fork 与卷所有权

**Files:** Modify `dragonball/sandbox.rs/capture.rs/factory.rs` 与共享 drive/storage helpers；Test `tests/integration/dragonball_m1.rs`，复用 `snapshot_attached_drive.rs`、`orchestrator/tests.rs` 的语义场景。

**Interfaces:** 现有 `fork(&[SandboxForkSpec]) -> SandboxCaptureResult<Vec<SandboxForkResult>>` 顺序契约不变；实现 `snapshot_volumes/freeze_and_snapshot_volumes/thaw_volumes`。只复用已捕获同容量/feature拓扑；M2补合法槽变化，不通过hotplug/feature开关临时绕过。

- [ ] **Step 1：写 `dragonball_m1_fork_has_independent_writable_disks`、`dragonball_m1_exclusive_volumes_fork_and_ro_share`、`dragonball_m1_failed_child_keeps_source_and_sibling_running`**。一次capture；source先Running；至少两个child在RAM同页/rootfs/attached盘/volume写不同内容，父/兄弟/新child不串写。
- [ ] **Step 2：在相同 capability/隔离 runner 下运行 `cargo test -p agentenv --test integration dragonball_m1_fork -- --ignored --test-threads=1 --nocapture` 和 `cargo test -p agentenv --test integration dragonball_m1_exclusive -- --ignored --test-threads=1 --nocapture`**。Expected：真实磁盘/身份/lease断言 RED，不只比较返回metadata。
- [ ] **Step 3：使用现有VolumeManager reservation/fork/rollback与shared RO memory/volume leases**。创建每个child独立writable upper；每child返回独立结果，错误不终止成功兄弟；mount override/volume_snapshots分离。删除源卷不能影响已发布snapshot。
- [ ] **Step 4：加入 uploading/failed 挂载拒绝、mounted删除拒绝、guest freeze失败/thaw、部分分配失败/取消和删除源卷后恢复**。Expected：child分配完全回滚，source/兄弟与exclusive ownership保持正确；最后owner才释放临时盘。
- [ ] **Step 5：提交** `feat: support Dragonball fork and volume lifecycle`。

## Task 10：paused backend 分派与异类记录保留

**Files:** Modify `src/sandbox/backend.rs`、`src/orchestrator/persistence/file_backed.rs`、DB factory/state、相关测试；保持 FC decode 与旧 root路径兼容。

**Interfaces:** factory 新默认方法 `fn supports_restore(&self, identity: &SnapshotRestoreIdentity) -> bool`，FC 保留合法旧schema默认，DB精确覆盖。paused外层新增 backend/reader identity；读取可识别外层metadata后，先检查支持再decode；DB artifact roots隔离为 `<paused-root>/dragonball/`，旧FC root不移动。

- [ ] **Step 1：写 `paused_unknown_backend_is_retained_not_decoded`、`paused_unknown_db_abi_keeps_artifacts`、`legacy_fc_paused_record_still_resumes`**。记录、layers、保护引用都保留；异类记录可见但不可恢复；正确factory可在后续重新加载，不把decode失败当GC授权。
- [ ] **Step 2：运行 `cargo test -p agentenv --lib file_persister` 和 `cargo test -p agentenv --lib paused_`**。Expected：新异类fixture在当前cleanup路径 RED，原可识别损坏旧schema的清理测试保持其合法语义。
- [ ] **Step 3：实现外层分派及保护引用**。可识别backend/ABI不支持不是损坏；未知outer schema无法解码时保留相关record/artifact且warn，不静默删。保持resuming/rollback、durability、same-ID、runtime auth/context/ext params；不改变Orchestrator shutdown的永久关闭语义。
- [ ] **Step 4：在新进程中执行 `dragonball_m1_server_restart_loads_same_paused_id`，同时重跑FC orchestrator fixture与mixed-mode持久化测试**。Expected：旧进程完全退出后新实例resume；不兼容旧进程不得scan/GC DB根。
- [ ] **Step 5：提交** `feat: preserve backend-qualified paused sandbox records`。

## Task 11：M1 活跃负载、故障和交付证据

**Files:** Modify `Makefile`、`tests/integration.rs`；Create `scripts/tests/run-dragonball-m1.sh`、`docs/src/internals/dragonball-m1-results.md`；extend `tests/integration/dragonball_m1.rs`。M1脚本不修改系统service或旧数据。

**Interfaces:** `make test-dragonball-m1` 显式准备独立kernel/tools/assets、namespace和runtime模式，缺前提失败；非rootCargo构建，已有capability委派只执行所需wrapper。默认 `make test` 保持FC；manual M1 job按能力标签运行，不让普通PR带任意root权限。

- [x] **Step 1：写 repeat-capture I/O 验收和故障用例**。负载至少覆盖持续块写+checksum、RAM递增、RX/TX、两个并发child、1/2/8GiB、4GiB以上GPA hole；反复snapshot和pause/resume后验证内容/队列进度。写 `m1_runner_death_retains_leases_until_reap`、`m1_publish_failure_does_not_stop_source`、`m1_record_mismatch_allocates_nothing`。
- [x] **Step 2：跑完整新suite，确认上述未实现检查各自RED**。Expected：不是因网络/依赖缺失而未运行；记录因真实行为不满足的断言。前任务已GREEN的同场景无需为凑流程重复RED。
- [x] **Step 3：补齐实际缺口和Make/手动CI入口**。只为真实失败修改相关owner；所有guest ready/auth、worker ack、flush、source Running、child隔离、lease释放均有可观测证据。保留full export普通文件对照，不声称M2完成。
- [x] **Step 4：执行 `make fmt clippy test test-unit`、`make test-dragonball-m0`、`make test-dragonball-m1` 及相关attached-drive/volume/ublk真实过滤测试**。Expected：退出0，明确普通ignored与显式执行清单。POSIX publication用于M1端到端；完整OSS/P2P/registry/产品配置矩阵在M3，但现有FC suites不得退化。修改Go时另跑规定的services检查。
- [x] **Step 5：一次完整M1分支独立审查并修复Critical/Important**。证据文档包含实际forkSHA/profile/CPU/布局、full-memory对照、命令退出码/日志、错误与取消分类、所有lease、未覆盖项和审查rulings；撤回临时root/Docker委派，确认旧service仍卸载。提交 `test: qualify Dragonball active-io lifecycle`，M1满足全部退出门槛后再形成M2详细计划。

## 自审与执行门槛

对应设计：§5进程/协议→4/7/8；§6.1屏障→1–3/8；§6.2 correctness/full-memory→3/6/8；§7生命周期/卷→7–10；§8身份/设备/CPU→3/5/7/9；§9逻辑持久化/兼容→5/8/10；§10最小fork/build→1–4/11。§6.2增量、6.3lazy ublk、6.4balloon/recording及可变槽留M2；server/template/packaging/完整仓库留M3；灰度性能/退役见路线，不从M1计划丢失。

所有任务共享同一CaptureToken/MemoryLayout、portable identity和现有backend签名；review focus五项分别有明确测试owner。任务1–3Kata补丁必须独立验证再pin，任务4不能拿未实现能力构造假的Hello。M1是可验收的非默认实现，不是静止probe改名。用户已于 2026-10-01 审阅并确认此计划与 Native 执行；进度和真实检查见 [M1 证据](./dragonball-m1-results.md)。
