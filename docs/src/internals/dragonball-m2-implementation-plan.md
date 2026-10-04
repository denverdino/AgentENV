# Dragonball M2 生产内存与设备实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. 用户已确认自动执行 M2–M5；保持 Native 方式，无需逐项重复确认。步骤使用 checkbox 跟踪，阶段末只做一次完整分支独立审查。

**Goal:** 在已验收的 M1 一致性屏障上，实现共享只读 ublk RAM 的私有 COW、固定基底累计增量、balloon 回收、合法卷槽恢复和 startup manifest，并以 full dump 逐字节验证。

**Architecture:** runner 保有真实 CPU、设备、映射和 dirty 集合；节点持有 capture/read lease，通过现有 ProcessVmReader 和 OverlayBD writer 导出范围。所有设备、磁盘和 RAM 仍属于同一个 CaptureToken。复用 UblkDeviceManager、VolumeManager 和现有仓库，不新建内存长期格式，不在本阶段接入默认 server。

**Tech Stack:** Rust 2021、现有锁定依赖（vm-memory 0.17.1）、Linux x86_64 KVM、Tokio、OverlayBD/ublk、现有 guest/envd；不接入 Kata shim/runtime。

**Spec:** [已确认总体设计](./dragonball-hypervisor-design.md) §6、§8.1、§9、§12–13；[M1–M5 路线](./dragonball-m1-m5-roadmap.md)。M1 基线为 AgentENV `e0150eb`、Kata `0bc22d5ce53a072aabf4fe42c133dacefeaa9c72`，实测见 [M1 结果](./dragonball-m1-results.md)。本计划已获确认，M2 实施中；不表示阶段已完成。

## Global Constraints

- Linux x86_64、KVM、host-default CPU、单 NUMA；本阶段只认证 4096 字节 host/guest page、既有 packed layout 与 GPA hole。PVM、aarch64、hugetlb、prealloc 和新 DMA 设备在分配前拒绝。
- 保留 M1 full-memory 普通文件路径作为显式 oracle；生产增量路径失败不得静默伪装成功或降级成 full 性能结果。默认 server 仍为 Firecracker，完整 OSS/P2P/模板装配在 M3。
- “每个运行实例保留‘自冷启动或最近 load 起’的累计 dirty/zero 集合”；普通 capture 成功或失败不清空累计 dirty，不做滚动基底。zero 表示当前确认为零的页，后续写入必须撤销，不能用空洞继承旧非零 lower。
- “host_base 仅用于节点在 capture 期间读取其子进程内存，不能写入 committed manifest”。读取前验证真实 PID/启动身份、runner nonce、token 和 layout；全部读任务结束前禁止解除 read pin 或释放 backing。
- 只读 memory device 必须 MAP_PRIVATE；禁止对共享 lower/device 做 punch hole、MADV_REMOVE 或写入。最后一个 mapping、读取任务、prefetch/recording lease 退出后才可释放设备。
- 每次 capture 保留 M1 CPU/全部 worker ack、状态未知不重试 mutation、已 seal/restack 后失败为 Terminal、Recoverable 必须确认 Running 的规则。保留审查修复的 immutable volume generation 与 API allocation/rollback owner。
- 逻辑 slot、MMIO/IRQ、queue 和 negotiated features 不随新卷 ID 改写；容量只在明确 slot 协议下改变并通知 guest，实际 RO 必须由打开权限及写请求拒绝执行。
- AgentENV 沿用当前隔离 worktree；Kata 从 M1 pin 新建 `.worktrees/kata-dragonball-m2`、topic `agentenv/dragonball-m2`，只向 `denverdino/kata-containers` 推送已验证提交；不推 upstream/main，不编辑 Cargo checkout cache。
- 在 `147.139.168.159` 上使用独立 M2 config/state；Cargo/tests 为 aenv UID999。root 仅用于既有 capability 委派和 scoped 故障准备，临时权限结束撤回，旧 service/数据/卸载备份不动。不同 guest suite 串行执行。
- 测试 pin/build/profile 必须可追溯；未知 M1/M2 ABI 记录保留且明确拒绝，不删除记录以通过测试。更新 pin 前保存 M1 reader 二进制、构建身份及校验和到任务证据目录。

## Review Focus

