# Dragonball M0：构建资格验证记录

日期：2026-09-30。

状态：**M0 专项与 workspace lint 已通过；追加授权后的原 Firecracker profile 全量 `make test`、`make test-unit` 也已通过。** 一次 M0 全分支独立审查发现的三项 Important 问题已修复并经失败→通过的回归验证。runner、协议、进程保护、KVM 初始化、真实 guest 冷启动/认证/metadata 隔离及受控 checkpoint/restore 已通过。原 Firecracker kernel 不兼容 Dragonball 启动描述；通过的是独立 M0 kernel profile，tools 未改。早期 sudo 阻塞及后续真实失败/修复均保留在下面的记录中。默认后端仍为 Firecracker。

设计见[已确认设计](./dragonball-hypervisor-design.md)，执行范围见[M0 计划](./dragonball-m0-implementation-plan.md)。实施工作区为 `.worktrees/dragonball-m0`，功能分支为 `feat/dragonball-m0`，隔离起点 `c748eb45c71eb664cfe75318a430c4848f265674`。尚未合并或切换生产后端。

## 环境与隔离

- 测试机：`147.139.168.159`，Linux x86_64，内核 `7.0.0-28-generic`。
- CPU：Intel Xeon Platinum 8269CY，104 logical CPUs，family 6/model 85/stepping 7，VT-x。guest 资格用例为 1 vCPU、512 MiB、`host-default`，恢复在同一主机进行；不宣称跨 CPU/宿主迁移兼容。
- SSH root 用于检查、同步、host 准备及后来经用户授权的旧部署卸载；Cargo 构建及 Rust 测试使用现有 `aenv` 用户，UID `999`。capability runner 的 root-UID 行为验证按其契约显式选择 UID 0，仅保留两个指定 capabilities，不以 root 构建。
- Rust `1.98.0` 工具链复制到独立目录，不更改服务器已装工具链。
- 源码：`/tmp/agentenv-dragonball-m0-20260930/src`。
- Cargo 缓存：`/tmp/agentenv-dragonball-m0-20260930/cargo`；仅复制 registry/git 缓存，不复制凭证。
- 独立 `state`、`deps` 目录；VM 测试额外进入 disposable network/mount namespaces，并重新挂载只读 sysfs，避免 NetworkManager 的 host rules/interface 扫描使用生产网络。
- `/dev/kvm`、`/dev/ublk-control` 存在。aenv 属于 kvm 组。
- 初次 M0 资格验证前后，现有 ublk daemon PID `391112`、AgentENV Firecracker PID `391302`/`391304` 保持运行，测试不使用其运行状态；之后用户要求卸载旧部署，见下方追加记录。

## 已执行检查

