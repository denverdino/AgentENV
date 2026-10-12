# Harbor / Dragonball 小型评测环境

使用 Harbor 的自定义 `BaseEnvironment` 接口，通过 AgentENV HTTP API 创建
Dragonball 沙箱、执行命令及传输文件。运行时采用既有 M4 隔离网络、状态目录、
账号与资源清理机制；无需修改 Harbor 或 AgentENV 的 Rust 运行时。

按步骤运行 hello、Terminal-Bench 和 Computer Use 的命令示例见
[Harbor 测试示例](../../docs/src/use-cases/harbor.md)。

## 首批任务

固定 Harbor 0.24.0 源码提交
`4b94505a91c5ddcb70b5740ac95718ddee13e5a0`，使用其原始任务文件：

| 任务 | 检查能力 | 评分步骤 |
| --- | --- | --- |
| [hello-workdir](https://github.com/harbor-framework/harbor/tree/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/examples/tasks/hello-workdir) | 配置指定的工作目录 | 1 |
| [hello-user](https://github.com/harbor-framework/harbor/tree/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/examples/tasks/hello-user) | 非 root 用户执行、文件权限 | 1 |
| [hello-multi-step-simple](https://github.com/harbor-framework/harbor/tree/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/examples/tasks/hello-multi-step-simple) | 同一沙箱内连续写入、跨步骤状态 | 2 |

任务自带的解答和 verifier 不作修改，verifier 不下载额外依赖。
每个任务分别运行 Oracle 和 `nop`：Oracle 的全部步骤应得 1，空操作的全部
步骤应得 0。因此一次验收包含 6 个 trial、8 次步骤评分。
这组任务适合后端接入冒烟；Oracle 运行不衡量模型能力。

## 已准备的远程环境

服务器：`root@147.139.165.124`；目录：`/var/tmp/hb1008`。
Harbor、服务器、VM、任务镜像构建均以 `aenv`（UID 999）运行；root 只负责
主机准备、隔离网络和清理。Harbor 使用 `venv/`，任务镜像在 `prepared-v1/`。

该环境支持以下重复运行命令，输出目录必须不存在：

```bash
ssh root@147.139.165.124
bash /var/tmp/hb1008/rerun.sh /var/tmp/hb1008/run-next
```

入口每轮检查至少 40 GiB 可用空间，预算为 20 GiB 工作空间和 20 GiB 保留空间。
源码 manifest、完整任务文件集合、任务镜像 archive、账号可读性和运行依赖均
在分配 VM 前检查。已有失败日志保留；入口不自动删除其他任务的数据。
退出时关闭自有 VM、夹具、cgroup、设备及网络，并恢复临时主机文件和模块。

`run-006/receipt.json` 是增加 Terminal-Bench 入口后的冒烟回归记录；任务结果在
`run-006/jobs/dragonball-smoke/`，主机恢复记录在 `evidence/restore-run-006.log`。
`run-005/receipt.json` 保留先前重复运行入口的验收记录。
`run-004/receipt.json` 保留首次完整成功记录。
`run-001` 至 `run-003` 保留准备/CLI 环境问题，不合并为评分结果。

## Terminal-Bench 与百炼模型

另一个入口运行 Terminal-Bench 2.0 的 `openssl-selfsigned-cert` 和
`log-summary-date-ranges`，源码固定为
`2fd12b88aafdd04a52c298e3940bcb189f9766d6`。保留原始任务、Oracle 解答和
测试脚本，使用任务声明的官方镜像；冻结下载后的 image ID、archive SHA256
与 registry digest。镜像及任务位于远程 `terminal-prepared-v2/`。

先运行四个 Oracle/`nop` 对照 trial，检查每个 Oracle 为 1、每个 `nop` 为 0；
对照失败时不启动模型。随后使用 Terminus-2 和百炼 `qwen3.8-flash`，每个任务
运行一次。模型得分为 0 是有效评测结果；异常、缺失结果或不完整评分不算成功。

远程重复运行时，从受保护的运行环境提供 `DASHSCOPE_API_KEY`，避免把密钥
写入命令历史或配置文件：

```bash
bash /var/tmp/hb1008/tb_rerun.sh /var/tmp/hb1008/run-tb-next
```

基础 `run_lab.py` 通过 `--prepared` 选择任务，增加 `--model qwen3.8-flash`
启动模型阶段；也支持 `--model qwen3.8-max`，资源与阶段预算保持一致。
省略 `--model` 只运行评分对照。模型使用百炼 OpenAI 兼容接口
`https://dashscope.aliyuncs.com/compatible-mode/v1`，关闭 thinking，
每任务一次、沿用任务阶段超时，不设旧冒烟的 25 轮上限，每次输出最多
4096 tokens，无 trial 自动重试。历史冒烟结果仍按当时 25 轮配置解释。
Harbor/LiteLLM 自带单次 API 异常重试仍保留。结果包含 token 用量；未配置
美元计价，不能把框架显示的零成本视为实际账单。

模型密钥仅通过模型 CLI 进程环境注入，不交给 guest 或写入 job JSON。
`dashscope_agent.py` 在原始 Terminus-2 的模型日志落盘前递归移除密钥，
同时保留原始请求/响应、轨迹和 token 数据。该包装依赖固定 Harbor 版本的
LiteLLM 日志接口，升级时需复核真实 agent factory 选择和脱敏测试。

官方 verifier 与终端工具会联网安装依赖。Lab 使用非 root HTTP/CONNECT
代理：监听 socket 在隔离网络内创建，出站连接在父网络中创建；专用
`198.18.0.1/32` 地址及路由只存在于自有 Lab/node 网络，不修改主机路由或
防火墙。VM HTTP/HTTPS 探针通过后才开始评分。该方式支持上述任务使用的
代理协议，不能推广为任意 guest TCP/UDP 公网访问。
代理沿用专用测试宿主的网络访问权限，未按目标地址做隔离，能连接 localhost、
私网和元数据地址；本地缓存及 OSS 内网访问也依赖这一路径。此入口仅供受信任
任务在专用评测机上使用，不作为生产 guest 网络策略或多租户隔离边界。

远程 `run-tb-002/receipt.json` 完整验收通过：Oracle 两项均得 1、`nop` 两项均
得 0、Qwen 两项均得 1，六个 trial 无异常且全部沙箱删除，主机临时修改已恢复。
模型阶段约 160 秒；其中每项 agent 执行约 39 秒，其余主要为终端工具准备和
verifier 安装/执行。累计输入 34,006 tokens（含缓存命中 21,248），输出
3,770 tokens。这是两个任务各一次的接入评测，不代表完整 benchmark 分数。
`run-tb-001` 保留私网代理地址被 guest 策略拒绝的准备失败；它未执行模型，
不计入模型评分。

从源码准备时，在原有准备命令上增加 `--terminal-bench-source` 与
`--terminal-bench-manifest`；manifest 的格式相同，文件集合覆盖两个完整
任务目录。镜像下载使用具有 Docker socket 权限的非 root 账号。

## Computer Use 小型任务

使用同一固定 Harbor 提交中的
[computer-1-click-calibration](https://github.com/harbor-framework/harbor/tree/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/examples/tasks/computer-1-click-calibration)。
它在 guest 中启动 Xvfb、XFCE 和 Chromium，依次检查点击、双击、右键、
输入与 Enter、拖拽、滚动，以及截图裁剪读取随机 CODE。规格为 2 vCPU、
4 GiB 内存、无 GPU，网页为本地文件；沿用原始任务和 verifier。

准备时将该完整任务目录加入 Harbor 源码 manifest，在独立 Python 环境安装
`Pillow==12.3.0`，并在准备命令上增加 `--computer-use`。评测命令沿用
`run_lab.py --model qwen3.8-flash`，`--prepared` 指向 Computer Use 产出。
任务镜像按 Harbor 的 task short name 映射，支持目录名与声明名不同的任务。

先运行独立的 `computer-calibration-control` GUI 正向对照和 `nop` 负向对照，
再用 `computer-1` 的 LiteLLM 通用视觉循环运行 Qwen。固定版本的 provider
注册名为 `litellm`；不仅检查工厂选中的类，也实际构造 agent 验证配置。
模型接收截图并执行桌面动作，`extra_tools=[]` 禁用模型 shell 工具。
正向对照通过截图颜色定位 GUI 控件，完成后从窗口标题读取 CODE；这仅用于
验证环境与评分器，不计入模型成绩。模型成绩允许为 0，异常另行报告。

任务声明 `no-network`；guest 不注入代理，模型调用通过驱动侧代理完成。
VM 传输预检也使用该任务资源和显式解析的网络策略，记录实际配置及公共
HTTPS 不可达结果。隔离实验室本身没有直接公网转发，此结果不单独证明
防火墙策略的因果效果。截图、动作轨迹、随机 CODE 校验和清理回执均保留。
预检启动沿用原任务 `environment.build_timeout_sec`，传输检查另限 180 秒，
清理另限 60 秒；外层预算为三者之和再加 15 秒。启动失败的内部删除也限
60 秒，删除错误不覆盖原始启动错误。异步截止依赖取消传播，Lab 最终清理
仍负责回收。`transport-probe/checks.json` 在执行前和退出时落盘，保留失败阶段。
目录传输归档放在 guest `/var/tmp`，避开任务镜像启动时清空 `/tmp` 的竞争；
归档删除另限 5 秒，保留传输原始错误；传输成功时删除失败仍报告失败。
不改变任务的工作目录、原始测试或评分规则。
官方 verifier 检查完成状态与 CODE，不强制发生 zoom；动作覆盖需另查轨迹。
这一任务用于接入验证，不代表完整 OSWorld 或 Computer Use benchmark 分数。

2026-10-09 在 `147.139.165.124` 完成 `run-cu-003`：GUI 正向对照 1/1、
`nop` 0/1；Qwen 0/1，记录 `AgentTimeoutError`，因此整体回执保留失败状态。
模型完成前 3 个阶段，在输入阶段重复点击输入框下方，输入未生效。
轨迹含截图放大动作，但未到达最终 CODE 校验；不能宣称模型完成全部动作。
原始 agent 预算为 600 秒，记录执行阶段为 663.971 秒。固定 Harbor 实现
在 `run()` 的 `finally` 中还会请求模型补写答案，取消后的收尾仍耗时约
64 秒；该行为未修改，不能把 600 秒配置当作整个阶段的硬截止时间。

运行前清理过期 Docker 构建缓存；最终可用空间约 224.6 GiB。三次评测
trial 的 VMM 均为非 root UID 999，sandbox 均已删除、API 列表为空、
主机资源检查及临时权限恢复通过。16 项脚本测试通过；既有 hello 用例
`run-007` 六个 trial 复测通过。失败尝试 `run-cu-001`（short name 映射）
与 `run-cu-002`（provider 构造）均保留，不计为模型尝试。证据目录为
`/var/tmp/hb1008/computer-evidence-final`，包含原始结果、截图、动作轨迹、
输入与源码清单、镜像 hash 和清理回执；归档前凭据扫描无命中。

## 从源码准备

Linux 主机需具备 Python 3.12+、Docker、KVM、ublk、项目 capability runner
及非 root 运行账号。先按 [Dragonball 回归指南](../../docs/src/internals/dragonball-regression.md)
准备已验证的运行时、内核、tools 和 OverlayBD 依赖。

克隆上述固定 Harbor 提交，安装到独立 Python 环境。生成源码 manifest 时，
从干净 checkout 的 `git ls-files` 读取 `src/`、`pyproject.toml` 和上述三个
完整任务目录，记录每个文件的 SHA256 以及真实 `git rev-parse HEAD`。
`source-manifest.json` 格式为 `{"revision": "…", "files": {"相对路径": "SHA256"}}`。
当前远程目录保留了这份完整清单，可用于核验相同源码。

以具有 Docker 访问权限的非 root 账号运行：

```bash
python scripts/harbor/prepare_tasks.py \
  --harbor-source /path/to/harbor \
  --source-manifest /path/to/source-manifest.json \
  --output /path/to/new-prepared
make test-harbor-tools REGRESSION_PYTHON=/path/to/venv/bin/python
```

`prepare_tasks.py` 构建官方 Dockerfile，保存镜像和环境 hash。构建前与运行前
均拒绝源码文件的新增、删除、修改及 symlink 替换。

`run_lab.py` 接收冻结的 `--inputs` JSON、`--prepared`、全新的 `--output`
和 Harbor 可执行文件 `--harbor`。输入沿用 M4 的 `assets` 格式，并需明确
`server`、`daemon`、`ublk_cli`、`regctl`、`deps`、`s3`、`registry` 路径。
参考远程 `inputs.json` 和 `assets.json`；它们依赖已准备的共享资产，不是
可直接复制到任意机器的独立发布包。

## 测试口径与限制

默认每个 trial **4 vCPU / 8192 MiB**、每台最多 **8 个并发 trial**，无自动重试。
通过 `--task-resources --n-concurrent-trials 1` 使用原始任务规格与串行口径。
原始声明与实际分配分别写入回执；新旧规格成绩分别统计。传输探针沿用本轮任务的
有效 CPU/内存与网络策略，检查身份、用户/环境变量、非零退出码、命令截止时间和目录保真，
再检查六个准确的 `(task, agent)` 组合及多步骤顺序。
CPU 与内存由 VM 配置限定；任务里的 `storage_mb` 暂不作为磁盘硬配额执行。

任务镜像按 OCI digest 引用，任务文件和镜像改变会要求重新准备。
默认冒烟入口采用隔离网络；Terminal-Bench 入口通过上述代理联网。
以上小型评测结果不覆盖完整 Terminal-Bench、Docker Compose、多服务、GPU 和生产容量。
适配层拒绝未声明支持的功能，不能把此结果推广为完整 Harbor provider 兼容性。

当前镜像构建采用原始 `ubuntu:24.04` 标签；本次实际产出的 image ID、
archive SHA256 和 registry digest 均记录在回执中。将来重新构建可能产生
不同基础镜像；比较结果时应复用冻结镜像或明确重新构建的差异。

## 使用 ACR 镜像评测 Terminal-Bench 2.0

`prepare_acr.py` 接收已校验的镜像同步交付目录，其中包含 `images.json`、
`task-source-manifest.json` 和原始 `tasks/`。当前固定官方 registry 对应的
任务提交 `69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`，共 89 个任务。
它检查完整任务文件集合及环境 hash，以 linux/amd64 固定 digest 导出选中的
镜像。重复 `--task` 可以准备批量任务，`--all-supported` 选择过滤后的完整集合。
固定 89 项中排除 Windows/QEMU 完整虚机任务 3 项，剩余 86 项属于评测范围；
Linux 桌面、游戏与用户态 MIPS 解释器保留。范围资格不等于 Oracle 已通过。随后 `run_lab.py` 导入隔离 registry，并要求导入后的 digest 保持一致。
不会重建或修改官方 Dockerfile、任务、solution 和 verifier。

以非 root 账号在能访问 ACR 的服务器准备，登录凭据通过临时
`DOCKER_CONFIG` 交给工具，退出后删除；不要写入 job 配置：

```bash
python scripts/harbor/prepare_acr.py \
  --delivery /path/to/verified-acr-delivery \
  --task openssl-selfsigned-cert \
  --task cancel-async-tasks \
  --output /path/to/new-prepared \
  --harbor-source /path/to/harbor \
  --source-manifest /path/to/source-manifest.json \
  --regctl /path/to/regctl
```

执行使用 `run_lab.py --inputs /path/to/inputs.json --prepared /path/to/new-prepared
--output /path/to/new-run --harbor /path/to/venv/bin/harbor`。默认 4c8g、每台 8 trial；
重复任务与非冻结任务在准备前拒绝，MicroVM 排除项写入准备回执。原始
agent/verifier/build 预算保留，传输等待与 Sandbox 生存期覆盖整个任务，
总预算按并发批次计算。Oracle 与 nop 分别占一个并发槽。
准备与运行阶段均检查磁盘；准备还按固定 digest manifest 预留所有导出归档，
每个镜像前重新检查。8 槽运行典型需要 100 GiB 可用空间及 176 GiB
可用内存。节点预算固定为 64 CPU / 128 GiB / 8192 pids，夹具另留 32 GiB，
宿主保留 16 GiB；小批次和模型阶段使用相同节点上限。磁盘准入仍按活跃 trial 计算。
实际 cgroup 限制、原始任务声明、有效覆盖与 sandbox 分配都写入回执。
`storage_mb` 用于准入预留，尚未作为 guest 硬配额执行。

先严格验证 Oracle=1、nop=0、无框架异常及完整清理。保留失败现场，
只回收成功且无引用的本轮临时数据。镜像准备与执行使用相同规格参数；
自定义覆盖使用 `--override-cpus`、`--override-memory-mb`、`--n-concurrent-trials`。
独立传输探针用于诊断，资源与本轮任务一致；它不能替代 Oracle 资格。

2026-10-10 已完成 86 项环境资格验证，74 项合格；用户授权仅在 AgentENV
运行这 74 项的 Qwen 任务评测，不做 Docker 对照，12 项未合格任务记录并跳过。
使用 `--model-only --model qwen3.8-flash --template-start --shared-templates reuse`
与 `--oss-env`，无需启用 verifier 依赖缓存，保留原始 verifier 和完整 Harbor
记录。入口已移除旧 25 轮限制，仍为 4096 输出 token、thinking=false。
本轮不与历史模型分数合并，最新状态见 [专项记录](../../HARBOR_INTEGRATION.md)。带模型的
诊断模式允许记录 Oracle 0 分，不得当作严格 Oracle 资格。进一步经验、
历史结果与待办见 [HARBOR_INTEGRATION.md](../../HARBOR_INTEGRATION.md)。

精简镜像不保证包含 Python；探针会分别记录 HTTP/CONNECT 建连和
完整 HTTPS 应用请求的覆盖，未执行的 TLS 检查不计为通过。

2026-10-09 全量分片已在两台服务器启动（44/45 个任务），
前两个校准任务 Oracle=1、nop=0、模型=1，20 项驱动测试均通过。
原始结果位于 A 的 `/var/tmp/hb1008/full-20261009-v4`（此前 v3 的 regex-log 评分保留）和 B 的
`/var/tmp/hb1008/full-20261009-v2`；本轮已按用户要求终止，不能据此宣称
89 个模型任务已完成。实际 server 为 `0.2.2` / git `84e21d5`，
runner/kernel/tools SHA 均在任务回执中，不将它记作重新构建的 main。

停止时 43 项流程结束、38 项有模型阶段记录：12 通过、21 无异常的 0 分、
4 项 agent 超时、1 项 tmux 初始化失败；两项用户中断不计为解题失败。
主评测、监控和 `aenv-hbfull-20261009-model-completion` 均已停止，
没有补跑模型任务；当前任务清理和宿主恢复检查通过，失败现场保留。
本轮证据与统计在 `/tmp/agentenv-tb-full-20261009/REPORT.md`，
详细诊断见同目录 `FAILURE_ANALYSIS.md`。

21 项无异常的 0 分中 17 项触及 25 轮；43 项 Oracle 有 12 项未通过。
不得把上述冒烟预算下的部分结果作为完整模型能力成绩，也不能直接归因后端。
已确认 guest 本地 `0.0.0.0:5000` 请求未被 NO_PROXY 排除，
另有运行时下载和工具安装失败。镜像缓存不覆盖这些动态依赖。
未认定 tmux 安装失败的完整根因。

Oracle 资格修复轮已经将 `0.0.0.0` 加入大小写 `NO_PROXY`，统一 guest
代理配置，并删除代理连接固定 180 秒总期限；阶段预算和夹具清理仍限制任务。
先运行真实 HTTP 和 socket 字节转发回归，再在两台服务器非 root 运行
全部 22 项驱动测试，两边均通过。Python 可用的 guest 探针还会验证
`0.0.0.0`、`127.0.0.1`、`localhost` 的本地 HTTP 请求。

本轮 Oracle 对照目录为两机 `/var/tmp/hb1008/oracle-standard-20261009`：
A 使用 Harbor 原生 Docker 加固定 ACR digest 适配，B 使用 Dragonball。
先执行 `openssl-selfsigned-cert` 正向校准，再验证上述 12 个失败任务；
保留原始任务、解答、评分器、资源与阶段预算，每 agent 一次、零重试，
本轮不调用模型。Docker 原生网络与 VM 出站夹具、内核仍有差异；
任务镜像固定也不冻结运行时下载，不能将本轮作为性能基准。
远程实测结果须以新目录的 `campaign.json` 和各任务原始评分为准；
这轮不替换停止的模型测试成绩。


2026-10-09 后续用户明确限定 AgentENV 验证范围：排除 Windows，以及通过
QEMU 启动完整客体或依赖嵌套虚拟化的任务；Linux 桌面图形、游戏和用户态模拟器场景保留。
不能仅凭容器声明 `os=linux` 就认定适用，需检查任务实际负载。
本轮将 `install-windows-3.11` 移出资格集合，保留已经产生的 Docker
历史结果，不纳入 AgentENV 验证统计；Linux `make-doom-for-mips` 保留。
Dragonball 当前 Linux 任务完成后使用新目录
`/var/tmp/hb1008/oracle-standard-20261009-db-linux-v2` 继续剩余任务，
当前完整资格集合为 12 项；新版驱动校准独立记录，不作模型重复尝试。

专项对照发现无显式命令超时的 verifier 继承 envd 忽略 SIGINT 的状态。
`cancel-async-tasks` 因此在最后一项取消测试等待 5 秒超时，而 Docker 对照通过。
驱动统一使用现有 GNU timeout 包装；未指定超时时以 0 禁用计时器，同时恢复
SIGINT 默认行为，Harbor 阶段预算和外层 HTTP 截止保持原有语义。
真实子进程回归先失败后通过，新候选 v6 在两台服务器各 23 项测试通过。
官方 Oracle 验证单独记录于 A 的
`/var/tmp/hb1008/oracle-standard-20261009-driver-v6`：openssl 校准和原始
cancel-async-tasks 均 Oracle=1/nop=0；取消测试六项全过，三个取消子测试约
1.52 秒，与 Docker 对照一致，清理与宿主恢复通过，临时凭据删除。
修复验证绑定冻结镜像和实际 timeout 实现，不覆盖 v3/v5 原始结果；
原始 task、solution、verifier、镜像 digest 与资源配置保持不变。

`filter-js-from-html` 属于允许的 Linux 浏览器场景，但原始 verifier 创建
浏览器失败时仍默认未发现攻击，超时也按通过处理。Docker Oracle 虽为 1，
日志含九次创建驱动异常，并观察到容器 OOM 和多组 Chromium 残留。
官方分数保留，单独标记评分器覆盖不可信；不能把该分数作为图形功能验收。

`crack-7z-hash` 的独立资源扩展验证已在 B 完成：通过 Harbor
`environment.override_cpus=4` 分配 4 vCPU，内存保持 2048 MiB，原始任务、
解答、评分器、镜像 digest 与 900 秒阶段预算不变。Oracle=1/nop=0，解答
耗时 326.2 秒，真实 guest/John 均确认 4 CPU/4 线程；资源清理、宿主恢复
及临时凭据删除通过。证据在 `/var/tmp/hb1008/crack-7z-cpu4-20261009`，
独立汇总在 `/tmp/agentenv-harbor-oracle-standard-20261009/cpu4-summary.json`。
该结果证明适当资源下 MicroVM 可以完成此任务；原始 1-vCPU 的 900 秒超时
保留，不将不同资源的结果作为等价后端性能成绩。


`gpt2-codegolf` 原始配置为 1 CPU/4096 MiB。B 的独立 4c8g 实验通过
Harbor `override_cpus=4` 和 `override_memory_mb=8192` 增加资源，保持任务、
解答、评分器、镜像 digest 与两个阶段各 900 秒预算不变。Oracle=1/nop=0，
原始编译/运行测试通过（pytest 用例 9.12 秒，包含工具准备的整个 verifier
阶段 74.3 秒）；真实 guest 确认 4 CPU、约 8 GiB、无 swap、overcommit=0。
清理、宿主恢复与临时凭据删除通过，证据在
`/var/tmp/hb1008/gpt2-4c8g-20261009`，独立汇总在
`/tmp/agentenv-harbor-oracle-standard-20261009/gpt2-4c8g-summary.json`。
原配置的 SIGSEGV 保留；此前 NULL 分配探针来自 Caffe 的 2-GiB VM，
不能视为 GPT-2 同规格证据，原进程 malloc 返回值仍待验证。

## 依赖 OCI 镜像与批次模板

新增依赖模块采用[独立设计](../../docs/src/internals/harbor-dependency-reuse-design.md)，首轮验证冻结的
`openssl-selfsigned-cert` 和 `fix-ocaml-gc`；入口已扩展至全部 86 个 MicroVM 适用任务，
全量 Oracle 资格另行验证。持久依赖为 ACR 普通 OCI 数据镜像，
不替换任务启动镜像。OCaml 完整 bare mirror 在 agent 结束后才注入 verifier，
原始 clone、checkout、测试集替换、编译与评分脚本保持不变。Git 仅允许 file
协议；apt、curl 和 uv 仍可能联网，当前不是全离线环境。

准备阶段由非 root 测试账号执行，`--mirror` 必须指向已经完成下载的原始 bare
mirror；认证使用临时 `DOCKER_CONFIG`，不能放入镜像、快照或 task 环境：

```bash
python scripts/harbor/prepare_dependencies.py \
  --delivery /var/tmp/hb1008/acr-sync-20261009 \
  --mirror /path/to/ocaml.git --output /path/to/new-publication \
  --cache /var/tmp/hb1008/dependency-store-v2 --regctl /path/to/regctl
```

发布仅允许专用 `hb-deps-*` repository 的 `bundle-<manifest-sha256>` tag；
已存在同 digest 时只读复用，冲突即停止。原始任务 tag 和 latest 不更新，
不删除旧镜像。第二台主机用 `dependencies.load_image()` 拉取回执中的 digest，
校验 OCI blobs、manifest、bundle 和每个文件后原子写入节点缓存。依赖绑定的
`bundle_root` 必须指向本机已校验的缓存，缺失或损坏直接报错。

全任务绑定可以只读复用既有发布，不重新推送 ACR：

```bash
python scripts/harbor/prepare_dependencies.py \
  --delivery /var/tmp/hb1008/acr-sync-20261009 \
  --reuse-binding /path/to/verified-pilot/dependencies.json --all-supported \
  --output /path/to/new-bindings --cache /var/tmp/hb1008/dependency-store-v2 \
  --regctl /path/to/regctl
```

每机独立生成本机绑定，校验 86 项任务与原始镜像摘要。`verifier_cache_status`
明确记录 `prepared` 或 `not_prepared`；目前额外动态依赖缓存仅覆盖 OCaml Git，
其余任务使用原始 verifier，运行期下载仍可能联网。构建期依赖已包含在原始 ACR 镜像内。
缓存保留 OCI manifest/config，并重新计算数据层摘要；没有 OCI 证明的旧缓存明确拒绝，
应使用新缓存目录重新按 digest 拉取，不自动改写旧缓存。冲突的 Git URL rewrite、
协议限制或 opaque runtime 参数在 guest 传输前拒绝。

全任务准备仍按批次调用 `prepare_acr.py --task ...` 和下方模板入口；不要一次 capture
86 个任务。模板内存、根盘、所选依赖的归档与解包副本、镜像导出空间均计入准入。
Oracle/nop 各执行一次，每机并发上限 8，实际可同时选多少任务由剩余磁盘决定；
同一主机的 Lab 串行，不与依赖准备或恢复宿主权限竞争。严格资格失败保留现场，
不能按成功结果清理，也不能将任务自身失败加入 MicroVM 排除名单。

原有 `run_lab.py` 调用仍从原始镜像冷启动。显式增加下列参数才使用新路径：

```bash
python scripts/harbor/run_lab.py \
  --inputs /path/to/inputs.json --prepared /path/to/prepared \
  --output /path/to/new-run --harbor /path/to/venv/bin/harbor \
  --dependencies /path/to/dependencies.json --template-start
```

Root 执行输入摘要预检和隔离 Lab 编排；模板创建与 Harbor trial 在配置的
非 root 账号下运行，verifier 会再次校验依赖包。`--dependencies` 单独使用表示冷启动加 verifier 缓存；
`--template-start` 还会逐任务创建干净 VM、计算工作区摘要、capture，再删除
源 VM。模板绑定任务全文件 hash、原始镜像 digest、CPU/内存、运行资产和批次，
恢复后核对新 Sandbox ID 与初始工作区摘要。磁盘准入另外计入完整内存快照、
rootfs 空间和活跃 trial 的依赖副本。原始阶段预算与零重试设置保留。

`templates/receipt.json` 记录准备阶段，trial 的 `agentenv.json` 记录启动方式、
模板键和工作区，`dependencies.json` 记录 verifier 注入及清理。
ACR 依赖镜像跨批次保留；默认批次模板在成功 payload 回收后重新 capture。
跨服务器持久模板使用 `--oss-env` 与 `--shared-templates prepare/reuse`，见
[共享模板说明](OSS.md#两台物理服务器共享任务模板)。每个 trial 仍创建独立 sandbox，
共享 OSS 数据不随 Lab 回收删除。完整模型评测继续停止；首轮真实资格结果单独记录，不能将
单元测试通过写成全套件或模型评测通过。
## 失败取证、下载复用与候选冻结

verifier 失败或取消时，驱动先写 `failure-evidence/receipt.json`，再用独立
15 秒预算保存最多 256 KiB 日志尾部和 `df`、内存、进程及 cgroup 状态。
取证失败仍保留原始异常；原始评分器、评分和任务预算不变。取证属于评分结束后的
诊断，不能用它延长 verifier 运行。系统命令从 `/` 执行，不依赖任务工作目录。
`agentenv.json` 的 `guest_disk` 在启动和删除前记录文件系统容量/占用；
`storage_declared_mb` 为任务声明，`storage_limit_enforced=false` 表示未实施硬配额。

当前公共下载配方仅覆盖 `caffe-cifar-10` 的 CIFAR 数据和两任务的 uv 0.9.5
Linux GNU 工具归档。以非 root 账号在 Lab 外持久准备：

```bash
python scripts/harbor/prepare_downloads.py \
  --delivery /var/tmp/hb1008/acr-sync-20261009 \
  --cache /var/tmp/hb1008/download-cache \
  --output /absolute/path/to/new-downloads.json \
  --task caffe-cifar-10 --task path-tracing
```

随后在现有 `run_lab.py` 命令增加 `--downloads /absolute/path/to/new-downloads.json`。
缓存检查官方校验值、任务文件摘要、原始镜像 digest 和实际文件 SHA256；后续准备
命中已校验文件，不再拉取该归档，冲突或损坏直接失败，不覆盖旧绑定。
CIFAR 保留原始 HTTP URL，经测试代理读取同一归档；uv 安装器使用其支持的
`UV_DOWNLOAD_URL`，只向 verifier 配置公开工具镜像地址。任务、解答、测试脚本
不改写；未准备的 apt/pip、Python 包和安装脚本仍可能联网，不能宣称全离线。
缓存不会随 Lab 清理删除，不自动发布或覆盖 ACR 镜像。

候选冻结工具从已检查的基线复制依赖文件，并覆盖显式选中的驱动与配置示例：

```bash
python scripts/harbor/freeze_driver.py \
  --baseline /absolute/path/to/previous-candidate/source \
  --output /absolute/path/to/new-candidate
```

输出逐文件 SHA256 清单和 tar 包；已存在的目录/归档、symlink 和真实 dotenv
被拒绝。该工具负责候选打包，尚未替代外部控制器的 host lease、恢复和进度汇总。

## Harbor 专用 OSS 配置

使用 [OSS 执行文档](OSS.md) 和 [.env.example](.env.example) 配置
`run_lab.py --oss-env /private/path/.env`。该选项跳过 MinIO，只影响 Harbor Lab，
不修改项目默认配置。真实 `.env` 必须归属运行账号且权限为 0600，不得提交或推送。


## 交付、缓存准备与阶段检查

使用固定环境的 Python 调用工具。`make test-harbor-tools REGRESSION_PYTHON=/path/to/venv/bin/python`
先拒绝 Python 3.12 以下版本，再检查 Git 中的私有 dotenv，最后执行驱动测试；冻结包
没有 Git index 时检查完整候选文件集与摘要。失败退出码直接向外返回，读取日志时保留
原命令状态，不使用日志尾部命令作为成功判断。

镜像及候选传输先写 `.part`，等待传输命令成功退出，再发布独立校验副本：

```bash
/path/to/venv/bin/python scripts/harbor/artifact_delivery.py \
  --partial /path/to/image.tar.part --output /path/to/image.tar \
  --sha256 FROZEN_SHA256 --bytes FROZEN_BYTE_COUNT
```

工具校验完整大小与 SHA，原子发布另一个 inode，拒绝覆盖已有 final；原 `.part`
保留为证据。源进程后续写入不会改变 final。发布期间还需要一份文件副本的磁盘空间。
准备与测试仅使用 final。固定值来自已核验的生产端回执，不从正在传输的文件计算。
优先用两机 VPC 地址传送数据，SSH 公网地址用于控制；校验标准相同。

准备入口支持显式的本机缓存清单，见 [示例](examples/cached-archives.example.json)：
示例路径、全零 SHA 和字节数均须替换为当前宿主已核验归档的实际值。

```bash
/path/to/venv/bin/python scripts/harbor/prepare_acr.py \
  --delivery /path/to/frozen-acr-delivery --task openssl-selfsigned-cert \
  --harbor-source /path/to/harbor --source-manifest /path/to/source-manifest.json \
  --cached-archives /path/on/this-host/archives.json --output /path/to/new-prepared \
  --n-concurrent-trials 1
```

该模式替代 `--regctl` 导出，检查每个归档及全部 OCI blobs、原 manifest/config 与
layer descriptor；不查询旧批次 prepared.json，不访问 ACR，不改变原镜像。
未命中或损坏直接失败。原 `--regctl` 准备入口保留。

冻结控制脚本时显式增加参数；解包后在执行前核对完整文件集：

```bash
/path/to/venv/bin/python scripts/harbor/freeze_driver.py \
  --baseline /path/to/old-candidate/source --output /path/to/new-candidate \
  --control-input /path/to/controller.py --control-input /path/to/setup_host.py \
  --control-input /path/to/restore_host.py --control-input /path/to/collect.py
/path/to/venv/bin/python scripts/harbor/freeze_driver.py --verify /path/to/new-candidate
```

控制脚本位于 `source/harbor-control/`，属于同一摘要清单；传入的是不含凭据的已审查
Python/shell 脚本。凭据只留在私有文件中。准备脚本及取证脚本也应显式包含。
冻结不会重写脚本中的旧绝对路径；执行计划必须调用冻结副本，并预检它实际引用
的本机输入。上传后的候选及归档也须能被运行账号读取，不能只检查 root 可读。
宿主路径按各机填写 [输入预检示例](examples/host-inputs.example.json)，由已有
`scripts/tests/dragonball_m4_inputs.py` 以实际非 root UID 读取/解析；预检通过后才
开始昂贵工作。顶层有工作负载的控制脚本列入 `files`，不作为可执行 import 的 `modules`。

阶段编排沿用 `scripts/tests/dragonball_m4_campaign.py`，见
[现有执行器](../../docs/src/internals/dragonball-regression.md)。计划 `inputs` 显式包含
实际执行的冻结控制脚本、宿主输入及资产清单；`steps.argv` 用固定解释器和冻结脚本
路径，按传输→发布→非 root 输入预检→准备→受 host lease 保护的控制器→资格检查执行。
执行器检查输入/产出并保存子命令退出码，阶段失败时返回非零并停止后续阶段；
VM 控制器继续负责 finally 中的宿主恢复；
执行器本身不推断 VM 清理，也不代替 host lease。真实 dotenv 不进入候选或 Git。

共享模板资格检查可以比较多个运行目录：

```bash
/path/to/venv/bin/python scripts/harbor/qualification.py \
  --run-dir /path/to/producer/warm --run-dir /path/to/consumer/warm --runtime-uid 999
```

该命令检查原始 Oracle/nop、零异常、资源/资产对齐、同模板、不同身份和一致的初始工作区摘要。
`reuse` 还要求 builder=0、模板首次预检、空 Lab 快照/转换层缓存。输出明确区分首次
恢复与后续 trial；宿主 OS page cache 不清空。Lab 清理、VMM UID 与探针清理均须通过，
宿主恢复及 cloud 审计仍读取独立回执。当前验收证据统一见
[专项记录](../../HARBOR_INTEGRATION.md)，本节仅说明现行操作。


## 持久 Lab 持续执行入口

`run_campaign.py` 面向冻结的完整任务格式，复用已提交 OSS 模板。每台一组
AgentENV／registry／proxy，原生 Job 各一个 trial，最多八槽；删除未确认时
不补位。`run_lab.py` 仍保留小规模批次兼容入口。两者启动时都先在宿主租约
下清理超过 24 小时的已记录、无引用 Lab，保存计划与可追踪的部分失败回执。
需要预先完成宿主 setup，并在整轮结束恢复；不可直接运行缺少 ublk 模块的宿主。

```bash
# 宿主 setup/restore 控制器持有同一租约时，执行这一整轮入口。
python -B scripts/harbor/run_campaign.py \
  --inputs /path/inputs.json --prepared /path/new-prepared \
  --output /path/new-campaign --harbor /path/venv/bin/harbor \
  --oss-env /path/private/.env --shared-templates reuse \
  --downloads /path/downloads.json --n-concurrent-trials 8
```

正式模型另加 `--model qwen3.8-flash --model-only`，仅模型 CLI 取得凭据。
不更改 task／verifier／阶段预算，attempt=1、retry=0；每项的 Job config、lock、
result、日志与模型轨迹独立位于 `trials/<trial-id>/jobs/`。
`--acceptance-trials 9` 仅用于一个冻结任务的无模型调度夹具（guest sleep＋原
verifier）；其结果属于工程验收，不能替代任务资格或模型通过率。

`events.jsonl` 保存实际准入／删除／结束事件；`campaign.json` 保存 completed、
interrupted、pending 与 cleanup_errors；`campaign-monitor.json` 保存 VMM 峰值
和宿主最低空闲空间。任务仍声明 10 GiB、模板虚拟盘仍为 64 GiB、storage 硬
限额未执行。OSS committed 源层按真实大小共享计数，registry／commit 的
已核验物理块可抵扣；私有写入目前保守预留至删除确认，紧盘时可能少于八并发。
保留 20 GiB 实际安全水位，低水位停止新准入并有界终止，不生成伪造评分。

远程验收冻结 `harbor-control/repair_controller.py`，显式提供 source、prepared、
stage 和 trials，复用同候选 setup_host／restore_host。它剥离模型凭据，记录
model_calls=0、宿主恢复和最终无残留；控制脚本是冻结操作输入，不是全局后台服务。

## 素材准入、生效配置与失败摘要

正式模型入口接受 `--material-inputs /path/materials.json`，清单格式见
[素材示例](material-inputs.example.json)。它绑定固定 Harbor/task revision、原始
任务树摘要、实际来源和内容大小/SHA。示例的两项均为 `unchecked`，不能直接
作为合格输入使用。当前内置必需源只有视频和 MTEB 两项，未宣称扫描了全套
任务；其他任务没有已知源条目不等于已证明可离线执行。

主机和 guest 的素材检查独立于 Oracle/nop；HTTP 必须读取有界完整正文并验证
内容。YouTube 网页 HTTP 200 不证明视频可下载。缓存内容匹配只记
`cache_verified`，尚未证明原 URL 的 guest delivery 时仍禁止模型准入。
未合格项记录为未执行，保留原始选择分母，其余任务继续；不改原任务或评分器。

`effective-profile.json` 在模型请求前核对实际节点配额、guest 规格、阶段预算及
候选/资产摘要。旧冒烟轮数和额外超时覆盖会被拒绝；节点仍为 64c/128GiB，
guest 默认 4c8g，每机最多八槽。AgentTimeout 的现场取证在 sandbox DELETE 前
执行，独立总预算 15 秒；取证失败单列，不修改原错误或 reward。

每轮 `summary.json` 从原生结果和准入事件生成，记录通过、正常 0 分、异常、
未执行、token 和原生阶段耗时。槽位积分与实际 VMM 峰值分别报告；未测得的
准备/恢复/安装/清理时间和缓存消费不填零，不推算提速比例。输入 token 包含
缓存 token；框架 cost 不是百炼实际账单。原生 result、轨迹和评分保持不变。
只读重建摘要时用固定解释器执行：

```bash
python -B scripts/harbor/benchmark_summary.py \
  --report /path/run/receipt.json --output /path/summary.json
```

此前独立诊断候选为冷启动 `/run` 目录增加了 package tmpfiles 初始化。
该 guest 行为修复、相关测试和 tools 0.1.3 配方留在独立工作区，未纳入本次 Harbor
合并；Flash/Max 均使用原 tools 0.1.2-p4.1 与原模板。详见
[集成记录](../../HARBOR_INTEGRATION.md#复盘改进执行2026-10-11)。