1. read-map 已返回，但调用方取消、PID 重用或 runner 先退出：不得读新进程或过早释放设备。Task 2、6、10 覆盖。
2. AIO/raw iovec 绕过 vm-memory bitmap，或 dirty-log 读取后发布失败：不能漏设备页或丢历史写入。Task 2、3、7 覆盖。
3. 非零祖先上的 free-page reporting、inflate/deflate、再次写入：不得污染兄弟，零标记不能遮盖新写入，RSS 回收必须真实。Task 4、7、10 覆盖。
4. slot backing 改 RO/容量时，旧 queue 与已协商 feature 仍生效：不能仅改 JSON、只依赖 guest mount-ro 或改变盘序。Task 5、8 覆盖。
5. 主发布取消或完成，recording/prefetch 仍在阻塞读取：最后 lease 不得提前删除 backing，失败不得损害已提交 snapshot。Task 9、10 覆盖。

## 文件与接口归属

下文 `Kata/` 表示新的完整 Kata worktree，其他路径相对 AgentENV；列出的新增接口均为拟实施契约，不声称上游已提供。

| 边界 | 文件与责任 |
| --- | --- |
| RAM backing / dirty | `Kata/src/dragonball/src/address_space_manager.rs`；新增同级 `memory_tracking.rs`，只管理固定布局、累计页集合和受检映射 |
| VMM held export | `Kata/src/dragonball/src/{snapshot/capture.rs,vm/capture.rs,api/v1/vmm_action.rs}`；state-only、read pin、范围查询 |
| 用户态写页 | `Kata/src/dragonball/crates/dbs_virtio_devices/src/{block/handler.rs,block/request.rs,net.rs}`；真实完成路径与 queue bitmap |
| balloon | `Kata/src/dragonball/crates/dbs_virtio_devices/src/balloon.rs`、`src/device_manager/{balloon_dev_mgr.rs,capture.rs,persist.rs}` 和 snapshot DTO；冻结、持久化、私有回收 |
| volume slot | Kata block device/manager/persist；AgentENV `extra_drive.rs`、`dragonball/{factory,snapshot,sandbox}.rs`、manifest/restore_identity；显式槽位绑定 |
| wire / runner | `crates/vmm-protocol/src/{runtime.rs,runtime_memory.rs}`、`crates/dragonball-runner/src/{runtime_service.rs,runtime_snapshot.rs,vmm.rs,seccomp.rs}` |
| 节点内存 | 新 `src/sandbox/{process_vm_reader.rs,dragonball/memory.rs}`；`dragonball/{capture,sandbox,probe,paused,runtime_client}.rs`；共享设备和固定基底 lease |
| 录制与验证 | 新 `dragonball/startup_pack.rs`；抽取 FC startup_pack 的共用设备预取部分；现有 snapshot/startup_pack、manager、ublk/device；新 `tests/integration/dragonball_m2.rs` |

依赖顺序：1 backing → 2 CPU/state-only → 3 device dirty → 4 balloon → 5 slots → 6 pin/wire/read owner → 7 COW+incremental → 8 guest slots → 9 recording/DAMON → 10 全量资格。每项先窄范围 RED→GREEN，最后才声明该阶段能力。

## Task 1 受检 backing 与真实 COW 映射

**Files:** Kata address_space_manager、snapshot/capture、vm/capture；新增 backing 专项测试。

**Interfaces:** 新 `MemoryBackingInfo { kind: MemoryBackingKind, capacity: u64 }`，kind 只有 RegularFile/BlockDevice；`inspect_memory_backing(file: &File) -> Result<MemoryBackingInfo>`。held restore 消费已打开的 File 和 GuestMemoryState；先完整校验，再构造拥有正确 FileOffset/flags/bitmap 的 GuestRegionMmap，注册 KVM slot 后才允许激活设备。普通文件 oracle 复用相同校验，不使用保留匿名元数据的裸 MAP_FIXED 替换来冒充 file-backed region。