| 检查 | 环境 | 结果 |
| --- | --- | --- |
| `cargo test -p agentenv-dragonball-runner --test identity_cli --offline`，初始 scaffold | macOS，无 KVM | 预期失败：输出 `{}`，revision 断言失败，0/1 通过 |
| `cargo test -p agentenv-dragonball-runner --locked --offline`，身份实现后 | macOS，无 KVM | 1 个 CLI 集成测试通过；无 VMM 编译/运行证据 |
| `cargo test -p warm-pool --locked` | Linux，aenv | 基线 13 个单元测试通过 |
| 最小 profile 的 runner identity test 构建 | Linux，aenv | Dragonball 编译失败，16 个错误；测试未运行 |
| 修复后固定 fork 的 runner suite | Linux，aenv | 退出码 0，CLI 身份测试 1/1 通过，实际使用 git dependency，无本地 patch override |
| fork 独立最小 profile consumer 的 `cargo check` | Linux，aenv | 退出码 0，不启用 hotplug/PCI/upcall |
| fork 独立 consumer 原有 hotplug/PCI/upcall 组合 | Linux，aenv | 退出码 0，检查既有构建组合未破坏；不是生产 profile |
| 临时扩大编译 features 的 `cargo check` | Linux，aenv | 退出码 0；仅用于定位 feature gating，非批准 profile 的验收 |
| `cargo fmt --all -- --check` | macOS | 退出码 0 |
| `cargo clippy -p agentenv-dragonball-runner --all-targets --offline -- -D warnings` | macOS | 退出码 0；不含 Linux VMM 检查 |
| `git diff --check` | 本地隔离工作区 | 退出码 0 |
| 协议 happy-path scaffold、边界检查实现前测试 | macOS | 分别预期失败；边界测试初始 4/10 通过，拒绝行为尚未实现 |
| `cargo test -p agentenv-vmm-protocol -p agentenv-dragonball-runner --locked` | macOS、Linux aenv | 协议 12/12、身份 1/1 通过；没有启动 VMM |
| `cargo clippy -p agentenv-vmm-protocol -p agentenv-dragonball-runner --all-targets --locked --offline -- -D warnings` | macOS | 退出码 0；Linux adapter 尚未实现 |
| runner 状态机 scaffold | macOS | 预期失败，0/9 通过；实现后 10/10，包括部分 worker 启动失败清理 seam |
| 父进程保护 scaffold / 实现后 | Linux，aenv | 预期失败 0/2；实现后 2/2，子进程由 subreaper 实际回收；2 个 ignored helper 由测试显式调用 |
| seccomp scaffold / 实现后 | Linux，aenv | 预期失败，实际 mode 为 0；实现后 1/1，继承到线程，禁止 mount 触发整个子进程 SIGSYS；helper 被显式调用 |
| `cargo test -p agentenv-dragonball-runner --test runner_cli -- --ignored` | Linux，aenv，无额外 capability | 2/2；真实 KVM 初始化、UDS、实例拒绝、超限帧拒绝、Stop 和缺失 kernel 清理；**没有 guest boot** |
| 清除 supplementary groups 的 CLI 启动 | Linux，UID 999，无 KVM 权限 | 预期退出 1，明确 `/dev/kvm required` / Permission denied；socket 已删除 |
| targeted runner/protocol clippy `--all-targets --locked -- -D warnings` | Linux，aenv | 退出码 0；遗留 proc-macro-error2 future warning 不属于新 lint 错误 |
| 原有 Firecracker MMDS tests，值迁移前 | Linux，aenv | 6/6 通过；迁移到共用值后同样的 6 项断言及新 hash/保留字段测试 7/7 通过 |
| metadata 初始 scaffold / HTTP / 隔离规则 scaffold | Linux，aenv | 分别按预期失败：token 未实现、namespace 引用提前释放、HTTP 返回 501、缺少 TAP-only/DNAT 规则 |
| `cargo test -p agentenv --lib metadata --locked` | Linux，aenv | 最终 38/38 通过（含其他已有 metadata 命名测试）；真实本地 TCP、过期/替换 token、畸形请求、1 MiB 限制和清理；**非 guest/netns 验证** |
| 滴流连接清理回归，修复前 / 后 | Linux，aenv | 预期失败 0/1，逐次 read 超时被重置，耗时 740 ms；改为整个连接固定截止时间后通过并纳入 38 项测试 |
| 根 package clippy `--lib --tests --locked -- -D warnings` | Linux，aenv | 未通过：新 listener/slot helper 尚无任务 5 调用，产生 dead_code；未加 lint suppression，接入后须重跑 |
| M0 profile/进程/取消回归 | Linux，aenv | profile、强制退出、取消保留 lease、父线程存活 4/4；短 launcher 的父线程死亡竞态先复现失败再修复 |
| 已有 Firecracker drive boot-argument 回归 | Linux，aenv | 2/2；共享 helper 保持 custom mount/subPath 语义 |
| 实际 M0 guest suite | Linux，UID 999 + capability runner | 4/4：命令/挂载、正确身份/错误 token、deny-all metadata、邻居实例 token 拒绝；真实 runner 运行时所有线程无 capabilities，NNP=1，seccomp=2 |
| `make test-dragonball-m0` | Linux，aenv | 退出 2：构建/单测/资产准备完成，最终 capability runner 被 sudo 策略拒绝；不修改 sudoers。通过已授权 SSH root 单独委派同一 runner 到已编译 test executable，实际测试仍 UID 999，4/4 |
| checkpoint 协议/验证/路径/I/O 预检单测 | macOS、Linux aenv | 5/5；Running Save、重复 Restore、损坏 state/region/vCPU/profile、短 memory、逃逸/覆盖输出被拒绝，预检 I/O 错误后 Status 保持原 phase |
| 真实 M0 guest suite，加入 checkpoint 后 | Linux，UID 999 + capability runner | 5/5；新 runner 恢复原 PID 及 RAM 标记，tools digest 一致，metadata 重新绑定，源 memory SHA256 不变，最后 artifact drop 后 backing/work lease 释放 |
| `make fmt clippy` | Linux，aenv | 退出 0；workspace、all-targets、all-features；保留上游 proc-macro-error2 future warning，无 lint suppression |
| `make test-unit` | Linux，aenv | 退出 2；新增 runner/protocol、AgentENV 959/959（4 ignored）、linux-cap 3/3、aenv CLI 110/110 通过；ignored/capability 阶段 sudo 被拒绝，后续 ublk/安装脚本阶段未运行 |
| `make test` | Linux，aenv，原 FC kernel 配置 | 退出 2；setup-only/build 完成，首个 capability test runner 被 sudo 拒绝；envd/storage 后续 targets 未运行，不能称全套通过 |
| 既有 `fc::microvm_lifecycle_and_snapshot_preserve_disk_state` | Linux，UID 999 + 相同隔离 wrapper，原 FC kernel | 首轮 manifest fetch 无网络失败，0/1；显式复用已真实解析的 materialized rootfs 后 1/1：写盘、pause/resume、stop、新 sandbox restore 保留数据；不是 registry 网络回归 |
| 既有 `fc::` 本地快照/恢复回归 | 同上 | 5/5；包括 lifecycle、memory 格式、encoded pause state、独立 child disk 和 parent snapshot handle 释放后的链式恢复。明确排除互联网 egress 与 OCI tools registry 用例，未把排除项记为通过 |
| 任务 7 再跑 `make test-dragonball-m0` | Linux，aenv | 退出 2；runner/protocol、probe 单测、资产准备、integration 构建成功，最终 sudo 委派被拒绝；采用相同 wrapper 的 SSH 委派后再单独验证实际 5 个 VM 用例 |
| 审查：启动分配期间取消，修复前 / 后 | Linux，aenv | rootfs 分配 await、已取得 rootfs/第一块 extra drive 后的下一次分配 await：0/2 失败→2/2 通过；实际使用的 startup ownership seam 保留 work 与既有 leases，完成登记后释放全部 fake leases |
| 审查：无法确认 runner 退出，修复前 / 后 | Linux，aenv | 注入 blocking reap 失败：0/1 失败→1/1 通过；仍存活的 runner 后面共享 backing 与 restore directory 全部保留，fixture 随后实际 kill/wait |
| 审查：保存设备状态与 machine stamp 不一致，修复前 / 后 | macOS、Linux aenv | 缺失 block/net 等状态被接受的测试先失败；修复后 checkpoint suite 6/6。校验实际保存的设备 classes、数量、顺序、ID、路径、RO/root 和完整最小设备配置，不只核对外层 stamp |
| 修复后的完整专项重跑 | Linux，aenv，独立 config/state/deps | workspace fmt/clippy 退出 0；runner/protocol suite 通过，KVM CLI 3/3、probe 7/7、metadata 38/38、FC drive args 2/2、原 FC VM 回归 5/5、Dragonball guest 5/5。实际 checkpoint 七种设备篡改均返回 `snapshot_preflight_error`，随后原文件恢复成功；AgentENV 单元测试另跑 962/962，4 ignored |

