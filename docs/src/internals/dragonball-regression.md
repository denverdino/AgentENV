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
清理检查始终保留；跨入口进度汇总与主机互斥仍待实现。

## 性能候选的构建来源与共享预检

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
共享 Cargo 输出；跨入口主机锁、M4 续跑和构建缓存自动命中策略未在本次实现。
此入口仅为 server 生成回执，runner、daemon、guest 资产仍由既有 M4 输入摘要记录。
