# Dragonball 开发与功能回归

日常按功能选择测试，不按已经结束的 M0–M5 实施阶段串行推进。共用夹具位于
`tests/integration/dragonball_support.rs`，测试模块相互独立；生产实现仍按现有
sandbox/runner/protocol 职责划分，运行时协议和快照身份由各后端维护。

| 目的 | 当前入口 | 运行边界 |
| --- | --- | --- |
| 生命周期、捕获/恢复 | `test-dragonball-regression` + `--suite lifecycle` | 预构建 DB-only harness |
| 增量内存、balloon/slots、启动优化 | `--suite memory` / `--suite slots` / `--suite startup` | 默认与 lifecycle 一起执行，套件不重叠 |
| 平台/OSS/模板功能 | `test-dragonball-platform` | 显式 native S3/registry/server 夹具 |
| 持久化身份兼容 | `--suite compatibility` | 显式选择，日常默认不执行旧 revision 专项 |
| 底层 VMM 诊断 | `test-dragonball-probe` | 保留独立 probe 保存/恢复能力 |
| 双后端 feature 构建 / DB-only 完整运行时 | `test-dragonball-dual-backend` / `test-dragonball-runtime` | 两种 feature 覆盖分别保留 |
| 同机跨节点 / 跨物理主机 / 性能 | `test-hypervisor-cross-node` / `test-hypervisor-physical` / `test-hypervisor-performance` | 显式输入与容量预检，性能默认仅生成计划 |
| 测试工具契约 | `test-regression-tools` / `test-hypervisor-tools` | 后者要求 Linux；真实 namespace/termination/physical 测试仍显式 opt-in |

Makefile 共用 `prepare-dragonball-tests` 和测试环境；M0–M4 旧 Make 目标仅为薄别名。
命令包装由 `run-dragonball-tests.sh` 统一检查 probe/runtime/platform 的前置条件，
再委派给 `run-dragonball-isolated.sh`。历史 M4 Python 模块构成同一个仍在使用的
lab 工具集，继续复用，功能入口不要求理解编号。`AENV_DRAGONBALL_M0_ASSETS` 等既有
夹具变量及 probe wire/profile 标识保留，避免无收益的配置和协议扰动。

发布/资产清单能力由[打包脚本](dragonball-packaging.md)和后端组件测试负责。
验证结论及已知限制见[验证摘要](hypervisor-validation.md)。

## 使用

在隔离的 Linux x86_64 测试服务器上，以原有非 root 测试账号运行。
需要 Python 3.11+、已有 capability runner 的 sudo 委派、KVM/ublk 访问权限，
以及已验证的 Dragonball 内核和工具配置。入口显式传递 PATH/HOME/TMPDIR，
并在权限边界内核对相关环境摘要，避免 sudo 的环境过滤改变实际输入。
以下路径仅为示例，应选择新的短路径，
不要使用线上服务的状态目录。

```bash
export AENV_CONFIG_PATH=/var/tmp/db-dev/config.toml
export AENV_TEST_STATE_DIR=/var/tmp/db-dev/state
export AENV_TEST_DEPS_PATH=/var/tmp/aenv-deps
export AENV_REGRESSION_OUTPUT=/var/tmp/db-dev/results-01

# 构建、setup-only、生成本地 guest assets；首次运行或产品输入变更时执行。
make prepare-dragonball-regression

# 经实际 capability/namespace 入口预检；不启动 VM。
make test-dragonball-regression AENV_REGRESSION_ARGS=--preflight-only

# 默认串行执行 lifecycle、memory、slots、startup。
make test-dragonball-regression

# 同一候选失败/中断后，复用通过阶段，重新执行未通过阶段。
make test-dragonball-regression AENV_REGRESSION_ARGS=--resume
```

`REGRESSION_PYTHON` 可指定 Python 3.11+ 的可执行路径。
准备入口在构建和显式 `--setup-only` 之前取得主机锁并预检。新状态目录在磁盘、
路径和所有权检查通过后创建；四份 OverlayBD global JSON 必须位于隔离 HOME，
准备完成后校验并记录摘要。此入口仍可能安装或下载已有 setup 所需的依赖；
测试执行入口不会隐式调用准备。准备日志保留在 `state/prepare-*/build.log`。