## 追加：全量 Make 与旧部署卸载

用户确认测试机可授予所需权限后，临时配置 capability runner 的 sudo 委派及 Docker 访问。私有 PATH 使用已安装的经典 sudo，保留 `-E` 环境；没有改变系统 sudo alternatives。构建及 Rust 测试仍为 UID 999。所有运行使用独立配置、`/tmp/aenv-full-20260930/state` 和测试 deps，不读取旧服务的数据。缩短 state 路径是因为首轮 Firecracker socket 长度 113 超出 Unix socket 上限；真实 Firecracker 报错已留存，没有修改 VM 实现绕过。

profile 显式选择原 Firecracker kernel `6.1.175`、tools `0.1.0`、KVM，关闭预热池；不设置 prepared rootfs override，21 个集成用例实际走 ImageResolver、registry 和网络路径。envd 使用独立容器名及端口 49983，MinIO 使用测试容器。**此轮不是默认 tools 0.1.1、Dragonball 生产后端、startup-pack 或完整 OSS E2E 的验收。**

全量执行发现既有 `orchestrator_lifecycle` 在关闭进程全局 NetworkManager 后同进程重建 orchestrator，恢复必然失败。卸载旧服务后该失败仍复现。fixture 修复为：父进程持有目录，旧测试进程完成 pause/shutdown 并退出，新进程读取 persisted paused record 后 resume/delete。两个 VM orchestrator 用例隔离并串行执行；原身份、fork token rotation、paused metadata/proxy、resume timeout/readiness/authentication 和删除断言全部保留，生产 shutdown 语义不变。首次嵌套进程尝试仍持有旧 ublk daemon/RocksDB 锁而失败，最终方案等待旧进程实际退出。

本次独立测试修复审查有一项 Important：`tempdir()` 默认目录权限并非私有，handoff 中的临时 token 可能被其他本地用户读取。新增 `restart_fixture_is_private` 对实际权限验证先失败（`mode & 077 = 45`），显式以 `0700` 创建后通过。测试 token 不进入进程参数，失败断言不打印它；目录由父进程在两个阶段结束后清理。

