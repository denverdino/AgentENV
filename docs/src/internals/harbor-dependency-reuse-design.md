# Harbor 依赖准备与模板复用设计

日期：2026-10-09。状态：用户批准执行；首轮两任务实现及远程资格验证完成（28 trial）；适用范围、候选差异和遗留项见集成记录，设计条款本身不作为通过证明。
既有结果与约束见 [HARBOR_INTEGRATION.md](../../../HARBOR_INTEGRATION.md)。

## 目标与本轮范围

用户要求减少每轮评测的重复依赖下载，利用 Dragonball capture 能力复用准备结果，并将依赖镜像推送到现有 ACR。保留固定任务、解答、verifier 文件和原始预算；每 trial 默认 4c8g，每机最多 8 trial；全量模型评测保持停止。本轮成功条件为：两机从 ACR 按 digest 取得同一已校验依赖包，跨批次复用，批内从干净模板启动独立 sandbox，验证阶段复用 OCaml 完整仓库，Oracle/nop 的评分和关键步骤与原始入口一致。

先支持明确列出的任务和依赖，不自动从任意脚本推断依赖。首个下载优化场景为 `fix-ocaml-gc`；模板身份与隔离对照使用 `openssl-selfsigned-cert`。完成这两个场景后才扩大任务覆盖。86 项范围筛选继续适用，不将范围资格改写为缓存或 Oracle 资格。

## 已确认的缺口

| 现状 | 需要完善的行为 |
| --- | --- |
| `AgentENVEnvironment.start()` 每 trial 请求 `/sandboxes-cold` | 准备阶段 capture；执行阶段根据已验证绑定请求 `/sandboxes` |
| ACR 保存原始 OCI 镜像，未保存运行期 Git/uv 等下载 | 独立、可校验的依赖产物，跨批次复用 |
| Lab 的 MinIO 和 registry 数据位于批次 payload | 依赖存储独立于 payload；快照 ID 的有效期明确限定为该 Lab |
| 原始 OCaml verifier 再次 clone 完整仓库 | agent 结束后注入本地仓库，用 Git 配置改变来源，保留原始 clone/checkout/编译/判分逻辑 |
| 现有传输将文件整体读入内存 | 大依赖包使用文件流上传与分块下载，受独立大小和时限约束 |
| 磁盘准入考虑镜像归档和活跃任务 | 增加依赖准备、传输副本、模板及失败取证的空间预算 |

这里的 MinIO 是测试夹具模拟 OSS，不是生产对象存储。成功 payload 回收会删除 `repo/s3` 和 `repo/registry`，其快照记录、内存及磁盘层不能继续被旧 ID 引用。ACR 中已经同步的镜像不受此清理影响。保留临时目录本身也不等于建立了可靠的模板生命周期。

## 方案选择

| 方案 | 收益 | 成本与边界 |
| --- | --- | --- |
| **依赖 OCI 镜像发布 ACR + 批内 capture（采用）** | 两机共享固定依赖包，消除已覆盖依赖的重复外网下载，复用现有 ACR 与快照接口 | 每批重新准备一次 VM/capture；验证器依赖仍需传入每个 trial |
| 派生任务 OCI 镜像（预装公开依赖） | 公开依赖可以跨批次直接复用 | 需要维护派生任务镜像和原镜像绑定；verifier 数据不能预装进 agent 镜像 |
| 独立持久快照仓库 | capture 结果也可跨批次、跨节点复用 | 需要持久 endpoint、命名空间、注册表来源、版本兼容、租约及回收策略；不是延长临时 Lab 生命周期即可 |

第一种方案解决本轮可验证的边界。依赖镜像是只装载数据的普通 OCI image，不作为任务的启动镜像。持久快照仓库与派生任务镜像作为后续独立改造，不将临时快照 ID 写成跨批次资产。首轮不实现 HTTPS 解密缓存，不改现有 CONNECT 代理为通用下载缓存。

## 模块与数据流

```mermaid
flowchart LR
    Source[固定任务与 OCI digest] --> Prepare[依赖准备模块]
    Prepare --> Store[ACR 依赖 OCI 镜像]
    Store --> Local[按 digest 校验的节点缓存]
    Local --> Builder[批内准备 VM]
    Builder --> Capture[干净快照]
    Capture --> Trial[每轮独立 Sandbox]
    Trial --> Agent[Oracle 或 nop]
    Agent --> Cache[验证器缓存包装器]
    Local --> Cache
    Cache --> Verifier[原始 Harbor Verifier]
    Verifier --> Receipt[评分 覆盖 阶段和清理回执]
```

