# Dragonball M1 实施证据

状态：M1 退出门槛已通过，最终复验于 2026-10-02 完成（Asia/Shanghai）。用户于 2026-10-01 确认 [M1 计划](./dragonball-m1-implementation-plan.md) 并选择 Native 执行。默认后端仍为 Firecracker。任务 1–3 的 Kata 补丁已推送用户 fork，AgentENV Cargo pin 为 `0bc22d5ce53a072aabf4fe42c133dacefeaa9c72`；任务 4–11 完成协议、归档、共享存储、启动、捕获、fork/卷、暂停持久化与完整运行中一致性验收。以下逐任务记录保留当时的状态，最终结论见文末；M2–M5 未完成。

## Task 1：block drain/hold

独立完整 Kata checkout：`/Users/yili/work/AgentENV/.worktrees/kata-dragonball-m1`，分支 `agentenv/dragonball-m1`。提交 `3a685f8058101ad5fe4c96bc4b5d016b7c82cf41`（`feat: add virtio block capture gate`）；任务 1 当时未推送，后续推送和 pin 验证见任务 3。

新增 generation 控制、worker-owned gate 和 ack，复用 kill eventfd 唤醒独立控制 channel。held 时统一禁止取新 descriptor，END_IO 仍更新 status/used ring，pending 清空且底层同步成功后才确认。resume 主动处理已消费通知的队列；旧 generation、未激活/退出 worker、超时和同步失败不返回成功报告。

测试机 `147.139.168.159`，构建与测试为 `aenv`（UID 999），Rust/clippy 1.98.0。未启用额外设备 feature 或设置新的 sudo/Docker 委派；旧 `aenv.service` 仍为 not-found/inactive/MainPID=0。

| 检查 | 实际结果 |
| --- | --- |
| 原 virtio-blk crate 基线 | 46 passed、0 failed、0 ignored |
| 行为 RED | control/drain 接入但队列未设 guard 时，3 个 descriptor/limiter/同步失败隔离断言失败；退出 101 |
| 空 worker 集合 RED | 新测试证明两个空 vector 的长度相等会产生错误成功；9 passed、1 failed，退出 101 |
| 最终 crate suite | 56 passed、0 failed、0 ignored，退出 0；含 10 个新 capture 测试 |
| 实际异步写入 | LocalFile/Aio worker、真实 guest descriptor/used ring；hold 前写入 `0x5a`，held 保存未取请求，resume 后写入 `0xa5`，两次 ack 均无 pending |
| 最小 consumer | `cargo check --manifest-path src/dragonball/tests/minimal-profile-consumer/Cargo.toml`，退出 0 |
| 格式与源码一致性 | 6 个修改/新增源码和 Cargo.lock 的本地/远程 SHA-256 一致；rustfmt --check、git diff --check 退出 0 |
| 严格 clippy | 未通过；依赖 kata-types 有 6 个未修改的 lint，no-deps 下本 crate 有 4 个既有 lint；固定 bfc470b 基线复现相同 4 个 |
| 新 lint 检查 | no-deps、-D warnings，仅允许基线的 useless_borrows_in_formatting/repeat_vec_with_capacity 两类，退出 0；不称为严格 clippy 通过 |

远程命令通过隔离 `remote-cargo.sh` 执行：

```bash
cargo test --manifest-path src/dragonball/Cargo.toml -p dbs-virtio-devices \
  --no-default-features --features virtio-blk --locked
cargo clippy --manifest-path src/dragonball/Cargo.toml -p dbs-virtio-devices \
  --no-default-features --features virtio-blk --all-targets --no-deps --locked -- -D warnings
```

日志：`/tmp/agentenv-dragonball-m0-20260930/m1-logs/` 下 `task1-baseline.log`、`task1-behavior-red.log`、`task1-empty-set-red.log`、`task1-final-suite.log`、`task1-consumer.log`、`task1-clippy.log`、`task1-clippy-scoped.log`、`task1-clippy-baseline.log`、`task1-clippy-new-lints.log`；本地副本在 M1 独立执行 workspace。

执行裁决：新增 `Ufile::sync_all`，未知后端默认 Unsupported、LocalFile 使用 fsync。现有 `Write::flush` 行为不改变；代价是未经认证的存储后端会拒绝 capture。严格 lint 的既有失败保留并报告，不修改无关模块；只在独立诊断命令中允许已复现的两类，仍需整个 M1 的独立审查。

以上 Task 1 证据只覆盖 block worker，不独立证明完整运行时捕获。

## Task 2：net 与完整设备集合屏障

Kata 提交 `f1cc588214f0e90fda795cb7f7f82ee969357c61`（`feat: coordinate Dragonball device capture barriers`）；任务 2 当时未推送或更新 pin，后续见任务 3。

真实 net subscriber 与 capture control 共用同一 handler mutex；取得锁并处理已完成 descriptor 的延迟 IRQ 后才确认。held 时禁止 RX RAM/TX used-ring 写入，保留未消费 descriptor；暂存的 host RX 可丢弃，但不回滚已完成 guest buffer/ring。可读 TAP 订阅暂停，resume 主动唤醒队列并恢复订阅。锁等待遵守共同 deadline，不等待当前 VMM loop 给自身回消息。

`DeviceManager::begin_capture/end_capture` 使用可变 receiver：按配置的逻辑 block/net ID、每个 block queue 聚合报告，严格检查完整集合、generation、pending、memory writers 和 block flush。控制器在 MMIO 锁内克隆，所有等待在锁外。原始控制集合和 generation 保留在 transient capture session，全部恢复确认后才释放；部分 hold 失败的 session 不允许恢复为成功。不支持的 DMA/内存写入设备在任何 hold 前拒绝。此 session 不写入持久化 schema。

| 检查 | 实际结果 |
| --- | --- |
| Task 1 后、Task 2 前联合基线 | 66 passed、0 failed、0 ignored |
| net 行为 RED | 2 failed、1 passed；RX/TX 在 held 时仍修改内存/ring，退出 101 |
| 完整设备集合 RED | 漏确认、配置存在但未激活、SPDK writer 3 个断言失败，退出 101 |
| 原始集合恢复 RED | 冻结后配置集合清空，旧恢复实现误返回成功但 TX 仍被 hold；1 failed，退出 101 |
| 空集合 release RED | generation=0 被误报成功；1 failed，退出 101 |
| 实际 block 激活 | 两个 LocalFile/Aio queue、fsync 和精确 `block:root/q0/q1`；回退旧 ID 时断言 RED，恢复后 GREEN；结束时 join 实际 worker |
| 最终联合 device crate suite | 73 passed、0 failed、0 ignored，退出 0；含 17 个 capture 测试 |
| 最终 manager 专项 | 最小 block/net profile 8 项；加 rng profile 9 passed、0 failed、0 ignored，退出 0 |
| 实际设备/事件路径 | 两个已激活 MMIO/net 的 used ring 在 hold 时不变、resume 后推进；部分 ack 失败为 Terminal；TAP 订阅、controller/MMIO 锁 timeout、原始集合 release 均覆盖 |
| 不支持的 writer | SPDK 配置被拒绝；rng 配置存在时在 hold net 前拒绝，真实 net 随后仍处理 TX，证明未部分冻结 |
| Dragonball 全套最小 profile | 87 passed、9 failed，退出 101；固定 bfc470b 基线为 79 passed、同样 9 failed，无新增失败 |
| 扩展 feature 构建 / consumer | `cargo check -p dragonball --all-features --locked` 和最小 consumer 均退出 0 |
| 格式 / 一致性 | 13 个源码文件及 Cargo.lock 的本地/远程 SHA-256 全一致；选定文件 rustfmt --check、git diff --check 退出 0 |
| 严格 clippy | 未通过；no-deps 的 device crate 4 个、Dragonball 3 个既有 lint，固定基线分别复现；未删除或隐藏基线失败 |
| 新 lint 诊断 | 仅允许已复现的 4 类基线 lint，退出 0；自身新增 Rust 2018 panic-format 告警已修复。此诊断不是严格 clippy 通过 |