最终命令（从测试源码根目录，以 aenv 执行）为 `make SHELL=/bin/bash .SHELLFLAGS=-ec fmt clippy test test-unit`，**退出码 0**。严格 shell 选项避免同一 recipe 中后续成功掩盖前序失败；没有修改 Makefile 或降低检查范围。

| 最终检查 | 实际结果 |
| --- | --- |
| `make fmt clippy` | workspace/all-targets/all-features 通过；只有已有的 proc-macro-error2 future warning |
| `make test` 的 AgentENV 阶段 | lib 962、VM integration 21、orchestrator 3（含权限回归）、doc tests 4 通过；4 个 capability ignored 单测随后显式运行并通过 |
| `make test` 的 envd/storage 阶段 | envd 4；OverlayBD lib 383、bin 1、integration 18；ublk lib 18、integration 4；daemon lib 49、integration 37；MinIO 7 均通过 |
| `make test-unit` | runner/protocol、AgentENV 962、linux-cap 3、CLI 110、capability 4、ublk 18、daemon 49 及三项 capability/安装脚本验证全部通过 |

普通 `make test` 的 6 个 integration ignored（5 个 Dragonball M0 用例及 startup-pack）、OverlayBD 的 3 个 ignored 及 1 个 doc ignored 仍保留，不计为此轮通过；M0 五个真实 VM 专项的此前证据仍见上表。没有重跑 `make test-agent-integration` 的独立 OSS E2E，也未把整个 Rust workspace 的每个 crate 都算作聚合目标覆盖。

审查中另外考虑但未列为问题的五项，执行者逐项结论如下：

- daemon 退出可能晚于旧进程：接受非阻塞判定，既有 parent pidfd 和 socket-available 等待覆盖该过渡，实际顺序重启通过。
- 超时不等待所有后代同步退出：本 fixture 保留 `kill_on_drop` 和非零结果传播，daemon 监视父进程；额外进程树清理不扩展到生产代码，不把失败路径记作成功清理。
- 人为注入内部 marker 环境变量：正常 Make 不设置它们，非受支持调用方式，不新增生产接口。
- 未来重命名可能令 exact filter 找不到测试：当前名称与实际模块匹配，日志显示三个断言用例运行；lifecycle 后半段还必须读取前半段 handoff，不接受当前存在跳过。
- managed seed 持久化：保持原固定测试 seed，未新增或声称该项覆盖。

用户要求卸载的旧 `aenv.service` 已停止、禁用并移走，旧 server、daemon 及其两台 VM 均退出。安装的 server/CLI/daemon、service、env、sysctl/modules-load 和 server config 备份到 root-only `/var/tmp/agentenv-uninstall-20260930.6clQzq`；`/var/lib/aenv` 的持久数据、aenv 账户、其他 Kata 实例、SSH 和 Docker 保留。安装验证只操作 mock 和临时路径，没有重新安装服务。最终确认 service `not-found/inactive/MainPID=0`，临时 sudo 委派和 Docker 组权限撤回。

完整日志与隔离运行 helper/config 位于测试机 `/tmp/agentenv-dragonball-m0-20260930/full-suite/`；最终日志为 `evidence/all-checks-private-final.log`，原失败和权限 RED/GREEN 证据同时保留。后续构建仍需重新授权 capability/Docker 委派，不保留 root 访问策略供普通用户长期使用。

## 最终审查与收尾边界

独立 reviewer 对完整分支审查一次：无 Critical，三项 Important 全部在一次修复阶段中完成 TDD 和完整专项重跑；未发起第二次审查。启动任务在 caller 取消后继续持有分配 owner，包括顺序 extra-drive 准备；退出无法确认时不释放共享 backing/恢复目录；恢复检查实际保存的 block/virtio-net state，拒绝缺失或替换设备后才进入 upstream typed restore。

两项 Minor 留待后续：非变更性 checkpoint 预检失败保留目录，同路径重试会得到 AlreadyExists；未加入真实 physical-extra guest fixture，`/dev/vdc` 证据仅是预留 placeholder。未把这些留项记为通过；早期聚合阻塞已由上面的追加授权和完整实跑解除。保留功能分支和隔离 worktree，不合并、不开 AgentENV PR、不切换默认后端。M1–M5 仍是生产替换的后续工作。

## 受控 checkpoint 的边界

fixture 在根文件系统 `fsfreeze -f /` 成功、全部 host 控制流结束后 Pause；阻塞进程使用 `/run` tmpfs FIFO。保存前观察 ublk 在途请求为零，完整 block counters 连续三个 100ms 采样不变。CPU `kvm_regs` 的字节数组中 RIP 非零，恢复后原 PID 继续完成随机 RAM 标记。这不是通用 block/net worker drain 的证明。