准备阶段保留 `state/regression-build.jsonl`，从 Cargo 的 compiler-artifact 中
选取 integration harness，复制为 `state/regression-integration`；测试阶段不构建。
assets 保存为 `state/dragonball-assets.json`。
`AENV_REGRESSION_BINARY` 可覆盖 harness 路径，提供已有的冻结 DB-only harness。
使用外部 harness 时，调用者负责核对其构建来源；脚本记录二进制摘要与当前源码，
**不会据此证明外部二进制由当前源码编译**。

平台套件独立使用 Make 入口，由包装脚本管理磁盘临时目录及退出清理；
不并入当前支持续跑的功能套件。它需要显式 S3/registry 可执行文件和 DB-only server。

已有完整环境的编排可直接调用 Python 入口（`--help` 查看参数），复用显式的
`AENV_HOME_PATH`、`AENV_RUNTIME_PATH`、`AENV_DEPS_PATH`、runner、daemon 和 assets。
`--suite memory` 可只运行内存套件，可重复传入 `--suite` 选择多项。
恢复时套件选择必须与原记录相同；重组前的阶段回执不能用于当前候选。
套件内失败会重跑整个套件，不逐用例复用，以保留套件进程内交互的覆盖。

## 预检与结果有效性

预检检查实际非 root 身份、命名空间、capabilities、4 KiB 页、KVM API、ublk
访问权限、明确的隔离状态目录、磁盘/inode 预算及 runtime socket 路径。
默认要求 home/run 和临时目录所在文件系统至少剩余 20 GiB、10,000 inode；
`--min-free-gib` 可按工作负载调整。这是准入预算，不是内存或磁盘峰值保证。
生成的 rootfs/memory/resize/convert OverlayBD JSON 必须存在且可解析；
脚本不代替服务端完整配置验证，也不自动安装依赖或修改主机权限。

输入身份包含 harness、runner、daemon、内核、tools、rootfs 本地 lower、已安装
依赖文件、生成配置、源码内容/提交、相关环境摘要和主机/启动身份。
凭据及配置正文不写入结果。路径和测试日志仍需按项目证据策略保管。
依赖目录中的目录符号链接会被拒绝，以免漏掉其指向的内容。
每阶段后重新核对输入；变更后不能把该轮标为通过。

只有同时满足以下条件才复用阶段：输入和阶段选择一致、上次状态为通过、
测试清单一致、原日志摘要一致。显式的零匹配或退出 0 但未完整执行的阶段都不算通过。
源码/配置/二进制/环境变化、主机重启时，使用新结果目录重新执行；当前实现保守地
使整批记录失效，尚不自动推断哪些源码变化只影响某个阶段。

`results.json` 保存每次尝试的测试清单、状态、退出码、耗时和日志摘要。
`lifecycle-1.log`、`memory-1.log`、`memory-2.log` 等日志不覆盖，复用会明确打印 `reused`，
不计为新执行样本。每阶段默认超时 1,800 秒，可用 `--timeout` 调整。
中断/超时会通知子进程组退出并保留非通过回执；SIGKILL 后留下的 `running`
也不能复用为通过。恢复前仍执行预检：存在 ublk 设备或已知 runner/daemon 进程时
拒绝运行，要求先审计。工具不负责删除残留设备、历史快照或共享数据。
不同结果目录共用同一状态目录时也互斥，但不会替其他测试工具加锁。

`make prepare-dragonball-regression`、功能套件的隔离执行器、
`dragonball_m4_build.py` 与 M4 `Lab` 共用主机锁
`/var/tmp/agentenv-dragonball-test-host.lock`，跨 checkout、状态目录和 root/测试账号
互斥；冲突立即失败。准备、构建、功能进程通过经过校验的文件描述符传递锁，
递归 Make 与受支持子进程共享租约；控制器退出时，存活子进程仍持锁。
M4 Lab 的锁由控制器持有，以便审计恢复入口在控制器被杀后取得锁；残留 cgroup
和 ublk 检查会阻止新测试直接进入。锁文件不删除，不能把锁释放视为清理完成。
直接调用任意 Cargo、`prepare-dragonball-tests`、独立 runner CLI 和其他 Make
入口不在此互斥范围内。