全套的 9 项失败为已有 block/PCI 用例，在未启用 `host-device/hotplug` 的 profile 中仍假设相关能力。独立基线 checkout `/tmp/agentenv-dragonball-m0-20260930/kata-m1-baseline` 的 HEAD 为 bfc470b，只加相同一行 hotplug 专用测试 `cfg`，以解除原有编译失败；没有本次 runtime 修改。没有为令全套变绿而扩展最小产品 profile 或跳过这 9 项。

构建为正常 `aenv` 用户执行 `cargo test --no-run`，随后解析实际 Cargo 输出的 test binary，在既有 capability wrapper 的非 root/隔离 netns 下运行。未修改 HOME，未增加持久 sudo/Docker 委派。旧 service 再确认仍为 not-found/inactive/MainPID=0，旧安装的三个 binary 仍不存在；旧数据与卸载备份保留。

日志在同一 `m1-logs` 目录，含 `task2-net-behavior-red.log`、`task2-manager-behavior-red.log`、`task2-manager-original-set-red.log`、`task2-manager-empty-release-red.log`、`task2-block-activation-id-red.log`、`task2-final-dbs-suite.log`、`task2-final-manager.log`、`task2-final-dragonball-suite.log`、`task2-dragonball-baseline.log`、`task2-{dbs,dragonball}-clippy.log`、`task2-dragonball-clippy-baseline.log`、`task2-final-clippy-new-lints.log`、`task2-final-all-features-check.log` 和 `task2-consumer.log`。本地执行 workspace 保留副本。

任务 2 本身不证明 VMM held restore 或完整 sandbox 生命周期；VMM 屏障验证见任务 3。生产 OverlayBD、卷、paused store 和 M2–M5 仍未完成，M1 不宣告通过。

## Task 3：VMM held restore 与完整 RAM 导出