任务 7 实测 UTC 时间：source `db-m0-AnILOk` Start 请求 `09:12:12.783998`、guest ready `09:12:13.347851`、Stop 请求 `09:12:16.899411`、reap/listener/device/slot 清理完成 `09:12:17.013452`；新 runner `db-m0-MFEQZF` Restore 请求 `09:12:17.035135`、guest ready `09:12:17.199763`、Stop 请求 `09:12:17.238090`、清理完成 `09:12:17.270011`。源 memory SHA256 `b3e89b10b4466043a0a3982e76b33ed441eda55c6fd729bba0c539e7a7aadbed` 恢复运行前后相同；这组单次观测不是性能门槛。

仅接受单个低于 3 GiB MMIO hole 的 RAM region、固定 revision/profile、相同 machine/设备配置和普通全量 memory 文件。输出不覆盖文件、不得逃出实例工作根；保存及恢复持有已打开 FD，避免上游按名字重新打开时被重定向。输出预检/恢复输入验证失败不改变 phase；任何 VMM action 后的错误仍为终止性错误，必须 teardown。

源 probe、artifact 和恢复 probe 持有原 frozen ublk backing 的共享 lease。测试先停止源 runner、释放源 slot，再顺序重新取得同一 IP 布局；这不是独立可写 rootfs clone，不支持生产 fork。artifact 的最后 owner 释放后等待 daemon 释放 backing，再删除 work。失败测试的 runtime 退出可能故意保留工作目录和 lease，避免取消异步 release 后误删 backing；未把这些失败尝试记为 cleanup 成功。生产 capture barrier、held restore、设备重绑定/slot patch、压缩和 repository 发布均未实现。

## Guest 输入与实测限制

- 原 kernel `6.1.175` SHA256 `cd7c5a06a2026a2d8a0cddf60b41a5d76a6ac025e54439eb3ee5c9e57a60c67b`：真实启动后无 vda，root mount error -6/panic，测试失败并完成 rollback。其 ELF 无 cmdline-MMIO 注册符号；固定 Firecracker reference config 关闭 `VIRTIO_MMIO_CMDLINE_DEVICES`、`X86_MPPARSE`，而 Dragonball 使用 cmdline MMIO/MP tables。
- 独立 `6.1.175-dragonball-m0.1`：上述两项开启，`LOCALVERSION` 独立命名；其余从固定 Firecracker config 生成。构建脚本 `scripts/tests/build-dragonball-m0-kernel.sh`，非 root，拒绝覆盖已有 kernel。产物 SHA256 `5456ed3da1101862c8da3c49a92b0f6ea380aad0dd1bc22d42cdce0f7c5e3945`，config SHA256 `10f99d7bf4b5b08e00a277f5022d488d2e7f78c26afdc059f912f7d0ff676e6d`。未发布 release，未更新默认 deps manifest。
- 原 tools `0.1.0/tools.ext4` SHA256 `60ba806a44975c7b2ca382f680de9da87af22ac65e288a683a921d1829e1c03d`；envd revision `9c3b7c5dbd181ba819084276b8228f70936e911e`，未改 guest。没有声称默认 tools 0.1.1 已验证。
- ImageResolver 输入 `ghcr.io/linuxserver/baseimage-ubuntu:noble`；解析 manifest `sha256:fcd345bc4a42d9ea323a9b226fc3b566dd814df3b6466b6b1cef1a2fc91e7162`，独立 cache 中 materialized lower，原始 imageConfigs 数组逐值验证。VM 测试 namespace 无外网。
- 命令证据：`/dev/vdb` 用户 rootfs、`/agentenv` 挂载 `ro,relatime`、`DRAGONBALL_GUEST_OK`；预留 volume slot `/dev/vdc` 存在。尚未验证真实 physical-extra guest、生产 slot patch/hotplug 或 managed volumes。
- envd `/init` 异步启动 MMDS identity poll；harness 在实际 process 环境变量匹配前不返回句柄。错误 token 的 process 调用失败，独立 HTTP `/envs` 返回 401；HTTP middleware 的 JSON 拒绝被 tonic 表示为 Internal，未假定它一定为 gRPC Unauthenticated。
- `guest-first.log` 保留原 kernel 失败。最新通过日志及 `state/run/dragonball-m0/evidence/*/{serial.log,runner-error.log,runner-security.txt,metadata-requests.json}` 保留新 VM 证据，HTTP token 已脱敏，不保存响应正文。
- 普通 stop/rollback：先 reap runner，再 stop metadata，再释放 ublk/network；成功 fixture 的 work 已释放、netns 目录为空，失败尝试的 fail-closed work 保留已在上文单独说明。初次资格验证期间生产 PIDs `390970/391112/391302/391304` 保持运行，前后去时间戳/计数器的 host iptables SHA256 均为 `7edefca24d9fb111b7175259015b2e5e2db2ea8c917eb1916c4b1f1a0e471368`；后来按用户要求卸载，不将该历史摘要作为当前 host 状态。