共享预检支持非 root 构建／功能模式。所有 M4 Lab 入口默认预检，默认磁盘预留
20 GiB；显式 profile 可指定工作负载预算。`resources-*.json` 将准备、构建、
功能和 Lab 的控制器、目录／cgroup 身份、输入与证据摘要、结果和清理状态关联。
这是证据清单，不授予新删除权限；失败与中断状态保留，SIGKILL 可能留下 `running`。
现有 M4 成功 payload GC 仍执行独立归属和引用检查，其他状态不自动删除。

### M4 中断后的审计与恢复

功能隔离执行器和 M4 Lab 在取得主机锁后检查残留 `db-m4.*` cgroup 与 ublk
设备，发现残留即拒绝启动。M4 Lab 将 root/cgroup 的设备号和 inode、主机 boot ID、
控制器 PID/启动时间以及已就绪的命名空间写入 root 所有的 `lab-manifest.json`。
新入口可审计这份清单，并在控制器已退出且身份匹配时回收该 cgroup 内的进程：

```bash
# 使用日志中明确记录的单个 Lab 路径；默认只读，不回收资源。
sudo python3 scripts/tests/dragonball_m4_recovery.py --root /var/tmp/db-m4.RUN_ID
# 明确执行回收，保留状态目录、日志、失败镜像和所有 ublk 设备。
sudo python3 scripts/tests/dragonball_m4_recovery.py --root /var/tmp/db-m4.RUN_ID --apply
```

入口与测试共用主机锁，拒绝与正在执行的任务并行。主机重启、root/cgroup 身份不符、
缺少清单或清单路径不可信时拒绝回收；旧实验目录不能仅凭名字自动接管。
JSON 的 `clean` 返回 0；`orphaned`、`active`、`manual_review` 均不是清理成功。
每次实际恢复尝试独立保存 `recovery-*.json`，允许在处理阻塞项后重复执行。

恢复仅针对已确认归属的进程/cgroup。残留 ublk 设备和外部进程的文件、挂载及
命名空间引用会列入 `manual_review`；历史日志中的设备编号不能证明当前归属，
不会据此删除设备。此入口不删除历史数据，也不替代通用 GC 或跨套件续跑。
真实 SIGKILL 资格测试使用隔离命名空间、非 root 工作负载，不启动 VM：

```bash
sudo env PYTHONDONTWRITEBYTECODE=1 python3 scripts/tests/test_dragonball_m4_recovery_linux.py --linux
```

### M4 准入、归档与跨入口续跑

`dragonball_m4_preflight.py` 提供只读预检：Linux/Python 版本、非 root 工作账号、
目标账号 KVM 访问、显式工具/环境、socket 路径字节长度、cgroup、残留资源和磁盘预算。
`--required-env` 比较编排进程与 `runuser` 后的值摘要，不输出值；这不代替其他权限
包装器的环境验证。Docker 仅在指定 `--docker` 时检查。各预算用
`--disk-path`、`--reserve-bytes`、`--planned-bytes`、`--retained-bytes` 显式传入。
`Lab(..., preflight_profile=...)` 在持有主机锁、创建 cgroup 前执行该检查；未传 profile
的调用使用默认 Lab 预检和磁盘预算。磁盘准入不是预留空间或文件系统强制配额。

`input_files` profile（CLI 为 `--input-files` JSON）可指定绝对路径列表：`files`
检查读取、`executables` 另检查执行权限、`json`/`toml` 检查解析、`assets` 检查
冻结资产清单及其 runner/kernel/tools/rootfs lower，`modules` 在新解释器中导入
可信测试模块。所有检查以工作账号执行，不输出文件内容或模块的 stdout/stderr；
模块必须是导入时不启动任务的库文件。它不执行二进制，也不验证 TOML 的业务语义。
临时驱动应在 profile 中列出自身模块和生成配置，不能只复用磁盘/KVM 预检。
性能入口已自动接入直接使用的二进制、资产和驱动模块检查；生成的 server TOML、
regctl JSON 在启动 server 前另以工作账号检查。路径来自当前机器的冻结清单。

成功的 M4 性能 cell 可使用 `dragonball_m4_retention.py` 归档及回收：