Kata 提交 [`0bc22d5ce53a072aabf4fe42c133dacefeaa9c72`](https://github.com/denverdino/kata-containers/commit/0bc22d5ce53a072aabf4fe42c133dacefeaa9c72)（`feat: add held Dragonball snapshot capture and restore`）。用户 fork 的 `agentenv/dragonball-m1` 分支经 `git ls-remote` 读回相同 SHA；未推 upstream 或 main。AgentENV manifest、Cargo.lock 和节点/runner 身份校验均使用该 SHA。lockfile 只更新 15 个 Kata 来源 URL，不升级其他依赖。

新增四个独立 VMM action：BeginCapture、EndCapture、LoadSnapshotHeld、ExportHeldSnapshot。先确认全部 vCPU 离开 KVM_RUN，再确认完整设备集合，形成相同 generation 的 transient report；未完整确认的切换保留 terminal session，要求 teardown。LoadHeld 在 worker/subscriber 激活前预设 gate，vCPU thread 从 Paused 启动；只有显式 release 才执行。旧 SaveMicrovm/StartMicroVmFromSnapshot 保留原行为，但不能绕过活跃 capture session。

请求持有已打开的 state/memory 文件，导出不重开路径。错误 token 在输出 metadata/I/O 前拒绝；输出 inode 必须互异，不能别名到 live restored COW backing。持有期间导出完整 RAM，布局来自实际 guest mappings/KVM slot：GPA hole 不占 packed 文件空间。恢复前检查 machine/layout、完整逻辑设备集合、format epoch、CPU requirements、MSR/XSAVE/XCR；未知序列化 writer 拒绝，不能被 serde 静默丢弃。M0 probe snapshot 没有新 capture CPU 记录，因此不是生产 held 输入。

| 检查 | 实际结果 |
| --- | --- |
| 激活前 block/net gate RED → GREEN | 两个真实设备未 gate 时消费队列，2 项失败；修复后均通过 |
| generation 行为 RED → GREEN | 仅移除 generation 比较，错误 token 覆盖输出，断言失败；恢复校验后通过，原 sentinel/长度不变 |
| held CPU RED → GREEN | 提前 resume 最初被 immediate_exit 遮蔽，RAM 未变；增加真实 fd.run 计数后同一变异失败，正确 paused start 为 0 次 KVM_RUN |
| 最终 device suite | 75 passed、0 failed、0 ignored |
| 最终 VMM/manager capture suite | 18 passed、0 failed、0 ignored；含 10 个 VMM 测试 |
| CPU + block 联合 held restore | 实际 MMIO block/Aio/guest descriptors；held 时 RAM counter、used ring、磁盘均不变，KVM_RUN 计数为 0；release 后 CPU 运行、used ring=1、磁盘写入 `0x5a` |
| 运行中 capture/full export | 真正执行 guest counter，capture 后冻结；16 MiB RAM 导出 marker 与 held RAM 一致，release 后继续运行；旧 generation 拒绝 |
| 实际 GPA-hole 布局 | 8 GiB 多 region 实际映射，host address/KVM slot 对应，packed offset 连续，总量等于 RAM；该测试不执行 8 GiB dump |
| FD 所有权/别名 | 替换两条输出路径后新 sentinel 路径不变，原打开 inode 收到完整 state/RAM；live mmap backing 的同 inode 输出被拒绝且未截断 |
| 旧 snapshot API | 原 roundtrip 1/1；追加 real-KVM save/start 用例实际运行 guest，不仅使用旧 unit mock |
| 完整最小 root suite | 97 passed、9 failed、0 ignored；与已记录基线相同 9 项失败，不称为全套通过 |
| 最小/既有扩展 consumer | 外部 consumer 构造四个新 action 和 report；最小及 hotplug/host-device/dbs-upcall 组合均编译通过；all-features library check 通过 |
| Kata lint/fmt/一致性 | 严格 lint 仍有既有错误；仅允许已复现四类的独立诊断通过。15 个源码及 Cargo.lock 本地/远程 SHA-256 一致；rustfmt/diff check 通过 |
| pin 身份 RED → GREEN | 新 SHA 断言先因旧 pin 失败，更新 manifest/lock 后通过；KVM CLI 3/3、节点 probe 7/7 |
| 新 pin 的实际 M0 guest | 5 passed、0 failed、0 ignored：冷启动/挂载、认证、deny-all metadata、相邻实例隔离、quiesced full restore；恢复原 PID 355、RAM marker、tools digest、重新绑定 metadata，源 memory checksum 未变 |
| AgentENV workspace 检查 | fmt --all --check、clippy --workspace --all-targets --all-features -- -D warnings 退出 0；runner/protocol 非 ignored suites 均通过，ignored 的 KVM CLI 另跑 |

同主机 CPUID 比较发现物理 APIC ID 随线程迁移变化，只归一化 leaf 1 EBX[31:24]、0x0b/0x1f EDX；其余 CPU feature/geometry 保留精确比较，篡改 requirements 被拒绝。固定 XSAVE buffer 拒绝大于其容量的 VM XSAVE2 状态，不宣称跨 CPU 策略兼容。新测试使用按 VM FD 登记的真实 KVM execution；RAII 持有 VmFd 防止重用，默认旧 mock 与生产 fd.run 路径不变。

日志在同一 `m1-logs` 目录：`task3-{prearm-behavior-red,prearm-green,generation-behavior-red,early-cpu-count-red,closure-dbs,closure-capture,closure-legacy,closure-full-minimal,consumer,expanded-consumer,closure-all-features,closure-strict-clippy,closure-new-lints}.log`，以及 `task3-pin-{identity-red,identity-green,runner-protocol,kvm-cli,probe,fmt,clippy,real-guest}.log`。资产准备的两次失败日志保留：先补 setup-only 生成的隔离配置，再修正 task-owned 父目录权限；未安装/运行 server service。构建与测试仍为非 root aenv，通过原 capability runner 进入隔离 namespace。

任务 4–11、生产 sandbox 生命周期及 M2–M5 尚未完成。此次不是新 pin 的完整 AgentENV `make test` 验收，也不把 M0 的静止 probe snapshot 当作生产 capture 资格证据。

## Task 4：独立 runtime 协议与 runner 状态机

CLI 显式 `--runtime`，双方 wire envelope 必须包含 `mode="runtime"`；仍使用版本 1、request/instance IDs、budget 和 1 MiB body 上限。原 M0 协议、DTO、profile 与 version-json 结构不变。新增共享恢复身份、packed RAM layout、capture token 和完整 CPU/worker proof；不暴露 host address，配置不携带 registry/OSS 凭证。

每次 runner 创建独立随机 nonce，capture generation 单调递增；跨进程、旧 generation、重复 request 拒绝且不触达 VMM。真实 adapter 使用固定上游的四个 held action，保留已打开的输入 FD，输出只写已打开的文件。实际 probe 标记不能通过伪造 `probe_only=false` 绕过。结构完整性与配置的精确 worker/CPU 集合均须确认，超出协议可表示的设备数量在 Configure 前拒绝。

Hello 的 ABI 为 `dragonball-held-fullram-v1@<qualified-revision>`，build ID 使用实际 runner/protocol 源码、manifests、lockfile、编译器及 target/profile/flags 的 SHA-256，不仅是包版本。身份严格比较可能保守拒绝其他构建，后续兼容资格认证不能用删除旧快照代替。

节点客户端在首次 await 前标记 mutating request 的未知结果。deadline 或调用方取消不重发 Export；仅查询、同 token 确认释放或 teardown。串行 service 对已确认的 preflight/export 拒绝保留可用 phase；其他未知错误撤销 held proof、进入 Failed 并尝试 teardown，不能把 timeout 当取消或资源已释放的证据。

| 检查 | 实际结果 |
| --- | --- |
| 四个规定行为用例 | 未加 guard 时 0 passed/4 failed；实现后通过 |
| 新 runtime frame suite | 7 passed/0 failed；包含模式、版本、归属、reply kind、held proof、RAM 布局 |
| runtime service suite | 8 passed/0 failed；完整 ack、stale/process token、重放、unknown outcome、build ID、配置数量边界 |
| CPU ack 转换 | 错误 generation 曾被丢弃，RED0/1；修复后 runner unit 2/0 |
| 节点真实 Unix socket | timeout 和调用方取消均先复现重发 RED，再 GREEN2/0；不是伪造 VMM 生命周期证明 |
| runtime KVM CLI | 3 passed/0 failed：实际 readiness、所有线程 seccomp=2/NNP=1、0600 socket、probe 文件拒绝、总帧 deadline |
| 旧 KVM CLI | 3 passed/0 failed；原 checkpoint preflight、失败清理与 stop 不退化 |
| runner/protocol 全套 | 非 ignored suites 全通过；M0 frame 12/0、旧 service 10/0，父进程退出与 seccomp 用例仍执行 |
| 旧节点 probe | probe ownership/cancellation 5/0；节点 process 用例在最终闭环另跑 |
| 新构建实际 M0 guest | 5 passed/0 failed；重新编译的 integration binary，恢复 PID354/RAM/tools/新 metadata/private-memory checksum |
| AgentENV workspace | fmt --all --check、clippy --workspace --all-targets --all-features --locked -- -D warnings 和 git diff --check 全通过 |
| 源码一致性 | 14 个 Rust 文件及 Cargo.lock/runner manifest 共16个 SHA-256 与远程相同 |

总帧 deadline 用真实逐字节客户端先复现约2.4秒阻塞，再用同一1秒绝对 deadline 限制全帧；最初 request ID99 的失败属于 fixture 错误，修正为 ID0 后才作为行为 RED。Tokio timeout/write_all/peer_cred 和 sha2 增量摘要 API 通过 Context7 查询并由固定版本实际编译验证，没有升级依赖；Cargo.lock 只为 runner 增加已锁定 sha2 0.10.9 的 build dependency。

日志位于同一 `m1-logs` 目录：`task4-final-{suites,client,kvm-cli,probe,fmt,clippy}.log`、`task4-{integration-build,real-guest,frame-drip-behavior-red,frame-drip-green,worker-limit-red}.log` 等。测试仍使用非 root aenv 和隔离 state/namespace；旧 service 保持 not-found/inactive/MainPID=0。此任务不证明 production runtime 的完整 guest capture/restore、存储发布、paused store 或跨节点能力；这些属于后续任务。仍未宣告新 pin 的完整 `make test` 通过。

## Task 5：便携归档身份与恢复预检

新增 runtime envelope version 1，保存受检 identity、逻辑磁盘/网络、packed RAM layout 与完整上游 state。捕获只转换 host block path/TAP 为逻辑标记；恢复按完整 ID 集合重新绑定，不按位置兜底，不改写 guest CPU、transport、协商 feature 或 config space。上游 state 经固定类型反序列化/序列化完全相等检查，防止未知字段被默默丢弃。

节点在分配设备/网络前比较记录、manifest、envelope 与实际 Hello reader：backend、revision、ABI、build SHA、epoch、CPU/device/layout profile 及捕获的 kernel/tools/envd 版本。RAM 绑定实际单 NUMA/no-firmware 的 GPA0/3 GiB 与 GPA4 GiB recipe，不仅互相比对。boot tools `rootfs` 与 writable `user_rootfs` 分开校验；manifest rootfs 大小对应后者。旧 FC 记录默认 backend=firecracker，原四参数版本构造不变，DB revision 不写入 firecracker_version；新 DB manifest 必须携带完整 identity。

输入文件要求绝对无 parent traversal、O_NOFOLLOW、regular file 和 16 MiB 有界解码。runner 在 VMM action 前拒绝短 RAM、非法布局、未知 schema/ABI、缺设备、重复 IDs 与 profile 篡改，并检查已保留 block FD 的实际容量。只将 node-bound state 写入 create_new 后立即 unlink 的临时 FD；便携归档不持有 node-local 路径。

| 检查 | 实际结果 |
| --- | --- |
| 新归档/节点守卫 RED → GREEN | 路径泄露、忽略未知字段、identity 冲突、epoch-only reader 均先复现失败再修复 |
| 联合篡改与设备 profile | 同时改 envelope/raw GPA 与 offered/acked unknown feature 曾被接受，真实 RED；固定布局/advertised bits/RawBlock geometry 后拒绝，不强改 guest 状态 |
| runtime snapshot suite | 6 passed/0 failed；完整 typed fixture、逐字段拒绝、多磁盘重新排序按 ID 绑定；fixture 不等于实际来宾执行 |
| 节点 restore_identity/reader | 9 passed/0 failed；36 个 identity 字段冲突、tools/user-root graph、opaque topology、未知 reader 与 IO 先行拒绝 |
| 旧 snapshot types / manifest | 31 passed/0 failed 和 5 passed/0 failed；旧缺 tools 版本记录仍可读 |
| runner/protocol 与 KVM CLI | 非 ignored suites 全通过；新旧 KVM CLI 各3 passed/0 failed |
| 节点 Dragonball suites | 12 passed/0 failed，包括已有 cancellation/cleanup 所有权测试 |
| 新构建实际 M0 guest | 5 passed/0 failed；重新编译 integration binary，恢复 PID353、RAM marker、tools digest、新 metadata 与 private-memory checksum |
| workspace lint/fmt | 新 fixture 的 field_reassign_with_default 被严格 clippy 拒绝，改为单一 initializer 后严格 workspace clippy 与 fmt check 均退出0，无 lint suppression |
| 源码一致性 | 15 个 Rust 源码的本地/远程 SHA-256 完全一致 |

日志在同一 `m1-logs`：`task5-final-{runner-fixed,kvm-cli,node-identity,node-dragonball,types,manifest,clippy-fixed,fmt-fixed}.log` 以及 `task5-{integration-build,real-guest}.log`。仍为非 root aenv、隔离 state，旧 service 未安装。生产 sandbox 的完整捕获/发布/恢复、实际 runtime guest 和 M2–M5 不由本任务的 typed fixture 证明；完整 `make test` 仍未宣告通过。

## Task 6：共享内存 writer 与存储捕获所有权

新增 `MemorySegment {image_offset, source_offset, length}` 与共享 `write_memory_overlaybd`。FC 保留原 dirty-range/HVA adapter；DB 完整 RAM 的 producer 使用 packed file offsets，不引入 FC GPA-hole 常量。writer 校验 4 KiB alignment、正长度、目的边界、源 overflow 和目的 overlap，并按 OverlayBD 最大 segment 长度拆分。FC sparse dirty ranges 仍受支持，完整 DB coverage 由完整 layout producer 保证。

临时 lower 使用同目录唯一 RAII 文件，compact/finalize 后同步，再 hard-link 不覆盖式采用；错误或调用方 future drop 时移除临时路径，已存在 final 拒绝而不覆盖。shared publisher 不删除 durable 文件。现有 seal/restack、runtime upper 重新连接、layer budget、descriptor 和 terminal boundary 连同测试移至 `storage_snapshot`；本地 raw/发布时压缩策略不变。backend payload 保留 rootfs/extras/volume 的结果元数据，与共享 `StorageCaptureOwner` 目录 lease 一起存活，不重复一套配置或 daemon/repository。

| 检查 | 实际结果 |
| --- | --- |
| neutral writer RED → GREEN | 先复现非法 alignment 被接受和成功覆盖 durable final，2 项失败；实现校验及 no-replace 后通过 |
| focused memory | 7 passed/0 failed/1 ignored；含实际 OverlayBD reader、两个 packed region、非零祖先上的零值覆盖；short-read/输入 errno 校验具体原因，不以 EEXIST 冒充正确拒绝 |
| 真实输出 ENOSPC | 单独运行1 passed/0 failed；非 root aenv 写入隔离64 KiB tmpfs，1 MiB RAM 导出失败且无 final/partial lower |
| 旧 FC dirty-range/publication-failure | 5 passed/0 failed；原测试保留 |
| shared storage/owner | 7 passed/0 failed；6 项原 sealing/budget/descriptor 测试，发布 continuation 在 caller/source drop 后仍读到 rootfs/extras/volume 文件；lease 行为是既有契约的保留 |
| 真实 FC Linux snapshot/attached drive | 9 passed/0 failed，约26.5秒；含 raw memory pause/restore、snapshot chains、模板派生、随机 lifecycle 所有权、attached drives 与 frozen volumes |
| workspace lint/fmt/源码 | 严格 workspace clippy、fmt check、diff check通过；6 个修改/新增 Rust 源码 SHA-256 本地/远程一致，无 lint suppression |

FC 回归最初错误使用 DB-only kernel，9 项均在 envd readiness 前失败；独立串口诊断看到 virtio probes errno22、无法挂载 vda 与 kernel panic。保持同一 source/binary/config，仅换回已存在的 FC kernel vmlinux-6.1.175，memory snapshot/restore 单项1/0，再跑全9项通过。两种内核的资格配置保持分开，不以修改产品 driver/boot policy 掩盖测试配置错误；失败日志保留。

日志位于同一 `m1-logs`：`task6-{memory-red,fc-baseline,memory-full-green,fc-diagnostic,fc-default-kernel}.log` 和 `task6-final-{memory,storage,fc-unit,enospc,fc-linux,clippy-fixed,fmt}.log`。全部构建/执行仍为非 root aenv，capability runner 只提供隔离 Linux 测试权限，未安装 server、改动旧数据或增加永久权限。生产 Dragonball backend 启动与一致捕获仍是后续任务；M1/M2–M5 和完整 `make test` 不宣告通过。

## Task 7：生产 backend fresh/held 启动

实现明确的 `DragonballRuntimePolicy`、factory、Backend/Executor；测试直接选择 factory，server 默认选型不变。启动由独立 owner 持有全部网络、磁盘、metadata、runner 资源；取消调用方不会取消 cleanup。metadata listener 在 CPU 放行前安装当前 token/hash/身份。Ready 前执行 root envd identity bootstrap，再初始化原 USER/workdir/env；未 Ready、已 held、已停止或 runner 死亡时拒绝 executor。验证 runner 每个线程的 capability、NoNewPrivs 和 seccomp，无降级路径。

离线 `--runtime --version-json` 返回精确 reader build identity，不分配 KVM/磁盘/网络、不伪造运行时 nonce；启动后的真实 Hello 再严格匹配。新增 checked rate DTO 与原生 RawBlock 配置，保存捕获限速以校验 opaque state，恢复只替换当前 host path/TAP/rate，禁用限速清掉继承值。扩展 client 使用明确 policy 配置；start 失败向上传递，stop best-effort 并匹配同一 sandboxInstanceId。

为 held-start 认证提前实现 Task 8 的最小完整 RAM/sealed-disk producer；它不是 probe 归档或测试绕过。成功 pause 保持 Held；完整 snapshot/resume/fork、发布、故障与持久化仍须完成后续任务。未 Ready 的源不能返回 Recoverable；runner 未确认退出时保留已封存 live lowers，不允许目录 lease 提前释放。

| 检查 | 实际结果 |
| --- | --- |
| 原生 rate / 离线 reader RED → GREEN | rate 反序列化行为 RED0/1，runtime snapshot7/0；新 build CLI RED0/1，包含旧 M0 CLI 的 identity suite2/0 |
| fresh / held / unsupported | tools vda/user vdb、RO tools、UID1000、workdir/env/E2B ID；PVM/CPU256 拒绝且无 runtime 分配 |
| 真正持久磁盘 held | `/var/tmp` 位于 vdb，持续 fdatasync 写固定 marker；read-only debugfs -D 观测 held 时 marker 不变，恢复后继续增长；source rate非零、target rate禁用，当前机器配置确认清空 |
| auth / neighbor / deny-all / runner death | 两个真实来宾，错误 envd token 拒绝、邻居 MMDS token401；只 SIGKILL 已确认 UID/PID 的测试 runner，强制 reap 后释放资源，兄弟仍执行成功 |
| metadata / extension / cancellation | 过大 metadata 启动失败回滚；显式 HTTP503 配置被忽略的真实 RED 后修复、paired stop；启动 future取消后 owned cleanup 完成 |
| Recoverable / retained lower RED → GREEN | 已Held二次pause错误分类 RED0/1，Terminal修复后全7/0；移除 failed-reap sealed-layer retention 后真实 sleep child断言RED0/1，恢复后节点12/0，测试 child始终手动reap |
| 完整 startup/security | 7 passed/0 failed，最终约5.5秒；非root、隔离 network/mount namespaces |
| runner/protocol / KVM CLI | 非ignored suites退出0；旧/新 KVM CLI分别3/0，typed snapshots7/0 |
| 节点/共享/旧代码 | Dragonball12/0、restore identity9/0、custom extension12/0、memory7/0+1ignored、旧FC writer5/0、storage7/0 |
| 实际 M0 / FC guest | 5/0 和9/0，约11.6/25.8秒；FC使用正确独立内核，包含 snapshot/attached-drive/volume回归 |
| lint / fmt / source | 严格 workspace clippy退出0；冗余测试Ok/问号修正，无suppression；workspace fmt退出0；20个Rust源码本地/远程SHA256一致 |

第一次 held fixture 使用 `/tmp` marker（来宾 tmpfs），并对64GiB用户盘做两次完整hash，运行约641秒；该日志保留作诊断，不能冒充持久 marker 资格。改用实际vdb `/var/tmp` 后单项约3秒。新旧测试均来自真实 Cargo 产物；source sync 完成后才编译，不编辑 Cargo cache。

日志仍在同一 `m1-logs`：`task7-{rate-red,rate-green,build-identity-red,build-identity-green,held-behavior-red,held-first,held-disk-counter,extension-behavior-red,held-recoverable-red,retained-lower-red,node-green,startup-security-final,closure,clippy-final,fmt-final}.log`。旧service复核 not-found/inactive/MainPID0；main checkout、旧数据和系统安装不变。尚不宣告 M1、M2–M5 或全量 `make test` 完成。

## Task 8：一致捕获、pause/resume 和 owner

完整捕获与可选源恢复由一个 owned task 持有 RuntimeStarted、token、原始 RAM、全部磁盘、metadata 和封存目录。snapshot 返回前 EndCapture 并显式查询 Status Running；pause/capture_to_dir 留 Held，in-place resume 保留同一 ID/RAM、可重复调用。成功 snapshot 的 CapturedSandboxSnapshot 持有独立 Arc 产物；source stop 后产物仍在，最后 owner 删除未发布目录。live restacked lowers 的目录 lease 则独立由 source resources 持有。

每个 Recoverable 都再次确认真实 Running；Export 状态未知时不重发 save，只用原 token 释放并确认。已 seal/restack 或无法确认 capture/恢复的错误为 Terminal，返回前 stop/reap；无法确认退出仍保留所有 lease。低层 backend future 取消后 detached owner 完成操作，未取回的 runtime 通过退出确认再释放；没有声称直接取消后仍有可调用句柄，API/orchestrator 的请求屏蔽保持在其拥有层。持久化 paused codec/分派在 Task 10，完整 POSIX publication 和大内存/并发 fork 矩阵仍为后续门槛。

真实 guest 的新静态 C 负载连续 O_DIRECT 写4096字节并 fdatasync，每个512个64位word带序号校验和；独立 tmpfs 共享 RAM 页保存唯一 marker、issued/completed 序号。捕获后 source 完全退出，目标 LoadHeld 后比较私有 full-RAM 文件中唯一物理页与实际 vdb 的持久块；完整校验和正确且 completed <= disk <= issued，issued-completed至多1，恢复后内存序号继续增长。无 fsfreeze/sleep 作为捕获前提。

| 检查 | 实际结果 |
| --- | --- |
| 新生命周期 RED | 真实负载建立>10次fdatasync后 snapshot 返回未实现；pause捕获成功后 resume 返回未实现，分别0/1；保留失败日志 |
| 活跃块写切点 | 新检查在 Task 7 提前实现的 producer 上已1/0；不撤销已有资格来制造 RED；最终全组重跑通过 |
| snapshot / same-ID pause | snapshot 返回源 Running/工作负载继续、payload跨source stop存活；pause后in-place resume两次、完全停止后新runtime同ID/RAM恢复 |
| 实际丢响应 | 测试专用UDS代理转发真实runner，仅丢弃成功Begin/Export响应；Begin未知则Terminal/reap，Export用原token释放后Running；每次操作只发1次 |
| 实际 production deadline | 不改生产超时：Begin30秒、Export60秒均真实超时；前者teardown，后者同token恢复Running并执行guest命令 |
| caller取消 | 在真实Export完成/RAM已导出但响应未交付的oneshot屏障取消调用方；资源目录保持，owner完成后确认reap再移除，无save重发 |
| 实际 ENOSPC | 非root CAP_SYS_ADMIN隔离mount namespace，64KiB tmpfs；检查内层原始errno ENOSPC，源Status Running/guest执行；无partial/final RAM lower，只留调用者拥有的staged vm-state |
| post-restack failure | preflight之后阻塞最终image.json采用；真实rootfs/snapshot.commit已封存且非空，再staging失败；Terminal和资源teardown已确认 |
| 最终 Dragonball 全组 | 17 passed/0 failed/0 ignored，约114秒，含Task7所有安全启动测试和真实90秒deadline |
| 原runner/node/共享/M0/FC | whole runner/protocol、两套KVM CLI、node/reader/extension/共享writer退出0；实际M0 5/0、FC9/0，FC过滤另有1个DB ignored测试已在17项中显式执行 |
| 严格检查 / 一致性 | workspace clippy/fmt/diff退出0，无lint suppression；3个Rust源码和C fixture本地/远程SHA256一致 |

前置测试失败未冒充 capture RED：数字HTTP username1000被envd拒绝；使用默认USER后旧tools的上传响应为text/plain，与生成客户端JSON模型不符，测试改用正常认证multipart并检查HTTP状态。期间401/模型/编译失败日志全部保留，不修改guest鉴权或生成代码。ENOSPC mount最初把nosuid/nodev放入tmpfs数据而非mount flags，fixture修正后真实压缩失败；错误外层只显示compact上下文，检查内层io errno后通过。固定长度分块的clippy错误改为as_chunks，无suppression。

同一m1-logs下包含 task8的api/setup/behavior-red、lifecycle、real-fault、storage-fault、production-timeouts、enospc-final、clippy-final、fmt-final及closure日志；真实命令仍使用UID999和隔离state，旧service仍卸载、main checkout/旧数据未动。上述不是M1整体、M2–M5或全量make test通过声明。

## Task 9：并发 fork 与卷生命周期

fork 只捕获一次，确认源 Running 后并发尝试所有 child，保持输入顺序与独立结果；一个 child 的纯预检或实际额外盘分配失败不终止成功兄弟。每个 child 重新建立 metadata、token、实例 ID、网络 lease、完整私有 RAM 普通文件和 writable upper。父 token 必须被 child 鉴权拒绝，不能用任意 RPC 错误代替。

卷 reservation、上传状态、持久层发布、独占 child 分配与回滚仍归现有 VolumeManager/API；backend 只消费已解析的 child drives。替换独占卷的 catalog ID 不改写捕获的 virtio config/transport/queue/feature；在分配资源前验证容量、RO、mount/subpath 与完整槽位数，再将其当前 backing 绑定到原物理槽的逻辑 ID。非卷 attached disk 不允许替换或追加；RO 卷可重新绑定同拓扑 backing。合法新槽、容量/feature 变化仍为 M2。

FC 的 opened-FD fsfreeze/thaw 脚本与持久卷 sealing helper 提取到共享 volume_snapshot，原流程不变。DB 先同步或冻结 guest 文件系统，再用真实 BeginCapture 完整 CPU/worker ack 屏障封存卷，原 token 释放并确认 Running；不为只封存卷导出 RAM。已知 freeze 拒绝必须解冻先前 mount；thaw 只能作用于冻结时的 filesystem device，挂载点变更不能误解冻另一个 filesystem。未知 freeze/hold/seal/release 结果为 Terminal，需要 stop/reap，不能返回假 Running。操作与 child 分配有独立 owner，调用方 future drop 不回收仍被 VMM 使用的 lease。

| 检查 | 实际结果 |
| --- | --- |
| fork / volume 行为 RED | 成功启动并写入真实 guest 后，fork、volume capture、成功 freeze 分别失败于原未实现接口；不是缺 KVM/依赖 |
| 真实多 child 隔离 | rootfs、同一 tmpfs RAM 页、attached disk、exclusive volume 写入父/兄弟不同内容，无串写；新 child ID/metadata 与独立 token 生效 |
| child 部分失败 | 不存在的捕获 ID 在预检拒绝；另一个 child 在 root/网络分配后因实际缺少 volume image 配置失败；成功兄弟与源仍可执行，失败卷 reservation 释放后删除 |
| child 取消 | 两个真实 start-resume HTTP hook 同时到达 barrier，drop fork future 后源仍 Running；释放 barrier 后未交付 child 的 runner/新建 work/device/network lease 回收 |
| 独占与 RO | 两个 child 独占 upper、RO 两个 catalog owner；RO guest 写入拒绝，mounted 独占卷删除拒绝 |
| 删除源卷后的恢复 | 源停止并释放 reservation 后删除其目录；从已发布 child 卷重新 cold boot 能读到捕获 seed，新恢复 guest 写入不污染现存 child upper |
| freeze / thaw | 第二个 volume 已卸载时拒绝 freeze，并解冻第一个；成功 freeze 后 bind 更换 mount 被 thaw 拒绝，恢复原 mount 后可 thaw/写入 |
| VolumeManager suite | 7 passed/0 failed，含 uploading/failed 挂载与 clone 拒绝、部分分配回滚、RO 最后 owner、父删除后持久层仍可用；新 characterization 不改变既有 policy |
| 完整实际 DB guest | 22 passed/0 failed，包含所有 Task7/8 及5个新 Task9用例，134.29秒 |
| 原 M0 / FC | M0 5 passed/0 failed；FC snapshot/attached-drive/volume 9 passed/0 failed，另1个 DB ignored 已在22项中显式执行 |
| 严格检查 | workspace clippy --all-targets --all-features -D warnings、fmt --all --check、git diff --check 退出0；10个 Rust 源码本地/远程 SHA256 一致 |

取消测试初次误比较了整个共享 qualification 目录，包含13个之前保留且没有活 runner 的失败证据目录，导致 deadline。只将本次测试新建目录作为释放断言范围，既不删除旧证据，也不忽略本次 child 泄漏。旧 tools 在 gRPC 路由返回 JSON HTTP401，tonic 因 framing 将其报为 Internal；最终改用真实 HTTP /files 检查父 token 为401、新 child token 为200，4项 fork/取消/部分失败/volume 用例复验通过。编译/fixture 错误日志保留，不冒充行为 RED。

这里的删除后恢复证明的是已发布 volume layers，完整 VM snapshot 的 POSIX record/manifest/envelope/volume-slot 读取与恢复仍是 Task11 的端到端门槛；paused backend 持久化仍是 Task10。默认 server/API wiring 保持 FC，M3 才切换产品配置。无服务重装、旧数据清理、依赖升级或 upstream/main 推送。日志位于同一 m1-logs 的 task9-*-{red,first,final}、task9-volume-unit、task9-clippy、task9-closure，以及 task7-closure-m1/m0。

## Task 10：暂停持久化、异类记录与进程重启

paused 外层保存 backend、完整 restore identity 和独立 runtime artifact 引用；在调用具体 codec 前同时校验外层、metadata 与当前 reader。未知 backend/ABI、qualified decode 失败和其他 virtualization mode 的记录保留，作为不可恢复的 Paused metadata 可见；resume 在状态 CAS/mark-resuming 前拒绝。未知 outer schema 保留原始 bytes，缺少可重建引用图时停用自动 image GC，不能用空保护引用替换原有 liveness。可识别的损坏 legacy FC state 与原 Resuming 清理语义保留。

DB 的 records.db 与 artifacts 一起隔离到 `<paused-root>/dragonball/`；FC 原 root/schema/codec 不移动。具体 factory 提供 fresh runtime versions，直接选择 DB factory 的 Orchestrator 不再写出 FC header，默认 server 仍选择 FC。新的严格 node-local paused DTO 不包含 runtime token；恢复使用有界便携 envelope 和 allocated-root 路径校验，保存同一 ID、env/context、网络、extra drives、control port 和扩展参数，分配前复用 factory 预检。它不是跨节点的 portable published record。

存储目录仅在 durable record 写成功后转交 persister；失败写入不能绕过 source 的 live lease 删除 sealed lowers。后续 capture 把实际保留目录交给已有 runtime-suffix adoption，用独立 hardlink/copy 保存继承层，避免失败回滚后的重复暂停依赖被旧进程清理。FC 使用原默认 runtime roots、dirty-memory adapter 和调用顺序。

| 检查 | 实际结果 |
| --- | --- |
| 分派与未知记录 RED → GREEN | 原 paused_ 23 passed/3 failed；补外层一致性、未知 outer GC、缺引用图、resume 状态前置检查的实际 RED 后通过 |
| 持久化单测 | file_persister 1/0、完整 file_backed 22/0、paused_ 29/0、orchestrator 155/0；含 legacy FC codec、mixed-mode、mark/rollback 和合法旧损坏清理 |
| 新进程行为 RED | DB guest 启动时误记 FC header、未实现 encode、提交后 guard 删除 memory 配置分别真实失败，修复后新进程可读 |
| 真实写失败与重复暂停 RED | RocksDB path 被测试文件阻塞；恢复 Running 后 sealed lower 被删除，随后又发现第二次提交引用第一代已删除 lower；两个实际故障均复现并修复 |
| 实际重启 | 两个独立 Orchestrator 进程；旧 server/runner PID 完全退出后同 ID 恢复，rootfs/RAM/env、UID1000/workdir/tmp 和持久化管理的鉴权种子正常；shutdown 保持永久关闭 |
| 扩展与 token | 非空嵌套参数原样跨进程恢复；四个真实 HTTP hooks 按 start-fresh/stop/start-resume/stop 配对，新 instance ID；token 不写入 node-local state |
| 完整回归 | DB 23 passed/0 failed，M0 5/0，FC snapshot/attached-drive/volume 9/0；FC 过滤中的1个 DB ignored 已显式执行 |
| 严格检查与一致性 | workspace fmt/clippy --all-targets --all-features -D warnings、diff check 退出0；13个 Rust 源码本地/远程 SHA256 一致 |

实际旧 FC server 使用卸载备份的临时副本，不安装 service；副本与备份的 SHA256 都是 `79d464c930dd1ac5c1e34bca926f074d60ea2a7403ac339a28c4c616dc5507fd`。在 DB create/restore 之间启动旧 server，使用独立 home/runtime 和相同 paused root；首次通过1/0，最终完整 DB23/0 闭环也确认旧进程确实读到该 root、API readiness 与退出日志，随后 DB 仍恢复同一暂停记录。原备份、旧数据和 main checkout 均未改动。

日志位于同一 `m1-logs`：`task10-{paused-red,paused-green,conflict-red,gc-red,missing-graph-red,failure-green-unit,closure-final,qualified-closure}.log` 和 `task7-task10-{restart-red3,codec-red,owner-red,store-failure-red,restart-qualified,adoption-green,legacy}.log`。fixture 的独立 runtime 目录缺失、权限和 UUID 目录不匹配均保留诊断，不冒充产品行为 RED。构建与测试仍为 UID999，使用隔离 namespace/state；aenv.service 保持 not-found/inactive/MainPID0。本任务尚不是完整 VM snapshot 的 POSIX publication、大内存矩阵或全量 `make test` 验收，M1/M2–M5 不宣告完成。

## Task 11：活跃负载与提交后恢复

完整 RAM 对照保持普通文件导出/恢复。来宾持续 O_DIRECT 块写、fdatasync 和 RAM issued/completed 计数；host 与来宾交换逐字校验的 UDP 数据，捕获的 RAM 同时包含 RX/TX 计数。流式扫描完整导出文件定位实际工作页，并按 memory layout 转换回 GPA；磁盘切点满足 `completed <= disk <= issued`。每个内存规格执行两次暂停/恢复，再运行两个同时存活的 child，检查 rootfs/RAM 写入隔离及网络继续推进。

| 检查 | 实际结果 |
| --- | --- |
| 1 GiB 活跃负载 | 1 passed/0 failed，75.02 秒；GPA `0x4d39000`，两次 issued/completed/disk 为 249/248/249、393/392/393 |
| 2 GiB 活跃负载 | 1 passed/0 failed，153.22 秒；GPA `0x4e85000`，切点 250/249/250、406/405/406 |
| 8 GiB 活跃负载 | 1 passed/0 failed，85.93 秒；实际工作页 GPA `0x1036aa000`，大于 4 GiB；切点 228/227/228、360/359/360，RX/TX 366/365、390/390 |
| runtime budget RED → GREEN | 实际 KVM adapter 将 node 的 60 秒请求误裁为 probe 的 30 秒；修复后 runtime 最多 60 秒、probe 仍最多 30 秒，1/0 |
| POSIX 根盘提交后恢复 | 原未实现 factory 实际 RED；修复后源 VM、源捕获目录移除，加载已提交 record 恢复 rootfs/RAM/计数、UID1000/workdir、新 ID/token/env，1/0，5.32 秒 |
| record 冲突 | 实际 catalog backend 篡改，factory 在资源分配前返回 identity conflict，目录集合不变；RED → GREEN，1/0 |
| 发布失败 | 两次实际捕获竞争相同 alias，晚期仓库提交失败；源保持 Running、RAM 继续推进，无失败 record，1/0 |
| runner SIGKILL | 未 reap 时 PID、work/capture、磁盘与 TAP lease 保留；stop 确认 reap 后释放，新的 VM 实际复用相同 netns；共享 tools 文件保留，1/0 |
| POSIX 混合磁盘恢复 | 附加盘新层命名导致发布拒绝、缺 volume 物理映射导致恢复拒绝分别实际 RED；补齐后删除原卷 catalog，使用新卷 ID、倒序传入，rootfs/RAM/attached/exclusive/RO 恢复，1/0，5.80 秒；完整入口中同容量双写卷及五种非法绑定拒绝矩阵也通过 |

8 GiB 在测试机块存储上无法于现有 60 秒 Export deadline 内完成；两份失败日志保留，第二次原始串口/error 位于 `/var/tmp/db-m1-memory-68zfIa`。通过的正确性测试使用隔离 mount namespace 内的 32 GiB tmpfs，需要至少 40 GiB 可用 host RAM；全部 owned runner stop 后才卸载、移除该次 scratch。结果不证明慢盘导出时延、增量内存或 lazy/COW RAM，性能资格属于 M4。

附加盘按既有共享发布契约使用独立捕获目录下的 `snapshot.commit`；持久卷目录保留唯一层文件名。manifest 新增可省略的 `volumeDevices`，只记录捕获的逻辑设备 ID、guest mount/subpath、RO 和容量。实际层引用仍由 committed `volume_snapshots` 保存，volume catalog/readiness/reservation 仍由 VolumeManager 管理。恢复先验证 record/manifest/envelope，按 mount 匹配新卷，再按已捕获逻辑 ID 排列物理绑定。旧 FC manifest 默认空字段；M1 不增加可变设备槽。

新增显式 `make test-dragonball-m1` 和 non-root 前提检查；复用隔离 capability wrapper，实际执行 KVM adapter budget 用例及 ignored M1 guests。manual CI job 使用 `dragonball-m1` self-hosted 标签、独立 state 和同名 environment；管理员仍需配置 runner 和 environment protection，尚无 GitHub job 执行证据。默认 `make test` 继续运行 FC/guest/storage suites。

远程 `make fmt clippy test test-unit` 已退出 0（`task11-make-fc5.log`），随后显式 M0 5/0；完整 M1 首轮为 28 passed/3 failed，仍未满足 M1 退出门槛。日志在 `m1-logs/task11-*` 与 `task7-task11-*`。Tokio 文件/UDP/timeout 与 GitHub Actions 配置使用 Context7 文档核对，并由固定依赖构建及实际网络负载验证。

聚合入口的实际失败也保留：首次为新增测试的三个严格 clippy 告警，修复后检查通过；第二次主库 987/0、二进制 1/0，但 21 个 FC integration 因环境丢失失败。最小 probe 确认主机 `sudo-rs 0.2.13` 忽略 `-E`；本次任务 PATH 使用已安装的 `/usr/bin/sudo.ws` 后，六个指定配置变量完整传递，FC integration 21/0（41.37 秒）、Orchestrator 3/0。系统 sudo alternative 未改动。随后聚合 ignored unit suite 暴露 ENOSPC 用例依赖手工 tmpfs 的测试入口缺口；补为独立子进程 mount namespace 自备 64 KiB tmpfs，单项 1/0，聚合显式 ignored lib 5/0。诊断脚本的 `RUST_BACKTRACE=1` 又使既有 CLI 精确错误文本断言失败；聚合脚本恢复默认的 0 后 CLI 110/0，无产品改动或跳过测试。测试账号 sudo/Docker 权限仅在监督脚本运行期间临时开放，EXIT 自动撤回。

`fc5` 完整入口还包括 agent lib 987/0、FC integration 21/0、Orchestrator 3/0、envd 3/0、OverlayBD unit 383/0、registry/ublk 4/0、ublk unit 18/0、daemon 49/0、ublk integration 37/0、显式 MinIO 7/0，以及三个 shell verifier；各 Make 目标可能重复执行 suite，不将计数相加冒充唯一测试总量。普通 ignored 用例仍须按资格范围显式执行。

M1 首轮 1/2 GiB 在两次 held cut 通过后于并发 fork 报 `Broken pipe`，8 GiB tmpfs 通过；另一个 post-restack 故障用例报 early EOF。完整恢复 RAM 的同步文件写入与 fsync 会阻塞同一 executor 上的兄弟控制连接，而 runner 的收帧期限为 1 秒。修复使用异步 open/write/flush/sync，不放宽帧期限、不重试未知结果的修改请求。原 1 GiB 用例随后 1/0（74.37 秒），post-restack 单项 1/0；新的完整 M1 **31 passed/0 failed/0 ignored，473.79 秒，Make 退出 0**（`task11-async-full-m1.log`）。本轮 1/2/8 GiB 实际工作页 GPA 分别为 `0x4d17000`、`0x4eb0000`、`0x1036b4000`，均完成两次切点和两个并发 child；30/60 秒故障与 post-restack 顺序重放也通过。修复后聚合检查和独立分支审查仍进行中。

异步 RAM 修复后的 `task11-async-final-closure.log` 也已退出 0：重新执行 `make fmt clippy test test-unit`，显式新旧 KVM CLI 各 3/0、VolumeManager 7/0、io_uring discard/rewrite/reopen 1/0（四组实际 406/407/419/420 SQE）、FC snapshot/attached-drive/volume 9/0。FC 过滤中的两个 ignored DB 用例已包含在显式 M1 31/0 中；默认跳过的 cache stress、非 Linux sparse-upper 和 M3 startup-pack 矩阵不冒充已测。收尾检查账号仅属 aenv/kvm，临时 sudoers 已不存在，旧 `aenv.service` 为 not-found/inactive/MainPID=0。随后完整分支独立审查发现四项问题，修复后的全量复验仍待完成，故此处的历史通过不作为 M1 最终退出证明。

本轮主机为 x86_64、104 CPU（Intel Xeon Platinum 8269CY @ 2.50GHz）、两个 host NUMA node、约 187 GiB RAM，Linux 7.0.0-28-generic、Rust 1.98.0。VM 使用已限定的单 NUMA、block/net profile 与同主机 CPU 校验；这些结果不宣称跨 CPU 或多 NUMA VM 兼容。

### 完整分支审查与一致性修复

一次 fresh-context 独立审查覆盖 AgentENV `c748eb4..dddd26e`、Task11 未提交改动，以及 Kata `bfc470b..0bc22d5`。两项 Critical 和两项 Important 均接受，未发现 Minor；审查没有因已有 31/0 和聚合通过而判定可合并。按一次修复流程逐项复现，未重复委派审查。

| 问题 | 实际回归与修复 |
| --- | --- |
| fork 绑定预分配旧卷，而非实际捕获版本 | child 分配后源写入并同步 root/RAM/volume，旧实现 child 读到 `cutcut` 加三个零字节而非 `cutcutcut`，0/1；改为从实际捕获采用 immutable lowers 到已分配 child catalog，1/0，4.57 秒 |
| 延后发布读取后续卷版本 | 捕获 A，再写入并捕获 B，最后发布 A；held/vCPU 尚未放行时直接读取目标 ublk 得到 `later-volume` 而非 `published-volume`，0/1。每次捕获拥有独立卷配置/lower lease，发布使用该捕获而非最新 catalog；1/0，6.77 秒 |
| 失败 child 遗留共享 RO 预留 | 真实 POSIX/VolumeManager 回归先 0/1；将所有 reserved IDs 与新建 created IDs 分开，释放前者、只删除后者，1/0；源实例及成功兄弟的预留保持不变 |
| API 请求取消中断卷回滚 | 在真实子卷部分分配后，以及真实 fork 失败、回滚已开始且仍有 child catalog 时取消，分别失败；将分配、fork outcome 和清理纳入同一 owned task，连同 RO 清理三项回归 3/0，0.28 秒 |

延后发布的最初 guest-resume 检查被页缓存/文件系统恢复掩盖，不能证明物理盘版本；增加上述 held ublk 检查后才获得有效 RED。API 清理测试不伪造 VM 成功：真实卷分配后由实际 Orchestrator 返回 NotFound，再验证取消不妨碍回滚。这里保证请求取消，不把 runtime/process 强制退出称为事务恢复。

定向日志：`task7-task11-review-fork-cut-{red,green2}.log`、`task7-task11-review-publication-held-red2.log`、`task7-task11-review-publication-cut-green2.log`、`task11-review-ro-cleanup-{red,green}.log`、`task11-review-cancel-red.log`、`task11-review-cancel-rollback-red.log`。所有结果来自非 root 测试进程。

审查未覆盖项分别裁决保留后续门槛：M2 lazy/shared RAM、dirty/zero、balloon/slot/recording；M3 server/template/完整 OSS/P2P/registry/配置打包；M4 慢存储性能、跨节点/CPU、灰度及默认切换；M5 历史依赖清零与 FC/PVM/aarch64 退役。未知外层 schema 保留 bytes/artifacts，但不能凭未知格式重建可见 metadata；本次也不等同于新的仓库级安全扫描。这些范围不会用本机 full-memory 正确性代替，也不会删除旧状态来满足门槛。

### 最终 M1 验收

四项审查问题均完成有效 RED→GREEN 后，使用同一份固定源码重新执行：

| 最终检查 | 结果与日志 |
| --- | --- |
| `make fmt clippy test test-unit` | 退出 0，`task11-review-final-closure.log`；主库 990/0、显式 ignored lib 5/0、FC integration 21/0、Orchestrator 3/0、envd 3/0、OverlayBD 383/0、ublk 18/0、daemon 49/0、ublk integration 37/0、显式 MinIO 7/0、CLI 110/0；三项 shell verifier 通过 |
| 补充真实回归 | 同一 closure 退出 0；新旧 KVM CLI 各 3/0、VolumeManager 7/0、io_uring discard/rewrite/reopen 1/0（406/407/419/420 SQE）、FC snapshot/attached-drive/volume 9/0，25.38 秒 |
| `make test-dragonball-m0 test-dragonball-m1` | 退出 0，`task11-review-final-db.log`；M0 5/0，11.50 秒；M1 31/0/0 ignored，471.56 秒 |
| 大内存与故障 | 1/2/8 GiB 均完成两次 held cut 和两个并发 child；工作页 GPA 为 `0x4d15000`、`0x4ead000`、`0x103697000`。实际 30/60 秒超时、ENOSPC、发布失败、runner SIGKILL、重启和卷版本修复均通过 |
| 一致性与主机收尾 | 18 个代码/构建/脚本文件 SHA-256 本地与远程一致，diff check 通过；aenv 仅属 aenv/kvm，临时 sudoers 不存在，`aenv.service` 为 not-found/inactive/MainPID=0 |

完整日志已复制到本地 M1 执行证据目录；失败日志和旧数据保留。Make 聚合目标可能重复测试，不叠加这些计数冒充唯一测试数。上述结论是 AgentENV M1 资格通过，并不改变前述 Kata 全套最小 profile 的 9 项既有失败和严格 lint 基线问题，也不声称 GitHub 手动 CI、M2 内存路径或 M4 性能已经通过。分支暂不合并或切换默认，继续按路线进入 M2 详细计划。