- [x] **Step 1 写测试。** `m2_backing_rejects_short_or_wrong_kind`、`m2_block_capacity_uses_blkgetsize64`、`m2_private_mapping_metadata_and_cow`：真实 RO ublk 的 st_size=0 但 ioctl 容量正确；逐 region 验证 packed offset、overflow/越界；两份映射写同页互不污染，lower digest 不变；prealloc/hugetlb/非4KiB 在任何映射前拒绝。
- [x] **Step 2 跑 RED。** 非 root 构建 `cargo test --manifest-path src/dragonball/Cargo.toml -p dragonball m2_ --no-default-features --features virtio-blk,virtio-net --locked`；真实 block 测试经既有 capability runner 显式执行。Expected：原长度判断或元数据/COW 断言失败，不以设备缺失代替。
- [x] **Step 3 实现。** 普通文件 metadata.len，block 使用 BLKGETSIZE64；两者都验证全部 region 和容量。用锁定 vm-memory 的 MmapRegionBuilder 构造 MAP_PRIVATE+FileOffset，再统一注册 KVM；不升级依赖或跳过容量检查。
- [x] **Step 4 验证。** 相同专项、原 held/full-export/legacy snapshot、最小及扩展 consumer。Expected：专项全通过；原有效 profile 无新增失败，既有 baseline 失败单列。
- [x] **Step 5 提交。** Kata `feat: support checked block-backed private memory restore`；尚不更新 AgentENV pin。

## Task 2 累计 CPU dirty 与有所有权的 state-only export

**Files:** Kata 新 memory_tracking.rs、address_space_manager、vm/capture、snapshot/capture、vmm_action。

**Interfaces:** `FrozenMemoryRegion` 的 guest_base/host_base/image_offset/length 均为 u64；`MemoryRange` 的 image_offset/length 均为 u64；`FrozenMemoryMap` 含 generation/page_size:u64、regions:Vec<FrozenMemoryRegion>、changed_ranges/zero_ranges:Vec<MemoryRange>。新增 `ExportHeldState { generation, deadline, state_file: Arc<File> }`、`ExportHeldMemoryMap { generation, deadline }`、`ReleaseHeldMemoryMap { generation }` actions。首次 map 导出创建该 held generation 的 read pin；重复查询不重置 dirty，EndCapture/映射变更在 pin 存在时拒绝；Stop 可 teardown，不能假称读任务已结束。

- [x] **Step 1 写测试。** `m2_cpu_dirty_survives_capture_and_failed_export`、`m2_read_pin_blocks_release_and_stale_map`、`m2_cold_export_covers_complete_layout`。真实 KVM 写页跨 GPA hole；连续两次 capture 和一次失败后重试均保留第一次写；首次无基底覆盖全部 RAM；旧 generation 和输出 inode alias 拒绝。
- [x] **Step 2 跑 RED。** `cargo test --manifest-path src/dragonball/Cargo.toml -p dragonball m2_ --no-default-features --features virtio-blk,virtio-net --locked`。Expected：上述实际行为失败；缺新符号的编译错误仅为实现入口，不算有效 RED。
- [x] **Step 3 实现。** 启用 KVM slot dirty logging；每次 held 获取 bitmap 后立即并入累计集合，后续错误不撤销。固定基底只在 cold/load 时初始化，不随 snapshot 发布换代。changed_ranges 包含 zero 页；zero_ranges 是其当前值确认为零的子集，每次 held 重新核实，防止旧 zero 标记遮盖新写。state-only 与 map 使用同一 proof，不导出整个普通 RAM 文件。
- [x] **Step 4 验证。** 上述专项与 M1 token、别名、held CPU/worker 用例全部通过；显式尝试 read pin 期间 EndCapture，必须拒绝且 RAM 不变。
- [x] **Step 5 提交。** Kata `feat: export held cumulative memory ranges`。

## Task 3 覆盖设备 DMA 与 used-ring 写页

**Files:** Kata address_space_manager 的具体 memory aliases/builders、dbs_virtio_devices block/handler、request、net；新增 `Kata/src/dragonball/crates/dbs_virtio_devices/src/memory_tracking.rs` 并在 lib.rs 注册；相关 Cargo feature 声明只启用已有版本的 backend-bitmap。

**Interfaces:** 具体 guest memory 使用 `GuestMemoryMmap<AtomicBitmap>`；常规 GuestMemory/VolatileSlice 写入与 queue 更新采用其 bitmap。新增受检 `mark_guest_write<M: GuestMemory>(mem: &M, guest_addr: GuestAddress, len: usize) -> Result<()>` helper，供绕过 VolatileSlice 的 raw/AIO/iovec 路径显式标记；跨 region 分段，非法范围拒绝。Task 2 的累计集合在完整 worker ack 后 OR 软件 bitmap，不在普通 capture 中 reset。