```bash
sudo python3 scripts/tests/dragonball_m4_retention.py archive --root /var/tmp/db-m4.RUN_ID
# 先将返回的 archive_sha256 保存到 Lab 外的任务证据目录。
sudo python3 scripts/tests/dragonball_m4_retention.py audit --root /var/tmp/db-m4.RUN_ID --archive-sha256 RECORDED_SHA256
sudo python3 scripts/tests/dragonball_m4_retention.py apply --root /var/tmp/db-m4.RUN_ID --archive-sha256 RECORDED_SHA256
```

只接管同一次主机启动内、可信清单及输入核验通过的成功性能结果。归档保存结果、
配置和日志；删除前重新检查引用、身份及归档摘要。失败、未完成、pin、符号链接、
未知布局和身份变化均拒绝。`a/run`、`b/run` 连同 socket 始终保留，回执中的
`retained_bytes` 只统计这些运行目录，不是整个 Lab 的剩余大小。该入口判定的是
可回收数据的资格，不证明保留目录中所有 UNIX socket 都已失活，也不删除 ublk 设备。
中断或部分删除记录失败及剩余/未知字节数；SIGKILL 无法保证写回执，歧义现场须人工审计。

`dragonball_m4_campaign.py` 可按显式 JSON 计划串行调用已有测试入口：

```bash
python3 scripts/tests/dragonball_m4_campaign.py plan --plan PLAN.json --output OUTPUT
python3 scripts/tests/dragonball_m4_campaign.py run --plan PLAN.json --output OUTPUT
python3 scripts/tests/dragonball_m4_campaign.py run --plan PLAN.json --output OUTPUT --resume
```

计划指定 `source`、`cwd`、`inputs` 文件列表、`environment` 变量名及 `steps`；每步
包含唯一 `name`、`argv`、`timeout_seconds`、`artifacts`。必须显式列出相关源码、
二进制及配置，目录不会自动递归绑定。成功步骤仅在输入、环境、日志及产物摘要均
一致时复用；失效步骤及后续依赖重跑，失败尝试保留。计划绑定变化需要新输出目录。
回执记录每次执行/复用原因、阶段耗时、失败类别、磁盘/inode 及未执行步骤；子入口
仍负责验证真实用例数量、VM/设备清理和结果语义，命令退出 0 不自动等于生产资格。

M4 节点就绪改用独立管道、总截止时间及严格 PID/namespace 校验，`ready` 文件只是
原子写入的证据。未就绪、提前退出、畸形或超长消息会失败，退出和回收仍归 Lab。
同时调用 probe 与 dual-backend 时，共享库测试只执行一次；两种 VM 过滤条件和
独立的内存导出预算测试保留，不按名称相似合并不同后端/权限覆盖。

覆盖盘点还需区分：`test-unit` 包含有权限要求的 ublk 测试；`test` 与
`test-integration` 的 envd/ublk 公共前置目标可在同一次 Make 调用中去重，但后者
额外的 snapshot OSS E2E 仍需保留。DB-only 与双后端 feature、ignored 与普通运行、
同机 namespace 与物理跨机、三种 E2E 部署模式不能互相替代。`dragonball_apf::`
虽在相应平台/feature 下编译，却不属于默认四个 DB 功能套件；需要时显式选择。
`services` 的聚合 test 只选择 gateway/scheduler，shared/API 的完整模块检查仍使用
该目录下的 `go test ./...`。

## 回归范围建议

| 改动 | 日常验证 | 合并/资格验证 |
| --- | --- | --- |
| 文档、测试编排脚本 | diff、脚本契约测试、实际入口预检 | 受影响的真实场景 |
| DB 模块实现 | 定向库测试、受影响的 DB 功能套件 | FC 兼容检查及适用 Clippy |
| Kata、内核、恢复 ABI、公共生命周期 | 定向问题复现 | 适用功能/平台、应用、跨机验证 |
| 性能或容量 | 独立矩阵与资源预算 | 不与编译及其他负载混跑 |

脚本契约测试使用 `make test-regression-tools`；Linux lab 工具契约使用
`make test-hypervisor-tools`。后者自动发现测试，特权 namespace、真实 termination
和 physical 用例仍需显式入口与输入。跨套件恢复编排和自动 GC 尚未实现。

