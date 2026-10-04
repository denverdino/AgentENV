# Dragonball M2 工作报告

日期：2026-10-03。**M2 Tasks 1–10 已完成，M3–M5 尚未全部完成。** 本文区分已运行证据与剩余门槛，不把组件装配或单机测试当作发布认证。Firecracker 和 Dragonball 保留为编译时可选组件，默认仍为 Firecracker。

## 环境与资产

- 隔离分支 `feat/dragonball-m0`，本轮起点 `1804bdd4a58c61220e562bfc9fd2d573f2fae21c`；最终运行树的变更代码逐文件 SHA256 见 `task10-final-source.sha256`，已与本地提交内容核对一致。
- Kata fork pin `c77812d669737c11cb16321827acc1c79bf065f3`。
- Linux 测试机 `147.139.168.159`；构建和 VM 测试为 `aenv` UID 999，只有测试所需网络/挂载能力。普通 runner 自身不保留这些能力。
- 主机内核 `7.0.0-28-generic`、4 KiB 页，Intel Xeon Platinum 8269CY（family 6/model 85/stepping 7）。不据此声明其他内核或异构 CPU 已通过。
- 最后 probe 修复后的 runner build ID：`sha256:bebcf7111fea8a3b9d058a5a307c7192b184a1703743ec649ea682e44f06b4a0`；format epoch 2，profile `db-virtio-blk-net-balloon-v2`。
- 日志根 `/var/tmp/agentenv-dragonball-m2-20261002/logs`；测试状态与旧安装分离。大型 RAM 正确性测试使用隔离 tmpfs，不作为磁盘性能结果。
- DAMON 资格内核 `6.1.175-dragonball-m2.1`，SHA256 `d3792b0442c673f774f6c31f58d41231478fe9b8ca5b56c2991f719181c1230b`。这是资格资产，不是已发布依赖；Firecracker 继续使用原内核。

## 本轮已验证

Task 9 的共享 startup-pack 单测在 FC-only 和 DB-only 下各通过 33 项；真实 FC POSIX startup-pack 发布、解析和恢复通过 1 项；DB startup/DAMON 通过 5 项，含专用 recording device、发布取消/关停 drain、坏 manifest/预算控制和实际 guest RAM 范围。证据为本轮 `task9-final-preflight.log`（计划工作目录）及远程既有 `components-*` 日志。

完整 M1 基线重放为 23 通过、9 失败，见 `task10-m1-baseline.log`。其中六项旧故障代理已不符合严格 PID 校验，并仍观察旧导出操作；新测试专用 `reply-fault` example 在真实 VMM 进程中注入 `ExportHeldState` 响应故障。发布 runner 没有新增测试开关，生产 IPC 的 PID/UID 校验保持不变。

暂停记录重载暴露了旧设备计数校验遗漏未绑定卷槽的问题。移除过时的重复检查，继续复用恢复路径的完整拓扑校验后，真实跨进程同 ID 恢复通过 1 项，见 `task10-restart-fixed.log`。

guest 数字用户回归还暴露 `/etc/passwd` 直接追加与镜像启动期 `usermod` 替换 inode 的竞争。可控锁回归先失败，修复后 envd 相关单元测试 15/0，见 `task10-account-lock-{red,green}.log`。修改使用 shadow 兼容 PID 文件锁，限制等待时间且只释放自己获取的锁；不删除其他写者或遗留的锁。完整 M1 重跑 **32/0**，耗时 395.21 秒，见 `task10-m1-current.log`，包含 1/2/8 GiB 活跃 I/O、卷、故障注入和节点重启。后续 example 的额外 peer/成功响应断言仍由完整 Make 入口复验。

## 可重复入口

最终默认 `make fmt clippy test` 与 `make test-unit` 已退出 0，见 `task10-final-default.log`、`task10-final-test-unit.log`；最后 probe 修复后的严格 lint 再次通过，见 `task10-final-probe-clippy.log`。默认 FC 单测 975/0（普通忽略 5 项另行执行为 5/0），CLI 110/0，ublk 18/0，daemon 49/0，MinIO 7/0，shell 验证脚本全部通过。最后 probe 修改只影响独立 Dragonball runner，不改变此默认 FC 产品。

