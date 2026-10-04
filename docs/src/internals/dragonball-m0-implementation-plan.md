# Dragonball M0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在独立 runner 中接入固定版本 Dragonball，验证现有 AgentENV kernel/tools、块盘、TAP、envd 鉴权和静止点全量快照恢复，为 M1 的运行中一致性工作提供可重复证据。

**Architecture:** 每个验证 VM 使用一个 `aenv-dragonball` 进程，由节点通过私有 UDS 管理。验证 harness 复用 NetworkManager、UblkDeviceManager 和 envd，元数据服务绑定到对应 namespace。M0 只提供明确标记的资格验证入口，生产 factory 和默认后端在完整能力验收前继续使用 Firecracker。

**Tech Stack:** Rust 2021、Dragonball Rust library、Linux x86_64/KVM、现有 Tokio/Serde、virtio-mmio、RawBlock、用户态 virtio-net、OverlayBD/ublk、现有 envd 客户端。

**Spec:** [已确认的替换设计](./dragonball-hypervisor-design.md)，用户确认日期 2026-09-30。AgentENV 基线 `875faeecfbc2c69eed709de06764003b86ac5c28`；Dragonball 固定到 Kata Containers `cd7051a8dbc0f8e0ac5a63ee4c3884fe8b4ac894`。

**Implementation pin:** 用户已提供 `denverdino/kata-containers` fork；针对上述基线修复最小 profile 的条件编译后，实际依赖固定为 `bfc470b07aedaeddaf801ff091f5cdb1a12c69bc`。所有构建身份断言核对实际修复提交，不继续把未修补的审查基线作为实现身份。

**资格范围修订：** 原 Firecracker kernel 实测失败；按设计 8.3 使用独立 `6.1.175-dragonball-m0.1` candidate 验证，原 tools 未改。候选 kernel 未发布，也未替换默认资产。metadata 的 in-place `replace` 只编入契约测试，M0 恢复采用新的 listener/authority；M1 接入复用时再提升为运行时接口。实际 Make 的 sudo 委派阻塞与等价非 root VM 运行证据分别记录，不把整项 Make 标为通过。

## Global Constraints

- 对外 E2B API 和已有 sandbox 生命周期语义保持兼容。
- 不迁入 Kata runtime、kata-agent、containerd 或 Kata 的镜像管理链路。
- 第一阶段资格范围为 Linux x86_64 + KVM；aarch64 checkpoint 和 PVM 为后续独立资格项。
- 不承诺 Firecracker 与 Dragonball 的内存/设备快照互读。
- 设备 profile：virtio-mmio、RawBlock、用户态 virtio-net、串口；M0 关闭 hotplug、VFIO、vhost、virtio-fs、virtio-mem、balloon。关闭 balloon 表示能力未齐。
- 保留 tools `vda`、用户盘 `vdb`、physical extras、volume slots 的顺序；生产 slot 变更支持留给后续阶段。
- 使用完整上游仓库的固定 git revision，不复制单个 Dragonball 目录，不引入 `kata-hypervisor` 整个 crate。
- 所有 privileged tests 使用 Makefile 的 capability runner 和独立 `AENV_TEST_STATE_DIR` / `AENV_TEST_DEPS_PATH`，非 root 执行。
- M0 不提供通用运行中 snapshot、公开 pause/fork 或生产 `SandboxBackend`；静止点全量导出不构成这些能力的验收证据。
- 文件和接口名称是本实施计划的决定；发现必须改变已确认设计的情况时，记录证据后重新评审对应设计。

## Review Focus

1. UDS 帧截断、超限或回应 instance ID 不匹配：拒绝继续解码/误归属，且不执行 VMM action。任务 2 固定断言。
2. runner 启动到一半超时、父进程退出或停止失败：先确认进程退出，再归还设备和 namespace。任务 3、5 固定断言。
3. namespace slot 被新实例复用但 guest 缓存旧 metadata token：拒绝旧 token，仍支持读取本实例数据。任务 4 固定断言。
4. PVM、非认证 CPU policy、重复设备 ID 或磁盘缺失：在启动 guest 前失败，不静默降级。任务 3、5 固定断言。
5. 恢复时状态文件损坏、机器布局不同或调用者取消：拒绝错误状态；失败实例和后台 probe 最终完成清理。任务 6 固定断言。

---

