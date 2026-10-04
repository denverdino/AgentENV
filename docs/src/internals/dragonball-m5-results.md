# M5 双后端保留与候选交接

日期：2026-10-03。本轮 M5 范围已验收，范围见 [M5 设计](./dragonball-m5-design.md) 与
[实施计划](./dragonball-m5-implementation-plan.md)。本阶段不删除 Firecracker、
不切换默认、不安装旧服务、不推送/发布。M4 的真实双物理宿主机与性能门槛继续开放。

## 历史恢复依赖

测试机 `147.139.168.159`，只审计明确指定的旧安装路径；未启动旧 server 或调用
persister 的加载/GC。新 `examples/recovery_inventory.rs` 用 RocksDB 只读打开，
日志放在独立临时目录；报告仅包含版本、恢复身份与 guest 资产引用白名单，key
用 SHA256 表示，不输出 metadata/env/credentials。未知 schema/backend 或损坏
JSON 均标记保留。分类只是声明的依赖，不验证 VM state 完整性或资产可用性，
`restore_qualified` 始终为 false。

| 范围 | 证据与保留策略 |
| --- | --- |
| `/var/lib/aenv/persisted-sandboxes/records.db` | owner aenv；确认无 LOCK 持有者后，在私有 mount namespace 中只读 bind，再以 UID999 扫描。0 条暂停记录；扫描前后所有文件 SHA256 相同。完整保留数据库。 |
| `/var/lib/aenv/deps`、image-cache、snapshot-local-cache、ublk、firecracker-work、config、logs | owner aenv；保留旧 FC 1.15.1-patch-v1、kernel 6.1.175、tools 0.1.0、OverlayBD/regctl 及目录。image-cache/metadata 是缓存元数据，不作为暂停记录统计。 |
| `/var/tmp/agentenv-uninstall-20260930.6clQzq` | owner root、0700；保留旧 server、daemon、CLI、service/config/env 和 host 配置备份。只读取恢复相关配置键，原配置为 POSIX 仓库；未展示 env/凭据。 |

没有旧暂停记录可执行历史 same-ID resume，不能把新建夹具的 resume 算成历史
恢复证明。扫描没有覆盖外部 OSS/registry、其他节点或任意用户目录；不能用本地
count=0 授权退役或删除。旧配置未覆盖默认 POSIX `snapshot-store` 路径，该默认
目录在 `/var/lib/aenv` 下不存在；这同样不是全局 snapshot 清零证明。
内存快照不可跨后端转换；重建 OCI/recipe 必须用新 ID/alias。

## 独立候选包

新增 `package-firecracker.sh` / `verify-firecracker-bundle.sh` 与 FC 专属配置，
沿用 DB 包的显式输入约定：绝对资产路径、SHA256、版本、source revision、
各组件 LICENSE/NOTICE。拒绝 debug/both/wrong-backend、错误架构/源码身份、
错误 FC 版本/摘要、缺资产/许可证及已存在输出。打包不下载、不覆盖、不发布。
`make test-hypervisor-packaging` 同时检查两套离线契约；假 identity CLI 仅用于
打包负例，不作为 VM 资格证据。

新 FC 包仅是 linux/amd64 KVM 本地候选，不替代已有 PVM/aarch64 发布路径。
两种候选均以 aenv 打包，解压到与生成目录不同的位置，再从 `/` 执行 `verify.sh`。
server、后端二进制、daemon、kernel/tools 及 LICENSE/NOTICE/manifest/checksums
均随包提供；OverlayBD/regctl、host 依赖和 rootfs 仍需显式准备，不宣称离线 OS 安装器。

远程根目录 `R=/var/tmp/agentenv-dragonball-m5-20261003`：

| 最终候选 | tar SHA256 |
| --- | --- |
| `R/candidates/dragonball.tar.gz` | `cfe9a2988b205339a15fe75e0e4be9d13f93622986dddd8f11cde267eed8d0bd` |
| `R/candidates/firecracker.2.tar.gz` | `c85eded6a5d7ca707ef593f3f46783c13c48a3fac1646dc2c6b72e551e7ac393` |