- [x] **Step 1 写测试。** `m2_aio_read_marks_payload_status_and_used_ring`、`m2_net_rx_and_tx_used_ring_are_dirty`、`m2_raw_write_crosses_region_boundary`。无 vCPU 写入时，仅真实 AIO 完成、TAP RX、TX used-ring 更新也必须出现在 dirty map；检查 exact buffer 和 ring 页，不仅检查某页非空。
- [x] **Step 2 跑 RED。** `cargo test --manifest-path src/dragonball/Cargo.toml -p dbs-virtio-devices m2_ --no-default-features --features virtio-blk,virtio-net --locked`，网络病例复用隔离 netns。Expected：省略相应 raw 写标记即失败。
- [x] **Step 3 实现。** 逐条审计 block read payload/status、net RX、used/notification ring 和初始化写入；在完成/hold ack 前标记。禁止将整个 RAM 无条件标 dirty 来伪装增量实现，不以 KVM bitmap 代替用户态 writes。
- [x] **Step 4 验证。** device crate 全套和 Task 2 map tests；低 dirty 工作集的返回范围小于总 RAM，同一 held 时刻重建结果与 full export 一致。保留各路径实际 AIO/TAP 证据。
- [x] **Step 5 提交。** Kata `feat: track Dragonball device memory writes`。

## Task 4 Balloon 持久化与共享 backing 上的安全回收

**Files:** Kata balloon.rs、balloon_dev_mgr、device_manager capture/persist、snapshot DTO 与 address_space/memory_tracking。

**Interfaces:** 新 `BalloonState` 保存设备 config、feature/acked feature、queue/transport 与 activation；balloon controller 纳入 CaptureReport，含 reporting/used-ring writer。新 `reclaim_private_range(guest_addr: u64, length: u64) -> Result<()>` 由 memory owner 执行，保持 KVM HVA/slot，记录与实际一致的 private-zero 覆盖区间；不把它解释为可写 file backing。

- [x] **Step 1 写测试。** `m2_balloon_held_blocks_reporting`、`m2_balloon_restore_preserves_queues_and_progress`、`m2_balloon_reclaim_zero_rewrite_keeps_lower`。真实 reporting/inflate/deflate 队列；非零共享祖先→回收→读取零→再次写入新值→重复捕获；兄弟和第三个新 child 不变。
- [x] **Step 2 跑 RED。** device/VMM 专项增加 `--features virtio-blk,virtio-net,virtio-balloon`。Expected：原 snapshot 缺失/held 写入或 backing 回收语义失败；不能把恢复旧 lower 的 MADV_DONTNEED 当成零。
- [x] **Step 3 实现。** gate 共享事件回调、完整持久化并在激活前 arm。对已确认 guest 交还的完整4KiB private 页，用同地址 anonymous-private zero 映射实现回收，统一记录覆盖映射/dirty；不得碰共享文件。未对齐/越界区间拒绝；再次 CPU/设备写入后 zero 分类以实际内容重新计算。
- [x] **Step 4 验证。** 真实 KVM/reporting roundtrip，shared lower 全量 digest 不变；以实际映射和 RSS/PSS/驻留页证明回收与 COW，而非仅计数器。profile 中遗漏 balloon ack 必须拒绝 capture；rng/SPDK 等未认证 writer 仍拒绝。
- [x] **Step 5 提交。** Kata `feat: persist and safely reclaim private balloon memory`。

## Task 5 固定卷槽的容量与只读契约

**Files:** Kata block/device、block/request、blk_dev_mgr、persist、held restore tests。

**Interfaces:** 新 `RestoreBlockBinding { slot_id: String, path: PathBuf, capacity_bytes: u64, backing_read_only: bool }`；`restore_slot_binding(&RestoreBlockBinding) -> Result<()>` 仅对显式预留 slot 生效。guest 协商的 RO feature 与当前 backing 的实际写权限分开，transport/queue/acked features 保持捕获值。