## 物理测试主机配置

真实服务器的 SSH 目标、内网 IP 和资产路径由本地 JSON 配置提供；仓库仅保存
示例。单元测试使用固定的虚构地址，不读取服务器配置。

- 控制端复制 [physical-hosts.example.json](../../../config/tests/physical-hosts.example.json)
  为同目录下的 `physical-hosts.local.json`，填写两台主机的信息。
- 每台测试主机复制 [physical-inputs.example.json](../../../config/tests/physical-inputs.example.json)
  为 `physical-inputs.local.json`，填写本机私网地址及已准备好的二进制、资产和依赖路径。
  `assets` 指向测试准备流程生成的资产 JSON，`deps` 包含 `overlaybd` 和 `regctl`。
- `hosts` 中的 `script`、`inputs` 是对应远端主机上的绝对路径；`ssh` 可填写
  `~/.ssh/config` 的主机别名。远端 SSH 会话需能执行 root 编排，工作负载仍由
  lab 以 `aenv` 账号运行。两台机器须满足既有 Linux/KVM/ublk/cgroup 要求。
- `private_ip` 必须是两台机器之间可路由的 RFC1918 地址；SSH 控制地址与数据传输
  地址可以不同。控制端通过 `--private-ip` 传入 `hosts` 中的值，覆盖 worker 输入
  文件中的同名字段；直接运行 worker 时使用输入文件中的值。缺失或非法地址会在
  分配测试资源前失败。

```bash
make test-hypervisor-physical \
  AENV_M4_HOSTS="$PWD/config/tests/physical-hosts.local.json" \
  AENV_M4_OUTPUT=/var/tmp/physical-test-run
```

输出目录必须不存在。单机 worker 清理测试在已准备好的 Linux 测试主机上，以具备
root 编排权限的进程显式运行：

```bash
AENV_PHYSICAL_TEST_INPUTS="$PWD/config/tests/physical-inputs.local.json" \
  python3 -m unittest discover -s scripts/tests -p test_dragonball_m4_physical.py -v
```

`config/tests/*.local.json` 已加入 Git 忽略规则。实际地址、登录凭据和实验输出不放入
版本库；SSH 认证由本机 SSH 配置负责，JSON 不保存私钥或密码。

## 迭代顺序

先完成交叉契约审阅、定向测试和 debug VM 冒烟，再冻结源码、构建 release 及生成
回执。设备策略变更同时核对创建、typed snapshot 和 probe snapshot 校验。
新状态先执行 setup-only，定向测试复用实际 capability/namespace 入口的预检。
先在外部构建 harness，再进入隔离环境执行，避免在隔离网络内解析 Cargo 依赖。

性能对照核对实际打开的资产 inode/标志、CPU/NUMA 和缓存条件，不能只比较配置
文本。构建与采样分时；小样本用于定位，完整资格另行运行。失败记录、数据校验和
清理检查始终保留；跨入口进度汇总、构建与测试之间的统一互斥仍待实现。

## 性能候选的构建来源与共享预检

`make check-dragonball-revision` 是无需 Cargo/网络/KVM 的轻量检查（Python 3.11+）。
它核对 runner manifest、Cargo.lock 的 Kata 来源、qualified reader、probe client
和当前版本的 CLI/打包正例；历史拒绝用例保留旧 revision。DB 构建、准备及 VMM
单元测试入口先执行此检查；Rust CI 的编译任务依赖对应检查任务。

性能矩阵现在要求 `server_receipt` 和 `server_fc_receipt` 两个绝对路径，分别
绑定 `server`（DB）与 `server_fc`（FC）。功能场景及开发回归使用各自的输入格式。
旧性能二进制不能补签回执；通过下列入口重新构建并冻结，Cargo 可复用已有缓存。
构建以非 root 用户执行，输出放在源码 checkout 外的新目录，不覆盖历史候选：

```bash
python3 scripts/tests/dragonball_m4_build.py --source "$PWD" \
  --backend dragonball --output /var/tmp/candidate-db
python3 scripts/tests/dragonball_m4_build.py --source "$PWD" \
  --backend firecracker --output /var/tmp/candidate-fc
```