## 交付边界与后续拆分

这是设计 M0 的完整实施计划。M1–M5 与 M0 有依赖，设备屏障、回收和卷槽的实现细节要用 M0 证据细化，分别形成后续计划。下列映射防止把 M0 完成误记为整个替换完成：

| 阶段 | 后续计划范围 | 对应设计章节 / 本阶段门槛 |
| --- | --- | --- |
| M0 | 本文任务 1–7 | 3–5、8、10、12；独立 runner、guest、元数据和静止点恢复证据 |
| M1 | worker capture 屏障、held restore、全量 OverlayBD、paused store 和 fork | 6.1–6.2、7、9；活跃 I/O 和失败恢复全部通过 |
| M2 | ublk backing、累计 dirty/zero、balloon persist/COW、slot restore、startup recording | 6.2–6.4、8.1；full/diff 对照、共享内存和卷正确性 |
| M3 | 正式 factory/模板、版本/仓库、配置/依赖/运维集成 | 2、5.2、8.3、9–10；FC 回归与 DB POSIX/OSS/扩展/P2P |
| M4 | 分池灰度、模板重建、性能与回滚验证 | 11、13–14；正确性全通过，冻结性能门槛后评估 |
| M5 | FC 代码/资产退役 | 11–12；存量恢复依赖归零或已明确保留 |

首版采用分池入口，因此混合调度 protobuf 修改不属于上述首版必需工作。M4 不自动授权部署、生产流量调整或资源删除；实施前应提供具体批次和回滚记录。

## 文件结构与依赖

| 路径 | 职责 | 首次引入 |
| --- | --- | --- |
| `crates/dragonball-runner/{Cargo.toml,build.rs,src/lib.rs,src/main.rs}` | 构建身份、CLI 和 runner 入口 | 任务 1 |
| `crates/vmm-protocol/{Cargo.toml,src/lib.rs,src/frame.rs,src/probe.rs}` | AgentENV 自有协议、长度边界、M0 probe DTO | 任务 2 |
| `crates/dragonball-runner/src/{service.rs,vmm.rs,process.rs,seccomp.rs}` | 请求校验、Dragonball action 翻译、进程退出、过滤器 | 任务 3 |
| `src/sandbox/{metadata.rs,network/metadata.rs}` | 共用 metadata 值和 namespace 服务 | 任务 4 |
| `src/sandbox/dragonball/{mod.rs,client.rs,process.rs,probe.rs}` | 仅供资格验证的节点 client、runner guard 和资源 harness | 任务 5 |
| `crates/dragonball-runner/src/probe_snapshot.rs` | 静止点保存/恢复，局部临时产物 | 任务 6 |
| `tests/integration/dragonball_m0.rs` | Linux KVM/ublk/netns/envd 验证 | 任务 5–6 |
| `docs/src/internals/dragonball-m0-results.md` | 实测身份、输入、通过/失败证据与后续改动清单 | 任务 7 |

依赖顺序：1 → 2 → 3；4 在 2 后可完成；5 依赖 3、4；6 依赖 5；7 汇总全部结果。共享文件变更按此顺序落地；执行方式另行选择。

## Task 1：可构建的 runner 与真实构建身份

**Files:** Create `crates/dragonball-runner/Cargo.toml`、`build.rs`、`src/lib.rs`、`src/main.rs`；Modify `Cargo.toml`、`Cargo.lock`；Test `crates/dragonball-runner/tests/identity_cli.rs`。

**Interfaces:** 产生 `RunnerBuildIdentity { protocol_version: u16, runner_version: String, dragonball_revision: String, feature_profile: String }` 和 `pub fn build_identity() -> RunnerBuildIdentity`。M0 profile 字符串固定为 `m0-mmio-blk-net`；协议版本 `1`。CLI `aenv-dragonball --version-json` 输出单个 JSON 对象，不打开 KVM。