依赖准备模块的接口为 `prepare(spec) -> bundle_record`、`publish(bundle_record) -> immutable_image_ref`、`load(immutable_image_ref) -> verified_bundle`。它负责内容校验、下载预算、写入锁、OCI 发布和节点缓存；不执行解答或 verifier。spec 明确依赖所属任务、公开/验证器阶段、来源、不可变版本和允许影响的目录。job 的依赖绑定显式记录 image digest，避免每轮重新查询可变 tag。

模板准备模块的接口为 `prepare_templates(task_bindings, bundles, runtime) -> launch_bindings`。在 Harbor job 启动前串行准备；同一绑定只 capture 一次。它通过现有冷启动、guest 命令和 `/sandboxes/{id}/snapshots` 接口工作，不将 VM 实现放入 Harbor adapter。资源固定在 capture 时，不在恢复时改 CPU/内存。

environment adapter 根据 launch binding 选择现有 cold 路径或快照路径，保留第一次 guest 命令的 Sandbox ID 检查、目录准备、原始 environment 上传和删除行为。快照启动必须生成新 ID；正常 pause/resume 的既有语义不变。

缓存 verifier 通过 job 的 `verifier.import_path` 选择。它在 `verify()` 内准备该任务的验证器依赖，再委托固定版本的 `harbor.verifier.verifier.Verifier`；不重写上传 tests、执行脚本、读取 reward 或收集原始日志的逻辑。准备工作在 Harbor 原始 verifier 截止内，不能另开一段未计入预算的等待。首轮验证两个 single-step/shared-verifier 任务；入口已扩展至冻结86项（静态审计均同一形态），多步骤仍明确拒绝。额外依赖未准备的任务直接委托原始verifier，全量资格另据回执验收。

候选文件职责：`scripts/harbor/dependencies.py` 管理 manifest 和依赖存储；`prepare_dependencies.py` 准备显式配方；`prepare_templates.py` 管理模板准备与绑定；`shared_templates.py` 管理共享 OSS 索引；`cached_verifier.py` 包装原始 verifier。`run_lab.py` 只负责准入、模块调用和生成 job；`agentenv_environment.py` 负责启动绑定和已有传输接口。先不改 Rust 快照、存储或 VMM 协议。

## 依赖资产与模板绑定

持久依赖存储选定为 `agent-eval-registry-vpc.ap-southeast-5.cr.aliyuncs.com/terminal-bench`。依赖使用独立 repository，例如 `hb-deps-fix-ocaml-gc-verifier`，不覆盖已同步的原始任务镜像。采用普通 linux/amd64 OCI image 的 config/manifest/layer，不依赖新 OCI Artifact/referrers 功能。一个数据层只包含 `/harbor-deps/manifest.json` 和 `/harbor-deps/bundle.tar`；该镜像不启动容器或 VM。tag 按依赖 manifest 内容摘要命名；推送后重新读取并核对 manifest digest，产出 `repository@sha256:...` 绑定。

用户明确要求不覆盖之前的镜像：保留所有既有 repository、tag 和 digest，不重新发布原任务 tag，不更新 latest，不删除历史镜像。发布前检查目标 tag；已存在且 digest 一致时只校验复用，不重新推送；不一致时停止并记录冲突，不能强制覆盖。新版本使用新的内容摘要 tag。发布账号之间的并发写入须串行，发布后的读取校验不替代发布前的冲突检查。

两台远程测试服务器均通过现有 VPC 地址推拉。registry 认证仅用于非 root 准备进程，使用隔离 Docker/regctl 配置，退出时删除；不进入镜像层、依赖 manifest、guest 或日志。先在 A 发布，再在 B 用固定 digest 拉取并核验，不能只以 A 本地镜像存在判定跨机复用成功。

节点缓存为测试账号拥有的 `/var/tmp/hb1008/dependency-store-v1`，不位于 `db-m4.*`；它是 ACR 内容的可再生成副本，不是唯一存储来源。入口显式指定 registry digest 和缓存路径，不依赖全局环境隐式启用。内容目录按 SHA256 命名，写入先进入独立 staging，验证 OCI digest、限定数据层路径、依赖 manifest 和归档内容后原子发布；同机同 key 使用文件锁。未命中从 ACR 拉取，命中仍校验；A/B 不复制依赖缓存的临时路径；持久 VM 模板另走共享 OSS 索引。

依赖 manifest 必须记录：schema、Harbor/任务固定提交、完整任务文件集合摘要、原始 OCI platform digest、配方摘要、阶段、源 URL、解析后的精确 commit/版本、每个文件摘要与字节数、归档摘要、解包大小、工具及平台版本、准备起止时间。OCI image digest 放在发布回执和 job 绑定中，不回写镜像内部的 manifest，避免内容自引用。URL 不保存认证信息。相同 environment hash 可能对应不同 verifier，绑定不能仅使用该 hash。