复现：先在 namespace 外以普通用户执行 `server --setup-only` 和 `cargo run --example dragonball_m0_assets -- <runner> <assets.json>`；配置显式选择独立 kernel、原 tools 和隔离目录。使用 `make test-dragonball-m0 AENV_TEST_STATE_DIR=<state> AENV_TEST_DEPS_PATH=<deps>`，账户须有 capability wrapper 的 sudo 委派。初次 M0 验证时无该委派，SSH root 启动 `AENV_RUN_USER=aenv scripts/tests/run-dragonball-m0.sh <integration-executable> dragonball_m0 --ignored --test-threads=1 --nocapture`，Cargo 构建仍以 aenv 运行；本轮全量 Make 不替代该专项 namespace/profile。

首次 Linux 基线的 `--offline` 检查无法取得仓库原有 `libublk-rs-sys` git 依赖；联网补齐独立缓存后重跑 warm-pool 成功。没有把首次未运行的测试记为通过。

## 固定依赖与 lockfile

上游审查基线为 `cd7051a8dbc0f8e0ac5a63ee4c3884fe8b4ac894`。实际完整 Kata git dependency 现固定为用户 fork [`bfc470b07aedaeddaf801ff091f5cdb1a12c69bc`](https://github.com/denverdino/kata-containers/commit/bfc470b07aedaeddaf801ff091f5cdb1a12c69bc)，仅含针对该基线的最小 profile 修复和构建回归。`default-features = false`，仅启用 `virtio-blk`、`virtio-net`；root package 不依赖 Dragonball，只有独立 runner 依赖它；Dragonball 编译仅适用于 Linux。

Cargo 首次解析暴露 nix 版本约束冲突。运行 `cargo update -p nix@0.31.2 --precise 0.31.3` 后可解析；直接相关 lock 更新为 nix `0.31.3`、libc `0.2.189`，并新增固定 Dragonball 的依赖闭包，没有全量升级其他包。后续仍需 AgentENV 的 Linux 回归；warm-pool 通过不等于整个 workspace 通过。

## 上游编译阻塞及根因

最小 profile 可复现命令，在上述源码目录中以 aenv 运行：

```sh
cargo test -p agentenv-dragonball-runner --test identity_cli --locked
```

主要错误位置均为固定提交的 `src/dragonball` 下的文件：

| 源码位置 | 错误与根因 |
| --- | --- |
| `src/device_manager/mod.rs:28` | 无条件导入可选 `dbs_pci` 的 `CAPABILITY_BAR_SIZE` |
| `src/device_manager/blk_dev_mgr.rs:23–24` | 无条件导入可选 PCI/upcall 类型 |
| `src/api/v1/vmm_action.rs:833–841` | block removal 路径引用只在 host-device 下定义的 `HostDeviceConfig` / `VfioDeviceError` |
| `src/device_manager/blk_dev_mgr.rs:424–645` | runtime 分支仍引用未编译的 PCI/hotplug 方法；`cfg!` 不是条件编译，不能移除需类型检查的代码 |
| `src/device_manager/net_dev_mgr.rs:456` | 无 hotplug profile 下仍调用条件定义的 `insert_hotplug_mmio_device` |
| `src/device_manager/mod.rs:1289` | MMIO 清理调用只在另一特性条件下定义的通用 deregister 方法 |

作为对照，仅改变编译 features，运行：

```sh
cargo check -p agentenv-dragonball-runner \
  --features dragonball/hotplug,dragonball/host-device,dragonball/dbs-upcall
```

退出码 0，原 16 个错误消失；输出另有 `proc-macro-error2 v2.0.1` 的 future-incompatibility 警告。此命令未启动 guest，未验证安全 profile、snapshot、envd 或存储；这些额外 features **没有加入 runner manifest**，不能以对照通过宣称任务 1 完成。

完整远程证据保存在测试根目录的 `runner-build.log`、`feature-diagnostic.log`、`baseline.log`、`dependency-tree.log`。

## 修复与后续验证

已在用户提供的完整 Kata fork 中补齐上述 feature guards，修正 block removal 的错误类别和 MMIO 清理调用；不支持 PCI/hotplug 时明确拒绝，不以启用 hotplug/VFIO 绕过。fork 增加独立 `minimal-profile-consumer` 及相应 GitHub workflow，同时检查最小与原有扩展组合。第一次扩展组合回归捕获了 cfg 转换导致的 callback 移动错误，修正 MMIO 分支的控制流后重新通过。

补丁已推送到 `denverdino/kata-containers` 的独立 `agentenv/dragonball-m0` 分支，远端读回 SHA 与本地一致；首次推送的 GitHub 500 经普通重试解除。没有改动 fork 默认分支，没有新建仓库或 PR。AgentENV runner manifest 和身份测试固定到该真实 SHA，Linux 已从公开 git dependency 重建并通过，不使用临时源替换。

协议测试覆盖长度前缀、1 MiB 边界、读取前超限检查、截断、版本、预算、实例/请求归属、响应及尾随字节。远程证据为 `protocol-suite.log`。这些测试不访问 KVM，不构成设备运行证据。

runner 使用 channel/eventfd 调用固定 Dragonball API，并在停止时等待 master thread。工作目录要求 runtime 用户所有且为 `0700`，UDS 为 `0600`，读取消息前核对 peer UID；请求 ID 严格递增以有界地拒绝重放。父进程登记前后核对 PPID，并安装 `SIGKILL` death signal。过滤器在创建 VMM/worker 前安装，所有后续线程继承，包含 RawBlock 的 native AIO/io_uring，但不加入 mount/setns/exec/vhost/VFIO。任务 6 后 Save 仅允许 Paused，Restore 仅允许新 Configured；产物有 `probe_only` 身份，不能当作生产 capture。节点 kill+wait、调用者取消与 lease 归还顺序已有专项测试，仍不能仅凭停止回复释放 host 资源。证据见 `parent-red.log`、`seccomp-red.log`、`runner-service.log`、`runner-cli.log`、`runner-clippy.log`、`no-kvm.log` 及 checkpoint suite。

metadata 的值由 `SandboxMetadataDocument` 统一拥有，Firecracker 保留原内部名的 re-export，不改 JSON。listener 的 token authority 为临时 per-runtime HMAC/nonce/expiry；M0 恢复新建 listener/authority，in-place replace/旋转语义仅保留契约测试，待 M1 有消费者后接入。既有 MMDS v2 路径、header/别名、TTL `1..21600` 和 JSON/plain-text 读取按固定 FC 源码核对。M0 slot 才 opt-in 配置 metadata 地址和 TAP-only INPUT，guest `80` DNAT 到私有 `15001`，优先于 egress proxy，未增加 capability，也未更改普通 FC slot 初始化。连接读写共享 250 ms 截止时间，stop/Drop 实际 join worker 后才释放 namespace FD。任务 5 已实测正常/deny-all/cross-instance 路由和 token 隔离。

服务器缺少 protoc；在独立 `protobuf` 目录解包 Ubuntu `3.21.12-15ubuntu1` compiler/runtime/dev packages，没有系统安装。首次 include 设置失败后补齐匹配 schemas，再运行了真实失败/通过测试。记录见 `mmds-baseline.log`、`metadata-{red,value-green,state-green,http-red,policy-red,drip-red,green,clippy}.log`。

只读检查的原资产：tools `0.1.0/tools.ext4` SHA256 `60ba806a44975c7b2ca382f680de9da87af22ac65e288a683a921d1829e1c03d`；kernel `vmlinux-6.1.175/vmlinux.bin` SHA256 `cd7c5a06a2026a2d8a0cddf60b41a5d76a6ac025e54439eb3ee5c9e57a60c67b`。tools 的 `/init` 确认 vda/vdb 约定；envd ELF buildinfo 给出 infra revision `9c3b7c5dbd181ba819084276b8228f70936e911e`、Go `1.25.9`。任务 5 已录制并逐字段核对新 guest 请求的 method/path/契约 headers，token 脱敏，不保存响应正文。[固定 envd 请求源码](https://github.com/e2b-dev/infra/blob/9c3b7c5dbd181ba819084276b8228f70936e911e/packages/envd/internal/host/mmds.go)及[固定 FC 契约](https://github.com/kvcache-ai/firecracker/tree/9fbc98e4dc05d03e1f00902ba4bb796395465099/src/vmm/src/mmds)是兼容实现依据。

以下仍未测/未实现：`process_vm_readv`、held restore/worker drain、ublk memory backing、dirty/zero diff、balloon、生产 volume slot 变更/managed volumes、跨主机恢复、PVM/aarch64、OSS/扩展/P2P 的 Dragonball 生命周期。原 Firecracker profile 的 envd/storage/capability 聚合验证已在追加记录中完成，不等于这些生产 Dragonball 契约已验收。M0 不能跳到默认后端切换。

## CI 与 M1/M2 的具体输入

`make test-vmm-unit` 已纳入 `make test-unit` 的先决步骤，CI unit job 因此必跑协议及 runner 的非 KVM 测试；不是仅编译新 crates。integration workflow 的 `dragonball_m0` boolean input 默认 false，只在人工选择时派到 `self-hosted/linux/x64/dragonball-m0` runner，PR 不运行该 privileged job。Runner 必须预配置独立 kernel/tools/deps/config 和 capability 委派，显式请求但缺 prerequisites 时失败；此 job 未在 GitHub 实际触发，不宣称远端 CI 已通过。

隔离 FC 回归通过 test-only `AENV_TEST_ROOTFS_IMAGE_CONFIG=<已 materialized 的 image.json>` 复用准备好的镜像；不设置此变量的原有 common::setup 保持实际 ImageResolver 路径。原 FC kernel、binary、默认配置均未替换；没有 Go 修改或 Go 验证声明。

下列位置均基于 fork `bfc470b0` 和本分支实际调用，不以文件恢复成功推断后续能力：

| 后续项 | 已有源码/接口 | 必须增加的契约与验证 |
| --- | --- | --- |
| M1 capture barrier | Kata `src/dragonball/src/vm/mod.rs::{save_microvm,save_microvm_paused}`，block `crates/dbs_virtio_devices/src/block/{handler,device}.rs`，`src/device_manager/net_dev_mgr.rs` | vCPU Pause 之外冻结/排空 worker、AIO/io_uring、virtqueue 和网络 completion；给出可观测 generation/ack、超时和失败回滚，活跃 I/O 压测不能由 fsfreeze fixture 替代 |
| M1 held restore | Kata `vm/mod.rs::start_microvm_from_snapshot`、`restore_microvm`，AgentENV runner `vmm.rs::restore_quiesced` | 上游当前自动放行 vCPU；拆出 configure/map/restore-held、完整 rebinding、显式 resume。metadata/网络/盘均准备成功前不可运行；失败实例销毁 |
| M1 memory export | Kata `address_space_manager.rs` 的 RAM ranges，runner `vmm.rs`，节点 `client.rs`/`probe.rs` | 暂无受信任的 RAM 地址导出或 process_vm_readv 使用；加入 generation/region bounds 与 runner lifetime pin，再测试权限拒绝、短读、PID 退出、无恢复点发布 |
| M1 storage/persistence | 节点 `QuiescedProbeArtifacts`/`ProbeBacking`，`src/sandbox/ublk/`，`src/snapshot/` | 当前仅共享 frozen backing、顺序复用同一 slot；必须独立 seal/restack/fork 用户盘、附加盘及 volumes，发布逻辑 layer 引用，接入 paused store，失败回滚不得破坏源 |
| M2 memory backing | Kata `address_space_manager.rs` 中 MAP_PRIVATE restore；runner `probe_snapshot.rs::validate_snapshot` | 当前只接收普通文件、单低 RAM region；扩展 block-size/region alignment/offset/overflow 检查，并验证 ublk mmap/read/write/COW/引用计数，不能用 fstat 长度代替 block 容量 |
| M2 dirty/zero/balloon | Kata `src/device_manager/balloon_dev_mgr.rs`、snapshot state、RAM dirty tracking | M0 profile 没开 balloon；累计 dirty/zero 与 balloon persist/restore/COW 需要完整 device/state 协议和 full/diff 对照，不能直接开启 feature 后宣称完成 |
| M2 slot/recording | `src/device_manager/blk_dev_mgr.rs`，AgentENV `extra_drive.rs` 及 volume manager | 稳定 drive IDs 已使用，但槽内容 patch、snapshot 后替换和 volume ownership 尚未验证；在 worker barrier 后恢复并记录启动读页，不改现有所有权语义 |

`process_vm_readv` 不是仅凭同 UID 就可资格通过：需要核对实际 ptrace access 检查、dumpable、父子关系/namespace、Yama/其他 LSM 以及调用方 seccomp；读取必须处理短读，调用本身不提供一致性。见 [Linux man-pages](https://man7.org/linux/man-pages/man2/process_vm_readv.2.html) 与 [kernel Yama 文档](https://www.kernel.org/doc/html/latest/admin-guide/LSM/Yama.html)。M0 没调用该 syscall，没有修改 host ptrace 策略或增加 CAP_SYS_PTRACE。