每个目录包含 `server`、`receipt.json`、Cargo JSON 产物记录和 stderr 日志。
默认 release；`--profile debug` 可用于开发检查，`--cargo` 可指定 Cargo 路径。
将 M4 inputs 中两个 server 路径指向这些冻结文件，并加入对应的 receipt 路径，
再执行既有 `make test-hypervisor-performance`（默认仅做容量计划）。
构建参数、Cargo/rustc 版本、环境摘要、Cargo 配置摘要和实际
`--component-info-json` 输出随回执保存；凭据和完整环境正文不写入回执。
构建前后源码或构建输入变化、Cargo 失败、产物不唯一、嵌入 HEAD 不符时均不生成
成功回执。dirty 候选保留真实 HEAD，并另记源码树及二进制补丁摘要。

CLI 和直接调用 `run_cell` 都在创建 lab 前校验公共内核字节、两份构建回执、
冻结二进制、Cargo 产物日志及原源码/配置。root 编排器委派原构建 UID 执行
校验，不修改 Git 全局所有权例外；原构建账号须仍存在。必须保留原源码 checkout 不变；
对不同补丁使用独立 checkout。回执和构建输出应保留在同一目录。
每个 cell 保存所用回执；可比性同时检查源码和构建条件，不再只凭 Git HEAD。
开发记录仍不产生性能资格结论。

该回执用于受控测试主机上的过期输入检查，不是签名或可复现构建证明：依赖缓存、
外部工具及构建脚本的任意外部输入尚未逐项封存。构建期间仍须避免其他任务改写
共享 Cargo 输出；M4 续跑和构建缓存自动命中策略尚未实现。
此入口仅为 server 生成回执，runner、daemon、guest 资产仍由既有 M4 输入摘要记录。

性能 cell 在启动前及结束后重新校验两份构建回执，并核对 runner、daemon、guest
内核/tools/rootfs lower、夹具、依赖及编排脚本的内容和权限。输入变化或丢失会使
该 cell 失败；已有操作失败信息和清理结果保留，原始样本仍可诊断但不能作为有效
对照。汇总比较还要求各 cell 的输入摘要与矩阵开始时一致，拒绝 cell 之间的输入变化。
结果中的 `input_identity` 和 `inputs_verified` 记录这项检查。它用于发现
意外输入变化，不是隔离其他进程写入或可复现构建的证明。

自定义 guest tools 时，在 inputs 中显式设置 `tools_version`（省略仍为 `0.1.0`），
与资产内 `/agentenv/tools-drive-version` 一致；两个后端共用这个版本和同一份
`tools_path` 内容。比较 guest 补丁须分别冻结前后资产及编译输入，不能把某一后端
升级后的 tools 与另一后端的旧 tools 比较。旧快照内的 envd 不随主机配置自动升级。

M4 HTTP 响应按剩余请求预算逐段读取；流中的持续数据不能重置正文读取的总预算。
性能比较应对齐 API 发起至相同 guest 命令确认准确身份的终点、端到端时限和失败
计数；API 返回时间另列，不把后端各自的就绪功能契约当作相同的性能终点。
通用性能入口的 cold/restore/fork/resume 已采用相同 sandbox/template 身份检查，
每次 API 请求及后续探测共用一个 `request_timeout_seconds` 预算；并发请求各自
传入剩余预算，不修改共享 node 配置。结果声明
`readiness_contract=exact_sandbox_and_template_identity_v1`，相应操作名使用
`*_api_to_identity_ready`。不能与旧 `*_api_to_envd_ready` 样本合并计算分位数。
样本另存 `api_seconds`、`identity_seconds` 和 `identity_attempts`；API 成功后身份
确认失败也保留分段及错误。fork 的所有子实例共用原请求截止时间。
DB 新实例保持内部身份确认，同 ID resume 跳过重复探测；不能只比较 API 返回时间
就声称两后端具有相同身份就绪保证。

### 可选阶段计时分析