实际 `make test-dragonball-m0` 为 5/0；`make test-dragonball-m1` 最终为 32/0（405.03 秒），`make test-dragonball-m2` 的真实 M1/M2 集成矩阵为 **51/0**（591.96 秒），最终 Make 退出 0，见 `task10-final-m1-m2.log`。该 M2 入口还通过 DB-only 单测 946/0（5 个普通 ignored）、协议/runner 全套和实际 KVM CLI 3/0。

`make test-dragonball-m2 PROFILE=debug` 使用 DB-only 产品、显式 qualified runner/kernel/tools，执行协议/runner、真实 KVM CLI、DB 单测及 M1/M2 ignored 集成测试。M1 lifecycle 使用 FullFileOracle，M2 内存测试显式选择 SharedUblkIncremental。缺硬件、4 KiB 页、资产或 guest tracing 前提会失败，不计为跳过成功。

故障案例需要 `cargo build -p agentenv-dragonball-runner --example reply-fault --locked` 和 `AENV_DRAGONBALL_FAULT_RUNNER`；Make 已负责设置。该 example 不得作为发布 runner 打包。新增 CI 是 protected environment 下的 manual opt-in，不扩大普通 PR 的权限。修复审查问题之前，该入口已完整通过（49/0，`task10-make-m2-third.log`）；不能据此代替修复后的最终重跑。

## 内存生命周期审查修复

一次完整 M2 分支审查发现：runner 被 reap 不代表 KVM APF worker 已释放旧映射；共享 RAM 不应进入可能重新绑定的设备池。Dragonball RAM 现在保留兄弟实例间共享，但使用独立 raw generation；同步 DEL 成功后才释放底层快照，错误或取消时保留 owner。FC/tools/普通磁盘池不变，性能影响留 M4 测量。

- 开启 pool 且高水位足够的真实三子实例 COW/最后 owner 销毁测试：`task10-pool-red4.log` → `task10-pool-green.log`。避免因 pool 溢出碰巧删除设备而假通过。
- runner SIGKILL/reap 后，额外真实 block opener 仍持有内核 generation：`task10-kernel-ref-red.log` → `task10-kernel-ref-green.log`；底层配置直到同步 DEL 完成才消失。
- 删除成功/不确定时的 owner 单测 2/0，`task10-retirement-unit.log`。
- 原生 KVM 在途 APF 捕获/恢复：缺失页由测试专用 userfaultfd 阻塞，真实 guest #PF 产生 token；在尚未解析该缺页时保存 vCPU 状态并恢复到新 VM。保留 APF enable 得到 wake-all，移除 enable 的负对照不产生唤醒，1/0，`task10-apf-pending-second.log`。此机制不进入生产 runner，未更改全局 userfaultfd sysctl。

原生 APF 用例位于 Kata topic 的测试提交 `5b9bf96836`（本地提交，尚未推送），生产代码 pin 仍为上述 `c77812d`。实际 guest 的阻塞 I/O/进程死亡用例单独筛选 `dragonball_apf_runner_death`，需要 `AENV_APF_TRACE_LOG`；`scripts/tests/with-dragonball-apf-trace.sh` 仅由 root 收集独立 trace instance，Cargo/guest 仍由非 root capability runner 执行。它不属于普通 `dragonball_m2` 过滤器，必须显式运行，缺前提直接失败。

实际 guest 用例在最终 runner 上再次通过 1/0（3.02 秒），见 `task10-final-apf-guest.log` 与同名 `.trace`：所有本次 daemon 线程停止后，以单调时钟过滤出本次 vCPU 的真实 not-present APF；保持 I/O 阻塞并 kill/reap runner 时底层快照仍存在，恢复 I/O 后设备和快照最终释放。pidfd guard 确保失败时也恢复本次 daemon，独立 trace instance 已撤除。

三个原生 ignored 场景均显式通过：在途 APF、实际 RO ublk 容量/映射和 8 GiB 冷映射，各 1/0，分别见 `task10-final-kata-{apf,block,cold8g}.log`。冷映射导出约 8.01 秒，只作为本机 deadline 正确性检查，不是 M4 性能达标证据。RO ublk 的完整 lower SHA256 前后相同。

## 中间失败的处置