- [x] **Step 1：写 `identity_cli_reports_pinned_revision_without_kvm`**。启动 `--version-json`，断言正常退出、revision 完全等于固定的 40 位 SHA、`protocol_version == 1`、profile 正确、输出没有路径或 token；测试环境不给 KVM 访问权限。
- [x] **Step 2：运行 `cargo test -p agentenv-dragonball-runner --test identity_cli`**。新命令/接口不存在时应失败；记录原始输出，不能将未构建的测试写为已通过。
- [x] **Step 3：实现最小 CLI 和身份接口**。新增 Rust 2021 package `agentenv-dragonball-runner`，binary 名 `aenv-dragonball`。使用上游 git revision 固定 `dragonball`，仅开 `virtio-blk`、`virtio-net`；不继承 Kata runtime 的 feature 列表。根 package 不增加到 Dragonball 的依赖，避免互相依赖。
- [x] **Step 4：执行身份测试、`cargo build -p agentenv-dragonball-runner` 和 `cargo tree -p agentenv-dragonball-runner`**。预期构建成功且依赖树不含 Kata runtime/hypervisor。若上游 workspace/path 依赖不能从固定 git 包构建，先保存实际 Cargo 诊断，再为缺失项制定最小集成修订，不填入虚构 fork revision。
- [x] **Step 5：审查并提交本任务的准确文件集合**。建议提交信息 `feat: add pinned Dragonball runner build`，包括 lockfile；不提交整个临时上游仓库。

## Task 2：私有 UDS 协议及请求归属

**Files:** Create `crates/vmm-protocol/Cargo.toml`、`src/lib.rs`、`src/frame.rs`、`src/probe.rs`、`tests/frames.rs`；Modify 根 `Cargo.toml` 与 runner `Cargo.toml`/`Cargo.lock`。

**Interfaces:** package `agentenv-vmm-protocol`。产生 `RequestEnvelope { protocol_version: u16, request_id: u64, sandbox_instance_id: String, budget_ms: u64, command: ProbeCommand }` 和 `ResponseEnvelope { protocol_version: u16, request_id: u64, sandbox_instance_id: String, result: Result<ProbeReply, ProbeError> }`。`ProbeReply` 包含 `Hello(RunnerIdentity)`、`Status(ProbePhase)`、`Done`；`ProbePhase` 为 `Unconfigured/Configured/Running/Paused/Stopped/Failed`，错误 DTO 为 `{ code: String, message: String }`。`RunnerIdentity` 是任务 1 构建身份的协议值，不依赖 runner crate。

编解码接口：`pub fn encode_frame(value: &RequestEnvelope) -> Result<Vec<u8>, ProtocolError>`、`pub fn decode_request(bytes: &[u8]) -> Result<RequestEnvelope, ProtocolError>`、`pub fn encode_response(value: &ResponseEnvelope) -> Result<Vec<u8>, ProtocolError>`、`pub fn decode_response(bytes: &[u8]) -> Result<ResponseEnvelope, ProtocolError>`、`pub fn validate_response(request: &RequestEnvelope, response: &ResponseEnvelope) -> Result<(), ProtocolError>`。DTO 不含上游 Rust 类型。

`ProbeCommand` 仅包括 `Hello`、`Configure(ProbeVmConfig)`、`Start`、`Pause`、`Resume`、`SaveQuiesced { state_path: PathBuf, memory_path: PathBuf }`、`RestoreQuiesced { state_path: PathBuf, memory_path: PathBuf }`、`Status`、`Stop`。后两种 snapshot 命令须在输出/能力中标明 `probe_only`，与未来生产 capture 协议区分。

`ProbeVmConfig` 包含 `vcpu_count: u8`、`memory_mib: u32`、`kernel_path: PathBuf`、`boot_args: String`、有序 `Vec<ProbeBlockDevice>` 和 `ProbeNetworkDevice { iface_id, tap_name }`；block DTO 包含 `drive_id`、`path_on_host`、`read_only`、`is_root`。配置不携带 registry/OSS 凭证或 guest access token。

- [x] **Step 1：写帧测试**。`request_roundtrips_with_instance_identity` 保留全部归属字段；`truncated_frame_is_rejected` 拒绝不足 4 字节 header 和缺少 body；`oversized_frame_is_rejected_before_allocation` 拒绝 `1_048_577` 字节 body；`unknown_protocol_version_is_rejected` 拒绝版本 `2`；`response_from_other_instance_is_rejected` 不接受错误归属的 response。
- [x] **Step 2：运行 `cargo test -p agentenv-vmm-protocol --test frames`**，确认针对尚未实现的检查失败。
- [x] **Step 3：实现长度前缀 JSON 协议**。前缀为 4 字节大端 body 长度；最大 body `1_048_576` 字节；读流先检查长度再分配。拒绝零预算、空 instance ID 和消息尾部多余字节。请求/响应类型分开，client 用发送前登记的 `(request_id, sandbox_instance_id)` 验证 response。
- [x] **Step 4：运行全部协议测试**，预期每个恶意/损坏输入返回明确错误，VMM 不参与测试。对最大合法 body 的边界补充一次 roundtrip，不写只复制实现的快照测试。
- [x] **Step 5：审查并提交本任务文件**。建议 `feat: add private VMM probe protocol`。