- [x] **Step 1 写测试。** `m2_slot_capacity_not_restored_from_placeholder`、`m2_slot_backing_ro_rejects_raw_write`、`m2_slot_rejects_illegal_feature_transition`。使用真实 block request，不只检查配置 JSON；容量变化产生 config interrupt，held 时不取请求。
- [x] **Step 2 跑 RED。** block/manager `m2_slot` 专项。Expected：旧 config 恢复占位容量或新 backing 权限不生效时失败。
- [x] **Step 3 实现。** 预留 slot 使用固定 RW-capable guest feature 集合；RW→RW、RW→实际 RO backing 可绑定，后者 O_RDONLY 且写请求明确 IOERR，guest mount 同时 RO。捕获为 guest-RO 的盘不能直接提升为 guest-RW；该组合在分配前拒绝，需要新的 RW-capable slot 模板，不篡改协商位。容量必须匹配新 block backing、512字节对齐，改动仅限容量与合法 config-change；工具盘/普通 attached 盘不走 slot 放宽。
- [x] **Step 4 验证。** 合法容量增减的设备级读取边界、RO拒写、旧queue不变和明确非法组合；缩容不承诺文件系统自动缩小。设备级通过不替代 Task 8 guest 资格。
- [x] **Step 5 提交。** Kata `feat: restore volume slots with checked capacity and access`。

## Task 6 固定合格 fork 并接通有界内存协议

**Files:** AgentENV Cargo.toml/lock、vmm-protocol runtime.rs/新 runtime_memory.rs、runner runtime_service/runtime_snapshot/vmm/seccomp、node runtime_client、ProcessVmReader 共享模块。

**Interfaces:** wire 新 `ExportHeldState {token,state_path}`、`ExportMemoryMap {token,cursor:u64}`、`ReleaseMemoryMap {token}`。`FrozenMemoryMapChunk` 携带 token、layout_id、page_size、regions、changed_ranges、zero_ranges、next_cursor；每页最多1024个范围，整个 frame 仍≤1MiB；node 顺序取页并检查无重叠/缺页/重复cursor。protocol 的 `FrozenMemoryMap` 为完整聚合 DTO：CaptureToken、layout_id:String、page_size:u64 和 Task2同形状的 region/range vectors；由 runner 翻译 Kata 的 generation，不让 node 依赖 Kata crate。首次导出 map 对应 Task 2 read pin，Status 明确是否存在 pin。新增 `FrozenMemoryReadLease` 拥有进程身份、token、映射与读取任务；导出结束并 drain 全部读取后才 ReleaseMemoryMap。

- [x] **Step 1 写测试。** `m2_map_pagination_rejects_stale_cursor_and_wrong_layout`、`m2_cancelled_reader_retains_pin_until_drain`、`m2_process_vm_readv_rejects_wrong_process`；碎片化8GiB map 超过一帧仍完整，超限帧拒绝；真实父子进程在正常 UID/seccomp/Yama 下读页，权限拒绝不得加 CAP_SYS_PTRACE 或跳过数据。
- [x] **Step 2 跑 RED。** `cargo test -p agentenv-vmm-protocol --locked`、`cargo test -p agentenv-dragonball-runner --locked`、`cargo test -p agentenv --lib m2_ --locked`；现有单帧/取消路径不能满足新断言。输出编译错误与有效行为 RED 分开记录。
- [x] **Step 3 实现。** 先保存 M1 reader；验证 Kata Tasks1–5 后推 fork topic，以实际 SHA 更新 pin/lock/build identity。M2 runtime protocol/format epoch 为2，profile 为 `db-virtio-blk-net-balloon-v2`；M0 probe 保持原协议。移动复用现有 ProcessVmReader，FC adapter 保留原行为；受检读取持有真实进程 owner 和 pin，raw HVA DTO 不进入任何持久记录。
- [x] **Step 4 验证。** 两端 framing/状态机/实际 KVM CLI、父子 process_vm_readv、seccomp 与 held full oracle。旧 M1 identity 在新 reader 明确拒绝并保留记录；未完成 pin 的 capture 不允许 Resume，取消后必须 drain 或 teardown/reap 并继续持有未完成读 lease。
- [x] **Step 5 提交。** AgentENV `feat: add leased Dragonball memory export protocol`；记录真实 Kata SHA，不填假 release checksum。

## Task 7 节点共享 COW 恢复与固定基底增量 writer