Kata 最终受影响路径为 device 92/0、VMM scoped 39/0（另有 3 个 ignored 硬件场景显式执行），all-features/all-targets check 和 clippy 均退出 0。Kata clippy 仍有原有 dbs-* 警告，不等同于全树 `-D warnings` 通过；AgentENV workspace 的严格 clippy 已通过。

Kata 原始完整 minimal suite 仍有 9 项已复现的 block/PCI 基线失败，不属于本次 M2 新增失败，也不声明整个 Kata 全套绿色（`task5-root-full.log`，122 passed/9 failed/1 ignored）：

- `api::v1::vmm_action::tests::{test_vmm_action_insert_block_device,test_vmm_action_remove_block_device}`。
- `device_manager::blk_dev_mgr::tests::{test_add_non_root_block_device,test_add_one_root_block_device,test_add_root_block_device_first,test_add_two_root_block_devices_configs,test_block_device_update,test_root_block_device_add_last,test_update_blk_device_ratelimiters}`。

M0 全量恢复额外发现了实际兼容性回归：Kata 的空 balloon manager 序列化为 `{"devices":[]}`，旧 probe 只接受 `null`。真实 M0 4/1 和新增单测均先失败；现在仅接受这两种空表示，仍拒绝非空、缺字段或额外字段的 balloon manager。probe 单测 8/0，实际 M0 重跑通过；未修改 Kata 产品代码或放宽 M0 设备配置范围。

- 一次完整 Make 因资格 tmpfs 空间不足失败两项；保留原始日志，清理当次可重建编译缓存后原样重跑通过 49/0，并未降低 8 GiB/逐字节覆盖或放宽 RPC deadline。
- 一次修复后 Make 为 48/3：执行期间错误地并行重建了 integration 二进制，`/proc/.../exe` 显示 `(deleted)`，三个需要 self-reexec 的用例出现文件不存在。此轮不计为最终通过，后续冻结二进制串行验收。
- 默认 Rust `test-unit` 曾在已启用 NoNewPrivs 的外层运行，最后的 root setup shell verifier 无法再次 sudo。最终入口只让 setup verifier 的编排使用 root，所有 Cargo 和 VM 测试均经 UID999 断言执行，不跳过该 verifier。
- 保留的失败现场占测试状态约 22 GiB，隔离 tmpfs 上限从 40 调整为 56 GiB；没有删除旧部署或失败快照，所有大型 VM 测试串行。
- 双后端 integration 链接还触及编译 tmpfs 的 48 GiB 上限；清理仅本次可重建增量缓存并调整为 52 GiB 后构建通过。随后 M1 为 31/1，唯一失败是 8 GiB 用例要求至少 40 GiB 可用 RAM，而当时约 38 GiB。将约 5.9 GiB 静态编译产物移至磁盘并保留同路径软链接、逐文件校验 SHA 后，可用 RAM 回升至约 44 GiB；没有降低预检阈值。

## 交接与剩余门槛

- M2：全部适用 AgentENV Make 检查、Kata scoped/all-features 检查、一次完整分支审查及其 Important 修复均已闭环；没有遗留的 M2 审查问题。上述历史 Kata 基线失败单列保留。
- 临时 sudoers 与 sudo shim 已移出生效位置、保留撤回备份；`visudo -c` 通过，`aenv` 恢复为仅 aenv/kvm 组。`/dev/userfaultfd` 为 root:root 0600，全局 sysctl 仍为 0，测试 trace instance 已删除。保留本轮源码、日志、测试状态和编译缓存供后续阶段使用；未重新安装 AgentENV server。
- M3：组件装配已有[专项证据](./hypervisor-components-verification.md)，完整 OSS、私有 registry、扩展、P2P、独立发布资产与干净环境契约仍待验收。
- M4：缺第二台兼容 KVM 主机；capture 停顿和峰值资源 SLO 尚待 owner 明确。没有跨节点或性能达标声明。
- M5：保留可选 FC 和 DB，不删除历史 reader/资产。只读盘点确认两项旧服务均 `not-found/inactive/MainPID=0`；`/var/lib/aenv` 约 140 MiB、卸载备份约 105 MiB，均未删除或覆盖。尚未完成恢复依赖逐项审计或双后端干净环境打包验证。
