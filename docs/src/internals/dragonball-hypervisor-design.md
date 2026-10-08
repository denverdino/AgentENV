# Dragonball 后端架构

AgentENV 保留 Firecracker 与 Dragonball 两个编译时可选后端。Firecracker 是默认
产品；Dragonball 使用每个 sandbox 一个独立的 `aenv-dragonball` 进程，进程内嵌入
Kata 仓库中的 Dragonball VMM 库。它不引入 Kata runtime/shim、kata-agent 或第二套
镜像管理流程。产品选择规则见[后端组件](hypervisor-components-design.md)。

## 责任边界

| 模块 | 职责 |
| --- | --- |
| [公共后端契约](../../../src/sandbox/backend.rs) | sandbox 生命周期、执行器和捕获产物所有权 |
| [产品装配](../../../src/sandbox/selected.rs) | server、orchestrator、模板共用编译时 factory |
| [Dragonball adapter](../../../src/sandbox/dragonball/mod.rs) | guest 初始化、网络/磁盘绑定、捕获、恢复、资源回收 |
| [Runner](../../../crates/dragonball-runner/src/runtime_service.rs) | 驱动 VMM、设备静止屏障、held 状态与进程内安全约束 |
| [Runtime 协议](../../../crates/vmm-protocol/src/runtime.rs) | 私有 UDS 请求、运行身份、内存 lease 和快照拓扑校验 |
| [Probe](../../../src/sandbox/dragonball/probe.rs) | 独立的底层启动及保存/恢复诊断，不代替生产 runtime |

网络由现有 NetworkManager 管理，磁盘与内存设备由 UblkDeviceManager 管理，
guest 继续使用 envd。SnapshotManager、卷管理、模板和控制面保持原有职责。
公共代码只承载双方实际需要的契约；DB runner、held/read-pin、恢复 ABI 与安全
校验留在 DB adapter。下游维护及 upstream 拆分策略见
[工程准则](../../../AGENTS_Updated.md#9-dragonball-下游维护与逐步-upstream-策略)。

## 启动与 guest 契约

生产支持范围为 Linux x86_64/KVM。配置检查在分配 VM 前拒绝不支持的平台、CPU
策略及存储选项；具体条件由 [DragonballRuntimePolicy](../../../src/sandbox/dragonball/config.rs)
维护。runner、kernel 和 tools 必须是显式、合格的资产。

设备布局保持 tools 根盘、用户镜像、附加盘和固定卷槽的逻辑顺序。只读 tools
根盘允许 host page cache，用户盘和卷保持 direct I/O；创建和恢复对该策略使用
相同的严格校验。网卡仍接入现有 TAP/namespace，metadata listener 为 guest 提供
当前 sandbox/template 身份。

恢复先处于 LoadedHeld，完成身份、设备和限速绑定后再运行 guest。readiness
等待 envd health 和 `/init`，初始化最终 USER/WORKDIR/环境变量；内部挂盘明确
使用 root 与 `/`，并保留 runner 安全检查。

新建 sandbox、快照/模板创建、fork 和启动预取记录器会产生新身份，必须确认 guest
已读取当前 sandbox/template 身份后才发布 ready。身份确认使用带 runtime token 的
envd `/envs`，精确核对两个字段；该接口与新命令读取同一份默认环境，避免为校验
额外启动 shell。`/init` 的异步 metadata 刷新仍须等到字段匹配，保留原有截止时间。
同 ID 的 pause/resume（包括服务重启后恢复原暂停实例）跳过内部身份轮询；它仍保留健康检查、初始化和
必要挂盘。原地释放暂停的运行时沿用已有身份。
性能评测分别记录 API 返回耗时和准确身份就绪耗时，两后端使用相同的外部身份检查。
生命周期实现见 [sandbox.rs](../../../src/sandbox/dragonball/sandbox.rs)。

## 捕获、内存与存储

捕获使用跨 vCPU 和设备 I/O 的 hold/drain 屏障；单独暂停 vCPU 不代表设备写入
已经停止。磁盘封层、restack、附加盘和卷状态须与所捕获内存保持一致。
`CapturedSandboxSnapshot` 在仓库发布完成前持有资源。可恢复失败回到 Running，
终止性失败需要 teardown；pause 保持原 ID，snapshot launch/fork 使用新实例身份。

生产内存路径使用共享只读 ublk backing 与私有 COW 映射。增量采集包含 CPU、设备
写入及清零语义，并保持累计 dirty 范围和代际；取消时等待阻塞读取排空后释放 pin。
balloon 回收不能修改共享 lower。全量文件模式保留为诊断对照。实现入口为
[capture](../../../src/sandbox/dragonball/capture.rs)、
[memory](../../../src/sandbox/dragonball/memory.rs) 和
[memory reader](../../../src/sandbox/dragonball/memory_reader.rs)。

仓库保存逻辑层引用；节点缓存、运行时路径和持久化产物的所有权规则见
[持久化清单](persistence-artifact-inventory.md)。卷预约、fork 和发布状态仍由
VolumeManager/仓库负责；跨后端抽象不能改变这些生命周期。

## 恢复身份与依赖

恢复校验 backend、runner build ID、Kata revision、格式 epoch、ABI 以及设备/CPU
拓扑；不兼容记录明确拒绝并保留。沿用既有仓库键名不表示 FC 与 DB 状态可互读。
Firecracker 与 Dragonball 双向跨后端内存恢复不在支持范围内；模板可按 recipe/OCI
重新构建，节点按后端分池。

Kata 补丁保留在固定 fork revision，实际依赖以 [Cargo.toml](../../../Cargo.toml)
和 [Cargo.lock](../../../Cargo.lock) 为准。依赖升级需同步 runner 身份与打包断言，
并验证新候选的当前版本恢复；开发期旧版本升级集成测试按格式/兼容规则变更或
发布需要专项运行，不成为每次迭代的固定成本。

开发入口见[功能回归](dragonball-regression.md)，交付资产见
[候选包](dragonball-packaging.md)，已验证范围和已知限制见
[验证摘要](hypervisor-validation.md)。