## Task 3：runner 调用 Dragonball 与进程回收

**Files:** Create runner `src/service.rs`、`src/vmm.rs`、`src/process.rs`、`src/seccomp.rs`、`tests/service.rs`、`tests/parent_exit.rs`；Modify runner `src/lib.rs`、`src/main.rs`。

**Interfaces:** 消费任务 1–2。内部测试 seam `trait ProbeVmm { fn configure(&mut self, config: &ProbeVmConfig) -> anyhow::Result<()>; fn start(&mut self) -> anyhow::Result<()>; fn pause(&mut self) -> anyhow::Result<()>; fn resume(&mut self) -> anyhow::Result<()>; fn stop(&mut self) -> anyhow::Result<()>; }`；`RunnerService<V: ProbeVmm>::handle(&mut self, request: RequestEnvelope) -> ResponseEnvelope`。真实实现 `DragonballVmm` 以 channel/eventfd 调用 `VmmService`，只在 runner 内认识 `VmmAction`。

- [x] **Step 1：用 fake VMM 写状态与回收测试**。`wrong_instance_never_reaches_vmm`、`second_start_does_not_boot_twice`、`stop_is_idempotent`、`configure_rejects_duplicate_device_ids` 断言 action 调用次数；`parent_exit_terminates_runner` 启动专用父进程并使其退出，断言 runner 退出且可 reap；`startup_failure_joins_started_workers` 注入部分初始化失败，断言不遗留线程/FD。
- [x] **Step 2：运行 `cargo test -p agentenv-dragonball-runner --test service --test parent_exit`**，确认行为测试先失败。
- [x] **Step 3：实现串行 dispatch 和真实 VMM adapter**。CLI `--socket <path> --instance-id <id> --work-dir <path>`；节点在 spawn 前使子进程进入 netns，不要求 runner 对其他线程调用 setns。socket 为 mode `0600`、工作目录 `0700`，校验 peer UID。初始化完成后才报告 Hello ready；`Hello` 仅声明已实现的能力，任务 6 前 snapshot 命令返回 `unsupported_probe_command`。service 自身记录 phase；重复 request ID 不再次执行 action，失败不能伪报 Running。在 KVM/设备 worker 运行前安装涵盖当前配置的 seccomp；不复制包含无关设备 syscall 的 profile。
- [x] **Step 4：运行单元测试与 Linux 无 capability 的 runner 检查**。预期 fake 测试全部通过；缺 `/dev/kvm` 返回明确错误并退出；`--version-json` 仍不访问 KVM。父进程退出检测须处理登记前父已退出的竞争条件；Stop deadline 到期的清理由节点强制终止并 wait，不能报告 clean stop。
- [x] **Step 5：审查并提交**，建议 `feat: run Dragonball behind an isolated VMM service`。真实 VM 证据由任务 5 补齐，不能将 fake 测试等同 KVM 通过。

## Task 4：metadata 值复用及 namespace listener

**Files:** Create `src/sandbox/metadata.rs`、`src/sandbox/network/metadata.rs`；Modify `src/sandbox/mod.rs`、`src/sandbox/network/mod.rs`、`slot.rs`、`policy.rs` 和 Firecracker `mmds.rs`/`config.rs`/`instance.rs`/`sandbox.rs`；Test 新模块内单元测试和现有 MMDS tests。

**Interfaces:** 将已有 `MmdsMetadata` 的值构建逻辑迁为 `SandboxMetadataDocument`，保留现有 serde 字段、SHA512 和 extras 保护；Firecracker 仍传同样 JSON。`MetadataListener::start(netns: &Path, instance_id: &str, document: SandboxMetadataDocument) -> Result<MetadataListener>`、`replace(&self, instance_id: &str, document: SandboxMetadataDocument) -> Result<()>`、`async fn stop(&mut self) -> Result<()>`。listener 的 instance ID 从宿主绑定而来，token 与该代实例绑定。