候选 server 保留最后生产代码 `e6b568546e4ec44faae9c06f9cda97630331f4d6` 的
M4 release 二进制（本阶段仅新增审计 example、打包/测试工具和文档），没有把
旧二进制重新标为 M5 HEAD。两种 server 及 guest 资产 SHA 见
[M4 制品表](./dragonball-m4-results.md#环境与制品)，M5 manifest 逐项重新校验。
DB Kata pin 为 `c77812d669737c11cb16321827acc1c79bf065f3`。FC 来源库的
`aenv-deps` tag `90288c397d6d3fa576f9d367d997bdeebf835f10` 中版本为
1.15.1-patch-v1，随包保留 LICENSE、THIRD-PARTY 文本和 NOTICE。
FC `.1` 曾误用 DB-specific kernel NOTICE，保留为 superseded，最终 `.2` 使用
独立 FC kernel 说明。不宣称源码重现构建或公开分发的完整对应源码资格。

```bash
bash scripts/release/package-firecracker.sh /absolute/qualified-inputs.json /new/candidate.tar.gz
bash scripts/release/package-dragonball.sh /absolute/qualified-inputs.json /new/candidate.tar.gz
make test-hypervisor-packaging
bash scripts/tests/check-hypervisor-builds.sh
cargo test -p agentenv --example recovery_inventory --locked
# 离线数据库；最好从只读 bind mount 中运行，不在活动 server 上运行。
cargo run -p agentenv --example recovery_inventory -- /existing/records.db
```

JSON 输入结构见两套 `scripts/tests/verify-package-*.sh`，真实本机输入保存在
`R/candidates/{firecracker,dragonball}-inputs.json`；异机需替换路径并重新验证 SHA。

## 测试证据

- 只读 inventory：RED 未实现失败，最终 GREEN 5/5；涵盖缺目录不创建、版本/身份
  冲突/损坏记录、白名单输出、真实数据库全部文件字节不变。
- FC/DB 离线打包契约：本地和远程 UID999 均通过；实际 relocated 包分别通过。
- FC-only / DB-only / both Cargo check 全部 exit0；DB tree 无 FC client，FC tree
  无 Dragonball runner。默认仍是 `hypervisor-firecracker`，双 feature 仍选择 FC。
- 候选 restore：`R/logs/candidate-restore.log` exit0，证据
  `/var/tmp/db-m4.k88a9l1p/result.json`。源节点停止，B 空缓存断网恢复500，联网
  重试成功；新 ID、RAM/rootfs/独占卷 marker 正确，卷克隆，durable catalog 保留。
- 候选 rollback：`R/logs/candidate-rollback.log` exit0，证据
  `/var/tmp/db-m4.iedo0rps/result.json`。两后端 cold/capture/restore×3/fork1/3、
  16 MiB fsync、pause/same-ID resume，20/20 操作成功；新建选择改 FC 后，两种
  既有 paused 仍回原后端。异类 snapshot 双向400，记录不被清理。实际 UID999、
  release backend 身份已核验；两轮均有 cleanup receipt。
- M4 测试架单元11/11、Linux 隔离3/3、候选-backed SIGTERM 清理2/2；日志
  `R/logs/lab-linux.log`、`termination.log`。不声称 SIGKILL/宿主宕机自动清理。
- FC-only/DB-only 分别执行 persistence 22/22、restore_identity 6/6，覆盖旧 FC
  reader、异类/冲突记录保留以及准确 ABI reader；`R/logs/single-backend-tests.log`
  exit0，inventory 首轮同时重放3/3，审查修复后5/5。
- `make test test-unit test-agent-integration clippy fmt test-hypervisor-packaging`
  首轮整体 exit0，`R/logs/full-make.log`。FC lib978/0，另5 ignored 显式运行5/0；
  integration21/0（3 ignored 未算通过）、Orchestrator3/0；envd filesystem1/process3；
  OverlayBD383/0（3 ignored）、daemon integration37/0、MinIO7/0；ublk18/0、
  daemon49/0、CLI110/0、OSS E2E6/0。workspace 全 targets/features 严格 Clippy、
  fmt 和两套打包契约通过。保留既有 proc-macro-error2 future-compat 警告；
  其它普通 ignored 没有计为通过。Cargo/VM 为 UID999，root 只做编排与 host shell
  verifier，临时 Docker/sudo 授权已撤回。

## 最终审查

独立 fresh reviewer 检查 M5 全部改动及整分支的选择/恢复集成；M1–M4 深层 VM
内部依赖先前阶段审查，不声称重新穷尽全部 internals。无 Critical/Minor，两个
Important 在单轮修复中均先 RED 再 GREEN（`review-red.log` / `review-green.log`）：

1. FC paused 的真实 kernel/Firecracker/tools/envd 版本位于
   `metadata.runtime_versions`，原白名单漏了 kernel/Firecracker。加入精确版本
   路径和 `metadata.virtualization_mode`，以真实 `SnapshotRuntimeVersions`
   序列化验证，仍不输出用户 metadata/env。
2. 原分类只检查外层 backend/identity；现在与 metadata 的 backend/identity
   一致性一起检查，未来 backend、外层冲突及 identity 字段冲突均 `unknown-retain`，
   真正缺新字段的 legacy FC 仍保留原分类。未加入 VM 状态 decode 或删除动作。

审查修复后相同完整 Make 命令再次整体 exit0：`R/logs/review-full-make.log`，
包括严格 Clippy/fmt。最终 reader 再次在只读挂载中扫描历史库，count0、全部文件
SHA 与最初一致：`review-historical-inventory.json`、`review-history-after.sha256`。
最终检查无本轮 cgroup/UID999 进程残留，旧 `aenv.service` inactive；aenv 仅保留
原 aenv/kvm 组，临时 sudoers/Docker 授权已撤回。证据与历史状态保留。

## 复验边界

候选运行复用 [M4 单机隔离测试架](./dragonball-m4-results.md#重跑)：每轮新
namespace/home/run/deps/cache 和独立测试 S3/OCI 服务，真实 server/VM 为 UID999。
依赖源目录与构建缓存复用，不是全新 OS。FC/DB kernel 不同，功能结果不构成
可比性能基准；没有 PVM/aarch64、本轮历史实例恢复、真实第二台主机或生产切流资格。

## 验收后补充：CI 回归入口与性能定位

2026-10-03，未修改生产 Rust、默认后端或候选二进制。

### CI 入口

`make test-hypervisor-regression` 聚合两套离线打包契约、M4 Python 单测、
FC-only/DB-only 各自的 persistence/restore_identity 单测以及 recovery_inventory
example 测试；不需要 KVM 或 sudo。复用 Makefile 的独立测试 home/run，避免访问
活动服务状态。普通 CI 的 hypervisor-components job 调用此目标；release 脚本、
打包测试、M4 Python 和 config 变化也触发 CI。

Integration Tests 新增 `dragonball_candidates` 手动选项，依次运行现有 restore
和 rollback 场景。使用专用 `dragonball-candidates` runner label、同名受保护
environment 和串行 concurrency；PR/push 不会执行此 job。管理员需提前配置
environment 审批/允许分支规则（YAML 声明不会自动创建保护规则），并准备：

- 非 root runner 账户及非 root `aenv` 工作负载账户；Linux/KVM/ublk、cgroup v2、
  Python >= 3.11，以及 [M4 依赖](./dragonball-m4-results.md#重跑)。
- runner 环境中的绝对 `AENV_M4_INPUTS`，指向已验证且完成 relocation 的两种
  候选及显式 S3/OCI/OverlayBD/regctl 资产；格式同上文 `R/inputs.json`。
- 允许该专用测试机执行 `sudo -n make test-dragonball-m4 ...` 的预配置。
  root 仅编排隔离空间，实际 server/VM 使用 `aenv`；不要共用生产宿主机。
- 足够的临时磁盘；每轮 `/var/tmp/db-m4.*` 保留 result、samples、server 日志
  和仓库证据，不自动删除历史目录。失败也应保留日志供排查。

本轮只修改工作流文件，未注册 runner、变更 GitHub environment、推送或远程触发
GitHub Actions，因此不宣称 GitHub 云端 job 已通过。验证记录：

- 新 Make 入口实施前执行失败（No rule），实施后远程以 UID999 整体 exit0：
  两种后端各 22+6、inventory 5，共 61 个 Rust 测试；Python 11/11；双打包契约通过。
  日志 `R/logs/ci-followup.log`；远程 `Makefile-ci-followup` 与本地 Makefile 同内容。
- 本地双打包契约、Python 11/11、两个工作流 YAML 解析及 `git diff --check` 通过。
  本轮无生产代码变更，没有重跑完整 `make test`/Clippy，前节记录的是 M5 既有结果。
- 独立只读审查覆盖本轮 CI/Make/文档差异及相关调用路径，无 Critical/Important/Minor
  发现；未把该审查当作 GitHub 实跑、全迁移重审或远程数据的独立复测。

### 无并行构建的单机复测

使用原 M5 relocated release 候选和同一 inputs，执行：

```bash
make test-dragonball-m4 AENV_M4_INPUTS=/absolute/inputs.json AENV_M4_SCENARIO=rollback
```

证据目录 `/var/tmp/db-m4.uxh_iwpu`：`result.json`、`samples.jsonl`、
`a.server.log`（DB）、`b.server.log`（FC）。20/20 操作成功，same-ID resume、
双向异类 snapshot 拒绝与记录保留通过；控制器 exit0/cleanup passed，复查没有
本轮 cgroup 或 aenv server/runner/daemon/fixture 进程残留。测试期间没有本任务
Cargo/rustc 构建；不表示宿主全部背景负载已被排除。

| 本轮观测，秒 | Dragonball | Firecracker |
| --- | ---: | ---: |
| capture API | 19.068 | 0.807 |
| 本地 capture 日志区间 | 7.468 | 0.153 |
| capture 剩余区间：发布及 API 开销（差值） | 11.599 | 0.654 |
| 其中内存层 put_file 日志计时 | 6.164 | 0.266 |
| fork 1 / fork 3，含 guest ready | 8.532 / 8.412 | 0.335 / 0.400 |
| pause API | 9.771 | 0.302 |
| same-ID resume，含 guest ready | 0.313 | 0.170 |

本地 capture 以 `capturing sandbox snapshot` 至 `snapshot captured` 的时间戳
相减，不是精确 guest 停顿时间。DB 为 11:36:55.618218Z–11:37:03.086581Z；
内存层上传结束于 11:37:14.670416Z。`put_file` 包含文件读取/上传处理，不是
纯网络时间，且是上面发布区间的子项，不应重复相加。其余约 5.4 秒尚未细分；
代码路径包含无 descriptor 层的摘要计算、存在性检查等，不能把差值全称为 hash。

关键证据：DB 上传内存层 **1,073,754,112 bytes**（1 GiB RAM 加容器开销），
FC 为 **77,758,464 bytes**。DB 节点 cgroup 的 `253:0` 写入计数在 capture、
fork1、fork3、pause 区间分别增加约 1.074、1.278、1.074、1.487 GB；这是
节点整体块 I/O，含其它文件和延迟写回，不能当作精确内存层字节数。对应 CPU
usage 增量为 10.772、2.971、3.977、3.083 CPU 秒，不能只凭此断定磁盘是唯一瓶颈。

代码与数据一致：`dragonball/memory.rs::memory_segments` 在无基线时选择全部
RAM regions；只有从快照恢复并持有 `DragonballMemoryBase` 时才选择累计 changed
ranges。基线在加载时选定，不在 capture 后推进。因此本轮冷启动源的 capture、
连续 fork 和 pause 都仍进行全量内存读写；不是增量分支失效，也不能直接移除零页
写入（增量中的零必须覆盖父层非零内容）。FC 则使用 dirty-range adapter。

下一步优先评估冷启动全量层的零页稀疏表示，或建立有严格基线/dirty generation
协议的后续增量 capture；须覆盖零页覆盖、连续 capture/fork、取消和失败恢复。
然后对摘要生成、内存读取/compact/sync 加最小阶段计时，再决定是否优化重复扫描
或上传。此次仅定位，不改变生命周期或内存格式。

这些仍是每项 1 个样本（restore 3 个）的单机数据；内核不同、宿主页缓存共享，
不能据表中比值声称 Dragonball 固有慢多少倍，也没有 p95/p99 或 10% 性能达标。
精确 guest 停顿、物理双机、容量/高脏页/长稳矩阵仍未验收。测试后根盘约剩
18 GiB，证据未删除；后续大矩阵前需再次核对磁盘预算。

## 单机性能优化：独立全量内存层零页省略

2026-10-03，在同一测试机完成实现与复测。`dragonball/memory.rs` 对没有父层的
完整 RAM 镜像按 1 MiB 分块读取、4 KiB 页判断，仅把非零页交给现有 OverlayBD
writer，保持逻辑大小与 packed layout。扫描和 writer 二次读取均受原 frozen
read lease 保护；不推进 dirty baseline，不改变 runner 协议或后端默认值。
**有父层的增量路径完全保留原范围，包括覆盖父层所需的零页。**

新增 `payload_bytes/scan_ms/write_ms/total_ms` 阶段日志。扫描短读或错误在发布
配置前失败。测试涵盖无 dirty 位的非零内容、页/块/region 边界、全零、稠密 RAM、
短读/错误、增量清零再写入和取消后的 frozen lease 生命周期。

### 构建与正确性证据

本轮证据根目录 `P=/var/tmp/agentenv-dragonball-perf-20261003`。构建与测试为
UID999，root 仅运行隔离/授权编排；复用隔离状态，不访问旧服务状态。

- RED：原实现在稀疏/全零层大小断言上失败，稠密对照通过，见 `P/logs/sparse-red.log`。
- 最终 DB-only lib：953 passed、5 ignored；真实 M2 1/2/8 GiB full oracle、
  zero/rewrite/fork、发布失败保留 dirty 五项均通过，见 `P/logs/m2-final-and-lint.log`。
  full oracle 状态使用现有专用 tmpfs，不能拿其耗时作为磁盘性能数据。
- 最终 `make test test-unit clippy fmt` 整体 exit0，见 `P/logs/final-full-make.log`。
  早一轮完整测试通过后 Clippy 提示使用 `as_chunks`，已修正并整套重跑。
  两次临时 M2 wrapper 试跑分别因隔离 runner、缺少测试 OverlayBD 配置失败；最终
  使用仓库的隔离 runner 和 `prepare-agent-test-state` 后通过，失败日志保留。
- 独立只读审查无 Critical/Important/Minor 发现；它不替代实际 VM 测试。

最终复查：`aenv.service` inactive，UID999 无进程，本轮 cgroup 无残留；
临时 sudoers 与 Docker 组授权已撤回，aenv 仅保留 aenv/kvm 组。

最终 `memory.rs` SHA256 为
`539c6b0d32ee80da16b674f84c7ff7db32106f03af9255d0a0796516510ca30d`。
新 release 候选 `P/bin/server-db` SHA256 为
`0af38b2e91be2dd15fe4e428cdbc05eac95afe31f3fdf53affa3cdca0b3fdd2b`；
`P/bin/server-fc` 为
`bb358564228ef506bd33a62aed26b476e2aa05a947478404c4c59af56b27c943`。
构建使用远程 `d94710d` 基底加已验证的 M5 工具变更及本次内存改动，故二进制
仍报告 `git_commit=d94710d`，不是干净提交构建；不得只用这个 commit 标识重建。
旧 M5 候选未覆盖。runner、guest kernel、tools、Kata pin 均保持原值。

### 无并行构建的前后对照

`P/run-bench.sh` 串行运行一轮旧 M5 baseline、三轮优化后 rollback、一轮优化后
empty-cache restore，整体 exit0。四轮 rollback 均 20/20 成功，含双后端
same-ID resume、异类 snapshot 拒绝/记录保留、回滚新建路由；五轮均 cleanup passed。
对应目录依次是：

- baseline：`/var/tmp/db-m4.fjymfhji`
- optimized 1/2/3：`/var/tmp/db-m4.zqiuorp4`、`/var/tmp/db-m4.yhkjya5h`、
  `/var/tmp/db-m4.ky8m1qi9`
- empty-cache restore：`/var/tmp/db-m4.3r6hwg0g`

每轮保留 `result.json`、server 日志，rollback 另有 `samples.jsonl`；控制器
cleanup 回执在 `P/logs/*rollback*.log` 和 `P/logs/optimized-restore.log`。
同一 Dragonball guest kernel、tools、镜像、1 GiB/1 vCPU，禁用 pool/compression/
startup pack；节点 CPU quota4、内存上限12 GiB。以下只比较 DB 前后，不比较 FC/DB。

| 指标 | 优化前单次 | 优化后三轮中位数 | 本轮观测下降 |
| --- | ---: | ---: | ---: |
| capture API，秒 | 19.138 | 2.684 | 86.0% |
| fork 1，含 guest ready，秒 | 7.370 | 1.973 | 73.2% |
| fork 3，含 guest ready，秒 | 8.932 | 1.701 | 81.0% |
| pause API，秒 | 8.720 | 1.429 | 83.6% |
| capture 内存层文件，bytes | 1,073,754,112 | 83,410,944 | 92.2% |
| capture 区间节点 CPU，CPU 秒 | 10.825 | 2.662 | 75.4% |

三轮 capture 为 2.519/2.691/2.684 秒；其中内存扫描 893/937/932 ms，
writer 177/181/178 ms。内存层上传日志计时由 6193 ms 降至 285/282/280 ms。
节点 capture 后 cgroup lifetime peak 从 3437 MiB 降至 1552/1524/1531 MiB，
包含页缓存和其它节点进程，不是纯 VM RSS 或 capture 独占峰值。

empty-cache restore 确认源节点停止、断网恢复失败、重连后 RAM/rootfs/volume
标记一致；capture 2.756 秒，restore API 0.723 秒。它仍是同宿主隔离节点测试。

### 尚未覆盖

这只是原有低内存占用场景的有界优化：一轮 baseline、三轮 after，宿主页缓存未
控制，未生成 p95/p99，也未验收性能 SLO。稠密 RAM 正确性已测，但高占用/高脏页
性能仍需测试；该方案始终扫描全 RAM，稠密页会再次读取，不能据此承诺所有工作负载
均加速。1/2/8 GiB oracle 是正确性验证，不是相应容量的性能矩阵。物理双机、精确
guest 停顿、并发容量、长稳及 warm pool 仍按原边界待办。此次证据保留后旧机根盘
约剩 9 GiB，不应在未规划容量时直接启动大矩阵。

## 第二台物理测试机准备

2026-10-03，按授权配置 `147.139.165.124`，内网地址 `172.21.70.158/20`；
旧机为 `172.21.70.157/20`。双向内网连通，CPU 同为 Intel Xeon Platinum 8269CY，
104 逻辑 CPU、187 GiB RAM。新机 Ubuntu 26.04.1、内核 `7.0.0-31-generic`，
旧机内核 `7.0.0-28-generic`；相同型号不是跨机 snapshot 兼容性通过的证据。

新机准备记录位于 `N=/var/tmp/aenv-newhost-20261003`：`logs/setup-host.log`、
`packages-before.tsv` / `packages-after.tsv`、`sysctl-before.txt`。
源码为 `N/src`，`source-006ed4a.tar.gz` 加 `source-perf.patch` 保留构建来源。
创建非 root `aenv`（UID999），加入 kvm 组；KVM/ublk 控制设备权限分别为
`root:kvm 0660` / `root:aenv 0660`。持久化配置包括：

- `/etc/udev/rules.d/99-agentenv-ublk.rules`
- `/etc/modules-load.d/agentenv-test-ublk.conf`
- `/etc/overlaybd/overlaybd.json`

安装构建、网络、文件系统及容器测试依赖；Docker/containerd 为包管理服务，
未给 aenv 常驻 sudo 或 docker 组授权。未安装 AgentENV systemd 服务、迁移旧数据、
重启机器或开放公共测试 API。后续 VM/构建继续使用非 root 账户及仓库 capability runner。

已安装 Rust/Cargo 1.98.0、Clippy/rustfmt、protoc 33.4（匹配 deps manifest）、
GCC 15.2、Clang 21.1.8、CMake 4.2、Go 1.26、Docker 29.1.3/Compose 2.40.3，
以及仓库需要的 libaio/liburing/libssl/libclang、文件系统/网络工具。
`N/env.sh` 提供非 root 构建环境：独立 Cargo home/target/test state、8 个构建
job、关闭 incremental，以控制系统盘用量。没有预编译整个 workspace 或预热所有
Cargo/Go/Docker 下载缓存；后续首次构建测试仍需联网下载。

运行资产在 `N/imported`，使用原 M5 relocation 候选而非本次性能优化二进制，
报告 `git_commit=e6b5685`。两种后端 bundle 的哈希/组件验证通过；rootfs 八个 lower
的哈希和大小核对通过。`N/inputs.json`、`N/runtime-assets.json` 使用新机绝对路径，
不引用旧机文件系统。Rust 工具链复用已验证版本；protoc 初次路径检查失败后已改用
manifest 固定的 33.4，最终检查通过。原失败日志仍保留，不能将初次检查称为成功。

验证与证据：

- `N/logs/kvm-probe.log`：UID999 的 KVM API12、CREATE_VM、ublk-control open 通过；
  主代理另行实际复查通过。
- `N/logs/lab-linux.log`：Linux 隔离 3/3，通过独立 net/mnt/pid namespace、
  capability/cgroup 检查和 cleanup。
- `N/verify-host.sh` 最终 exit0；主代理独立重跑也 exit0，见
  `N/logs/main-verification.log`：工具版本、M3 prerequisite check、非 root
  Cargo metadata、M4 Python 11/11。不是完整 Rust 编译或全量测试通过的声明。
- `N/logs/candidate-restore.log` 与 `/var/tmp/db-m4.ulzl39zq/result.json`：
  真实 Dragonball release server/VM UID999，cold create、capture、源节点停止、
  empty-cache 断网拒绝与重连恢复通过，RAM/rootfs/volume 标记一致，cleanup passed。
  scope 仍为 `single-host-simulated-nodes`，不是新旧物理机之间恢复。

主代理另以非 root 重跑两种 bundle 的 `verify.sh`，均 exit0，见
`N/logs/main-bundle-db.log` / `main-bundle-fc.log`。`artifact-hashes.json` 保存
资产身份。最终审计无 aenv 进程、测试 cgroup 或 AgentENV systemd unit；无临时
sudoers/Docker 授权。lab 的 cleanup 回执之后另发现两个本轮遗留的 ublk 控制设备，
在确认无活跃块设备/模块引用后卸载并重新加载 `ublk_drv` 清除；最终仅保留
`/dev/ublk-control`，权限正确。临时传输 SSH agent 已关闭。日志见 `N/logs/final-audit.log`。

实际恢复不需要放宽 `vm.unprivileged_userfaultfd=0`；`kernel.io_uring_disabled=0`
保持原值。Docker 安装启用了 IPv4 forwarding（0→1）。宿主不需要重启；后续真实
双机恢复/故障/兼容性及性能矩阵仍待执行。准备与 smoke 后根盘约剩 85 GiB，开展
全量双后端构建及长期保留大矩阵证据前，应重新核算磁盘预算。

同日用户将新机云盘扩至 500 GiB；实机复查 `/dev/vda` 为 536870912000
字节，`/dev/vda3` 为 536659082752 字节，ext4 根文件系统已同步扩展。
`df -h /` 显示总量 492G、已用 12G、可用 461G。无需再执行分区或文件系统
扩容；后续矩阵仍按实际剩余空间逐批准入，不清除既有测试证据，也不据此宣称
所有并发/重复次数组合均有足够容量。

扩容后补充验证见 [M4 双机与同内核结果](dragonball-m4-results.md)：真实双向恢复、
断网失败后重试及共同 guest kernel 双后端功能验证已通过。旧机完整
`make test test-unit clippy fmt` exit0，另 DB lib953项通过；旧机空间不足时主动中止
后续 M2 大场景，未删除历史状态。将相同测试二进制移至新机后，1/2/8GiB 完整 RAM
oracle、失败发布及清零重写/fork 共5项全部通过（237.57秒），证据根
`/var/tmp/aenv-m2-regression.WNrdwY`。退出后自有进程/cgroup/ublk 清理审计通过，
无 OOM 或磁盘保护中断。物理恢复功能通过不代表统计性能、guest 停顿或 owner SLO
已完成验收。

2026-10-04进一步复核共同内核20轮低/高脏页矩阵：1,200项功能检查无失败，但
DB启动/恢复/fork的p95超过原设计相对FC退化10%门槛。2/8GiB大工作集单轮另120项
通过，不能替代统计门槛；并发补测的夹具 OOM 失败及修复证据保留在 M4 结果。
这些结果不撤销 M5 可选编译/独立打包的功能结论，也不能据 M5 完成宣称全部
Dragonball 性能或生产迁移完成。默认仍是 FC，未发布或切流。