**Files:** 新 dragonball/memory.rs，dragonball capture/sandbox/probe/paused/factory，共享 memory_snapshot，integration/dragonball_m2.rs。

**Interfaces:** `DragonballMemoryMode::{FullFileOracle, SharedUblkIncremental}` 为显式 backend policy，M3 之前不新增默认 server 选择。`DragonballMemoryBase` 持有不可变 image config/lower refs、layout 和 SharedReadOnlyDevice；load 成功后建立固定 base。新增 async `write_incremental_memory(map: &FrozenMemoryMap, reader: Arc<dyn VirtualFile>, base: Option<&DragonballMemoryBase>, output: &Path, mode: OverlaybdCompactOutput) -> Result<PathBuf>`；调用者持有 Task6 lease 至读取 drain，复用 MemorySegment 和 write_memory_overlaybd，返回现有格式的 memory image.json 路径。

- [x] **Step 1 写测试。** `dragonball_m2_shared_memory_cow_three_children`、`dragonball_m2_incremental_matches_held_full_dump`、`dragonball_m2_failed_publish_does_not_reset_dirty`、`dragonball_m2_zero_then_rewrite_and_fork`。同一 held token 同时取得 oracle 与增量；逐字节比较完整逻辑 RAM，不只校验工作页。多 child 同页写不同值、后续新 child 保持祖先值，最后 lease 才删除 ublk。
- [x] **Step 2 跑 RED。** 非 root build `cargo test -p agentenv --test integration dragonball_m2 --no-run --locked`，复用 namespace/capability runner 显式 ignored 用例。Expected：原 materialize 路径不能满足设备共享/增量范围断言；oracle 单独通过不算 M2 GREEN。
- [x] **Step 3 实现。** 使用 get_or_create_shared_mem，RO ublk 直接送 runner 校验和 MAP_PRIVATE，不先复制完整 RAM。cold 首次完整覆盖；load 后每次采用同一 base+累计 delta，不把上次 capture 当作新 base。零页在新 lower 中显式覆盖旧数据，可通过零值 reader 写入，但不得省略为继承。所有 capture/fork/paused/出版 payload 保留 backing 和读取 lease；原内存压缩与模板压缩边界不变。
- [x] **Step 4 验证。** 连续 snapshot→失败→重试→pause/resume→fork；1/2/8GiB、跨 GPA hole、CPU/AIO/net/balloon 写页与零页全部逐字节一致。故障注入 ublk I/O error、SIGBUS/runner死亡、取消阻塞读取；source恢复分类与所有 lease 符合 M1，真实 FC writer/snapshot 回归通过。
- [x] **Step 5 提交。** `feat: restore and capture Dragonball shared COW memory`。

## Task 8 Guest 可见的卷槽恢复

**Files:** AgentENV extra_drive、dragonball factory/snapshot/sandbox、manifest/restore_identity、volume 绑定辅助和 M2 integration。

**Interfaces:** 新 `SnapshotVolumeSlot { slot_id: String, mount_path: Option<PathBuf>, sub_path: Option<PathBuf>, guest_read_only: bool, capacity_bytes: u64 }` 保存全部预留槽位；未绑定槽 mount_path=None。复用现有 manifest.volume_drive_slots、`prepare_volume_drive_slots(physical_extra_drive_count, count, work_dir)` 及4KiB占位盘，槽位排在真实 attached disks 后、总 extras+slots≤24，不另建占位格式。launch 的已解析 ExtraDrive 提供当前 catalog ID/容量/实际 RO。factory 生成 Task5 RestoreBlockBinding，不在 opaque state JSON 中修改 features；VolumeManager 仍独占 catalog/readiness/reservation。

fresh 槽位数取 policy.app.volume.max_volume_count 与剩余24盘预算的较小值。模板未绑定槽允许新容量/实际RO绑定；已有挂载文件系统的快照卷继续保留M1容量/挂载/实际RO约束，不把设备容量更新等同于文件系统在线缩容或自动重新挂载。