- [x] **Step 1：写 `metadata_preserves_reserved_fields_and_hash`、`cached_token_is_rejected_after_instance_replacement`、`expired_token_is_rejected` 和 `listener_stop_releases_namespace_reference`**。保持 `instanceID`、`envID`、`address`、`accessTokenHash`；extras 不能覆盖这四项。用受控时钟发 TTL 为 2 秒的 token，在 3 秒后拒绝；replace 后旧 token 拒绝、新 token 只读新数据。
- [x] **Step 2：运行 `cargo test -p agentenv --lib metadata`**。确认新 listener 语义测试失败，并保存已有 MMDS tests 的基线结果。
- [x] **Step 3：实现值迁移与 namespace 服务**。使用已有 HTTP 解析/服务依赖，不增加第二套网络框架；监听 guest metadata 地址。MMDS v2 路径、header、TTL 上限与数据格式以现有固定版本 Firecracker 和实际 guest 的录制请求为契约，保存到测试 fixture；不自行引入 AWS 扩展。限制 JSON 数据为既有 `1_048_576` 字节，token 响应不得返回 envd 明文凭证。
- [x] **Step 4：通过单元回归，再在任务 5 验证真实网络**。namespace 中 metadata 规则先于 80/443 egress proxy 重定向，仅 TAP 可达；deny-all 不妨碍本实例读取。正常停止和异常 rollback 均先停 listener，再释放 namespace，pool 复用时不能继承旧数据/连接。现有 Firecracker metadata 序列化输出必须保持一致。
- [x] **Step 5：审查并提交**，建议 `feat: add namespace-scoped sandbox metadata service`。guest 对协议的实际兼容证据由任务 5 记录。

## Task 5：用现有 kernel/tools 启动资格验证 VM

**Files:** Create `src/sandbox/dragonball/mod.rs`、`client.rs`、`process.rs`、`probe.rs`、`tests/integration/dragonball_m0.rs`；Modify `src/sandbox/mod.rs`、`tests/integration.rs`、`tests/common/mod.rs`、`Makefile`、根 `Cargo.toml`/`Cargo.lock`。

**Interfaces:** 消费任务 2–4 及已有资源 manager。仅公开资格验证句柄 `DragonballM0Probe`；`pub async fn start(config: DragonballM0ProbeConfig) -> Result<Self>`、`pub fn executor(&self) -> Result<Executor>`、`pub async fn stop(&mut self) -> Result<()>`。`Executor` 复用 `src/sandbox/process.rs`，不新增执行协议。

`DragonballM0ProbeConfig` 包含 `runner_path/kernel_path/tools_path/image_path/work_root: PathBuf`、`vcpu_count: u8`、`memory_mib: u32`、`sandbox_instance_id: String`、`metadata: SandboxMetadataDocument`、`network_policy: SandboxNetworkPolicy`、`virtualization_mode: VirtualizationMode` 与 `cpu_policy: String`。image_path 指向现有 resolver 生成的 OverlayBD 配置，harness 持有其 ublk lease；vda/vdb 及预留槽由现有设备准备逻辑生成，不由调用者随意重排。M0 仅接受 `cpu_policy == "host-default"`；架构从编译目标验证，验证 helper 可注入架构值供非 x86_64 拒绝测试。内部 `RunnerProcessGuard` 持有 child，`async fn terminate_and_wait(&mut self, deadline: Duration) -> Result<()>` 在返回前确认进程终止。