Git 配方第一次准备时解析 tag 到 commit，记录并冻结整个 bare mirror 的 refs、对象与 testsuite 内容摘要；后续不自动跟随远端 tag。资格对照同时记录原始来源解析值，若与冻结值不同则拒绝比较，生成新的依赖版本。原始任务仍引用其 tag；本地 mirror 的该 tag 必须指向已记录 commit。

批内模板 key 另外包含有效 CPU/内存、公开依赖摘要、backend/虚拟化模式、server、VMM、kernel、tools 的资产版本或摘要。launch binding 保存 Lab/run 归属、准备 sandbox ID、snapshot ID、模板 key 和资源。默认批次模式在下一批重新 capture。显式共享 OSS 模式以 repository 身份校验代替
相同 run ID 要求，使用不含宿主标识的任务/镜像/资源/资产 key，索引指向已提交
快照；每轮输出和各 trial sandbox 仍独立。共享索引不使用可变 alias，不由 Lab
清理删除。操作见 [OSS 文档](../../../scripts/harbor/OSS.md#两台物理服务器共享任务模板)。
模板 key 不替代服务端的恢复兼容性检查。

`load()` 对 OCI、归档和 manifest 做完整校验；配方绑定、任务、镜像或版本不符在分配 VM 前拒绝。本地未命中可拉取指定的 ACR digest；指定产物不存在、认证失败、损坏或不匹配不能静默回退到原外网来源或 cold 路径。基线模式保持显式可选，成绩分开记录。

## capture 的干净状态

准备 VM 只使用原始任务镜像和已审核的公开依赖。新增系统包、模型或文件必须列在配方中，并单独验初始状态；不能因为是“依赖”就提前完成任务要求安装、生成或修改的内容。OCaml 首轮的公开依赖配方为空，capture 原始初始化状态；完整 Git 历史仅属于验证器依赖。

capture 前所有准备命令必须结束，没有在途下载、安装、编译及 trial 进程；检查预期公开文件、工作区和日志目录。不得导入 solution、tests、reward 或历史 trial 修改；不得带入模型 key、registry 密码或临时认证。记录初始工作区摘要和许可变更清单。准备 sandbox 在成功发布或失败后均删除，保留相应回执。

trial 恢复时重新传入当前环境、网络策略、超时和 metadata，确认返回身份与 guest 一致。两个子 sandbox 的写入互不影响，删除子实例不删除模板；不能以复用同一暂停 sandbox 代替模板创建。

## OCaml 验证器缓存

agent 阶段不注入 mirror、Git rewrite、验证器包或 tests 数据。到 `verify()` 才将已验证镜像归档以文件流传入 guest，解包到该 trial 的缓存目录；设置本次 verifier 命令及其子进程的 Git `url.<本地 mirror>.insteadOf` 配置。使用 `GIT_CONFIG_COUNT/KEY/VALUE` 环境，不写 agent 的全局 `.gitconfig`；与原有 verifier 环境合并时保留已有项，冲突立即拒绝。只匹配配方声明的原始仓库 URL。

原始 `git clone ... ocaml-original` 仍执行，`/app/ocaml-original` 事先不得存在；checkout 的 tag/commit、取回的 testsuite、`make clean/configure/make -j4` 和最终 pytest 都保持原流程。Git 完整历史不因使用镜像而删除，解答修改不被覆盖。缓存源只读、不包含 trial 写入；新 clone 是每轮独立副本。

首轮只消除这个 Git 仓库的外网拉取。原始 apt update、uv 安装脚本以及 Python 包下载仍可能联网，逐项记为未覆盖；完整离线验收不是本轮通过条件。后续 uv 缓存需绑定 uv、Python、平台和包版本，并验证原命令的命中行为。

阶段注入保证 verifier 数据不提前出现在 agent 初始环境，符合现有 Harbor 在验证阶段上传 tests 的边界；不声称能阻止恶意 root agent 留下后台进程读取后续验证数据。若要强化该边界，应另行设计独立 verifier sandbox 与产物交接，不能在本轮擅改任务为 separate 模式。

## 配额、错误与回收

依赖准备按批次串行、Harbor 启动前结束，避免在 8 台 trial 之外再叠加 builder。准备截止独立记录，trial 原始 agent/verifier 预算不变，job 外层预算覆盖所有阶段。依赖传输有总截止；长连接保活不能延长阶段截止。

现有运行磁盘预算之外，准备准入加上 staging 的下载及解包峰值、归档传输副本、所选模板内存镜像上限和根盘增量上限。内存镜像至少按每模板 8192 MiB 预算，不能假定压缩后很小；根盘增量用显式配方上限和实际观测复查。按阶段复查空间，未给出产物大小上限的配方拒绝启用。依赖存储已有数据计入实际可用空间，不重复视为待下载字节。大集合超出磁盘预算明确拒绝，由批次选择控制，不提前 capture 全部 86 项。

归属分开记录：ACR 依赖镜像、节点缓存、批内模板、准备 sandbox、trial sandbox、传输临时文件、日志和回执。现有成功 payload GC 不删除 ACR 镜像或节点缓存。首轮不增加 registry 或共享缓存自动 GC；只清理本次未发布 staging 和 guest 临时副本，失败现场按原规则保留。每批结束确认 sandbox、设备和进程清理，再回收其临时仓库；证据记录模板随仓库失效，不把它列为持久缓存命中。节点缓存后续可按归属和无引用证据回收，ACR digest 仍可重新拉取。

超时、坏包、版本不匹配、capture/发布失败、恢复失败或 verifier 注入失败均留下具体阶段和归属回执；不能写 reward=0 代替框架错误。取消路径完成临时文件和已知 sandbox 删除；捕获发布状态不明确的 ID 单独记录以供核对，不猜测成功后继续 job。

## 回执与验收

新增依赖、模板和 trial 回执分别记录内容摘要、原始/有效规格、准备/传输/capture/恢复/verifier 墙钟、缓存模式、命中和未覆盖项、清理状态。缓存注入耗时同时包含在原始 verifier 阶段和端到端时间里，分项用于解释，不重复相加。正式基线与缓存变体分开汇总，不能只因任务文件未改就宣称初始状态完全相同。

| 验收层 | 必须证明的行为 |
| --- | --- |
| 无 VM 的契约测试 | 坏 OCI/依赖摘要、资源/版本变化、阶段错绑、共享 environment hash、损坏包和旧 run binding 均拒绝；同 key 原子发布与锁竞争不产生半成品；已有同 digest tag 仅复用，冲突停止，两种路径均不写已有 tag |
| 传输与恢复测试 | 流传输保留字节/权限；两次创建 ID 不同、guest 身份正确、工作区隔离、初始状态一致；超时及取消留回执并回收 |
| verifier 包装测试 | 只在验证阶段注入；原 verifier 恰好执行一次；环境合并无丢失，异常/取消不变成正常分数，原截止覆盖注入 |
| A 资格 | 同机同规格各跑原始/cached 的 openssl 与 OCaml Oracle/nop，精确结果集、无框架异常、Oracle=1/nop=0，核对 OCaml testsuite/关键编译测试实际执行 |
| B 复用 | A 推送后，B 在无该节点缓存的目录中从 ACR 按同一 digest 拉取；verifier 的 Git 仅允许本地 file 协议，仍完成独立资格，apt/curl 保持原网络。回收上一批成功 Lab 的 payload 后，在新的空节点缓存目录中重新从 ACR 取得依赖包，不引用旧 snapshot ID |
| 容量 | 资格通过后确认 8 个 trial 重叠的资源、空间和清理；只用 Git 协议限制证明已覆盖 Git 不再外下载，不宣称整个任务离线 |

两机可并行做独立资格，但性能前后必须在同机比较。每次远程执行前清理已确认可回收的自有成功 payload、核对空间；不再次运行完整模型 campaign。启用模板模式前完成资格，保留原始冷启动路径用于回退和诊断。

## 来源与落点

- 现有 adapter、编排和代理：[agentenv_environment.py](../../../scripts/harbor/agentenv_environment.py)、[run_lab.py](../../../scripts/harbor/run_lab.py)、[egress_proxy.py](../../../scripts/harbor/egress_proxy.py)。
- 现有 API 与仓库生命周期：[sandbox.rs](../../../src/api/impls/sandbox.rs)、[snapshot manager](../../../src/snapshot/manager.rs)、[存储归属清单](persistence-artifact-inventory.md)。
- Harbor 固定提交 `4b94505a91c5ddcb70b5740ac95718ddee13e5a0` 的 `VerifierFactory`、`Verifier`、`SingleStepTrial` 和 job verifier 配置：远程源文件与 `source-manifest.json` 校验；扩展入口见 [固定版本 VerifierFactory](https://github.com/harbor-framework/harbor/blob/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/src/harbor/verifier/factory.py)，实际接口以固定源码为准。
- Git 环境配置和 URL rewrite：[git-config](https://github.com/git/htmldocs/blob/gh-pages/git-config.adoc)；协议限制：[git](https://github.com/git/htmldocs/blob/gh-pages/git.adoc)。
- ACR 推拉与 VPC 网络：[推送拉取镜像](https://help.aliyun.com/zh/acr/getting-started/use-a-container-registry-enterprise-edition-instance-to-push-and-pull-images)、[VPC 访问](https://www.alibabacloud.com/help/en/acr/use-cases/access-enterprise-edition-instances-across-regions-or-from-idc)。依赖 OCI 发布与第二台空缓存拉取均已执行，原有 89 个镜像 tag 摘要未改变；运行资格另据回执验收。