- [x] **Step 1 写测试。** `dragonball_m2_placeholder_slot_binds_real_volume`、`dragonball_m2_slot_resize_reports_new_capacity`、`dragonball_m2_slot_ro_policy_and_failure_rollback`。同一4KiB未绑定槽模板分别绑定64/128MiB合法文件系统；guest blockdev 看到实际容量，RW 可写，RO mount 和直接写都拒绝；交换等容量磁盘仍按 slot/mount 正确绑定；对已挂载128MiB快照卷尝试64MiB替换必须在分配前拒绝。
- [x] **Step 2 跑 RED。** 真实 M2 guest `slot_` 过滤；Expected：原 M1 的严格容量相等检查或旧 guest config 不满足，不删除旧非法普通盘测试。
- [x] **Step 3 实现。** 只为显式 slot 放宽允许字段，当前 mount 信息仍走 metadata/envd；不添加通用 hotplug。RO→RW guest-feature 升级在分配前拒绝，RW slot→RO backing 保留相同协商特性但强制后端拒写。源 capture immutable volume generation、失败 child 清理和 request cancellation owner 不得退化。
- [x] **Step 4 验证。** guest config-change 确实生效、tools vda/user vdb/extras 顺序不变；exclusive fork、共享 RO、uploading/failed、部分 child 失败、删源后提交恢复及 API取消回归全部通过。若现有 guest 无法合法处理，M2 保持未完成，不绕过协商检查。
- [x] **Step 5 提交。** `feat: bind qualified Dragonball volume slots on restore`。

## Task 9 Startup recording 预取与 DAMON 范围

**Files:** 新 dragonball/startup_pack.rs、dragonball/damon.rs；firecracker/startup_pack.rs 共用部分移动至 `src/sandbox/startup_pack.rs`；snapshot/manager、startup_pack、ublk/device、dragonball/{sandbox,capture,paused}.rs 和 M2 integration。核对 firecracker/config.rs 的既有 reclaim 参数，但不改变 FC 的 DAMON 行为；不修改 template builder 的 factory 选择。

**Interfaces:** async `dragonball::record_startup_pack(artifacts: Arc<DragonballCaptureArtifacts>, policy: DragonballRuntimePolicy) -> Option<PathBuf>`；在 publish_captured 的 backend payload 分派中接入，并用 StartupRecording::spawn_with_capture_lease。共用 trace/manifest、dedicated recording device 和本地预取实现不另建格式；recording 专用设备 delete_device，不进入共享 warm pool。新增 `damon_monitor_bounds(layout: &MemoryLayout) -> Result<(u64, u64)>` 从实际布局求首个有效页到最后 RAM 端点，不复用 FC 内存大小常量；沿用 guest DAMON_RECLAIM 单区间接口，其跨度可包含 GPA hole，内核必须按 online LRU page 过滤，不能把 hole 当成可读 RAM。