- [x] **Step 1：写真实 integration tests**。`dragonball_m0_cold_start_uses_existing_guest` 通过 envd 执行命令、验证 `vda`/`vdb` 与 mount；`dragonball_m0_secure_token_and_metadata_match_instance` 验证正确 token 成功、错误 token 失败、identity 和 imageConfigs 正确；`dragonball_m0_metadata_survives_deny_all` 验证 policy。fake process tests `failed_spawn_releases_resources_after_exit` 断言退出确认先于 device/slot release；`unsupported_mode_fails_before_resource_allocation` 对 PVM、非 x86_64 和不支持 CPU policy 断言零分配。
- [x] **Step 2：先运行 fake/resource-validation tests**。`cargo test -p agentenv --lib dragonball::` 应因缺少接口/顺序约束而失败。真实 integration tests 需要任务 1 的 runner 和 Linux prerequisites，预期尚未能启动，记录具体失败原因。
- [x] **Step 3：实现 harness，复用资源而不接入生产 factory**。分配网络 slot、metadata listener、tools 根盘、用户镜像 ublk、physical extras 和既有预留槽布局；以当前 Linux runtime 用户 spawn runner，进入 slot netns 后再 exec。传稳定 drive/iface ID。metadata 在 Start 前就绪，envd init/ready 成功后返回句柄；失败反序回收且先终止 runner。将 shared metadata listener 接入 slot 清理；不增加默认 Dragonball 后端配置。
- [x] **Step 4：增加并运行 `make test-dragonball-m0`**。该目标沿用 `prepare-agent-test-state`、debug ublk 构建、`CAPABILITY_TEST_ENV` 和 `CAPABILITY_RUNNER`，先运行对应单元测试，再执行 `cargo test -p agentenv --test integration dragonball_m0 -- --ignored --test-threads=1`。真实 VM tests 明确标记 Linux/x86_64 + `#[ignore]`；显式选择该目标但缺 KVM/ublk/assets 时必须报错，不能 skip 后显示通过。还验证另一个 namespace 不能读取目标 metadata。
- [x] **Step 5：审查并提交**，建议 `feat: add Dragonball guest qualification harness`。成功应有 kernel/tools digest、runner revision、UID/capabilities、命令结果和 cleanup evidence；只到 envd health 不算 guest/鉴权/存储验证完成。

## Task 6：静止点全量 snapshot/restore 的限定验证

**Files:** Create runner `src/probe_snapshot.rs`、`tests/probe_snapshot.rs`；Modify runner `service.rs`/`vmm.rs`、节点 `probe.rs` 和 `tests/integration/dragonball_m0.rs`。临时 probe 产物不进入 committed repository 或 paused store。

**Interfaces:** 扩展内部 probe trait：`fn save_quiesced(&mut self, state: &Path, memory: &Path) -> Result<()>`、`fn restore_quiesced(&mut self, state: &Path, memory: &Path) -> Result<()>`。节点 `pub async fn pause_probe(&mut self) -> Result<()>` 仅供已建立静止条件的 fixture 调用，不承诺 drain；`pub async fn save_quiesced(&mut self, output_dir: &Path) -> Result<QuiescedProbeArtifacts>`；artifacts 持有目录 lease、原始 state/memory 路径与完整 machine/device 配置；`pub async fn restore_quiesced(config: DragonballM0ProbeConfig, artifacts: &QuiescedProbeArtifacts) -> Result<Self>` 创建新 runner。

- [x] **Step 1：写限定行为测试**。`save_quiesced_requires_paused_probe` 拒绝 Running；`quiesced_restore_rejects_corrupt_or_mismatched_state` 拒绝截断 JSON、不同 region/vCPU/profile；`probe_cancel_keeps_cleanup_owned` 取消调用者后 worker 仍完成终止/cleanup。真实 test `dragonball_m0_quiesced_full_restore` 使用受控 guest fixture，保存 RAM marker 和 CPU 执行位置，恢复后检查 marker、继续执行、磁盘只读内容与 metadata 重新绑定；比较原始 memory 文件 checksum，证明新 guest 写入不改源镜像。
- [x] **Step 2：运行 `cargo test -p agentenv-dragonball-runner --test probe_snapshot`**。接口不存在/状态限制缺失时应失败；先用 fake 验证拒绝行为，不依赖 KVM。
- [x] **Step 3：实现 probe-only 调用**。输出目录必须位于本实例工作根内，禁止跟随逃逸 symlink 或覆盖已有 artifact。kernel/tools、`/dev/ublk*` 和恢复源不强求位于工作根：只接受节点配置中已解析并明确授权的输入，恢复期间保留原 artifact lease。显式 Pause 后调用上游 `SaveMicrovm`，恢复保持相同 machine/设备集合。输出身份中保留 revision/profile；新 runner 使用上游 `StartMicroVmFromSnapshot`。该上游 restore 自动放行 vCPU，所以在调用前准备新 namespace、metadata 和磁盘；外部入口仅在 envd init/ready 后开放。
- [x] **Step 4：运行任务 5 的完整目标**。snapshot fixture 使用已确认静止的设备、稳定只读磁盘内容和受控 RAM 工作负载；在结果中列明静止条件与观察到的限制。若无法建立可重复的静止条件或 restore 失败，M0 结果明确为不通过；不能将随机无错误运行视为设备一致性证明。验证文件 IO 错误后 runner 状态可查且 cleanup 完成。
- [x] **Step 5：审查并提交**，建议 `test: qualify Dragonball quiescent checkpoint restore`。不以此提交声称通用 snapshot/pause/fork 支持已完成；M1 必须加入设备屏障和 held restore 后重新验证。

