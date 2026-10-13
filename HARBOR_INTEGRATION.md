# Harbor 集成与评测经验

更新：2026-10-13。范围：AgentENV / Dragonball、Harbor、Terminal-Bench 2.0 与 Linux Computer Use。操作入口见 [scripts/harbor/README.md](scripts/harbor/README.md)。本文区分已验证结果、原因推断与待完成项。

2026-10-11 的过程与效率复盘见[本轮复盘](#harbor-集成过程与研发效率复盘2026-10-11)。
当前工程改进状态见[执行记录](#复盘改进执行2026-10-11)；下文按日期保留历史评测记录，
不作为当前任务正在运行的证明。

## Dragonball 推理期间 pause/resume 最小验证（2026-10-13）

已在现有 Harbor 自定义环境与 `DashScopeTerminus2` 中增加可选
`--pause-taskvm-during-inference`，默认关闭。参考
[harbor-agentenv](https://gitcode.com/ttcoding/harbor-agentenv) 提交
`ee1526f574d2a9488cc5c3f3023c9e2bb6b82ad4` 的宿主机推理边界，包装固定
Harbor 0.24.0 的 LLM `call()`，覆盖主推理与总结路径。Terminus-2 与模型
客户端留在宿主机；仅 tmux/任务所在 VM 暂停，同 ID 恢复并更新访问 token。
取消或暂停结果不确定时仍尝试恢复，模型响应须等恢复完成才交给工具执行。
支持范围和计时口径见 [驱动指南](scripts/harbor/README.md#推理期间-pauseresumedragonball)。

本次只测 **Dragonball**。B 机 `147.139.165.124` 复用既有运行时和合格
OSS 模板，原始 Terminal-Bench 2.0 `log-summary-date-ranges`，4 vCPU/8 GiB、
单并发、`qwen3.8-flash`、一次 model trial、零 trial 自动重试。原生 verifier
得 **1.0**，无 trial 异常；7 次模型调用边界共 21 条生命周期事件全部成功。
真实传输预检验证同 ID、后台进程 PID/启动时间和文件内容在休眠后保留。
全部 sandbox 删除、隔离 Lab 清理及宿主恢复通过；观测到的 VMM 均为
非 root UID 999 的 `aenv-dragonball`。未修改 Rust/Go 运行时、任务、解答或评分器。

| 本轮计时 | 实测 |
| --- | ---: |
| 模型 trial 首次休眠 | 49.999 秒 |
| 后续 6 次休眠 | 0.264–0.460 秒，中位数 0.292 秒 |
| 7 次唤醒 API | 0.348–0.373 秒，中位数 0.368 秒 |
| 7 次模型调用往返合计 | 47.713 秒 |
| 控制器跨度，含预检、模型、verifier 与清理 | 233.180 秒 |

独立预检的休眠/唤醒为 46.809/0.288 秒。上述唤醒行是 API 计时，首条命令
和进程/文件检查另行验证；本轮不提供唤醒至任意应用就绪的分位数。
仅一项任务，不代表全 Terminal-Bench 通过、RL 吞吐提升或生产 SLO。
首轮休眠明显较慢，尚未完成原因归因；不能用后续中位数掩盖它，也不能将
1 GiB 旧测量推广至本轮 8 GiB。Harbor 外层模型请求计时包含 pause/resume
包装，单独的模型往返使用 `pause-resume.jsonl` 中的 `infer` 行。

最终候选 v4 的 228 项 Harbor 工具检查通过。首次冻结基线残留当前仓库已移除
的 `test_runtime_dirs.py`，两项检查失败；调整本次候选测试集合后通过。
首次真实尝试的 VM 预检通过，但 CLI 在模型请求前拒绝未声明的 pause 参数；
补齐 Agent `options_model` 与前置校验测试后重新执行，保留首次失败和清理回执。
历史测试目录和共享模板保留，未执行历史 payload 回收。

证据：远程 `/var/tmp/hb1008/pause-resume-20261013/smoke-v2/`，本地
`/tmp/agentenv-harbor-pause-20261013/validation-summary.json` 和
`smoke-evidence.tar.gz`（SHA-256
`0433831051482980d89091c48db89c3282c8e301a17b88de2815001d3a54ba06`）。
归档包含原生结果、模型轨迹、切换记录和宿主清理回执；模型凭据扫描无命中。
最终候选 tar SHA-256 为
`e87f77dea6ce33d0c8065ab27cab4aebcd47b08555d8b584e9055b991518913d`。

## pause/resume 取消与响应丢失修复（2026-10-13）

已修复审查中的两个 P2。服务端 `resume_sandbox_inner` 遇到 `Pausing` 时，
复用既有状态转换等待机制，等待暂停完成，再恢复或确认已经回到 `Running`；
沿用 60 秒等待上限。客户端将确认暂停 `_paused` 与待恢复 `_resume_pending`
分开：pause 成功应答后才确认暂停，发送 resume 前撤销确认，成功核对同 ID
并刷新 token 后才清除待恢复状态。丢失恢复响应时，下次推理会重新请求 pause；
直接重试 resume 也会再次发送请求。

原实现先复现：客户端测试断言模型在运行中的 VM 上推理，服务端测试返回
`InvalidSandboxState(Pausing)`。修复后，非 root UID999 的 230 项 Harbor 工具
检查与 156 项 Rust orchestrator 测试全部通过；其中新增两项服务端回归验证
暂停请求被取消后的成功/失败恢复，以及恢复等待请求自身超时后仍完成后台恢复。
Rust 使用 Dragonball feature 编译，格式与 workspace 全 target/feature clippy
通过；clippy 沿用 Rust 1.98.1。上述计数包含回归用例，不重复计入单独重跑。
本轮使用模拟 VM 后端验证真实 orchestrator 状态机，未新增真实 VM 或模型 trial。

证据：本地 `/private/tmp/agentenv-pause-recovery-20261013/`，服务端测试与检查
位于 A 机 `/var/tmp/pr13/`，Harbor 冻结驱动与检查位于 B 机
`/var/tmp/hb1008/pause-recovery-green/` 和 `pause-recovery-python.log`。
本轮未提交或推送，也未替换评测环境的服务器二进制。

## 本轮复盘改进实现（2026-10-13）

已落实 HTTP 故障回归、交付与检查串联、普通单元测试状态隔离、固定 Rust
工具链和冻结文件集合校验。按用户后续要求，`.github` 下的 Actions 与工作流
没有变更；Rust 本地/远程检查采用仓库声明的 1.98.1，未运行或声称通过托管 CI。

`test_pause_resume_http.py` 通过真实 HTTP 客户端与适配器验证取消、请求截止和
恢复响应断开；服务端夹具保留独立捕获任务，模型调用断言服务器确实暂停。
新测试在旧适配器上先复现“运行中的 VM 上推理”，再在当前候选通过。
`delivery_checks.py` 复用现有阶段执行器，等待传输成功退出，按生产端固定
SHA/字节数发布独立副本，再安全解包、检查 manifest 并运行 Harbor 工具检查。
非零退出、超时、截断、错误 manifest 和不安全归档均有回归；子命令 stdin
关闭，原始阶段退出码与失败现场保留。操作见
[交付入口](scripts/harbor/README.md#交付缓存准备与阶段检查)。

最终冻结候选在 B 机由 UID999 完成真实 loopback HTTP 传输与完整检查：
240 项 Harbor、32 项 regression tools、17 项阶段执行器测试全部通过，无 skip。
158 个冻结输入在测试前后核验，当前实现与回归源码摘要和工作区一致；临时
HTTP 服务关闭。本轮未启动真实 VM 或模型请求，也未替换服务器二进制。
本地 macOS 同样通过 17 项阶段检查，32 项 regression tools 中仅一项 Linux
身份检查 skip，未计作通过；Rust 1.98.1 格式检查通过。

首个候选沿用基线的旧阶段测试文件，远程只执行 16 项；补齐冻结覆盖与先失败
后通过的残留测试后，重建最终候选并完整复验，首轮记录保留。
最终候选 SHA256：
`9f3b606624b932f204daa309db6e1eb65ffb35df3db35c09aaa23558af45b0ad`。
证据：本地 `/private/tmp/agentenv-retro-impl-20261013/`；远程
`/var/tmp/hb1008/retro-impl-20261013-v2/`，早期候选在同名无 `-v2` 的目录。
未提交或推送。

## Dragonball 增量内存零区间优化（2026-10-13）

Dragonball 捕获已统一使用现有 `compact_to_sparse` 写层入口。存在模板或恢复
base 时仍只读取 `changed_ranges`，边读取边检查 4 KiB 全零页；全零区间保存为
显式 `zeroed` 索引，不写 RAM 数据载荷，并覆盖父层可能存在的非零内容。
未变化页继续继承父层，清零后重新写入的非零页按实际 RAM 内容保存。
本地层仍为 RAW，没有更改 balloon/reporting、dirty 基底、快照格式或协议，
也没有增加配置开关。普通 Firecracker 捕获仍走原有写层入口。

已有增量栈测试增加数据载荷大小断言及多层零/重写覆盖：旧 writer 在零页载荷
断言上失败（8,192 字节而非 4,096），新 writer 通过；空增量不读取 clean 页。
非 root Linux release 检查通过：Dragonball 60 项、共享内存 writer 4 项、
sparse compaction 3 项；writer 的特权 ENOSPC 测试保持 ignored，未计为通过。
完整 workspace/all-targets/all-features 及 DB-only clippy 在 Rust 1.98.1 下
通过，格式检查通过。首次 Rust 1.99 clippy 对既有代码报告弃用和新增 lint，
日志保留；未以修改无关代码消除它们。

B 机真实 Dragonball 检查均通过：清零后重写、恢复与 fork 隔离；8 GiB 跨 GPA
hole 的增量镜像与同一 held token 下独立完整 RAM 导出逐字节相等；发布失败后
保留相对启动基底的累计 dirty 数据。复用已冻结 runner/kernel/tools/daemon，
server 与 harness 由本次源码以 UID 999 构建；接收端校验 SHA-256 后运行。
三个用例的独立 namespace/cgroup/设备及临时宿主修改均完成清理。

Terminal-Bench 2.0 `regex-log` 的 Flash 复测仍为 4 vCPU/8 GiB、单并发，
原始超时、一次 trial、零 trial 自动重试。新 server 摘要使原共享模板 key
失效；本轮用已合格依赖单独 prepare 该任务的新身份模板，随后模型阶段命中
它的缓存。没有重建全部任务模板。Oracle=1、nop=0，均无异常。

| 首次推理休眠指标 | 优化前 Flash 观测 | 优化后 Flash 观测 |
| --- | ---: | ---: |
| dirty 输入字节数 | 8,319,598,592 | 8,319,086,592 |
| 新增内存层文件大小 | 7.748 GiB | 228.281 MiB |
| pause API | 50.670 秒 | 5.284 秒 |
| 读 RAM、检测零与写层阶段 | 50.364 秒 | 4.971 秒 |

优化后新增层实际分配空间也是 239,370,240 字节，并非稀疏文件的虚拟长度。
后续 17 轮 pause 为 0.300–0.412 秒，新层为 13.211–31.062 MiB。
全部 18 轮推理边界和 6 轮真实预检 pause/resume 成功，无运行异常，宿主恢复
通过。Flash 原生模型得分为 **0**：正则额外匹配了五条应排除的日志，
`test_regex_matches_dates` 未通过；不把推理链路验证成功当作任务得分成功。
这是一项任务的两轮观测，新模板按运行时身份独立准备，不是生产 SLO 或完整
benchmark 结论。初次控制 CLI 退出和旧模板缓存未命中均单独保留。

证据：远程 `/var/tmp/hb1008/delta-zero-20261013/`，本地
`/tmp/agentenv-delta-zero-20261013/`。server SHA-256 为
`7f11d731141df321a5fbc2617383685b22defff0871a04a39a8b6981ab815245`，
构建来源、接收端摘要、原生结果、按当前内存配置顶层 inode 定位的大小和
阶段计时均保留；Flash 归档凭据扫描通过。

随后按用户要求换用 `qwen3.8-max`，复用同一新 server、原始任务、资源规格和
已通过的 Oracle/nop 对照，新模板缓存命中；独立目录 `model-max-v1`，一次
trial、零自动重试。原生模型得分 **1.0**、无异常，7 次推理边界及 6 次预检
pause/resume 全部成功，宿主恢复通过。首轮仍为增量，dirty 输入
8,319,336,448 字节，新层 254.625 MiB（实际分配空间相同），pause 5.681 秒，
读 RAM/检测零/写层阶段 5.374 秒；后续 6 轮 pause 0.298–0.395 秒，新层
13.516–25.211 MiB。Flash 的零分记录保留，Max 的通过不改写它。
Max 大小与计时分别保存为 `size-summary-max.json`、`capture-sizes-max.csv`、
`capture-timings-max.txt`，归档独立于 Flash。
最终归档 `evidence-max.tar.gz` SHA-256 为
`a41f1c5644f78e6eb296bd62fad17fc00a3e42dd8012149b3b0d8c8dce0ee230`，
凭据扫描、下载后摘要及逐文件 manifest 核对通过；汇总在本地
`validation-summary.json`，原生 Flash/Max 得分与异常分开保存。

## 首轮休眠大小诊断与第二个用例（2026-10-13）

仅 Dragonball、同一 B 机与冻结运行时，仍为 4 vCPU/8 GiB、单并发。
新增 Terminal-Bench 2.0 `regex-log`，`qwen3.8-flash` 一次 trial，原生得分
**1.0**、无异常，24 次推理边界全部完成 pause/resume。另对原任务
`log-summary-date-ranges` 做无模型重放：两个新 sandbox 各三次 pause/resume，
Oracle=1、nop=0，全部 VM、设备和宿主临时修改已清理。
诊断候选沿用通过的 228 项工具检查；统计代码仅放在独立临时候选，未修改
Rust/Go、快照格式、原任务或评分器。

在每次 pause 完成、resume 前，对新增 `overlaybd.commit`、磁盘层、VM 状态及
内存 `image.json` 记录文件长度、`st_blocks` 和 inode。继承层的硬链接单独记录，
按 inode 去重，以下表格只统计本次新生成的内存层，不是整个快照链或配置的
8 GiB 虚拟内存容量。

| 场景 | pause API | 新增内存层文件大小 |
| --- | ---: | ---: |
| `regex-log` 模型 trial 首轮 | 50.670 秒 | 7.748 GiB |
| `regex-log` 后续 23 轮 | 0.265–0.352 秒 | 8.934–30.074 MiB |
| 原任务新 VM A 首轮 | 35.954 秒 | 5.287 GiB |
| 原任务新 VM B 首轮 | 16.743 秒 | 2.673 GiB |
| 原任务两 VM 的后续 4 轮 | 0.227–0.239 秒 | 5.855–8.895 MiB |

大层的实际分配空间与文件大小近似相等，并非只报稀疏文件的虚拟容量。
原任务两次首轮各抽样 64 个分散的 4 KiB 块，63 个全零；`regex-log` 首轮
64/64 块全零。仅是抽样，不能声称逐字节验证整个文件全零。

**增量机制从首轮就启用。** 本轮 36 次捕获日志全部 `incremental=true`，
内存 lower 链在三轮预检中从 2→3→4。源码在存在 base 时只遍历
`changed_ranges`；变化页包含被清零的页，RAW 增量层实际写入这些零字节，
以覆盖父层可能存在的非零内容，不能直接当作稀疏空洞跳过。
`regex-log` 首轮变化范围为 8,319,598,592 字节，写内存层 50.364 秒；
内存映射完成到写入完成约 50.366 秒，之后磁盘阶段约 5.3 ms。该轮新增磁盘
层约 105.7 MiB，VM 状态约 89 KiB。慢点已定位到大量增量内存数据的写入，
父层配置构建、磁盘封存不能解释这 50 秒。

源头的最强线索是启用的 virtio-balloon free-page reporting：qualified Kata
`9e41795fea9109bc1acf7b1f6d6b878cfff7b72e` 的回收路径把私有 RAM 替换为
匿名零页并标记 software dirty，捕获合并这些变化页。大量零块的抽样与该路径
一致，但本轮尚未进行禁用 reporting 的独立 A/B，不能声称已独立归因全部零页。

首轮慢可复现，但不是固定 50 秒，也不是“第一次全量、以后才增量”。
`regex-log` 两个预检 VM 首轮分别 5.102 秒/912.7 MiB 和 0.284 秒/20.2 MiB；
其中一个 VM 第三轮仍达 4.978 秒/899.9 MiB。耗时随当前变化范围而变。
后续优化方向是减少零数据的实际写入，同时保留显式覆盖父层的语义；
本轮只诊断和验证，没有改动内存回收或快照写入策略。

证据：远程 `/var/tmp/hb1008/pause-sizes-20261013/`，本地
`/tmp/agentenv-harbor-pause-sizes-20261013/size-summary.json`、
`evidence/capture-sizes.csv`、`evidence/capture-timings.txt` 与原生 trial/清理回执。
归档 `size-evidence.tar.gz` SHA-256 为
`7b6e644f690a578779a954a1e77e2fd20fd293f792d7f721f75fca8d72c29b1e`，
下载后已核对；模型与 OSS 凭据扫描无命中。原 50 秒症状的快速诊断命令保留在
临时 `diagnostic-controls/check_repro.py`，10 秒及倍数阈值只用于诊断，不是产品 SLO。

## 当前评测目标与范围

2026-10-12 Max轮已于11:43:57（UTC+8）结束，71项全部完成：原生verifier得1
39项（54.9%）、无异常且得1也是39项、18项完成但未通过、14项AgentTimeoutError。
与Flash相同71项比较，原生得1从29项（40.8%）增加到39项，15项提升、5项回退；
此前Flash严格“得1且无异常”指标为27项（38.0%），两条指标保持分开。
控制器总跨度约从2小时17分缩短到1小时48分（21.5%），输入token减少31.1%；
这是两轮观测，不是独立VMM性能实验。全部sandbox删除、宿主恢复和冻结候选
摘要核对通过。任务契约、资源及server/runner/kernel/tools摘要完全一致。
完整对比、71项CSV和来源摘要位于
`/tmp/agentenv-harbor-qwen-max-20261012/COMPARISON.md`、`comparison-tasks.csv`、
`comparison.json`；回退和剩余超时原因继续保留，未改写原生得分。

2026-10-12 用户要求换用 `qwen3.8-max` 重跑，已启动独立轮次：两机
`/var/tmp/hb1008/qwen-max-20261012/shard-{0,1}/run`，本地
`/tmp/agentenv-harbor-qwen-max-20261012/`。基准仍为 **Terminal-Bench 2.0**，
任务固定提交 `69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`，Harbor仍为0.24.0；
复用Flash相同71项、模板和tools，另3项暂缓及12项未合格保持单列。
两机原模型预检各HTTP200/17tokens单列；各220项工具检查、三项控制测试通过，
候选摘要未变。清理后A/B可用约350/424GiB；实际均读回guest4c8g、节点64c128GiB、
八槽并已观察各八个VMM，首项通过并补位。原任务超时、thinking=false、4096输出
token、零trial自动重试保持一致。原生verifier分数与异常分别统计，最终结果
以新轮原生记录为准。Flash记录保留；仅增加模型选项，未提交或合并。

2026-10-12 已核对本轮最终结果：A 41 项、B 30 项全部结束，合计 **27 项通过、
11 项正常零分、33 项 AgentTimeoutError**，实际执行通过率 27/71＝38.0%。
原资格集合仍为 74 项，三项保持未执行；按原集合成功比例为 27/74＝36.5%，
不排除超时后重新计算。评测于 2026-10-11 22:10（UTC+8）完成，两机实际 VMM
峰值均为八个；全部 sandbox 删除、宿主恢复及冻结候选摘要核对通过。
控制器退出非零来自任务异常汇总，队列已完成且无清理错误。33 项超时删除前
取证均成功；GPT-2、circuit-fibsqrt 的 guest 内核日志确认 Python 进程遭 OOM
杀死。2026-10-12 只读归因及小规模代码重放确认：前者为模型生成的两份巨大
Python 前缀列表，后者为节点数不减少、字符串持续增长的循环；两项实际均为
4c8g，较原声明1c4g/1c2g更高。原始评分不变；其他31项超时待归因。详细证据
及重放保留在上述输出目录的 `oom-analysis/REPORT.md`，没有重跑模型或完整OOM。
其余31项事后分类保存在 `timeout-analysis/REPORT.md`：两项响应协议循环、两项
verifier已得1但Agent超时、一项生成的SIGINT自测挂起，其余为实现、求解、计算
及依赖准备的混合问题；未分离的环境性能贡献继续保留。原生verifier得1共
29/71＝40.8%，严格“得1且无异常”27/71＝38.0%，两条指标单列。
原生 token、耗时、错误与轨迹保留；最终报告和来源摘要位于
`/tmp/agentenv-harbor-benchmark-20261011/REPORT.md` 及 `final-evidence/`。

2026-10-11 用户要求执行新的 Terminal-Bench Qwen 评测，当前入口为两机
`/var/tmp/hb1008/terminal-benchmark-20261011/shard-{0,1}-r1/run`；本地实时记录为
`/tmp/agentenv-harbor-benchmark-20261011/`。启动前按所有权、过期时间及引用检查
清理历史 Lab，A/B 回收约 92.3/86.7 GiB，可用约 350.1/407.6 GiB；持久归档、
依赖缓存及 OSS 模板保留。实际读回两机节点 64c/128GiB、guest 4c8g，均已达到
八个调度槽并产生模型轨迹；首项通过并删除 sandbox 后已补位。这是启动验收，
最终通过率以本轮原生结果为准。

原资格集合仍为 74 项，另 12 项未合格保持排除。本轮预计执行 A 41/B 30 项：
Mailman 暂缓新模板及运行时验证，视频提取与 MTEB leaderboard 被必需素材门禁
阻断，三项保留为未执行。复用已验证 OSS 模板及 `0.1.2-p4.1` tools；诊断用
`/run` 修复镜像尚未进入本轮模板。采用最新驱动，不重复 Oracle/nop，也不做
Docker 对照；原预算、一次执行及零 trial 自动重试保持不变。

启动准备先遇到缓存清单封装读取错误，随后首次 Lab 因候选缺少
`scripts/run-with-capabilities.sh` 在调用模型前停止，两机恢复均通过。
这两次控制失败单独保留，不作为模型任务结果。最终候选 152 个文件包含已验证
运行脚本，之前两机检查通过的 119 个文件摘要全部未变；启动实际配额及轨迹
核对通过。脚本及本轮记录位于上述输出目录，代码未提交或合并。

2026-10-10 用户确认“好的，执行测试”后，已启动新的 Qwen 持续评测，独立目录
`/var/tmp/hb1008/qwen-continuous-20261010`。A 42 项、B 32 项，仅执行下述已合格
74 项；每机最多八槽，原生单 trial Job、删除确认后持续补位。启动前两机各回收
约 3.66 GiB 的成功修复验收派生数据，清理后可用约 297/354 GiB；旧结果、宿主
日志、失败现场、归档与 OSS 保留。两机 Qwen 预检均 HTTP 200，各 29 tokens
单列。驱动与已验收 v29 相同，v30 仅修改两个冻结操作控制输入以显式运行模型；
三项控制测试及 135 文件摘要前后核对通过。实际已观察两机各八个 4c8g 模板
sandbox 启动并产生模型日志；节点读回 64c/128GiB/8192 pids。该段为启动时记录；最终原生结果的只读核对见本轮工程执行记录。
本轮只读进度和报告在 `/tmp/agentenv-harbor-qwen-continuous-20261010/`，旧轮停止
状态和成绩保持不变。本轮未提交或合并代码。

2026-10-10 已只读调查当时完成记录中的 29 项 AgentTimeoutError，见
[超时归因](docs/src/internals/harbor-qwen-timeout-analysis.md)。确认 Mailman 的
运行目录兼容性贡献项、YouTube 外部素材阻断；CompCert 安装等待保留待定。
调查未重跑、调用模型或调整运行配置，严格评分保持不变。

用户最终明确：测定 AgentENV＋Dragonball 的稳定性、通用性及优化效果，并调用
`qwen3.8-flash` 观察实际任务通过率。Terminal-Bench 提供工作负载与评分器；
本轮只在 AgentENV 运行，不做 Docker 对照。完整资格验证已结束：86 项中
74 项 Oracle/nop 严格合格、12 项未合格。后续只执行这 74 项，每项一次、
零 trial 自动重试；12 项仅保留记录及失败现场，调研延期，不补跑。

Qwen 阶段保持 Harbor 原生 config、lock、result、模型消息/响应轨迹、token 用量、
阶段耗时、工具调用、错误、verifier 日志及轨迹，凭据脱敏。不重复 Oracle/nop
资格对照，不将资格通过率混为模型任务通过率。分母固定为 74，另列已完成、
成功、正常 0 分、异常与未执行，避免排除异常后抬高通过率。

已移除旧冒烟的 25 轮限制，沿用原始任务阶段预算、thinking=false 和每次最多
4096 输出 tokens。模型阶段仅允许复用已经提交的干净 OSS 模板，无需启用
CachedVerifier；缺失模板失败，不静默回退冷启动，模板容量仍计入准入。
两项回归先失败后通过，冻结 v19 两机 UID999 各 131 项检查通过，测试后候选
摘要一致。Qwen API 预检 HTTP 200；此辅助请求的 29 tokens 与任务用量分开。

Qwen 队列原分配 A 42 项、B 32 项，2026-10-10 用户要求终止，现已停止。
停止时完成 11/74，7 项成功、4 项异常，另 2 项运行中被取消；不是全量结论。
旧轮沿用批次 Lab 驱动；新的持续入口已完成两机无模型工程验收。两机受控清理和宿主恢复通过，无
UID999 进程、ublk 设备或 Lab cgroup 残留。本轮原生记录保留，不将取消项
改写为自然完成或模型失败。
用户明确两机是专用测试服务器，不长期保留过期过程数据。按精确清单和
引用检查回收历史产物，A/B 分别回收约 135.1/291.1 GiB，可用空间提升至
301.4/358.0 GiB；本轮证据、运行资产、持久归档、依赖缓存及 OSS 保留。
候选 124 个输入摘要未变。一次性清理已完成；新入口已接入宿主租约下的过期 Lab 清理，并完成真实运行验收。
[驱动修复方案](docs/src/internals/harbor-benchmark-harness-repair.md) 已获用户批准，
实施计划已确认执行，落实情况见上述修复方案与执行记录。
当前选择清单及报告在 `/tmp/agentenv-harbor-qwen-comparison-20261010/`；
远程回执及全部原生记录在两机 `/var/tmp/hb1008/qwen-agentenv-20261010/`。
该本地目录名来自先前对照提案，仅为产物路径，本轮没有 Docker 组。

## 驱动修复交付（2026-10-10，工程验收完成）

`run_campaign.py` 在每机一个持久 Lab 内运行原生单 trial Job，最多八槽；
传输预检先占一个槽，正常删除后才启动任务队列，完成及删除核对后持续补位。
共享源层从已提交 OSS catalog 读取真实层大小，按 digest 只预留一次；OCI
按需导入并核验 digest，每个 Job 只取得匹配任务的 uv 绑定。模板只查找复用，
不创建 builder；缺失明确失败。Oracle=1、nop=0 的校准要求仍保留。

磁盘分开记录 64 GiB 虚拟盘、10 GiB 默认任务声明、宿主物理块和未兑现预留。
共享物化只在自有 registry／commit 缓存有物理块证据时抵扣；活跃私有写入
尚不能逐 sandbox 可靠归属，因此保持保守预留（典型 10 GiB 写入＋8 GiB
恢复物化），直到删除确认后释放。它可能降低紧盘场景并发，不是硬配额。
启动前与准备后检查实际 20 GiB 水位，不能用未抵扣的私有预留导致负 headroom
去取消仍可正常运行的已准入任务。宿主空闲空间变化不单独归因为 guest 写入。

清理只处理超过 24 小时、精确身份匹配且 M4 引用审计为 clean 的已记录 Lab。
当前输入、归档、依赖及 pin 保留；未知／跨 boot／活动资源拒绝自动删除。
所有删除保留计划与进度回执，部分失败也记录 in_progress、错误和 free-after。
新入口还检查 ublk 模块和运行账号设备权限，缺失时在服务／VM 启动前失败；
独立冻结验收控制器复用既有 host setup／restore，退出只恢复一次宿主。

原生 Job 聚合 result 与 trial result 分开；CLI exit 0 不代替 trial 无异常或删除
确认。外层 deadline、用户取消、低水位与未运行待办另列，原错误／轨迹保留。
同步回归覆盖第九项补位、删除屏障、失败准备、取消、水位和共享预留回滚。
独立审查的重要问题已修正。最终冻结候选 v29 共 135 个文件，相比 v19 改动
18 个文件，完整清单、SHA 和增量 diff 独立保存。两机非 root 各通过 163 项
Harbor 检查及 2 项恢复控制测试，运行前后摘要校验通过。

两机各先通过模板传输预检及正常删除，再完成 9 项真实 guest 延迟／原生
verifier 任务；峰值均为 8 个 VMM，4c8g 与节点 64c/128GiB/8192 pids 实际
读回一致。第九项在快项删除确认后补位，分别比首个慢项删除早 42.33/105.99
秒；九个 Sandbox ID 独立、同一模板只复用、不构建。观测到整宿主磁盘增量
峰值约 510/499 MiB，最低可用约 294.9/351.5 GiB，不能归为单 guest 写入。
活动期 SIGTERM 两机均保留 8 项 interrupted、1 项 pending，未补位；原生
删除核对均通过，整个 Lab 与宿主恢复成功，最终无 UID999、ublk 或 cgroup 残留。

早期 v23 缺少 host setup、v25 registry 误走代理和 v28 取消停在 nsenter 的
失败记录保留；最后一项改为共享取消标记，等待协调器有界保存记录后收尾。
恢复阶段首信号及 restore 异常有独立同步复现。低水位／删除失败覆盖为同步
夹具，未实际填满宿主磁盘。未覆盖全量 74 项持续八并发、模型调用或初始
OSS 元数据查找期间的及时取消；因此不宣称生产容量或模型性能已验收。
修复验收结束时正式模型队列保持停止，控制延迟任务不算 Oracle 资格或模型成绩；
用户后续授权的新一轮执行见本文“当前评测目标与范围”。

远程证据：两机 `/var/tmp/hb1008/harness-repair-20261010/`；本地报告与镜像：
`/tmp/agentenv-harbor-qwen-comparison-20261010/harness-repair/REPORT.md`。

## 固定输入与运行约定

Harbor 固定提交 `4b94505a91c5ddcb70b5740ac95718ddee13e5a0`（远程 Python 3.14.4、Harbor 0.24.0）。Terminal-Bench 固定提交 `69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`，交付包含 89 个任务、857 个文件与对应 linux/amd64 镜像 digest。准备与执行均校验完整任务集合，保留原始 task、solution、verifier。

ACR 位于测试机可访问的 VPC 域名，namespace 为 `terminal-bench`。镜像同步只冻结镜像；运行时 apt、uv、git、模型及 API 下载仍可能变化。凭据通过进程输入传递，限定临时目录，退出时删除；文档与回执不保存密钥。

两台测试机为 `147.139.168.159`（A）与 `147.139.165.124`（B），各有 104 个逻辑 CPU、约 187 GiB 内存。root 仅编排 namespace/主机准备；Harbor、服务与 VMM 使用非 root 测试账号。固定运行资产与每轮驱动摘要，隔离测试状态，不使用线上状态。性能前后对照须在同机进行。

## 资源与并发口径

2026-10-10 用户最终确认：**每个 sandbox 4 vCPU / 8192 MiB，每台最多 8 个并行 sandbox，两台合计最多 16 个；两机 AgentENV＋VMM 统一为 64 CPU 时间配额 / 128 GiB 内存上限**。Harbor 的执行单位是 trial：Oracle 和 nop 各占一个 sandbox。报告分别列任务数、trial 数和实际活跃 sandbox 数；Oracle/nop 是环境资格验证，模型正式评测单独启动和统计。

旧 1c2g 来自任务原始声明与独立传输探针，不是 Dragonball 上限。多数原始任务为 1c2g；GPT-2、Protein 等为 1c4g。新入口通过 Harbor `override_cpus`、`override_memory_mb` 覆盖，原始声明和有效资源分别保存。`--task-resources` 恢复原始规格；`--n-concurrent-trials 1` 恢复串行。新规格成绩与旧规格分别统计，不覆盖历史结果。

节点配额固定为 64 CPU、128 GiB 内存、8192 pids，不随批次大小或 controls/model 阶段缩减；夹具另留 32 GiB，宿主再保留 16 GiB，准入需 176 GiB 可用内存。磁盘预算仍按实际最大活跃 trial 数计算。配额是 cgroup 上限，不是 CPU 绑核或内存预分配。共享 M4 原有 4 CPU 配额不能承载这一评测，Harbor 入口单独调整，不改变其他测试默认配置。旧冻结候选仍按批次计算节点上限；本轮运行配额的显式调整另保留变更时间及实际读回回执，旧成绩不改写。

磁盘准入按同时活跃任务的 storage 声明预留，并额外保留 20 GiB；典型 8×10 GiB 在运行前需要至少 100 GiB 可用空间；准备前还额外预留全部选中镜像的归档大小，通过固定 digest 的原始 manifest 核验，逐镜像复查磁盘。storage 尚未作为 guest 硬配额执行；此准入不是磁盘峰值保证。当前驱动未传磁盘大小，AgentENV 将普通 OCI 镜像转换为 64 GiB 虚拟根磁盘，模板启动继承模板容量；宿主按实际数据占用空间，任务的 10 GiB 声明不限制实际写入。只回收已成功、清理完成且无引用的自有 payload，保留日志、评分、配置与失败现场。传输探针沿用任务配置与本轮 CPU/内存覆盖，回执注明实际规格与诊断用途，不能当成 Oracle 资格证据。

### 环境一致性与调度效率复盘（2026-10-10）

问题首先出在测试工程：旧 `resources.py` 把批次最大活跃 trial 数同时用于
节点配额和磁盘准入，A 的四任务批次得到 36 CPU/80 GiB，B 的两任务批次
得到 20 CPU/48 GiB。旧资源测试明确断言单任务批次降为 12 CPU/32 GiB，
因此两机各 124 项检查通过仍不能证明节点规格一致。已有 Make/CI 的 Harbor
检查入口有效；缺失的是正确的跨批次资源契约，不应再建一个重复测试入口。

资源回归覆盖 1/2/4/8 个任务、Oracle/nop 与模型单试验：节点规格保持一致，
磁盘预算仍随工作量变化；低于节点 profile 的宿主容量和超出节点承载能力
的 guest 并发在启动前拒绝。具体远程验证和运行配额调整回执见
`/tmp/agentenv-harbor-node-profile-20261010/REPORT.md`。

| 问题 | 防止再次发生的措施 | 验收状态 |
| --- | --- | --- |
| 批次改变节点规格 | 固定节点 profile，资源测试覆盖批次大小及 controls/model 两种阶段 | 代码及定向回归已完成；远程证据以上述报告为准 |
| 将任务、trial、VM 混算 | 报告分别记录逻辑任务、各 agent trial、活跃 sandbox；Oracle=1/nop=0 仅作环境资格 | 已明确口径；持续调度已实现，真实受控任务峰值八槽 |
| 整批等待慢任务 | 一个持续 Lab 内按空闲 sandbox 槽补位，逐试验清理与准入，整轮恢复宿主 | 持续入口及两机长短任务补位已验证；取消与删除失败、低水位覆盖分别记录；没有提速百分比 |
| 配置与实际运行发生漂移 | 保存源码/配额 profile 身份、实际 cgroup 和变更时间；旧冻结候选与显式运行覆盖分别追溯 | 持续入口两机已实际读回 64c/128GiB/8192 pids、guest 4c8g；统一 profile 持续复用 |
| 缓存已准备却未使用 | 分别报告 prepared/bound/consumed；按任务、镜像、guest 资源和资产摘要查找干净模板，逐试验独立写入 | 持续入口复用 OSS 模板，每 Job 只绑定本任务 uv 缓存；模型入口已接线，本轮未调用模型 |
| 手工命令破坏冻结候选 | Python 入口使用 `-B`，既有检查入口给子进程传递禁止字节码环境并在测试后复查候选 | 两项回归先失败后通过；两机 v18 测试后完整性通过；统一 bootstrap 自动化仍待完善 |

复盘以执行器现有回执为依据，优先补自动检查；工程指南只保留入口与无法机械
判断的取舍。错误先区分准备、驱动、依赖、评分器与后端；首次分数不改写，
诊断复验单独保存。历史资格入口与新的持续模型入口分别记录；后者已支持原始
verifier 下的共享模板复用，本轮只验收无模型运行。缓存、持续调度和模型评测分别验收。

v18 两机 UID999 各通过 129 项驱动检查和 3 项配额操作工具检查，无 skip，
测试后完整候选校验通过。两机同内容 profile 已落盘；B 在当前任务中途应用
64 CPU/128 GiB、实际及祖先 cgroup 读回通过；A 已结束本轮 43 项，历史配额
与结果保留，新 profile 下轮就绪。本轮混合配置不能称为完整新规格复测。

## MicroVM 任务筛选

[scope.py](scripts/harbor/scope.py) 记录固定版本的范围排除，准备阶段筛选，执行阶段拒绝旧准备目录中的排除项。`--all-supported` 选择过滤后的集合；重复 `--task` 可选择小批次。完整 89 项来源与镜像校验不因筛选减少，准备回执保留排除理由。

| 排除任务 | 实际需求 |
| --- | --- |
| install-windows-3.11 | 在 QEMU 中运行 Windows 3.11 |
| qemu-alpine-ssh | 在 QEMU 中启动完整 Alpine 虚机并提供 SSH |
| qemu-startup | 在 QEMU 中启动完整 Alpine 虚机并提供控制台 |

剩余 **86 项属于当前评测范围，尚不等于全部已通过 Oracle 资格**。Linux 桌面、浏览器、图形、游戏、MIPS 用户态解释器继续保留。CPU 推理、计算任务不因出现 GPU/Windows 字符串而排除；Stan 任务历史 Compose 标记已迁移到 Dockerfile，不等于运行时依赖多容器。内存不足、任务超时、下载失败、解答或评分器问题单独诊断，不改记为 MicroVM 不支持。

## 已发现的问题与修正

| 问题 | 证据与处理 | 解释边界 |
| --- | --- | --- |
| 本地服务误走代理 | `0.0.0.0:5000` 未命中 bypass；已补本地 NO_PROXY 与真实 VM HTTP 对照 | 代理问题可能表现为模型/应用失败 |
| 长连接被驱动截断 | 外部代理连接原有固定 180 秒生命周期；已移除，保留请求阶段与外层预算 | 保活可重置 socket 超时，总预算仍需独立截止 |
| 取消命令继承 SIGINT 忽略 | envd 忽略 SIGINT；短子任务自然结束造成假通过。GNU timeout 包装恢复信号，未配置超时时使用 0 | 验证绑定实际 GNU 版本，不声称所有实现均相同 |
| VMM 漏采 | 可执行文件更名后 basename 不匹配；按二进制身份及 UID 采集 | 漏采不是模型失败；失败前先保存诊断回执 |
| GPT-2 内存证据混用 | 原始 4096 MiB 被误写 2048；NULL/ENOMEM 探针来自另一 2-GiB guest，已更正 | 6.75-GiB malloc 申请不等于 RSS；原进程分配返回值仍未直接验证 |
| 7z OpenMP 表述误读 | 1 vCPU guest 的 John 最大线程数为 1；4 vCPU 实验线程数为 4 | 不表示 Dragonball 禁用 OpenMP；容器 CPU quota 与 guest CPU 拓扑不同 |
| 模型预算沿用冒烟 | 25 轮、thinking=false 被用于全量诊断，正常 0 分中 17/21 达到轮数上限 | 停止的成绩不是默认 agent 参数下的正式全量结果 |
| 分数不能证明评分覆盖 | FilterJS Docker Oracle=1，但有浏览器启动异常、OOM、残留；DB 达 verifier 超时 | 保留官方分数，另记覆盖不可信，不直接作为功能资格 |

委派进程须核验真实 HOME、工作目录和写权限；root 或 runuser 下测试通过不能代替实际 setpriv CLI 验证。任务目录名、声明名与环境 short name 可能不同，镜像映射跟随框架解析结果；不同任务也可能共用环境 hash，资源回执按环境名称对应任务。agent 工厂可能优先选择注册名，必须实际构造并验证配置。凭据脱敏覆盖嵌套日志字段，在写入前执行。直接构造 environment 须显式传入解析后的网络策略，精简镜像探针使用已有工具，不为通过诊断改镜像。

Harbor 将 solution 的非零退出保存在 `exit-code.txt`，不自动判 trial 失败；沿用原始 verifier，不替换官方评分。Oracle 资格同时要求精确任务/agent 集合、Oracle=1、nop=0、无框架异常、驱动退出成功、实际资源匹配、清理成功。模型成绩、环境正向对照和实际动作覆盖分别记录。Computer1 取消收尾可能继续模型调用，600 秒预算曾出现 663.971 秒阶段墙钟，不能用于后端性能结论。

## 已验证结果与证据

- 原始规格 Docker/Dragonball 对照：openssl、HF inference、OCaml GC、POV-Ray 已通过。取消测试修复后 A 的原始 Oracle 六项通过，三个取消子测试约 1.52 秒，与 Docker 一致。v6 驱动两机各 23 项测试通过。
- 7z 资源实验：4 vCPU / 2048 MiB，Oracle=1、nop=0，John 四线程，解答阶段约 326.2 秒。仅 CPU 改变，保留原始 1 vCPU 超时记录。
- GPT-2 资源实验：4 vCPU / 8192 MiB，Oracle=1、nop=0，原始 verifier 1 passed / 9.12 秒；含安装下载的 verifier 阶段约 74.3 秒。CPU 和内存同时改变，不能单独证明内存根因。
- 原始资格中的共同问题：Protein 解答约束不满足；Cython 17 passed / 1 failed。Caffe 两端均 1200 秒超时，但 Docker 停在下载、DB 停在编译，失败阶段不同。Doom apt 下载问题不属于 MicroVM 范围排除。

权威证据：`/tmp/agentenv-harbor-oracle-standard-20261009/REPORT.md`、`qualification.json`、`cpu4-summary.json`、`gpt2-4c8g-summary.json`；停止的模型审计在 `/tmp/agentenv-tb-full-20261009/FAILURE_ANALYSIS.md`；Computer Use 在 `/tmp/agentenv-cu-20261009/REPORT.md`。远程原始产物位于两机 `/var/tmp/hb1008/` 下对应 run 目录。新资源配置与并发验证摘要在 `/tmp/agentenv-harbor-resource-profile-20261009/REPORT.md`、`summary.json` 与两机 `evidence-*.json`。

## 本轮资源与筛选验证

2026-10-09：最终修订两机各 33 项驱动测试通过；真实 ACR 准备测试均排除三个完整虚机任务，只导出保留的 openssl，manifest 大小预算及临时认证清理通过。两机非 root 校验完整 89 项／857 文件，生成 86 项范围清单。

A 常规 8 任务／16 trial 对照均符合 Oracle=1、nop=0，节点无 OOM，清理及宿主恢复通过；实际 VM 重叠峰值为 6。Harbor 并发槽包含准备阶段，配置 8 不能单独证明 8 台 guest 同时运行。A 另以一次启动屏障保持八台就绪后才执行 agent，4 任务／8 trial 对照通过，峰值 8、节点无 OOM、清理和宿主恢复通过。屏障属于独立容量夹具，不用于性能对比。

B 常规 8 任务／16 trial 对照全部符合 Oracle=1、nop=0，实际 VM 重叠峰值为 8，节点无 OOM，清理、宿主恢复及临时凭据删除通过。常规 Lab 墙钟 A 为 542.8 秒、B 为 2225.6 秒；B 的 OCaml GC verifier 等待完整 Git 仓库下载，原始 3600 秒预算保持不变，未重跑或改解答。这不是同机性能对照，不能归因后端性能。八台活动 guest 中读取 nproc=4、MemTotal=8153008 kB、SwapTotal=0。A 启动前按成功结果及清理记录核验精确目录，无活跃引用后回收 11 个旧成功性能 payload，约 85.3 GiB；保留结果、日志、失败与未归属数据。

常规 VM 批次冻结 v1 驱动 62 文件。审查后的 v2 仅调整资源回执映射与归档准入，分别以单元测试、真实准备及常规回执重放验证；不称其执行过完整常规 VM 批次。A 容量夹具基于 v2，另记录屏障与 import path 差异。证据目录分别为两机 `/var/tmp/hb1008/resource-profile-20261009`、`resource-profile-check-v2-20261009`，以及 A 的 `capacity-eight-20261009`。未合并或推送。

## 动态依赖缓存与模板复用（首轮已验证）

按[独立设计](docs/src/internals/harbor-dependency-reuse-design.md)实现了普通 OCI 依赖数据镜像、ACR 持久发布、校验节点缓存、流式传输、verifier 阶段注入和批次内干净模板。操作参数见 [Harbor README](scripts/harbor/README.md#依赖-oci-镜像与批次模板)。原始冷启动入口保留；显式 `--dependencies` 启用 verifier 缓存，增加 `--template-start` 才使用模板。首轮仅冻结的 openssl 与 fix-ocaml-gc，不表示 86 项全部已有缓存或 Oracle 资格。

新增 repository 为 `terminal-bench/hb-deps-fix-ocaml-gc-verifier`，OCI digest 为 `sha256:43ee0a35f9705ae5239aa94e9f1ac385f05c1c2a98b1544b79256872ee0e898b`，内容 tag 为 `bundle-398ec45ed57c5354c260f81dacf05e1354a180b183cb74df88e7adeadeb28b70`。只向专用依赖仓库发布新内容 tag；同 digest 只读复用，冲突停止，不写 latest、不删除旧镜像。两机最终各核对原有 89 个任务 tag，摘要均未改变；实际重复发布返回 published=false。

OCaml 完整 bare mirror 固定 commit `356d558bf89552ed9229253ec4135c691234f060`，bundle 为 542044160 bytes（约 517 MiB）。OCI blobs、清单、bundle、文件集合、类型、路径、大小与文件 hash 校验。仅 agent 结束后注入临时目录，通过 Git runtime URL rewrite 和 file-only 协议调用原始 clone；不提前放置 `/app/ocaml-original`。原始 checkout、测试集替换、编译和评分照常执行；Oracle 日志确认 40 tests passed 与 pytest 1 passed。apt、curl、uv 仍可能联网，本轮不是完整离线环境。

批次模板从原始任务镜像 capture，未预装 verifier 数据、解答、测试或模型凭据，删除 source builder 后才执行 trial。模板键绑定任务全文件摘要、原镜像 digest、资源和运行资产，另检查批次；verifier 包摘要单独绑定。未来公开依赖进入模板时还需加入其清单摘要。恢复后首次 guest 命令核对新 Sandbox ID，初始工作区摘要一致；源 builder 与各 trial ID 不同。ACR 依赖跨批次保存，snapshot ID 只属于当前 Lab，成功 payload 回收后必须重新 capture。

2026-10-09 首轮验证：

- 两机常规同机 cold/warm 对照，各 4 trial、两个任务均 Oracle=1/nop=0，保持原始任务、镜像、资源与预算，trial 自动重试为 0，模型调用为 0。
- B 从空节点缓存拉取固定 ACR digest 约 11.0 秒。A 回收旧成功 Lab payload 后，新空缓存从 ACR 拉取约 11.5 秒，新批次重新 capture，4 trial 全部符合，run/snapshot ID 不复用。
- B 独立容量夹具：两个任务各重复两次，八台就绪后释放 agent，8/8 trial 全符合，实际 VM 峰值 8；每台 nproc=4、MemTotal=8153008 kB、SwapTotal=0。重复次数、资源准入、Counter 与屏障差异单独冻结，不用于性能统计。本轮 A warm 常规峰值 4，不声称两机都完成八台 warm 并发。
- 最终产品 v2 两机各 57/57 驱动测试通过；A `make test-harbor-tools` 57/57；语法、文档链接和 diff 检查通过。v1 用于常规对照；v2 修复 registry404、空模板回执与解包副本磁盘预算，另用于跨批次及容量验证。托管 CI 未执行。
- 6 个完成 cell 共 28 trial，精确集合、无框架异常、节点无 OOM，依赖注入 10/10 清理通过；所有 sandbox 删除，成功自有 payload GC、宿主恢复与临时认证删除通过。最后磁盘 A 约 124.2 GiB、B 约 172.9 GiB。

两任务/四 trial 的新磁盘准入为 112 GiB：既有 60 GiB、完整模板上限 36 GiB、tar 与解包副本上限 16 GiB；八并发 fixture 为 168 GiB。storage 仍未作为 guest 硬配额执行，此准入不是实测峰值保证。

权威证据在 `/tmp/agentenv-harbor-dependency-reuse-20261009/REPORT.md`、`summary.json`、两机 `*-final-collected-evidence.json/tar.gz` 和源码冻结清单。远程均为 `/var/tmp/hb1008/dependency-reuse-20261009`。A 两次外层预检失败（绑定路径、准备竞争）另留档，未创建 trial VM；竞争期间恢复拒绝活跃工作进程，等待退出后按回执恢复，再以新目录验证。没有改写失败或延长原始预算。

独立审查无 Critical，三项 Important 均有 RED→GREEN 回归。首轮暂缓两个 Minor：缓存命中不保留 OCI-to-bundle 证明（当前入口另校验固定包摘要）；不显式拒绝冲突的旧 Git rewrite/protocol 环境（固定 pilot verifier 环境为空）。后续扩展已增加对应回归与实现，验证情况见下节。未提交、合并或推送代码。

## 全任务模板与依赖绑定扩展

2026-10-09：现有入口扩展至固定 86 项 MicroVM 适用任务，仍排除 Windows／完整 QEMU 的3项；Linux 图形和游戏保留。静态审计确认89项均为单步骤 shared verifier，工作目录全部绝对非根目录。运行期依赖不能从下载工具引用自动推断，构建、解答和 verifier 阶段分别记录；仅已验证的 OCaml Git 配方标记 prepared，其余额外动态依赖标记 not_prepared。

`prepare_dependencies.py --all-supported --reuse-binding` 校验既有任务／镜像绑定并只读复用 ACR digest；不覆盖原镜像，不重新发布既有 tag。移除模板入口的两任务白名单，保留原始文件、阶段预算、4c8g、每机并发上限8、独立ID与工作区校验。缓存命中保留并校验 OCI manifest/config 及数据层关联，旧缓存无证明时拒绝；原 Git rewrite/protocol 配置冲突在传输前拒绝。

最终冻结 candidate-v3，两机各61/61 Harbor驱动测试通过；共享夹具87项中76通过、11项明确特权跳过。candidate-v2曾因交付包缺C辅助源码导致夹具检查失败，未启动VM；补全交付清单后验证v3，失败日志保留。全量资格采用按磁盘预算选择的批次，逐批 capture；仅所选任务占用 verifier 副本预算。两任务且仅OCaml有缓存时新增副本上限为8 GiB，整体准入104 GiB，仍计入未压缩内存、根盘与tar+解包。此计算仅用于一次 Oracle＋一次 nop 的 controls，不适用于重复trial夹具。

全量 Oracle/nop 队列与回执位于 `/var/tmp/hb1008/all-tasks-20261009`，本地审计与候选清单位于 `/tmp/agentenv-harbor-all-20261009`。2026-10-10 第一轮全量编排结束，两机服务均正常退出，各43项已遍历；44批清理回执passed、宿主权限恢复退出0。严格Oracle=1/nop=0符合68/86（79.1%，A33/B35）；7项Oracle0且无框架异常，7项有传输或阶段超时，另4项因两次批次传输探针180秒超时未启动trial。实际164/172个trial结果。completed表示队列已结束，不表示全部通过。

82个模板准备及builder清理通过，164个trial均以模板启动、4c8192MiB、初始工作区校验通过，Sandbox ID各不相同，最终164个sandbox均deleted。两机无UID999测试进程、Lab cgroup或ublk残留，临时ACR认证已删除；失败payload继续保留。最终可用空间A63.7GiB/B139.8GiB。7项框架异常和4项预检阻塞需先检查驱动与测试方案；7项低分需再核对原任务/标准Docker，不直接归因Dragonball。

明细和最新报告在 `/tmp/agentenv-harbor-all-20261009/REPORT.md`，`failed-task-summary.json`、`failure-groups.json`、两机`*-latest.json`及`*-final-host-check.json`。额外动态缓存仍只覆盖OCaml Git，不宣称全部缓存或离线；模型评测保持停止。未提交、合并或推送代码。

### 驱动与传输预检排查（2026-10-10）

原始驱动在 B 复现 `break-filter-js-from-html` 的 Oracle 上传失败：文件 API
返回 HTTP 200 和正确路径，但下一条命令无法打开 `/tmp/hb-*.tar.gz`。
两种 guest 接口读取同一身份、同一文件；独立 `/tmp` 标记随后同时消失，
未发现串 VM。该镜像 PID 1 为 systemd，tmpfiles 策略包含
`D /tmp 1777 root root -`。用真实 `systemd-tmpfiles --remove --boot`
在归档生成与消费之间执行清理，上传、下载均稳定 RED；改用 `/var/tmp`
后相同测试 GREEN。启动清理与传输的竞争符合现场，尚未取得实际删除系统调用轨迹。

预检原来使用独立 1c2g，且外层固定 180 秒同时覆盖冷启动和传输。
A 的原始 MTEB leaderboard 镜像冷启动实测 223.95 秒，原任务启动预算为
600 秒；180 秒会提前终止合法启动。修订后预检沿用原任务策略、工作目录和
本轮资源覆盖（4c8192MiB），分别记录启动、180 秒传输、60 秒清理。
启动失败的内部删除另限 60 秒，归档删除另限 5 秒；两者保留原始异常，
传输成功后的清理失败仍抛出。异步截止为协作式，最终 Lab
回收仍保留。失败阶段记录不依赖预检正常返回。

冻结 `candidate-v4`（86 文件）在两机非 root 各通过 73 项 Harbor 驱动测试，
独立复审无未关闭重要问题。同一最终候选的 break-filter-js-from-html、
filter-js-from-html、path-tracing 原生复测共6个trial，均Oracle=1/nop=0、无框架
异常，6个独立Sandbox均deleted、Lab清理passed。真实复测结果见独立报告
`/tmp/agentenv-harbor-driver-diagnostics-20261010/REPORT.md`；旧 68/86 成绩保持原样，
修订候选的定向复测不能称为完整 86 项复验。诊断过程中临时夹具的脚本缩进、
回执读取路径和启动入口错误均保留，不计作产品测试结果。

最终候选 MTEB leaderboard / retrieve 独立冷启动传输预检均通过：startup
222.87 / 223.16秒，transport 7.13 / 6.97秒，cleanup均passed。任务保持
4c8192MiB，原始build预算600秒；结果直接证明180秒外层预算不足。
此夹具只执行传输探针，不算MTEB Oracle资格。

本轮最终两机宿主清理检查通过，无测试进程、Lab cgroup 或 ublk 残留，
临时认证已删除；可用空间 A 42.3 GiB / B 130.1 GiB，失败现场保留。

诊断候选的 path-tracing 原生上传异常消失，但该轮 Oracle 为 0：uv 下载报
`curl (18) Transferred a partial file`，随后 `uvx` 不存在，评分测试未执行；
尚未确定截断来自上游还是代理，不将这个 0 当作有效完整评分。该失败保留，
最终候选另行复验。

Caffe 原始 Oracle 日志显示 CIFAR 下载仍在低速进行，触发原始 1200 秒预算；
这是未缓存的动态下载问题。另 5 次 verifier 超时没有落地执行日志：固定 Harbor
在命令成功返回后才下载 verifier 日志，取消后 guest 已删除。现有证据不能区分
下载、计算或评分器卡住，不能归因 Dragonball，也未延长原始评分预算。

### 复盘措施与 Harbor 专用 OSS（2026-10-10）

已实现失败前置回执与有界 guest 取证：失败/取消后保存 verifier stdout 尾部、
文件系统、内存、进程和 cgroup 状态，不更改原始评分、超时或错误。日志不能保证
在 guest 已退出时仍可取到，取证失败单独记录，不据此吞掉原始错误。驱动同时
在启动和删除前记录实际 guest 文件系统容量与占用；任务 storage 声明、宿主
准入预留和硬配额分别记录，当前未实施硬配额。

公共下载缓存新增 CIFAR 官方校验值与 uv 0.9.5 归档配方；绑定固定任务、镜像
和内容摘要，Lab 外持久保存，命中也校验。缓存只覆盖这些明确归档，安装器和
其他运行期依赖仍可联网，未发布新 ACR 镜像。使用入口见 [README](scripts/harbor/README.md#失败取证下载复用与候选冻结)。

OSS 仅在 `run_lab.py --oss-env` 下替代本地 MinIO；默认项目及共享 M4 行为
保持原配置。提供 [.env.example](scripts/harbor/.env.example) 和
[执行文档](scripts/harbor/OSS.md)，真实 dotenv 必须私有并属于运行账号，
Git 忽略且 index 检查拒绝真实配置；生成配置只写凭据子进程，不写 AK/SK。
用户随后配置私有文件；两机对象仓库配置复测通过（读写、列举、分片上传与清理），
首次列举授权不足的失败回执保留。随后第二台服务器完成原始
`openssl-selfsigned-cert` 的 4c8g、单并发最小验证：真实 OSS 捕获、两个新 ID
恢复、首条身份及初始工作区一致，Oracle=1/nop=0、零框架异常；无 MinIO，
UID999 VMM，sandbox/Lab/宿主清理通过。云端本轮独立 prefix 的 11 个对象
保留，本地 payload 与失败现场保留；未验收跨机冷缓存恢复和跨批模板复用。
最小验证发现并修正 Harbor OSS 服务代理遗漏 guest 控制网段的问题，哨兵回归
先失败后通过；新候选 v8 两机各 99 项驱动测试通过。外层临时控制器的 host lease
传递失败亦保留。见 [OSS 执行记录](scripts/harbor/OSS.md) 和
`/tmp/agentenv-harbor-oss-smoke-20261010/REPORT.md`。

随后完成共享 OSS 模板最小资格：新增 `--shared-templates prepare/reuse`，
仓库固定为配置基础 prefix 下的 `shared-templates-v1`；任务/镜像/资源/运行
资产构成 key，不绑定宿主。默认批次模式保持 run ID 校验。索引按 snapshot ID
独立发布，读取核对 repository 和已提交 catalog；不使用条件 PUT、同名 alias
或分布式锁。`reuse` 缺失直接失败，Lab 清理不删除共享对象，持久 GC 未实现。

最终 candidate-v10 两机非 root 各 108 项驱动测试通过，无 skip。B 发布后，
两机各在新 Lab 从空快照缓存首次恢复同一模板，consumer 无 builder；三组
Oracle/nop 共 6 trial 均为 1/0，Oracle 各 6 项断言通过，零框架异常。新 ID、
初始工作区、UID999 VMM、无 MinIO、运行资源清理与宿主恢复通过；共享仓库
12 对象、773,517,666 bytes、catalog/index 各一，无未完成分片，消费不新增模板。
A 旧成功 Lab 经独立归属/无引用审计保存证据后回收 18.7 GiB；A 首次准备的
外部路径错误及失败现场保留，同一归档经 SHA/全部 OCI blobs 校验交付后通过。
这只验收两机相同 Xeon CPU 的 openssl 最小任务，未复验全部 86 项、异构恢复、
并发生产者、容量或共享模板 GC。证据：
`/tmp/agentenv-harbor-shared-20261010/REPORT.md`；入口见 [OSS 文档](scripts/harbor/OSS.md#两台物理服务器共享任务模板)。

冻结 candidate-v7（101 文件），两机非 root 各通过 98 项 Harbor 驱动测试及
23 项共享性能夹具测试，无 skip；包含无 MinIO 分支、凭据隔离、强制加入 `.env`
拒绝、取证取消、缓存损坏/断续读取和无网络复用。独立审查 v5 无未关闭重要
问题，v7 仅增加三项回归用例；未改变运行实现。证据统一记录在
`/tmp/agentenv-harbor-retro-20261010/REPORT.md`。没有重跑完整 86 项资格或模型评测。

B 已冻结 CIFAR 二进制归档（170,052,171 bytes，官方 MD5 一致）和 uv GNU
归档（21,370,871 bytes，官方 SHA256 一致）。官方 uv 安装器从本地缓存安装，
`uv --version` 正确；最终准备入口两次在网络调用明确拒绝时复用全部绑定，
源站请求为零。首次 CIFAR 串行下载较慢，完整分段归档校验后以 SIGINT 取消
冗余准备；未完成文件清理成功，临时分片亦已删除。分段夹具只用于此次准备，
不计作原任务 Oracle 通过或通用并行下载实现。

候选冻结工具已实现；通用统一 VM 控制器及托管 CI 执行结果仍未完成。
已有显式计划执行器支持输入绑定和断点复用，尚未形成全套件通用恢复编排。
首轮两次单元失败分别为 dotenv 重复键检查缺陷和诊断测试继承不可访问工作目录，
修订后最终候选通过；原日志保留，不把中间失败记录抹掉。

### 共享模板复盘后的工程保障（2026-10-10）

交付工具使用完整字节数和 SHA 校验 `.part`，发布独立 inode 的副本，原子可见且
不覆盖已有归档；生产者后续写入不改变已发布产物。候选冻结可显式纳入外部控制、
准备、宿主设置/恢复及取证脚本，执行前核对完整文件集和摘要。冻结不改写旧绝对
路径，计划必须显式调用冻结副本并绑定实际宿主输入。实际账号可读性先于准备检查。

`prepare_acr.py --cached-archives` 接受各机明确的归档路径、SHA 和字节数，检查
全部 OCI blobs 及原 manifest/config/layer descriptor，不依赖旧批次 prepared.json。
`qualification.py` 校验资源/资产/模板一致、每任务恢复覆盖、不同身份、原始
Oracle/nop 和首次恢复前的本地缓存边界；宿主 OS page cache 保持不变。
Make 检查入口统一固定 Python 3.12+，旧版本在依赖导入前拒绝，失败退出码不被
日志命令覆盖。操作命令与示例统一见 [README](scripts/harbor/README.md#交付缓存准备与阶段检查)。

冻结 candidate-v11（118 文件，含六份外部控制脚本），两台服务器以 UID999
各通过 124 项驱动测试，零 skip。各机通过已有阶段执行器完成输入预检、同一
44,184,576-byte 镜像归档的缓存准备、原有真实回执资格检查及小型候选包传输/发布。
模拟传输退出码 7 被原样保存，发布阶段未执行、final 不存在。首次候选归 root
且权限 0600，阶段绑定在工作负载前拒绝；修正归属后使用新输出目录验证通过，
首次失败保留。独立代码复核无未关闭问题。

原有三组回执自动检查通过：6 trial、两个首次本地冷恢复、8 个不同恢复 ID。
这是对已完成 VM 实验的回执检查，本轮没有启动新 VM、模型评测、云端操作或
镜像发布。通用 VM 控制器、全套件资格及托管 CI 仍待完成；不据此宣称这些已验收。
证据：`/tmp/agentenv-harbor-improvements-20261010/REPORT.md`。

## 工程措施与待完成项

已实现：资源覆盖、并发参数、CPU/内存/磁盘准入、批量准备、明确任务筛选、批次总预算、原始/有效资源回执。新增 `make test-harbor-tools` 与 CI job 使用固定 Harbor 源码；远程执行结果另记，不将已编写 CI 当成托管 CI 已通过。

2026-10-10 已启动最新驱动的完整 Oracle/nop 资格复测：86 项、172 trial，两机
各 43 项，4c8g、每机并发上限 8，实际批次由磁盘准入选择。冻结 v13 纳入两份
本轮编排脚本，Harbor 驱动与已验证 v11 一致，两机各 124 项检查通过。运行前
只回收有成功/完成清理/精确身份/无引用/非 pin 证明的临时 payload；使用 OSS
共享模板、持久镜像归档和 B 的 CIFAR/uv 公共缓存。首项 openssl 的严格
Oracle=1/nop=0、Lab 清理及宿主恢复通过；其余仍在运行，不合并为全量通过成绩。
回执 `/var/tmp/hb1008/terminal-benchmark-20261010/shard-{0,1}/campaign.json`；
进度与独立报告 `/tmp/agentenv-harbor-benchmark-20261010/REPORT.md`。本轮先运行
资格 controls，尚未调用模型；旧模型成绩与旧资格分数保持原记录。

| 优先级 | 待完成工作 | 验收 |
| --- | --- | --- |
| P0 | 正式模型轮数、thinking、超时口径 | 用户确认参数，先独立严格 Oracle 资格，再运行模型；继续保持全量模型评测停止 |
| P1 | FilterJS 等 verifier 实际覆盖 | 官方分数保留；检查关键步骤执行与 OOM/残留，报告可信性 |
| P1 | 失败任务重新资格验证 | 有界取证与公共归档缓存已准备；继续真实任务 Oracle/nop，保留阶段日志，不能反推旧5次超时原因 |
| P1 | 动态依赖缓存与模板复用 | OCaml Git 首轮资格已验证；86项入口已扩展，更多动态配方与跨批持久模板资格仍按回执推进 |
| P1 | Harbor OSS 扩大验收 | 最小任务跨机冷恢复、跨批共享模板、Oracle/nop 与清理已通过；更大范围、异构兼容、并发生产者与持久 GC 待验证 |
| P2 | 全套件统一编排与恢复 | 固定候选、断点续跑、归属审计、轻量进度汇总；目前未具备通用自动化 |

后续每次评测先校验方案与口径，再归因产品或性能。新增结果只写一次权威回执，摘要核对输入、规格、阶段、评分与清理；不把不同资源或不同任务探针合并成同一结论。

2026-10-10 全量队列续跑记录：A 因磁盘准入暂停后，按用户对历史目录的明确
授权，归档 51 个非 Harbor 旧性能 Lab 的日志/配置/结果并审计路径身份、pin
和引用，再回收临时 payload 与旧构建产物 141.6 GiB，可用 57.5→199.1 GiB。
实际执行输入摘要一致，当前失败及持久依赖、镜像、共享 OSS 保留；只接续
剩余 38 项。B 的无绑定下载缓存参数在 Lab 启动前被拒绝，修正外部控制器
按当前批次筛选缓存参数后接续未执行的 41 项。v14/v15 的 Harbor 驱动与 v13
完全一致，两机各 124 项检查通过，三项调度回归先失败后通过；原始预算、
4c8g、每机并发上限 8 和已完成分数保持不变。续跑回执在同一 run 下
`shard-{0,1}-resume-01/campaign.json`，活动指针 `active-shard-{0,1}.json`；
清理回执和首次错误、队列进度统一见上述独立报告。全量仍运行，未调用模型。


### 全量资格队列推进与 uv 缓存范围核对

只读观察器实时输出 `/tmp/agentenv-harbor-benchmark-20261010/LIVE.md`，以完成
回执统计；全量队列持续运行，仍未调用模型。GPT-2 和 HF 推理在 4c8g 下的
Oracle=1/nop=0 及所在批次清理已完成。核对的首 33 项共 66 个不同恢复 ID，
两机资产摘要相同、4c8g、UID999、OSS 模板、Lab 清理及宿主恢复一致；后续
新增任务以实时回执为准，不将未结束批次的部分分数提前计为资格。

当前未合格原因分别记录原始证据：Cython 的第三方库断言、Caffe 原预算超时
且输出停在 Git clone、pmars 固定 apt 版本获取失败、code-from-image 的 uv
下载中断导致 Oracle verifier 未运行、headless-terminal 的背景 HTTP 服务
连接拒绝（Oracle 6/7 断言通过）。原分数保留；没有证据将其统一归因 Dragonball，
也不因此排除 Linux 桌面/终端类任务。详情及每份日志入口见上述独立报告。

固定源码中 82 项引用 uv 0.9.5；排除三个已有嵌套 VM/Windows 任务后，适用
79/86 项。两机从同一验证归档准备新绑定，原任务与镜像摘要逐项校验，明确
禁止上游请求时准备通过，仅保存一份 20.4 MiB 归档；其余 7 项无此版本绑定。
新输入在各机本轮目录 `downloads-all-v1/downloads.json`，绑定摘要相同，不
覆盖正在运行的两项绑定或既有成绩；CIFAR 缓存另保留。这是冻结 v16 外部准备
脚本的实际验证，尚未接入仓库 `prepare_downloads.py` 的通用入口，未验收所有
apt/pip/Git/模型依赖离线，也没有在本轮切换缓存后重测失败任务。

### 持续补位设计与新增失败取证

核对远程实际 Harbor 0.24.0 源码：`TrialQueue` 已以 semaphore 持续补位，
整批屏障来自外部 campaign 等整个 `run_lab.py` 退出并恢复宿主后才开始下一批。
Harbor 会记录普通 environment stop 异常并继续发出 END，故 END 不能独立
作为资源释放证明。书面设计采用每机一个持久 Lab、八个 worker、原生单 trial
Job；probe、builder 和消费者共用八个槽位，单项清理确认后补位。镜像与模板
增量准备，磁盘不足允许低于八并发；模型模板启动与 verifier 缓存分离，仍保留
原始 verifier、预算和输入身份校验。设计及实际安装源码摘要位于
`/tmp/agentenv-harbor-continuous-20261010/DESIGN.md`、`evidence/source-sha256.json`。
该记录为当时的设计基线；后续实现与冻结候选验收见本文“驱动修复交付”。旧模型队列保持停止，未变更原始输入或成绩。

下列仅为本轮原始日志取证，未修复或重新资格验证；不据此排除任务或改写成绩。

| 任务 | 已证实的失败阶段 | 尚未确认的原因 |
| --- | --- | --- |
| make-doom-for-mips | Oracle 的 Debian 包下载 404，随后 make 失败；verifier 执行超时且缺少 frame.bmp | 下载状态与编译失败的完整因果链、修正依赖后的执行能力 |
| mcmc-sampling-stan | apt DNS 失败；RcppEigen 链接缺少 gfortran、RcppParallel 配置缺少 cmake，rstan 未安装 | sudo 是否清除代理、固定依赖版本及网络路径的影响 |
| rstan-to-pystan | apt DNS 失败、固定 build-essential 版本不可获得、add-apt-repository 缺失，最终缺少输出 CSV | sudo 是否清除代理、版本可用性与依赖配置的分别影响 |
| overfull-hbox | Oracle=1，四断言通过；nop verifier 超时 360 秒，无 reward | nop 卡住的具体命令与原因，不能称为 Oracle 失败 |
| protein-assembly | 原始 Oracle 的 DnaChisel 约束求解抛 NoSolutionError，未生成 gblock.txt | 包版本、算法随机性及容器基线差异 |
| pytorch-model-recovery | Oracle verifier 超时 900 秒；Oracle 日志另有缺少 NumPy 的 warning | verifier 超时根因；该任务期间节点配额发生变更，warning 不作为根因证明 |

证据来自 B 的 `shard-1-resume-01/batch-{010,013,015,016,019}/warm/jobs/`
原始 result、verifier/test-stdout 与 agent/oracle.txt，摘录保存在上述专项目录
`evidence/failures.jsonl`。B 的配额守护器持续应用到后续 Lab；07:25、07:27 UTC
变更回执分别标识 sanitize-git-repo、sparql-university，仍按混合配置历史报告。

## Harbor 集成过程与研发效率复盘（2026-10-11）

范围：本次会话中的 Harbor 集成、两机资格验证、持续执行改造和 2026-10-10
已取证的 29 项模型超时。不是当前整轮成绩更新，也不将改善前后的不同规格
结果合并。依据是上述原生记录、冻结验收报告、现有 Make/CI 和实际实现，
而非只凭通过数或会话中的解释。

主要成本来自测试契约和驱动边界缺失：冒烟参数进入全量、配置与实际配额不同、
外部依赖没有覆盖、失败现场清理后无法追溯、批次屏障降低槽位利用率。
我在执行中也出现了过早归因、复用错误环境参数和一次性控制脚本返工；应由
可检查的执行入口承担这些约束，不能继续要求每次人工记住。

### 按严重性排序的改进候选

以下均是候选建议；本次只做复盘及记录，未实施新运行行为。

| 优先级 | 问题与事实 | 建议改进 | 可检查的完成条件 |
| --- | --- | --- | --- |
| P0 | Agent 超时现场缺失。`EvidenceVerifier` 只在 verifier 抛异常时取证；AgentTimeout 后 verifier 正常返回 0 分不会触发，CompCert 内部 opam 日志已经无法取得 | 在 trial 错误路径、删除前复用有界取证，记录失败阶段、进程、资源、dmesg 和关键重定向日志；取证失败与原错误分开 | 注入 AgentTimeout＋正常 0 分 verifier，证明取证在 DELETE 前发生；guest 不响应时按独立期限退出，原异常与评分不变 |
| P0 | 资格验证证明了特定 Oracle 路径，没有证明 Agent 所需外部输入可用。视频 Oracle 不下载 YouTube，实际 Agent 被机器人验证阻断 | 为存在外部输入的任务增加素材可用性及版本检查，结果与 Oracle/nop 分开；缓存真实输入，不缓存解答 | 断网、401、源内容变化能在模型前发现；失败项有独立理由；正常缓存按源和 digest 核验 |
| P1 | OCI 文件存在不代表 VM 初始化后的运行环境等价。Mailman 安装脚本用 tmpfiles 创建 `/run/mailman3`，guest 挂新 `/run` 后没有补该规则，Agent 直接调用 CLI 又绕过包的 init 脚本 | 明确 guest 启动兼容性契约；对包声明的运行目录初始化做最小验证，优先复用规则而非硬编码单个应用 | 验证目录、owner/mode、首次服务启动；保留 tmpfs 生命周期、不复制旧 PID/socket；模板恢复和 pause/resume 不重新执行冷启动初始化 |
| P1 | 冒烟 25 轮沿用于旧全量，旧资源单测主动要求小批次降低节点配额。124 项通过也没发现契约本身错误；这些历史行为已修正 | 在既有准入入口核对一份生效配置：声明、覆盖、实际读回、agent 预算、候选和资产身份；跨机/跨阶段比较同一 profile | 对不一致 profile、隐藏轮数限制、资源覆盖遗漏注入失败；在模型请求前拒绝，显式实验差异单列 |
| P1 | 工作区已有 Harbor Make/CI 接线，不能称为没有 guardrail；但新增检查器和大量实现仍未纳入 Git，托管 CI 也没有执行证明 | 收口可审阅的变更，确保目标 checkout 包含入口和依赖；复用 `make test-harbor-tools`，验证托管运行 | 干净 checkout 可运行指定入口；托管检查覆盖新增文件和 Linux 夹具；通过状态来自相应 revision，不只是远程冻结副本 |
| P2 | 多次一次性控制器踩到 HOME/权限、代理、host lease FD、信号转发和外部路径；已有冻结/交付工具，但外围操作仍容易脱离它们 | 复用现有控制入口和租约生命周期，把剩余外部操作也绑定候选；局部修改只复验受影响阶段 | 故意丢 FD、设错误 HOME/owner、取消执行时准确拒绝或保留完整回执；重试仅执行未成功或输入失效的阶段 |
| P2 | 反复取完整远程轨迹、逐次 SSH 查进度增加等待与上下文成本；首次证据传输因 150 秒期限截断 | 使用同一只读观察器和远程摘要；按失败任务索引取完整证据，校验 SHA/CRC 后发布，本地保留复用 | 进度一次读取；不重复下载未变文件；截断包不进入报告；原始结果可由摘要追溯 |
| P2 | 已有事件和资源回执，但尚无等价效率基线；配置八槽不能证明八台 VM 活跃，命令 duration 也不是可消除空等 | 从已有事件自动统计活动槽、准备/恢复/安装/agent/verifier/清理时间、缓存 prepared/bound/consumed 和重跑原因 | 固定任务、规格和候选才能比较；分别报告虚拟盘、实际块、准入预留和硬限额；不给不同批次墙钟套用提速比例 |
| P3 | 导航跨 HARBOR_INTEGRATION、README、工程日志及多个 /tmp 报告；旧阶段的“未验证”与后续完成状态交错，增加重复探索 | AGENTS/CLAUDE 保留一个有条件的 Harbor 导航指针；当前状态只维护一处，历史证据按阶段引用 | 从指针能找到当前入口、配置、阶段状态和证据；固定命令由 Make/CLI 维护，避免在多个指南复制 |

### 已有效的改进，不再重复建设

- 固定节点 64c/128GiB 和 guest 4c8g 已有实际读回。跨批次数量和
  controls/model 的资源契约测试已存在；不再新增另一套资源测试入口。
- 持久 Lab、删除确认后补位已通过两机九项长短任务验收，实际峰值八台 VMM；
  第九项分别比首个慢项清理早 42.33/105.99 秒。这证明补位行为，未量化全量提速。
- 共享 OSS 模板已有跨机、空快照缓存恢复验证，独立 ID 和可写状态保留；
  ACR 依赖、uv/CIFAR 下载缓存已有内容校验和复用证据。已准备与实际消费仍需分别统计。
- 精确清理回收 A/B 约 135.1/291.1 GiB；水位、引用检查及删除屏障已接入。
  64 GiB 虚拟容量与 10 GiB 声明仍不是硬限额，不能用混淆这两者的方法改善并发。
- 最终 v29 两机各通过 163 项 Harbor 检查和 2 项恢复控制测试。已有 Make/CI、
  凭据 index 检查和冻结摘要工具应继续复用；远程通过不代替托管 CI。

### 对实现与审查的要求

机械问题优先交给既有测试、准入和 CI：规格一致、候选摘要、阶段期限、删除
屏障、缓存绑定、退出码和凭据索引。规则不应只写进 AGENTS 让实现者反复记忆。
审查重点是无法由单点断言替代的判断：资格是否覆盖真实输入路径、缓存是否改变
解题条件、模板与 resume 是否混用初始化、异常和评分语义是否被重新解释。

先补 Agent 超时取证和实际输入资格，再验证运行目录初始化；随后收口 CI 与
外围操作。继续保留原始结果、预算和失败，不用扩大超时或筛掉失败项掩盖驱动问题。
本次没有运行新任务、调用模型、改变候选或提交代码；文档更新不等于这些建议已完成。

来源：现有[驱动修复方案](docs/src/internals/harbor-benchmark-harness-repair.md)、
[超时归因](docs/src/internals/harbor-qwen-timeout-analysis.md)、
[测试入口](scripts/harbor/check_tools.py)、[verifier 取证](scripts/harbor/failure_evidence.py)、
[CI](.github/workflows/regression-tools.yml)。历史原生报告在
`/tmp/agentenv-tb-full-20261009/FAILURE_ANALYSIS.md`、
`/tmp/agentenv-harbor-qwen-comparison-20261010/harness-repair/REPORT.md` 和
`/tmp/agentenv-harbor-qwen-continuous-20261010/timeout-investigation/REPORT.md`。
后续 Mailman 包初始化检查保存在同一取证目录的 `mailman-package-init.txt`。

## 复盘改进执行（2026-10-11）

用户明确确认执行后，在既有 `test/harbor-dragonball` worktree 实施，不改原始
任务、评分器、模型阶段预算或旧结果。本轮不调用模型、不覆盖旧 ACR tag、
不自动重建旧模板；变更未提交、合并或发布。

| 项目 | 当前实现与验收边界 |
| --- | --- |
| 超时取证 | `AgentENVEnvironment.stop` 在 DELETE 前读取固定 Harbor 已写入的异常标记；AgentTimeout 后 verifier 正常 0 分也取证。独立总期限 15 秒，保存有界资源/dmesg/verifier/opam 尾部，凭据脱敏；原错误和 reward 保留。14 项定向检查通过 |
| 素材准入 | 新清单绑定 revision、task SHA、实际 source 和内容 SHA/大小，主机及 guest 独立检查。已知视频/MTEB 缺少输入或 unchecked 会阻止模型，该任务记为未执行，保留分母和 controls；14 项素材检查与 8 项入口检查通过。未扫描全套任务的所有外部源，也未实现视频 URL 缓存 delivery |
| 运行目录 | 共用 `pivot-init` 在冷启动挂载 `/run` 后应用 package tmpfiles；不清 `/tmp` 或复制旧 PID。真实 Linux 两项通过；DB 冷启的目录/权限/写入、新 ID snapshot 恢复及同 ID pause/resume 已通过；FC 冷启目录/权限/写入和 Mailman daemon start/status 已通过，额外 LMTP readiness 未通过，根因待定 |
| 生效配置 | 模型执行前核对实际配额与 agent budget，输出含资源、task contract、driver/runtime SHA 的 profile。节点仍 64c/128GiB、guest 默认 4c8g、最多八槽；低配额、25 轮、阶段超时覆盖的四项检查通过 |
| 摘要与操作 | 从原生来源派生只读摘要，原生 SHA 可追溯，model/control 分开；槽位积分与实际 VMM 峰值分开，未知阶段/账单/缓存消费为 null。19 项摘要检查通过，另有 2 项报告写入隔离检查。继续复用现有租约、setup/restore、阶段执行与冻结交付入口，不另建后台调度器 |
| 源码与 CI | 冻结包含全部新 Python、tools 配方及其 envd patch，CI 增加 tools 触发路径和 busybox 夹具依赖；最终候选 119 文件：A 冻结源、B 独立干净 Git fixture 各 220 项 Make 检查通过，源 SHA 前后不变；B 全部 119 文件已纳入临时 Git index。托管 CI 必须在提交后触发，远程通过不代替托管检查 |

`tools-image` 新配方为 0.1.3，本轮 VM 使用独立的诊断 tools 0.1.3-retro.1；
已发布 tools、默认依赖清单及已有模板未改。恢复旧模板不会执行冷启动脚本，
需要显式准备绑定新 assets 的模板才能应用修复；`reuse` 不会隐式重建或覆盖。
Mailman 额外配置、CompCert 安装等待及新模型通过率仍需各自证据，不将目录
修复当作全部超时的解释。尚无固定任务与规格的等价效率基线，不宣称提速比例。

操作契约只在[驱动入口](scripts/harbor/README.md#素材准入生效配置与失败摘要)
维护。独立审查发现摘要 CLI 可覆盖既有非摘要文件，已用失败夹具复现并修复；新目标及合法摘要重复生成保留，源码、凭据文件、其他报告和软链拒绝覆盖。v2 复核无未关闭重要问题。

本轮还对旧轮两份原始报告做只读摘要核对：A 42 项、B 32 项，合计 74 项原生结果，32 项成功、9 项正常 0 分、33 项异常；这是旧轮记录，不是本轮重测成绩。

本轮完整原始证据在 `/tmp/agentenv-harbor-retro-20261011/`，含 baseline、
定向 RED/GREEN、不可变 review patch、两机检查及 VM 控制器回执；最终验证状态
以该目录 `REPORT.md` 为准。

## 合并范围审计（2026-10-12）

Flash/Max 的实际节点与 guest 配额、任务契约、模板及 server/runner/kernel/tools
摘要一致；Agent 参数仅模型名称变化。本次合并 Harbor 驱动、测试与文档，不修改
生产配置、默认资产选择或 guest 初始化。历史 `/run` 修复、两项相关测试及 tools
0.1.3 配方留在 `test/harbor-dragonball` 工作区单独处理。原先 220 项 Harbor 检查
包含这两项 guest 初始化测试。本次排除它们，并新增冻结旧基线的运行依赖回归，
合并范围共 219 项，不能把历史 220 项当成本次结果。

合并审查复现冻结器仍保留旧 Lab、遗漏 capability runner 的问题；新增真实导入
回归先失败，再显式覆盖当前 Lab 与 runner 后通过。不改变 guest、预算或模型结果。

[Flash/Max 对比报告](docs/src/internals/harbor-qwen-comparison.md)记录相同 71 项的
得分、Token、耗时、回退和限制。私有 `.env`、字节码及原始过程数据不纳入 Git。

合并候选在 A 机以 UID999 复验：Harbor 219 项通过（无 skip），共享夹具
207 项检查通过（17 项显式特权/真实 VM 检查未执行），回归工具 30 项通过。
178 个冻结输入测试前后摘要一致；独立审查无未关闭重要问题。首次扩展检查因
测试包缺少既有 Rust revision 夹具而失败，补齐不变的输入后复验通过，失败日志保留。
本次未运行模型、重建模板或发布 tools，复用已有两轮运行证据。验证与环境审计
记录位于 `/tmp/agentenv-harbor-merge-20261012/`。