在性能输入中设置 `"phase_timing_versions": {"dragonball": "db-overlap-v1",
"firecracker": "fc-v1"}`，可为 cold/restore/resume 保存 `phase-timings.json`。
这会启用相关 debug 日志；诊断组两端均需启用，不能与未启用的样本直接合并。
版本必须对应冻结实现：`db-serial-v1` 用于 runner 与磁盘串行准备的旧候选，
`db-overlap-v1` 用于 `7e8bc3a` 后 runner 提前启动的候选；`fc-v1` 对应当前 FC
阶段边界。标签由实验输入显式声明，工具不会从二进制自动推断其计时语义。

采集器在每批请求前记录日志位置，在请求结束后按 sandbox ID 匹配该窗口中的
计时；操作 ID 包含 run、repeat、操作名和请求序号。同 ID 多次 resume 不依赖
全局日志顺序。保留日志区间和摘要；缺失、重复、日志截断/替换、后端矛盾及
非单调边界都会使取证失败，已执行的请求耗时仍保留。fork/capture 暂无此阶段分析。

```bash
python3 scripts/tests/dragonball_m4_phases.py /path/to/phase-timings.json --output /path/to/phase-summary.json
```

分析器对照 `expected_operations` 检查完整性，输出微秒均值/中位数；同一后端的
不同计时版本需分别分析。DB runner 重叠版本记录 spawn 加剩余等待，FC preparation
total 已包含 release；使用明确总边界，不将准备子阶段相加。CLI 不替代完整 cell
的输入冻结、操作成功和清理验收，也不产生生产 SLO 资格。

可选实验观察通过 `run_cell(..., observe_batch=...)` 接入；采样准备与取证在 API
计时外执行。取证失败保留已执行时间和原 guest 错误，另记 `proof_error`，该 cell
不可作为有效性能对照。`dragonball_m4_observe.py` 提供主机接收时间的 guest 心跳
间断、S3 夹具 TCP 9000 端口的 IP 字节计数和操作期间内存采样。心跳间断包含调度与
网络因素，不是精确 VMM 暂停；链路计数包含协议头和重传，不是对象净载荷；采样
内存峰值只是实际峰值的下界。具体实验需保留探针原始数据及这些观测边界。
心跳链路必须先在真实 guest 中验证；默认出站策略拒绝私网目标，不能假定 guest
可向 Lab 桥地址发包。本轮接收器绑定源 VMM 网络命名空间的 tap 网关，不更改产品
防火墙；capture 后先停止源探针并关闭接收器，再执行 restore/fork，每次 capture
使用新 token。恢复快照会包含探针进程，其开销仍属于恢复后观测的一部分。

`network_cache_slots` 可显式取 0/1，默认 1 兼容旧实验。`pool.network.enabled=false`
只关闭后台维护，不能据此宣称回收的网络资源不会复用；0 才禁止该回收缓存。
不同缓存策略的样本必须分别统计。

## 证据归档与取回

先用现有 retention 入口归档并核验成功 cell；它仍独占 payload 回收职责。
`dragonball_m4_evidence.py` 只打包明确列出的普通证据文件和当前结果摘要，不扫描或
删除 VM 数据。`--files` 是相对 `--root` 的 JSON 路径列表，`--summary` 应由本轮
已验证回执生成；工具保证摘要字节可追溯，不替调用方判断任务是否完成。

```bash
python3 scripts/tests/dragonball_m4_evidence.py pack --root /var/tmp/run \
  --files /var/tmp/run-files.json --summary /var/tmp/completion.json --output /var/tmp/run-bundle
python3 scripts/tests/dragonball_m4_evidence.py fetch --host root@TEST_HOST \
  --remote /var/tmp/run-bundle --output /tmp/run-evidence
# 需要原始日志时，再向同一输出目录取回完整包。
python3 scripts/tests/dragonball_m4_evidence.py fetch --host root@TEST_HOST \
  --remote /var/tmp/run-bundle --output /tmp/run-evidence --full
```

默认只取回 `receipt.json` 和已核验的 `summary.json`；完整包只有在整体摘要和
所有成员摘要通过后，才从 `.partial` 改为正式文件。无自动解压；链接、重复或
未知成员被拒绝，源普通硬链接保存为独立文件成员。SSH 使用既有认证，单次传输
总超时为 600 秒。中断/损坏文件保留为 `.partial`，重试使用新目录；既有输出
绑定原回执，不自动追踪远端新一轮结果。完整包和小摘要的校验状态分别报告。