## Task 7：Linux 检查入口、结果归档与 M1 输入

**Files:** Modify `.github/workflows/ci.yml`、`.github/workflows/integration-tests.yml`、`Makefile`；Create `docs/src/internals/dragonball-m0-results.md`；Modify `docs/src/SUMMARY.md` 和本计划进度。

**Interfaces:** 消费任务 1–6；交付可复现的 build/test 命令、M0 成功或失败报告及按源码位置列出的 M1 补丁需求。增加 runner/protocol 的普通 CI tests；真实 VM target 仅在满足 KVM/ublk/netns 条件的环境执行。

- [x] **Step 1：检查 CI 是否实际覆盖新增 crates**。确认现有 `make test-unit` 不自动跑这两个 crate；更新 unit job 或 Make target，使 `cargo test -p agentenv-vmm-protocol` 和 `cargo test -p agentenv-dragonball-runner` 纳入必需检查。报告应区分 compile、fake、guest 和 checkpoint 四类证据。
- [x] **Step 2：运行 `make fmt`、`make clippy`、新增 crate 单元测试、相关 metadata/resource unit tests 和 `make test-dragonball-m0`**。同时按配置跑现有 FC relevant integration 与 `make test`；M0 没改 services，无需声称已验证 Go 行为。缺硬件/权限/资产时记录具体阻塞检查；不将其标为通过。
- [x] **Step 3：写真实结果文档**。记录 commit/profile、CPU/内核/UID/capabilities、kernel/tools/image digest、测试命令与退出码、start/ready/stop 时刻和清理资源；失败保留根因、重现命令及相应上游文件。记录 `process_vm_readv` 资格条件、内存块 backing 校验、worker drain/held restore、balloon 和 slot 的后续补丁范围；没有运行的项标明“未测”，不推断成功。
- [x] **Step 4：完成 M0 review**。复核全部 checkbox 与结果；更新 M1 计划所需的实际接口/补丁点，确保用例没有把静止点恢复提升为任意活跃 VM 支持。M0 没达到必需项时继续诊断，不能进入默认后端切换。
- [x] **Step 5：审查并提交准确文件集合**，建议 `ci: verify and report Dragonball M0 qualification`。报告成功时只宣称 M0 接入资格验证成功。

## 本计划的完成条件

- runner 以固定 revision 可构建，build identity 可读，private protocol 的归属与边界测试通过。
- 现有 kernel/tools、用户镜像 ublk 和 TAP 能启动受控 VM，envd 命令、身份、token 和 metadata 隔离通过。
- 有明确静止条件的 full snapshot 可由新 runner 恢复，文件映射 COW 验证通过，损坏输入被拒绝。
- 启动失败、停止超时、父退出、调用者取消不遗留活跃 runner、ublk lease、namespace listener 或 slot。
- FC 行为与现有默认配置仍有回归证据，M0 结果真实归档，M1 改动点已具体到源码。

2026-09-30 原生执行：任务 1–6 已提交并通过专项验证（`27d514d`、`77ebdb3`、`fcbf351`、`8363e9a`、`a82e105`、`eb2d4b8`）；任务 7 的 CI/结果归档与专项复验已完成，最终独立全分支审查为另一个交付 gate。checkbox 表示步骤已执行，包括有明确阻塞记录的 Make 尝试，不表示未运行的聚合 suite 已通过，详见[实测记录](./dragonball-m0-results.md)。M0 完成不授权默认后端切换或 M1–M5 的生产部署。

## 执行交接

建议选择原生执行，由当前 agent 按任务 1–7 顺序实施；任务之间共享协议、进程所有权和 namespace 生命周期，连续执行便于及时校验接口。也可以选择分任务子代理实施与逐任务独立审查。两种方式均需先审阅本计划，再开始实现；实现开始时使用隔离工作区，遵循项目测试与提交规范。