捕获 payload 增加非序列化 `recording_policy: DragonballRuntimePolicy`，由源 factory 提供，manager 不从全局默认猜 runner/kernel；paused decode 经身份验证后由当前 factory 重建。boot 参数沿用现有 reclaim 数值，仅 start/end 从受检 layout 导出。Linux 6.1.175 的过滤依据见 [damon_get_page](https://github.com/gregkh/linux/blob/v6.1.175/mm/damon/ops-common.c)，实现仍需实际 guest 验证。

- [ ] **Step 1 写测试。** `dragonball_m2_recording_uses_dedicated_device`、`dragonball_m2_recording_outlives_publication_cancellation`、`dragonball_m2_prefetch_failure_keeps_snapshot_runnable`、`dragonball_m2_damon_uses_actual_regions`。实际 first-touch 记录，不被共享 block page-cache 命中掩盖；延迟 recording/读取退出前删除请求不能释放 backing。
- [ ] **Step 2 跑 RED。** startup_pack unit 与真实 `dragonball_m2_recording`/`prefetch`/`damon` 过滤。Expected：原 FC-only dispatch 不录制 DB，或区域常量不适配，观察对应断言而非依赖缺失。
- [ ] **Step 3 实现。** 仅抽取真正共用 prefetch/config helpers；DB recording VM 复用 held restore 和真实 readiness，trace/manifest 仍由现有 publisher 管理。启用/消费开关分离；缺失、损坏、超预算及录制失败 best-effort，cleanup 不随预算中止；lease 覆盖 recorder、异步上传和最后阻塞读取。
- [ ] **Step 4 验证。** 录制/消费四种开关组合、有效manifest/损坏/缺失、共享设备最后订阅者取消、发布完成/失败/取消和节点关停 drain。原 FC startup_pack 单测与 POSIX真实套件不退化；guest 实际启用 reclaim，1/8GiB monitor 参数等于布局推导值，非 RAM PFN 被拒绝，报告其单区间跨度而不宣称多区间 DAMON 接口。
- [ ] **Step 5 提交。** `feat: record and prefetch Dragonball startup memory`。

## Task 10 完整 M2 资格与一次分支审查

**Files:** Makefile、`scripts/tests/run-dragonball-m2.sh`、M2 integration/C workload、`docs/src/internals/dragonball-m2-results.md`；扩展现有受保护 manual CI opt-in，不扩大普通 PR 权限。

**Interfaces:** `make test-dragonball-m2` 明确区分 full oracle 与 incremental 模式；缺 Linux/KVM/ublk/4KiB page/必要 tracing 前提直接失败。报告普通 ignored 与显式执行清单，测试素材、source hash、Kata pin、runner build、CPU、memory mode 和 retained failure artifacts 可追溯。

- [ ] **Step 1 完成资格测试。** M1 全部 lifecycle/鉴权/卷/重启/发布故障，以及新增完整 RAM比对、shared COW、dirty/zero、balloon/slot/recording矩阵。重放 M1 的 child 分配后再写源卷、A捕获后B捕获再发布A、API取消两窗口，不能因新内存路径遗漏。
- [ ] **Step 2 验证有效 RED 记录。** 新能力各有真实 RED→GREEN；已通过行为不撤销代码造假失败。全量比较中有一字节差异、lower变化、提前释放或不合法features，即资格失败。
- [ ] **Step 3 跑完整检查。** `make fmt clippy test test-unit`、`make test-dragonball-m0 test-dragonball-m1 test-dragonball-m2`，外加 runner/protocol全套、显式 KVM CLI、FC snapshot/attached/volume、startup-pack和相关ublk故障套件。Expected：AgentENV全部适用检查退出0；Kata scoped suites全通过、all-features check通过，已知旧全套/lint基线逐项比较，不隐藏新失败。Go 未改则不追加无关工作。
- [ ] **Step 4 一次完整分支审查。** 包含全部 Kata补丁、pin、AgentENV差异和上述五个 Review Focus；Critical/Important 一次修复流程，每项 RED→GREEN+相关全量验证，不重复委派审查。报告全部未覆盖项、裁决与任何保留问题。
- [ ] **Step 5 提交与交接。** `test: qualify Dragonball incremental memory lifecycle`；临时权限撤回，旧service仍卸载，M2门槛满足后再形成 M3计划。不把单机 correctness/读取字节数当作M4性能或跨节点资格。

## 计划自检与待确认边界

总体设计 §6.1 延续 M1并增加balloon/read pin；§6.2 对应2/3/6/7；§6.3对应1/7；§6.4对应4/9；§8.1对应5/8；§9所有权贯穿6–10。§12的M2能力均有真实运行判据，server/template/完整OSS/P2P留M3，性能/迁移/退役留M4/M5。每个任务消费者使用前项定义，不预填尚未生成的Kata SHA。

本计划把“RO/RW组合合法”具体限定为：RW-capable预留槽可绑定RW或后端强制RO；已协商guest-RO不能直接升级成RW。需要该升级时使用新的RW-capable模板，不把非法改位当成功。backing恢复、dirty合并与回收的新增接口均须实际测试后才能pin。

文档核对：Context7两次检索未找到相关 rust-vmm 库，未使用无关结果；依据锁定 [vm-memory 0.17.1 bitmap源码](https://github.com/rust-vmm/vm-memory/blob/v0.17.1/src/bitmap/backend/atomic_bitmap.rs)、[mmap源码](https://github.com/rust-vmm/vm-memory/blob/v0.17.1/src/mmap/unix.rs) 与 [KVM dirty-log文档](https://docs.kernel.org/virt/kvm/api.html#kvm-get-dirty-log) 核对。KVM读取会清理内核bitmap，raw pointer写入不会自动进入软件bitmap，因此必须独立累计并补齐设备路径；这些文档不是M2已通过的证据。

下一步：请审阅本计划，确认它覆盖预期后按已选 Native方式实施；本次不重新询问执行方式，也不提前开始M2产品代码。
