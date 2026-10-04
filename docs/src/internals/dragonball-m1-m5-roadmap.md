# Dragonball M1–M5 执行与验收路线

2026-10-02 用户目标修订：Firecracker 和 Dragonball 保留为可配置组件，首版采用编译时选择；M5 不再删除 Firecracker。组件化实施见 [设计](./hypervisor-components-design.md) 与 [计划](./hypervisor-components-implementation-plan.md)。其余内存一致性、跨节点和性能门槛仍保留。

2026-10-04 用户优先级修订：功能测试与性能优化分开，先完成所有容量允许的功能
验收及稳定性故障闭环。性能10%门槛、p95/p99、停顿/峰值SLO独立保留，不阻塞功能
结果交接；功能通过不等于性能通过，也不自动授权生产切流。具体顺序见
[M4功能收尾](./dragonball-m4-implementation-plan.md#功能收尾先执行未完成不得由性能延期掩盖)。

同日最新指令进一步收敛验收：不要求容量测试，完成功能验证后将全部修改合并到
本地main。容量矩阵与性能资格继续保持独立、未覆盖项如实记录；不推送或生产切流。

2026-10-05功能收尾通过：最新PL0候选双向跨ECS实例恢复/故障/回滚与资源审计完成；
干净源码`6a57e3a`完整`make test test-unit test-agent-integration fmt` exit0，DB-only
release库956通过，严格workspace Clippy、双后端打包与M4驱动检查通过。
两处串行libtest同步标记问题仅修改测试，并保留失败及RED→GREEN证据。
详见[M4合并前回归](./dragonball-m4-results.md#2026-10-05-合并前完整回归)。
功能线闭环不改变下述历史失败，也不宣称容量/性能、断电耐久性或生产资格通过。

日期：2026-10-03。状态：M1 实施和审查修复后的完整验收已通过，证据见 [M1 结果](./dragonball-m1-results.md)。用户已批准自动执行 M2–M5；M2 Tasks 1–10 已完成，最终 Make M0 5/0、M1 32/0、M2 51/0，APF 补充资格及审查修复均已闭环，见 [M2 结果](./dragonball-m2-results.md)。编译时组件装配已通过[专项验收](./hypervisor-components-verification.md)；M3 Tasks 1–4、单轮审查修复、完整 Make 回归和独立候选制品 .2 均已通过，见 [M3 结果](./dragonball-m3-results.md)，不等于 M4/M5 完成。Native 执行方式保持不变。

依据：[已确认设计](./dragonball-hypervisor-design.md)、[M0 实测结果](./dragonball-m0-results.md)。AgentENV 起点为 `2d82c0a`，完整 Kata fork 起点为 `bfc470b07aedaeddaf801ff091f5cdb1a12c69bc`。继续复用独立 runner、原 guest/envd、现有网络、OverlayBD/ublk、仓库、卷和控制面，不接入 Kata runtime/shim。

M1–M5 不是一个可以通过一次配置修改完成的开关。按下列依赖顺序交付，每阶段保留自己的 RED/GREEN、Linux 实测、审查和未覆盖项。M1 的详细任务见 [M1 实施计划](./dragonball-m1-implementation-plan.md)；M2–M5 在前阶段接口验收后分别形成详细计划，不预先编造未确定的 upstream ABI 或 release checksum。

## 阶段和可交接产物

| 阶段 | 实施顺序 | 可交接产物与退出判据 |
| --- | --- | --- |
| M1 | worker 屏障 → held restore → runtime 协议 → portable envelope → full-memory writer → backend lifecycle/卷/paused → 活跃负载验证 | 真正 `SandboxBackend`，但尚不作为 server 默认；一次 token 覆盖 CPU、设备、RAM、所有可写盘。source 恢复 Running，child 独立，same-ID pause/resume 和进程重启通过 |
| M2 | backing-aware restore → memory export → 累计 dirty/zero → balloon → slot restore → startup recording | 多实例共享只读 ublk memory 的 COW；full/diff 逻辑内存逐字节一致；设备写页/清零/再次写入/回收均覆盖；卷容量/RO/feature 更新合法；recording lease 覆盖异步任务 |
| M3 | 选择 factory/模板 executor → 通用身份/仓库 → 配置/资产打包 → 扩展/registry/P2P → 运维和完整契约套件 | DB 通过 POSIX、OSS、私有 registry、扩展 start/patch/stop、P2P 与节点重启；旧 FC schema/paused 继续可读，异类记录不删除；默认仍为 FC |
| M4 | 冻结基准与 SLO → 分池模板/入口 → 同 ABI 跨节点 → 故障/回滚 → 默认切换 | 至少两个兼容 KVM 节点的恢复证据、冻结的性能判据、具体灰度批次和回滚报告；通过才改新部署默认，不让 FC 恢复 DB 内存 |
| M5 | 审计恢复依赖 → 保留双后端 → 独立构建/打包 → 从干净环境复验 | FC-only/DB-only 均通过各自契约；历史 paused/snapshot 与恢复资产保持可用；PVM/aarch64 保留 FC 支持，不因 x86_64 DB 通过而删除 |

### M1 → M2：接口锁定

M1 提供 `CaptureToken`、完整 worker ack、held/full export、合法 memory region/image offset、逻辑设备 ID、portable identity 和 capture/artifact owners。M2 在同一屏障内增加 `ExportMemoryMap` 与累计集合，不通过读取 KVM bitmap 单独证明完整 dirty 集合。普通 capture 成功或失败不清空累计集合；非零→零→再次写入必须撤销 zero 标记。

M1 恢复先将 OverlayBD memory 完整 materialize 为私有普通文件，作为正确性对照；不能以这条路径宣称 ublk 懒加载或性能达标。M2 用普通文件长度或 `BLKGETSIZE64` 校验容量，禁用 prealloc/hugetlb，保持 MAP_PRIVATE 与正确 mapping 元数据，测试 SIGBUS/I/O 错误和最后一个 shared-memory lease。

balloon 必须同时拥有 persist/restore、设备写页/zero 语义及 COW 回收证据。不得对共享 lower punch hole；不得把 `MADV_DONTNEED` 重新显示旧 backing 的页假装成零。slot restore 更新允许变化的容量并触发合法 guest config-change，实际只读打开和写拒绝仍是约束，不能强行篡改 negotiated features。

### M2 → M3：能力齐全后再完成产品装配

M1 为测试直接装配 DB factory；M3 才完成 `server` 的配置选择、模板 factory/executor、后端 recording task/lease 与发布打包。继续沿用历史 `firecracker-manifest.json` 存储键，通过内容识别 backend，不凭文件名猜测。没有业务理由不改 scheduler protobuf；首版使用分池入口。

配置保持两个独立维度：`hypervisor_backend` 和 `virtualization_mode`。不支持的 DB/PVM、CPU policy、architecture、device/profile 在分配资源前拒绝。提取真正公共设置，旧 `[firecracker]` 键有兼容与冲突校验，不让 DB 悄悄依赖该配置段。M0 candidate kernel 不能冒充 release；M3 发布独立版本、checksum、NOTICE 和后端选择策略，保留过渡期间旧资产。

### M3 → M4：性能和部署不是单机单测

冻结相同 CPU 池、kernel/tools、rootfs、存储/压缩、pool 和页缓存条件。测试并发 1/10/100（受容量约束）、内存 1/2/8 GiB、低/高 dirty 工作集。记录 API→envd ready、capture 停顿、pause/resume、fork 1/N、模板、块 I/O、CPU、RSS/PSS、读取/上传字节及失败率。

原设计的工程门槛为 p95 启动/恢复/fork、每 VM 内存占用及块吞吐相对 FC 退化不超过 10%；这不是已测结果。capture 停顿和峰值资源还需要 owner 提供或确认具体 SLO，必须在跑基准前冻结，失败后不能自行放宽。用户已批准在 `147.139.168.159` 上进行[单机双节点模拟](./dragonball-m4-single-host-design.md)：独立 net/mount/PID namespace、状态和 cgroup，通过真实网络仓库恢复。模拟功能可独立验收；跨物理宿主机门槛仍须第二台兼容主机，不能由单机成功替代。

M4 在测试环境演练独立 DB 节点池、模板别名、入口和 rollback。生产部署、流量调整及实际退役须提供目标清单和 owner 确认，不由“继续实施”推断生产变更权限。回滚只改变新建流量，DB paused/snapshot 留在兼容 DB 池，旧 FC same-ID resume 留在 FC 池。

### M4 → M5：保留可选组件与历史 reader

已卸载的旧 AgentENV 安装不等于其 snapshot/paused 依赖已清零。测试机 `/var/lib/aenv` 和 `/var/tmp/agentenv-uninstall-20260930.6clQzq` 保留了旧数据/资产；不自动删除或覆写。先对 scoped roots 进行只读 inventory，列出 backend、ABI、guest assets、owner 和保留策略。能重建的模板以原 recipe/OCI 生成新 ID/alias；内存状态不能转换成 DB，仅迁移 rootfs 不等于 same-ID resume。

保留 FC backend/client/pool、生成器/schema、资产和后端专属测试，通过 feature 控制是否编译到产品中。DB-only 构建不得依赖 FC client 或下载 FC 资产；FC-only 构建不得要求 Dragonball runner。每个后端独立验证恢复兼容性，未知后端/ABI 明确拒绝且不删除历史记录。

## 每阶段相同的证据规则

- 本地隔离分支实施；Kata 补丁只推用户 fork 的 topic branch，固定真实 commit；不改上游/default branch，不编辑 Cargo checkout cache。
- root 只做 host 准备和精确授权；Cargo 构建、Rust VM 测试为 aenv UID 999，使用 capability runner。临时 Docker/sudo 委派结束后撤回。
- 测试 state/config/deps 与旧部署分离。旧 `aenv.service` 保持卸载，不重装为测试前提。
- 先窄范围 RED/GREEN，再 `make fmt clippy test test-unit` 和适用的 privileged integration/OSS/ublk；默认 ignored 不算通过，新增真实场景不靠 fsfreeze 或 sleep 替代 worker ack。
- 若修改 Go/protobuf，同步生成 Go/Rust，运行 services tests、fmt-check、vet 和 `go test ./...`；没有修改就不为凑覆盖改控制面。
- 每阶段记录实际构建/profile/CPU/memory layout、运行命令、退出码、日志、失败分类、lease 回收和未覆盖项。未达到门槛的阶段保持未完成，不通过删测试或提前切默认跨越。

## 当前执行入口

M1 的 [11 个任务](./dragonball-m1-implementation-plan.md)、M2 的 [10 个任务](./dragonball-m2-implementation-plan.md) 和 M3 的 [4 个任务](./dragonball-m3-implementation-plan.md) 已完成各自资格。M3 修复后重放 M0 5/0、M1 32/0、M2 51/0，M3 Make 与 release 候选包分别 6/0；默认 `make test test-unit test-agent-integration clippy fmt` 整体 exit0，临时权限已撤回。M4 单机恢复/故障/分池回滚实测已通过，详情与开放门槛见 [M4 结果](./dragonball-m4-results.md)。不同 guest kernel 的串行 smoke 不构成性能资格；仍需真实双宿主机、受控统计矩阵与冻结的停顿/资源 SLO，不提前切换默认或清理 FC 历史状态。

M5 的[三项任务](./dragonball-m5-implementation-plan.md)已完成本轮范围：只读历史
inventory（0 paused、SHA 不变）、FC/DB 独立候选、三种 feature 构建、单后端
恢复/身份契约和候选-backed 干净状态恢复/回滚（20/20）。审查两个 inventory
问题 RED→GREEN，最终5/5；修复后的全量 Make、严格 Clippy/fmt 再次 exit0。
详见 [M5 结果](./dragonball-m5-results.md)。历史资产与双后端/default FC 保留；
没有历史实例可恢复不算历史 resume 通过，PVM/aarch64 未新做运行资格，M4
物理跨主机、性能与生产门槛不会因 M5 通过而关闭。

2026-10-03 后续补充：第二台物理机已配置并扩至500GiB，真实双向恢复、源 workload
停止、空缓存断网拒绝/重连、RAM/rootfs/独占卷 clone/catalog oracle 与资源清理已通过。
相同 guest kernel bytes 的 FC/DB 功能资格及完整操作 smoke（各15/15）也通过。
这关闭相应物理恢复和共同内核前置项，不是整机掉电或性能资格。2026-10-04复核
同内核1GiB低/高脏页20轮共1,200项功能通过，但启动/恢复/fork p95 未达到相对
10%门槛，不能宣称 M4 性能完成。2/8GiB低/高脏页单轮另120项通过；这些单轮
大工作集结果不产生统计资格。10并发中又发现 DB restore EOF 与并发发布超时，
功能门槛仍开放，不能只归因于原夹具 OOM。精确 capture 停顿/资源 SLO、对象存储字节、其余
容量矩阵及生产切流仍见 [M4 结果](./dragonball-m4-results.md) 中的开放门槛。
默认 FC 和历史恢复资产不变。
