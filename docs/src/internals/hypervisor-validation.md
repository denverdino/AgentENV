# Hypervisor 验证摘要与已知限制

最后核对：2026-10-08。本轮候选基于 `84e21d5`，增加 DB 默认环境身份读取和
guest 首次 MMDS 立即刷新；冻结补丁、二进制及验证边界见下方“身份刷新优化”。
仅同 ID 暂停恢复跳过内部身份轮询，新实例仍确认准确身份。较早的完整矩阵使用
`be6b051`，后续阶段观测使用 `1a8fce1`；各轮输入与结果分别保留。
以下证据只适用于对应输入，不代表后续修改自动通过，也不代表生产 SLO 验收。
日常执行方式见[功能回归](dragonball-regression.md)。

## 已验证范围

| 候选 | 验证结论 | 边界 |
| --- | --- | --- |
| `a99fd09` 启动/tools 策略 | runner/protocol 76 通过、11 ignored；envd 15、DB 单元 60 通过；11 项生命周期及 1 项真实 probe 保存/恢复通过 | 覆盖初始化上下文、鉴权/网络、held 身份、暂停恢复、发布/卷、回滚及重启；未重跑全量阶段/应用矩阵 |
| `ab3db37` 测试结构 | 编译后 78 个集成用例逐项保留；工具测试 79 通过、9 项需显式 namespace/controller/physical 输入而跳过；4 项真实冒烟通过 | 默认 60 个独立用例；原重复选择消除，旧 revision 身份专项保留为 opt-in |
| 两个候选 | workspace 全目标/全 feature 严格 Clippy、fmt、独立评审及测试资源清理通过 | GitHub 托管 CI 需在实际推送后确认 |
| `be6b051` 冻结运行时，本次 Linux x86_64/KVM 实跑 | DB 默认功能回归 60/60、平台测试 6/6 通过；功能回归无跳过项 | 功能入口源码为 `3f1a844`；后续仅改测试工具和文档，未改 Rust/Go 运行时或配置 |
| `1d23f66` 测试工具 | Linux regression 工具 20/20；M4 工具 79 项中 70 通过、9 显式跳过 | 另行实跑 Lab 4/4、控制器 SIGTERM 清理 2/2、部分创建成功后失败的资源回收；physical 专项未执行 |
| `5eb08f4` 测试工程加固 | Linux regression 工具 20/20；M4 工具 91 项中 81 通过、10 显式跳过；另行实跑 SIGKILL 恢复 1/1、既有 Lab 隔离/清理 4/4 | 真实外部命名空间 FD、绑定挂载和根目录 FD 会阻止误报 `clean`；未重跑 VM、OSS30 或性能矩阵，运行时未改 |
| `b813d45` 性能口径统一 | Linux regression 工具 20/20；M4 工具 94 项中 84 通过、10 显式跳过；创建诊断 120/120 | 两端共用准确身份终点与剩余总预算；取证失败保留已执行样本的实验编排注入检查通过；运行时未改 |
| `1fe9c99` 工程保障与观测工具 | 最终 Linux UID 999：M4 工具 165 项中 152 通过、13 显式跳过，regression 工具 20/20；另行 Lab 7/7 真实测试 | 包含有界 pipe、显式预检、归档回收、输入绑定续跑及观测；网络复用对照 40/40，修正后的生命周期矩阵 880/880；Rust/Go 运行时及配置仍与 `be6b051` 相同 |
| `b9848bc` 同 ID 恢复优化 | 全工作区 fmt/clippy、DB 单元 61/61；第二台真实生命周期 11/11、启动记录器 1/1，第一台同 ID 恢复回归 1/1 | 新身份与原身份两方向故障注入；保留快照/fork 的即时身份断言；未重跑完整功能/平台矩阵、OSS30、640 MiB 或生产容量 |
| `13b6f7a` 分段性能工具 | Linux regression 20/20；M4 167 项中 154 通过、13 显式跳过 | API 与客户端身份确认分开计时，失败保留原错误；双方补充恢复准备阶段日志 |

本次验证了功能隔离入口与 M4 Lab 的跨账号主机互斥、cell 前后及跨 cell 输入
一致性检查，以及 HTTP 连续分块响应的总读取预算。后续增加 M4 归属清单和独立恢复
入口；锁本身不执行资源回收。最新 P1 将互斥扩展到显式 regression 准备与 M4 build，
任意直接 Cargo 和其他 Make 入口仍未覆盖。详见[开发回归](dragonball-regression.md)。

## 性能结论

2026-10-08 同机创建诊断固定 1 vCPU/1 GiB guest、并发 1、相同内核/tools/rootfs、
CPU 0–7、NUMA bind 0 和 node 4 CPU 配额。node 内存硬上限 32 GiB，夹具独立预算
16 GiB。两端统一从 API 发起计时，直到相同 guest 命令返回准确 sandbox/template
身份；采用相同 5 ms 轮询和 180 秒总预算。API 耗时另列，不当作相同的就绪终点。
两批各 15 次，第二批反转后端顺序；每个后端、缓存条件共 30 次。所有 120 次均成功，
无失败、未执行或 OOM；8 个 cell 的输入核验、缓存/实际 tools inode 与打开标志、
CPU/NUMA 取证及清理通过。测试前约 64 GiB 可用，满足 40 GiB 整轮准入，未追加删除；
测试后约 57.2 GiB 可用，临时权限已恢复，测试进程、设备与 cgroup 无残留。

下表单位 ms，p50/p95 使用 nearest-rank；全部样本包含每个新服务进程的首个 guest。
“后续 p95”仅作首个样本敏感性分析，每端/条件 28 次，不替代全样本结果。

| 条件 | FC 身份就绪 p50 / p95 | DB 身份就绪 p50 / p95 | FC / DB 后续 p95 |
| --- | ---: | ---: | ---: |
| tools 全部驻留 | 710.4 / 993.4 | 712.9 / 984.5 | 723.9 / 726.1 |
| tools 文件缓存清空 | 730.7 / 995.0 | 737.0 / 987.4 | 747.9 / 750.5 |

本轮 DB p50 分别高 2.5 ms（0.36%）和 6.3 ms（0.86%），不据此宣称普遍性能优势
或回退。两批方向一致：暖缓存 FC/DB p50 为 711.6/713.1、710.4/712.6；清空缓存为
731.6/737.3、730.7/737.0。每端/条件的两个首个样本都位于各自最慢两次，主导本轮
p95；FC 首个样本 993.4–1023.6 ms，DB 为 984.5–1040.1 ms。样本量只支持诊断，
不提供 p99、完整存储冷缓存、应用吞吐、生产容量或 SLO 资格。

暖/清空缓存的 API p50：FC 666.5/684.5 ms，DB 707.1/733.3 ms。FC 在 API 返回后
身份确认耗时 p50 为 45.3/45.6 ms，DB 为 5.7/3.6 ms；DB 60 次外部探测均一次正确。
DB 原始分段日志显示，Status RPC 完成至 envd HTTP health 就绪的中位数为 546.9/570.1 ms，初始化后
到身份确认约 46.6/43.7 ms。前一阶段值得进一步区分 guest 启动与探测等待；当前
日志尚不能证明具体瓶颈或优化收益。各阶段中位数不应相加当作总体分位数。

后续 `1fe9c99` 工具的网络复用专项对照共 8 组、40 次创建，全部通过；两批反转
后端顺序，每个后端/配置包含 2 个首个 guest 和 8 个后续 guest。固定 tools 驻留，
其余资源与身份终点沿用上述配置。`pool.network.enabled=false` 只关闭维护，
`high_watermark=1` 仍缓存已释放的网络 slot：日志逐组确认复用 4 次、归还 5 次；
水位设为 0 后两者均为 0。旧表的缓存配置应按此解释。

| 后续 guest 的平均身份就绪耗时（ms） | FC | DB |
| --- | ---: | ---: |
| 保留 1 个已释放网络 slot | 715.6 | 725.0 |
| 不保留网络 slot | 743.1 | 746.3 |

DB 的后续 pre-runner provisioning 均值从 52.5 ms 增至 82.0 ms；首个 guest
分别为 202.9/204.3 ms。关闭复用后，首次创建的额外耗时仍约 250 ms，说明网络
复用只解释其中一部分。小样本用于诊断，不与上一轮 120 次结果混算，不据此承诺优化收益。

新增 trace 在全部 20 次 DB 启动中观察到：health 等待开始至首次失败完成平均
313.0 ms，首次至第二次失败完成平均 214.1 ms，后续失败间隔平均 4.3 ms；没有
触发 1 秒探测超时。这两个长区间在网络复用开关、首个及后续 guest 中均存在。
第二段包含重试 sleep，日志未记录每次请求开始，不能当作精确请求耗时。guest 启动
与探测重叠，现有证据仍不能把约 527 ms 全部归因于可消除的网络等待；优化前需关联
ARP/TCP、serial 和 envd 开始监听的时间，当前未修改运行时参数。

### 生命周期与资源观测

后续生命周期矩阵固定 1 vCPU/1 GiB、并发 1，guest 驻留 512 MiB；低/高脏页
每轮分别写约 10.24/512 MiB，轮间休眠 200 ms，实际写速率受执行时间约束。
沿用相同内核/tools/rootfs、CPU/NUMA 和 node/夹具预算；tools 每次创建前验证全驻留，
网络释放缓存设为 0，其余缓存自然演进。两批反转后端顺序，每个后端/脏页条件
20 轮，包含两个服务首个 guest。8 个分组、880 条操作及内存校验全部通过，
无未执行项、node/夹具 OOM 或输入变化。下表单位 ms，nearest-rank p50/p95；
样本量用于诊断，不提供 p99 或生产 SLO 资格。

| 脏页 | 操作 | FC p50 / p95 | DB p50 / p95 |
| --- | --- | ---: | ---: |
| 低 | capture API | 8496.4 / 11007.7 | 8627.3 / 10746.2 |
| 低 | restore 至身份就绪 | 513.2 / 568.6 | 591.7 / 699.2 |
| 低 | fork 1 至身份就绪 | 3080.8 / 3619.0 | 3341.3 / 4121.8 |
| 低 | pause API | 3037.8 / 3081.9 | 3187.4 / 3210.0 |
| 低 | resume 至身份就绪 | 240.9 / 255.6 | 304.7 / 333.0 |
| 高 | capture API | 8487.1 / 10420.2 | 8608.4 / 9739.7 |
| 高 | restore 至身份就绪 | 540.5 / 565.6 | 605.9 / 725.7 |
| 高 | fork 1 至身份就绪 | 3088.8 / 3764.4 | 3340.0 / 3369.5 |
| 高 | pause API | 3036.6 / 3099.5 | 3184.8 / 3218.5 |
| 高 | resume 至身份就绪 | 245.7 / 286.2 | 310.0 / 343.1 |

本轮 DB 的 capture p50 高约 1.4%–1.5%，fork 高约 8%，pause 高约 5%；restore
多 65–79 ms，resume 多约 64 ms。两批 capture 中位数方向一致；部分 p95 DB 较低，
但不足以认定尾延迟普遍更优。结合前述创建结果，当前证据不支持 DB 整体更快。

| 脏页 / 后端 | capture 心跳最大间隔 p50 / p95（ms） | capture 成功逻辑上传 p50（MiB） | node 采样内存最大值（GiB） |
| --- | ---: | ---: | ---: |
| 低 / FC | 2826.1 / 5349.3 | 610.7 | 9.22 |
| 低 / DB | 3059.6 / 5182.6 | 596.8 | 8.47 |
| 高 / FC | 2803.8 / 4763.9 | 609.3 | 9.62 |
| 高 / DB | 3035.5 / 3064.5 | 597.1 | 8.95 |

心跳使用主机接收时间，包含 guest/host 调度及网络，不是纯 VMM 暂停；所有 capture
均满足前后包络和序号连续。恢复的快照包含探针进程，不能声称完全无探针开销。
内存按 10 ms 采样，是整个 node（含服务、VMM、页缓存等）的观测下界，不是单个
VMM 的 RSS；对应累计 cgroup 峰值低/高为 FC 9.22/9.65、DB 8.47/8.95 GiB。
上传字段来自成功 repository 上传计数，不包含所有重试及元数据路径。
restore 至完整内存校验结束的 S3 下载 IP 字节 p50，低/高分别为 FC 611.8/610.6、
DB 597.8/598.4 MiB；包含 TCP/IP 头、重传及两次操作观测间的下载，不是对象净载荷。
原始两个观测窗口和间隙分别保留，不能只相加独立窗口而漏掉后台下载。

最初驱动将心跳发送到 Lab 私网桥地址，命中默认 `10.0.0.0/8` 出站拒绝规则。
原矩阵首个生命周期分组及独立复现均在 capture 前准备失败，旧记录和现场保留。
接收器改为源 VMM namespace 的 tap 网关后，独立 FC 捕获校验通过，再以新协议执行
上述完整矩阵；未修改产品防火墙，旧失败不混入新协议的成功样本。

执行前归档并回收上一轮 8 个成功分组约 6.36 GiB；本生命周期矩阵按组累计回收
101.18 GiB 可再生成数据，保留运行目录及证据。结束后约 61.6 GiB 可用，临时权限、
配置和模块恢复，进程、设备、cgroup 及任务挂载引用检查通过。执行器记录矩阵阶段
约 4290.6 秒（不含外层准备/校验）；
随后 8 步全部通过摘要核验复用，原尝试/日志未变，没有重新运行 VM。

历史 `1d23f66` 的 768 MiB memory.high 诊断每端 4/4 成功，FC/DB 身份就绪中位数
为 842.9/777.0 ms；压力 node 上限 4 GiB，另有 512 MiB 驻留辅助负载。本轮没有
重跑压力条件，也未把旧样本并入上述分位数。最初把两端首条命令立即正确作为通过
条件的实验，因口径不一致且驱动存在流式读取预算缺陷，仍整体排除于性能比较。

640 MiB memory.high 场景两端各计划 4 次、实际各执行 2 次：各 1 次成功、1 次
失败、2 次未执行。DB 在创建阶段身份就绪超时（69.14 秒），FC 在随后身份命令
阶段返回 HTTP 504（37.71 秒）；均未观察到 OOM。这些是失败诊断，不能只取成功
样本比较性能，也不能据此认定内存分配失败。按当前任务范围搁置快速失败和超时
调整，保留原始失败证据；未宣称该压力条件通过或根因已修复。

OSS 30 秒配置下，1 GiB/high dirty/并发 10、node 32 GiB、夹具 16 GiB、16 CPU，
固定两批重复中 FC/DB 各完成 20 次捕获及恢复，每端 120/120 条操作与数据校验
记录通过，输入核验、资源清理通过，无 OOM。此前另有一批每端 60/60 的基线记录。
OSS30 按本轮通过结果完成验收，不再安排追加复测。历史分片超时未在本次复现，
此次验收不代表已证明历史根因；原始结果及 RAM 写入/上传日志继续保留。

### 恢复与继续运行差异复核

对上述矩阵的 160 条 restore/resume 样本及 8 份服务日志进行离线复核：两端均由
`parallel_actions` 在 API 请求前开始计时，随后调用同一个 `wait_guest_identity`，
准确匹配 sandbox/template 身份才结束；外层总预算 180 秒、身份不匹配后间隔 5 ms。
内层 envd 健康探测均为 60 秒预算、3 ms 重试；网络释放缓存为 0，VMM 池关闭。
因此本批结果没有沿用只验证 API 返回或 `true` 命令的旧终点。

以下是 DB 减 FC 的**阶段均值**，低/高脏页各 20 次/后端；不能用于直接分解前文
中位数之差。阶段按同一服务日志的时间戳相减，外层余额为单调时钟总耗时扣除
服务开始至 launch completed 的耗时，包含进入该服务前和完成日志后的工作。

| 阶段 | restore 低 / 高（ms） | resume 低 / 高（ms） |
| --- | --- | --- |
| 服务开始至 envd 健康探测开始 | +25.6 / +36.8 | +53.5 / +56.0 |
| envd 健康探测等待 | +58.0 / +38.7 | +0.8 / +1.5 |
| 健康成功至 init 完成 | +1.9 / +2.2 | +1.2 / +0.5 |
| init 完成至 launch completed | +67.6 / +77.7 | +22.6 / +22.4 |
| 外层余额 | −55.3 / −69.6 | −12.2 / −14.1 |
| 总耗时均值差 | +97.8 / +85.8 | +66.0 / +66.3 |

历史候选 `be6b051` 的 DB 在 `/init` 后、API 返回前执行内部身份命令；FC 没有这一步，但外层测试仍需
等待准确身份。DB restore 内部身份检查均为 2–5 次，阶段均值低/高为 67.2/77.3 ms；
resume 全部 40 次均一次成功，均值 20.8/20.6 ms。FC 的外层余额更大，故不能将
DB 内部身份检查的整段耗时视作两端额外差值，也不能把它从 DB 总耗时直接扣除
来预测优化收益。DB 内部确认后，测试仍发一次外部身份命令；该指标代表请求到
客户端确认可用，包含这一串行交互，不是纯 VMM 恢复耗时。

resume 差距主要已出现在 envd 探测之前，健康等待几乎相同。DB 此路径中，runner
启动/Hello 约 6 ms、Configure 约 2 ms、内存 backing 准备约 49–50 ms、LoadHeld
及随后安全核验约 48–49 ms（均值）；这些是 DB 自身阶段耗时，不是相对 FC 的净
开销。FC 缺少相同粒度的现有计时，尚不能将准备阶段差距归于某一个 RPC、内存
映射或设备恢复步骤。restore 另有健康等待差距，但日志未区分页故障、guest 调度
与网络重连。下一步应对两端补同边界分段计时，再做最小对照；不得通过移除身份
正确性检查或只改变一端测试终点获得提速结论。

上述离线复核未修改运行时或重跑 VM。原始证据仍为 `dbp12/evidence.tar.xz`，派生分析在
Mac `/tmp/agentenv-remaining-20261008/{analyze-ready-stages.py,ready-stage-analysis.json}`，
包含逐样本分段及原始日志摘要；该派生文件不在此前冻结的证据包内。

### 同 ID resume 跳过重复检查

最终运行时 `b9848bc` 仅跳过同 ID `build_paused` 的 guest 身份 shell 轮询；新建、
快照/模板创建、fork 和启动预取记录器保留准确身份确认。原地 resume 本来就没有
这层轮询。第二台 `147.139.165.124` 完成 11 项生命周期和 1 项记录器测试；第一台
`147.139.168.159` 完成同 ID 回归及以下性能对照，跨主机耗时不混合。

同机比较 `13b6f7a` 的 FC/旧 DB 与 `b9848bc` 新 DB，两批正反顺序、每组 3 次，
每端每操作 n=6；1 vCPU/1 GiB、并发 1、512 MiB resident/低脏页、tools 暖缓存、
网络缓存槽 0、CPU 0–7/NUMA 0、node 4 CPU/32 GiB。三组均执行相同 cold、capture、
restore、pause/resume；本轮共同省略 fork，不能与前文含 fork 的矩阵作配对比较。
162/162 条操作/内容校验通过，输入及清理核验通过、无 OOM。下表为中位数，单位 ms：

| 指标 | FC | 旧 DB | 新 DB |
| --- | ---: | ---: | ---: |
| 冷启动至客户端身份确认 | 740.7 | 751.2 | 740.3 |
| 从快照创建至客户端身份确认 | 500.2 | 617.5 | 548.2 |
| resume API 返回 | 225.0 | 299.4 | 283.9 |
| resume 至客户端身份确认 | 252.5 | 305.9 | 305.0 |
| resume API 后身份确认阶段 | 21.1 | 7.1 | 19.7 |

API 返回减少约 15.5 ms，但端到端中位数只减少约 0.9 ms；不能宣称恢复显著加速。
两个 block 的 API 中位数均降低，端到端变化却分别为 −6.4/+4.0 ms。旧 DB 的内部
resume 身份阶段均值 20.8 ms，新 DB 六次均为 0 次探测；外部首次命令耗时上升，
因此不能把内部检查整段直接当作端到端收益。各阶段中位数不能相加。

从快照创建时的检查没有移除，新 DB 仍平均耗时 64.5 ms、每次 2–4 次探测。其合并
中位数下降不构成优化证据：旧/新 DB 两批中位数分别为 548.5/550.4 和 686.5/523.0 ms，
变化方向不同；旧 DB 第二批波动主要位于健康等待和身份阶段，不能归于本次 resume
分支修改。n=6 仅支持诊断，不报告 p95/p99 或 SLO。

新 DB 的 resume 仍比 FC 慢约 52.5 ms（端到端中位数）。对齐释放 guest 的边界后，
准备阶段均值 DB/FC 为 190.0/126.6 ms；其中 LoadHeld/FC snapshot-load 调用阶段为
47.9/9.0 ms，runner 准备为 8.4/0.5 ms，磁盘为 50.9/44.7 ms，内存 backing 准备为
51.6/45.8 ms。剩余差距主要在释放前准备路径，不能继续归咎于已跳过的身份轮询。
这些是调用方墙钟跨度，尚未细分 LoadHeld 内的 VMM 工作、调度和 IPC；FC 进程启动
与内存准备有重叠，不能把 runner 数值解释为纯进程启动成本。

DB preparation 原始 total 止于 held，分析加上 readiness 的 `resumed_us`，才与 FC
含 release 的 total 对齐。readiness 字段是累计值，健康/初始化/身份阶段按相邻值
相减；初始化阶段还含 daemon ready 通知，内部两段之和也不覆盖完整 API 路径。
原始样本保留首个/后续 guest 标签，分析记录逐 block 汇总及 54 条 readiness 分段。

测试前确认上一轮 8 份成功产物回收回执仍有效、可用磁盘约 59 GiB；本轮逐 cell
回收 26.22 GiB，失败现场保留。两台测试机最终无测试账号进程、ublk 设备、Lab
cgroup 或临时命名空间残留，临时权限配置及模块已恢复。功能用例首轮曾因隔离 HOME
缺少 OverlayBD 生成配置而失败，修正夹具后才取得有效 RED/GREEN，未当作产品故障。

## 恢复状态 I/O 缓冲优化（2026-10-08）

运行时代码 `5b81aea`，Dragonball 依赖 `73178f7c7d4904f546901f4260e8ae5a43d9af05`。
runner 使用 `BufWriter` 写私有、立即 unlink 的临时状态文件，显式检查 flush 后才
rewind 并交给 VMM；VMM 使用 `BufReader` 解析 JSON。持久化 JSON 格式、16 MiB
入口限制、逻辑设备绑定、身份/版本检查、LoadedHeld 的 CPU/设备静止证明及 guest
内存的按需映射均保留。缓冲只作用于 VM 状态 JSON，不预读整个 guest RAM。

真实文件回归在旧实现上读取 65,538 字节触发 65,543 次 read，改后为 14 次；
输入缺失合法快照头仍被拒绝。计数包含该线程读取 `/proc/thread-self/io` 的小量开销。
这证明逐字节文件读取已消除；真实 LoadHeld 收益由下面的配对实验确认。

最终候选在第二台通过 22 项底层 capture 测试、11 项 sandbox 生命周期及 1 项记录器
回归。第一台完成 workspace fmt/clippy，runner 51 通过/11 显式 ignored，protocol
26 通过，AgentENV DB library 61 通过及打包契约检查。ignored 项未计为通过；真实
生命周期回归另列。新身份确认、同 ID resume、RAM 一致性、held 未释放不运行、
卷/工作目录、fork 隔离、元数据拒绝与失败回滚均覆盖。

首次候选 `c36eb48` 漏同步 qualified reader revision，功能预检在 VM 创建前拒绝
runner；修正全部生产、CLI 和打包版本引用后重新冻结、构建和测试。该失败与旧候选
制品保留，未纳入性能样本。两机最终 runner SHA-256 为
`1c84e9e7971d2840981125e19e2519f0220ea976778bc0699637c70a4ec3b173`，
integration binary 为 `76a29bf9b54399511341995d7d855b2ac7678170127fcd30f7b8158224699170`。

同一台 A 主机、1 vCPU/1 GiB、单并发、512 MiB 低脏页工作集、tools 暖缓存、
网络缓存 0、VMM pool 关闭；CPU 0–7/NUMA 0、node 4 CPU/32 GiB、fixture 16 GiB。
FC `13b6f7a`、旧 DB `b9848bc`、新 DB `5b81aea` 各两批，每批 3 次，第二批反转顺序；
各自使用匹配的 runner 捕获/恢复，无旧快照跨版本放行。共同终点仍为客户端确认
准确 sandbox/template 身份，API 与客户端确认分开计时；没有重跑 OSS30 或 640 MiB。
162/162 条记录通过，54 条内部 readiness 分段通过核验，未发生 OOM。

下表单位 ms，阶段为均值，端到端及 API 为中位数（每项 n=6）：

| 指标 | 旧 DB | 新 DB | FC |
| --- | ---: | ---: | ---: |
| resume LoadHeld / snapshot-load 阶段均值 | 48.3 | 20.4 | 9.5 |
| resume 准备至释放 guest 阶段均值 | 186.4 | 157.9 | 131.9 |
| resume API | 277.5 | 248.5 | 231.5 |
| resume 至客户端身份确认 | 294.6 | 269.4 | 262.5 |
| 快照创建 load 阶段均值 | 48.6 | 15.4 | 21.4 |
| 快照创建准备至释放 guest 阶段均值 | 226.8 | 192.7 | 184.8 |
| 快照创建至客户端身份确认 | 554.0 | 551.3 | 513.9 |
| 冷创建至客户端身份确认 | 753.9 | 751.4 | 741.2 |
| capture API | 8624.7 | 8615.1 | 8495.3 |
| pause API | 3192.0 | 3199.6 | 2994.8 |

resume 的 load 阶段减少 27.9 ms（57.7%），准备阶段减少 28.5 ms，端到端中位数
减少 25.2 ms（8.6%）；两批旧/新 DB 中位数分别为 301.0/268.2 和 288.2/270.3 ms，
改善方向一致。本轮相对 FC 的合并中位数差距为 6.9 ms，但 FC 两批为 277.8/238.8 ms，
小样本不能据此宣称稳定持平或生产 SLO 达标。

快照创建的 load 同样减少 33.2 ms（68.3%），准备阶段减少 34.1 ms，端到端收益
却未稳定体现：两批旧/新 DB 中位数为 553.4/649.6 和 554.6/507.9 ms。新 DB 的
健康等待均值增加 38.6 ms、初始化跨度增加 14.3 ms；第一批前两次健康等待为
375.5/328.8 ms。这里只能确认等待波动抵消了加载收益，尚不能确定波动的更底层原因。
内部新身份确认仍保留（新 DB 均值 71.4 ms，3–5 次探测），同 ID resume 仍为 0 次。
初始化跨度含 daemon ready 通知，不能全部归为 guest 初始化；未更改测试就绪口径。

恢复进程在客户端身份确认后读取的累计 `/proc/PID/io` 进一步支持归因：resume 的
read 调用均值由 34,982 降至 625，write 由 28,309 降至 327；读取/写入字符量仍约
266 KiB/40 KiB。该计数包括进程启动至观察点的全部 I/O，并非 LoadHeld 独占计数；
两项缓冲一起启用，未独立分摊读和写各自的延迟收益。结合原生 RED/GREEN 与配对
阶段下降，可确认此前差距中相当部分来自未缓冲的 JSON 文件 I/O。

剩余 resume load 均值仍比 FC 多约 11.0 ms，runner/内存准备等也存在差项；当前
数据仍不能将这些调用方跨度全部视作纯 VMM 执行成本。内存状态对象直传、消除
重复 JSON 转换和 runner/内存准备并行化留待后续按收益验证，不在本轮实现中。

测试前核验旧成功 payload 已回收，A/B 分别约 59/250 GiB 可用；本轮成功性能
payload 共回收 26.21 GiB。两机临时权限和模块恢复，无测试进程、ublk、Lab cgroup
或临时命名空间残留。最终 A 可用约 56.7 GiB，B 约 248.9 GiB；失败现场及证据保留。

依赖提交尚未推送，当前通过本地 Git bundle 分发并按 commit 固定；普通远程 Cargo
构建需先使该精确提交可获取。此限制不代表已发布版本。当前无未处理的生产代码阻断项；
保留一项低优先级测试加固：读取次数回归可进一步匹配明确的缺失 header epoch 错误，
避免未来更早的文件错误也满足 `is_err()` 和次数上限。

## P1 诊断与测试工程收口（2026-10-08）

运行时 `1a8fce1`、Dragonball `9e41795fea9109bc1acf7b1f6d6b878cfff7b72e` 只增加
观测，不改变恢复顺序、身份判定、截止时间、held 屏障或快照格式。计时区分节点
IPC、runner 状态处理、VMM worker，以及 health 请求和 daemon ready 通知。
runner 的日志初始化只在二进制入口，写 stderr；版本 JSON 提前返回。

两批反转 FC/DB 顺序，每批三次，继续使用上轮 1 vCPU/1 GiB、单并发、512 MiB
低脏页工作集和共同客户端准确身份终点。FC 冻结为 `13b6f7a`，DB 为 `1a8fce1`。
108/108 条记录通过，36 条 readiness 分段通过核验，无 OOM；构建全部结束后才运行。
每个后端/操作 n=6，保留首个 guest；debug 与包头摘要采集使本轮属于诊断实验。

| 指标（ms） | DB | FC |
| --- | ---: | ---: |
| resume load 均值 | 20.66 | 9.57 |
| resume 准备至释放均值 | 160.73 | 134.15 |
| resume 至客户端身份确认中位数 | 267.58 | 248.12 |
| 快照创建 load 均值 | 16.72 | 20.35 |
| 快照创建至客户端身份确认中位数 | 530.37 | 511.94 |

DB LoadHeld 内部按相邻累计值相减，单位 ms、均值：

| 部分 | 快照创建 | resume |
| --- | ---: | ---: |
| runner 解码、重绑定、FD 检查及写回 | 2.20 | 2.13 |
| worker 输入校验及状态解析 | 1.14 | 1.11 |
| worker 内存映射及 vCPU manager | 0.18 | 0.16 |
| VM／设备／vCPU 创建 | 1.64 | 13.42 |
| VM／vCPU／virtio 保存状态应用 | 10.81 | 3.20 |
| 事件、CPU 线程启动、held 确认及报告 | 0.49 | 0.46 |
| VMM action 往返扣除 worker 区间 | 0.054 | 0.037 |

节点编码、连接、发送、接收后解码合计约 0.05–0.08 ms。剩余约 11 ms 的 resume
load 差距主要在 worker 恢复路径，不能归为 IPC 或身份轮询；表中 worker 是墙钟
跨度，仍包含内核调用、缺页与调度，并非纯 CPU 执行时间。快照创建和同 ID resume
的慢段不同，不能把两个入口合并后指定同一个优化点。对象直传、进一步拆分 KVM
与设备恢复、准备并行化属于后续优化，本轮未改变这些实现。

此前 329–375 ms 的 health 慢样本是第一次成功请求，本轮再次观察到约 329 ms。
24 个恢复/resume 健康连接均只有一次 SYN 和一次请求负载，没有 SYN 重传或重试
轮询造成的延迟。只采集 TCP 包头摘要，不采集请求正文或认证信息。均值如下：

| health 请求部分（ms） | DB 快照创建 | FC 快照创建 | DB resume | FC resume |
| --- | ---: | ---: | ---: | ---: |
| SYN 到 SYN-ACK | 135.27 | 135.12 | 36.34 | 39.06 |
| 发出请求到收到首个响应数据 | 116.38 | 98.57 | 36.91 | 37.83 |
| 响应到客户端成功日志 | 0.16 | 0.13 | 0.13 | 0.09 |

因此长等待发生在 guest 恢复运行后的网络响应路径，两端均存在；现有证据不支持
测试轮询口径或客户端处理是主要原因。DB daemon ready 通知只占快照创建约
0.45 ms、resume 约 0.27 ms；对应 init RPC 约 20.19/8.09 ms，不能再把两者混算。

独立补充组每端两次，以 5 ms 间隔采样恢复进程各线程，36/36 条记录通过；
不与上表样本混合。快照 health 区间 FC 188–211 ms、DB 229–233 ms，均出现
19–20 次 major fault；FC 线程 CPU 总量约 51–60 ms、排队 2.5–2.9 ms，DB 约
78–92 ms、排队 5.9–7.9 ms。采样边界误差均小于 10 ms。缺页活动与较小的
运行队列等待支持按需恢复的缺页／I/O 路径是后续调查重点；尚未测得每次缺页或
块 I/O 的独占延迟，不能把剩余墙钟全部归给某个 I/O 调用。未取消新身份检查来
缩短数字，同 ID resume 仍为零次内部身份命令。

工程候选 `9f3f24c`、`bbe7034` 将显式准备、M4 build、功能执行纳入真实 FD
主机互斥，M4 Lab 默认预检，并增加共同资源/证据回执。新隔离目录、四份生成配置
的归属和摘要在准备入口验证；直接执行缺失配置时拒绝。M4 控制器死亡后的独立
审计恢复仍可取得锁，残留资源阻止新测试进入；未新增通用删除权限。
证据写入失败的清理、新状态创建、递归 Make 锁传递和 `make -n` 无副作用均覆盖。

Linux 验证：M4 工具 163 通过/17 显式跳过，regression 30/30；另行实跑 Lab
9/9、控制器死亡恢复 1/1、跨账号/实际 capability 包装器 FD 传递 2/2。剩余跳过
项需要显式 worker/controller 输入；不能计为通过。运行时 fmt、workspace 全目标/
全 feature clippy、runner 51 通过/11 ignored、protocol 26、DB library 61、envd 15、打包
契约，以及第二台 22 项 capture、11 项生命周期、1 项 recorder 回归通过。
完整依赖安装/下载未重跑；这不等于覆盖任意 Cargo、其他 Make 入口或生产容量。

六个成功分组均在归档证据后按原有归属审计回收 payload；失败与功能测试状态保留。
两机最终无测试账号进程、ublk 设备、Lab cgroup 或临时命名空间残留，临时权限及
新加载模块已恢复；A/B 可用空间约 55.0/248.3 GiB。新增依赖提交仍未发布，需通过
保留的 Git bundle 获取，不能把本地通过视作远程构建或 CI 已验证。

## P2 恢复准备优化（2026-10-08）

本轮按用户要求搁置 MSR 专项，先处理设备创建和准备串行等待。`dbp17` 的完整
worker 系统调用跟踪将 resume 慢点定位到返回 FD 64 的 `eventfd2`；`dbp18` 独立
内核栈采样确认 `expand_files → synchronize_rcu`，当时 `FDSize=64`。此前过滤
调用后的约 12 ms 空白不是已证实的设备计算成本。第二台机器的 20 对独立子进程
实验中，提前扩容后该分配的中位数从 7.506 ms 降到 0.00118 ms，预留本身约
0.00952 ms；这些诊断值不混入 VM 性能样本。

`68ee8d2` 在 runner 创建线程前预留 128 项 FD 表容量，立即关闭临时描述符。
这不是进程 FD 上限；低 `RLIMIT_NOFILE` 下预留失败仍沿用原来的分配行为。
`7e8bc3a` 进一步在网络与扩展钩子准备后启动 runner，让其与磁盘准备重叠；
Configure/LoadHeld 仍等待所有设备就绪，`RuntimeResources` 从启动起拥有进程，
失败时先确认退出再释放资源。runner 阶段计时包含 spawn 与磁盘准备后的剩余等待，
准备总时长仍覆盖整个区间。快照格式、身份要求、held 屏障、按需 COW 内存及
Firecracker 实现均未改变。

FD 单项实验 `dbp19` 固定 DB node `1a8fce1`，仅替换 runner，FC 固定
`13b6f7a`；两个分组反转顺序，每端/操作 n=6。162/162 条操作和校验记录通过，
54 段 readiness 核验通过。resume 设备创建均值由 15.06 降至 1.59 ms，load 由
22.30 降至 8.71 ms，客户端准确身份就绪中位数由 272.80 降至 257.48 ms；
本轮 FC 为 246.17 ms。冷启动中位数由 752.47 降至 733.43 ms，FC 为 735.32 ms。
快照创建准备至释放均值由 187.87 降至 179.68 ms，但 health 均值由 227.89
升至 289.25 ms，端到端中位数由 519.65 升至 571.41 ms；FC 为 513.76 ms。
因此本组不能宣称快照端到端提速，慢健康响应样本全部保留。

最终重叠实验 `dbp20` 使用同一份 FD 预留 runner，比较 node `1a8fce1` 与
`7e8bc3a`，同时运行 FC；两个分组反转顺序，每端/操作 n=4，108/108 条记录和
36 段 readiness 核验通过。两轮均为 1 vCPU/1 GiB、单并发、512 MiB 低脏页
工作集、tools 暖缓存、关闭网络/VMM 池，性能机上构建完成后才开始测量。下表均为
客户端准确身份就绪的中位数，单位 ms；两轮样本分别统计，不跨轮相加推算收益。

| 操作 | DB 仅 FD 预留 | DB 最终候选 | FC |
| --- | ---: | ---: | ---: |
| 冷启动 | 858.85 | 849.99 | 872.97 |
| 从快照创建 | 552.24 | 560.68 | 528.68 |
| 同 ID resume | 265.83 | 265.82 | 246.96 |

重叠后 runner 阶段均值在快照创建/resume 分别由 14.54/8.45 降至 10.32/5.31 ms，
准备至释放由 199.78/152.45 降至 189.91/147.88 ms；FC 为 177.04/137.43 ms。
这证明剩余等待缩短，但本组没有进一步的 resume 端到端收益，也没有稳定的快照
端到端提速。最终候选比本轮 FC 的快照创建/resume 中位数仍多 32.00/18.86 ms。
首次 guest 响应及初始化/身份阶段仍有波动；小样本诊断不是容量或生产 SLO 验收。

两项新增回归均执行了有效 RED/GREEN：真实 runner 的 FD 容量预留，以及磁盘
配置错误时提前启动进程的回收。低 FD 上限、临时 FD 释放和 seccomp 保留由
5 项 runtime CLI 测试覆盖；最终节点的 61 项 DB 单元测试、workspace fmt/
全目标全 feature clippy 通过。第二台机器最终 16 项生命周期/记录器回归通过，
包含新身份、同 ID resume、held、fork、磁盘失败、扩展失败和 API 分阶段失败清理。
新增用例在第一台的首次执行曾因驱动路径、生成配置和资产清单权限不匹配未到达
目标断言；这些记录作为环境失败保留，只有修正预检后触及目标断言的运行计入 RED。
独立代码审阅未发现实质问题。最终源码、runner、集成测试二进制摘要与冻结记录
匹配；Git 检查发现的唯一额外文件是测试导入生成的 Python 字节码，记录清单后
已清理。两轮十二个成功性能分组归档后精确回收约 45.35 GiB；失败与功能现场保留。
两机结束时无测试进程、ublk、Lab cgroup 或临时命名空间残留，临时权限和模块
已恢复，可用空间约 51.8/247.8 GiB。提交仍在本地分支，未推送或合并。

原始证据：A 机 `/var/tmp/dbp17/evidence` 的 worker 跟踪、`dbp18/evidence` 的
内核栈；两轮 `/var/tmp/dbp19`、`dbp20` 下的 `protocol.json`、`frozen.json`、
`analysis.json`、`worker-analysis.json`、`results/` 与构建回执。B 机 `dbp16` 的
独立 FD 实验、`dbp19`/`dbp20` 的功能回执和 `host-restored.json` 分别保留。
完整 A 机归档 `/var/tmp/dbp20/host-a-evidence.tar.gz` 在两台服务器各保留一份，
归档摘要及 1,235 个成员文件均已校验；B 机归档的 402 个成员文件已在本地校验。
结果摘要与校验回执另存本地 `/tmp/agentenv-p2-20261008/completion-summary.json`。

## Retro 工程遗留收口（2026-10-08）

阶段工具 `b4fed02` 已补真实 VM 验证：A 机复用冻结 DB node `7e8bc3a`、runner
`68ee8d2` 和 FC `13b6f7a`，1 vCPU/1 GiB、单并发各一轮。两端 cold、capture、
restore、pause/resume 共 18 条操作及校验记录通过，6 段请求窗口计时解析与汇总通过。
快照恢复的新 ID 与源不同，同 ID resume 与源一致；输入前后摘要一致。此轮用于
验证采集器、身份关联和清理，不是新的性能对照或 SLO 验收。

预检在 VM 启动前拒绝了旧清单的 root-only 读取权限；为本轮复制可读清单后通过，
原文件和失败回执保留。两组成功现场由既有 retention 入口归档并回收约 4.03 GiB；
A 机临时权限/模块已恢复，无自有进程、ublk、Lab cgroup 或命名空间残留。

本轮增加离线 revision 一致性检查并接入 DB Make 构建及 CI 编译前置任务；故障注入
确认 pin 不一致时不会调用 Cargo。证据工具默认取回摘要，完整包经整体和逐成员校验
后才使用正式文件名；中断/损坏、非法成员及普通硬链接均有回归覆盖。
B 机非 root 检查：M4 工具 205 项中 188 通过、17 显式跳过，regression 30/30。
独立审阅发现的硬链接打包问题已按 RED/GREEN 修复；运行时 Rust/Go 未修改，
GitHub 工作流已接线但未远端执行。工程指南只保留当前覆盖与证据入口，历史记录从 Git 查阅。

VM 证据在 A 机 `/var/tmp/dbp21`，工程证据在 B 机
`/var/tmp/retro-closeout-1008/evidence`。A 机 bundle 的摘要优先取回及完整包取回均已
实测，66 个成员校验通过；B 机完整包的 11 个成员也已校验。B 首次打包因本轮输出
父目录归属不匹配而失败，修正归属后以新下载目录完成，原失败回执及 `.partial` 保留。
本地统一交接目录为
`/tmp/agentenv-retro-closeout-20261008`，当前状态由其中 `completion.json` 指向已验证回执。

## 身份刷新优化（2026-10-08）

本轮修复两个具体问题：DB 新身份校验改为带 runtime token 的 `GET /envs`，
精确读取命令共用的默认环境，避免额外创建校验 shell；guest envd 的 MMDS
轮询首次请求从固定等待 50 ms 改为立即尝试，失败后仍等待 50 ms，取消与原有
截止时间保留。`/init` 仍异步，身份仍来自 MMDS，不通过主机注入身份绕过确认。
同 ID resume 继续跳过内部身份校验，快照/模板创建和 fork 仍等待新身份正确。

先单独验证 GET 改动：6 组、108 条操作及校验记录、36 段计时全部通过；
每种配置每个操作 n=4，第二批反转顺序。旧/新 DB 的冷启动、快照恢复、resume
就绪 p50 分别为 843.3/848.2、536.6/536.4、248.5/251.3 ms，**没有端到端收益**。
首条外部命令耗时增加，与开销后移相符，但尚未证明具体缺页归因；不能把内部
阶段缩短当作性能改善。
该改动的确定收益是去掉身份确认对 BusyBox 的依赖，保留准确身份契约。

随后用同一 envd 上游提交 `9c3b7c5dbd181ba819084276b8228f70936e911e`、
Go 1.25.9、相同编译参数构建 timer 补丁前后版本。两份 tools 都从冻结旧盘复制，
仅替换 envd 和版本标记；同一版本的 DB/FC 使用完全相同的 tools 字节。
DB 两组均使用上述 GET 候选，FC node 固定为 `13b6f7a`；runner 为 `68ee8d2`，
Kata 为 `9e41795`。因此本轮只比较 guest timer 改动，
不与前一轮混算。版本分别为 `0.1.2-mmds-before.1`、`0.1.2-mmds.1`。

A 机固定 1 vCPU/1 GiB、并发 1、512 MiB 低脏页负载，tools 每次创建前全驻留，
CPU 0–7、NUMA 0、node 4 CPU/32 GiB、夹具 16 GiB、网络缓存与 VMM 池关闭。
两批反转四组顺序，每组每操作 n=4；保留服务首个 guest 和全部慢样本。
144 条操作及内存/身份校验、48 段计时全部通过，无输入变化。
两端统一从 API 发起至真实 guest 命令返回准确 sandbox/template ID，API 单独计时。
下表单位 ms；p50 为中位数，偶数样本取中间两值均值，不提供 p95/p99 或容量资格。

| API 至准确身份就绪 p50 | DB 补丁前 | DB 补丁后 | FC 补丁前 | FC 补丁后 |
| --- | ---: | ---: | ---: | ---: |
| 冷启动 | 851.9 | 833.8 | 875.8 | 859.9 |
| 快照创建新实例 | 520.8 | 544.7 | 529.0 | 502.7 |
| 同 ID resume | 267.3 | 253.6 | 239.7 | 236.2 |

DB 冷启动内部身份等待均值 35.6→1.1 ms，快照恢复 55.9→12.0 ms。
冷启动端到端中位数减少 18.1 ms，FC 减少 15.9 ms；收益小于移除的固定等待。
**DB 快照恢复未确认端到端改善**：均值 561.2→560.4 ms，范围分别为
495.6–707.5、428.9–723.5 ms，中位数反而增加 23.9 ms。同期 health 均值
272.4→302.3 ms、init RPC 25.5→33.2 ms、外部首条命令 11.3→25.9 ms，
抵消了身份阶段节省，不能将该小样本波动解释为已证实的产品回退或提速。
FC 快照恢复均值 521.0→496.3 ms，主要减少 API 后的身份等待。
同 ID resume 的内部身份轮询次数始终为零，其分位数变化不能归功于删除身份检查；
FC resume 的慢样本 331.7 ms 保留，均值实际为 238.4→258.9 ms。
DB 所有外部身份探测均首次正确；FC 补丁后恢复仍有 3/4 需要第二次探测，
不能把 FC API 返回视为身份立即就绪。

独立的线程阻塞诊断在双方均观察到文件缺页等待与块层 tag 等待；该轮只有每端
一次，且带 2 ms 采样，不混入性能统计。它没有证明 DB 独有网络瓶颈，也不足以
支持扩大内存预读。恢复 health/按需读页窗口仍是剩余诊断方向，未承诺尾延迟已消除。

验证与交付边界：

- Rust fmt、workspace 全目标/全 feature Clippy、envd 单元 18/18、DB 单元 61/61 通过。
  B 机同一组 12 项真实 VM 功能分别在旧 tools、新 envd tools、当前启动脚本组合上通过；
  包括禁用 BusyBox 后 fork/快照第一条命令身份、同 ID resume、挂盘/USER/WORKDIR、
  鉴权/网络、held 状态、回滚和记录器。旧实现的身份 shell 失败保留为 RED 证据。
- guest `go test ./internal/host ./internal/api` 通过；虚拟时钟测试覆盖首次立即成功、
  token/metadata 两种失败的 50 ms 重试、等待中取消与预取消。原实现 RED 保留。
  Linux M4 工具 205 项中 188 通过、17 项需专项输入而显式跳过。
- 当前仓库 `init`、`pivot-init` 与新 envd 另组装 `0.1.2-current.1` ext4，
  核对 BusyBox 1.37.0 及四个必需 applet；上述 DB 12 项与额外 FC 创建/命令冒烟（node `4b1e400`）通过。
  FC guest 内回读 envd/pivot-init SHA 与 tools 版本一致。该组合不并入 A 机性能样本。
- Docker 补丁白名单、版本接线、对原始源码的 `git apply --check` 已核验。
  后续在 B 机以非 root 账号完成 `33453da` 的实际 Docker/Buildx amd64 构建，
  候选版本 `0.1.2-p4.1`。FC 通过临时本地 registry 获取该 OCI，执行运行时
  OCI→OverlayBD 转换并真实启动；guest 内 envd、pivot-init、BusyBox 摘要和
  tools 版本均与构建产物一致。DB 使用该 OCI 原始 rootfs 生成的 ext4，
  同一组 12 项功能全部通过。它验证了交付链路，不并入 A 机性能样本。
  构建与导出进程临时获得 Docker socket 所需补充组，未修改账号永久权限。
  默认构建版本为 0.1.2，运行时 manifest 仍为已发布 0.1.1；没有对外发布、
  部署或升级旧模板。旧快照中的 envd 不会自动替换；ARM64 未验证。

冻结 DB server SHA-256 为 `52ad8dfa9456c911296581826891230072a835e7ea15b3abb8bf6a393e34498e`，
基于 `84e21d5` 的运行时三文件 diff 为
`80a65a02439b8c8559a032253d88851751af37f4549662837092e86fde153b00`。
guest 补丁为 `6288076dca8ca7ad9407bc8426169eb03bf073fa890bae2f3b4fcb56c8a74334`，
新 envd 为 `9a41d756ea49aa1f603a5d9dd9708d60f301597d71283ca480f3e177434696aa`。
应用补丁后的 guest 源文件、被测 Rust 文件和测试脚本均与本地交付逐字核验一致。

A 机诊断、GET 对照、guest 构建和 timer 对照分别位于 `/var/tmp/dbp22` 至 `dbp25`；
B 机对应旧/新 tools 功能在 `dbp22`、`dbp25`，当前启动脚本组合在 `dbp26`、`dbp27`。
测试前检查空间，成功现场归档后依次回收约 4.06、19.16、25.66 GiB；失败及功能
现场保留。两机临时配置/模块已恢复，无自有 VM、ublk、Lab cgroup 或命名空间残留；
A 机收尾可用约 49.7 GiB。证据及整体/逐成员校验回执集中在 Mac
`/tmp/agentenv-p3-20261008`，正式包摘要另见其中的交接回执。

## 恢复读页窗口与 tools 交付补验

对上述 timer 对照的 32 次 restore/resume 离线匹配健康检查 TCP 流，全部得到唯一
匹配。进入 health 阶段至发出 SYN 约 0.1–0.2 ms，响应到达至就绪日志约
0.1–0.2 ms；主要等待位于 SYN→SYN-ACK，以及观测到请求包至响应包之间。
DB 补丁后 restore 的这两段均值约 134 ms、168 ms；FC 分别约
147 ms、79 ms。双方使用同一健康探测实现，现有证据不支持继续减少探测轮询
间隔。结合此前双方都有的缺页/块层等待，转而验证已有启动页预取能否缩短该窗口；
不能仅凭 TCP 时间把全部等待归为缺页，也未证明 DB 独有网络问题。

已有启动页预取的对照验证了一个有效的配置优化，未修改运行时或默认开关。
本节的恢复指从已发布模板/checkpoint 创建新 sandbox，不包括同 ID pause/resume。
范围检查确认：正常暂停时 DB 重建不带预取描述符的 manifest，FC 显式将
`memory_startup_pack` 置空，两者均不启动记录器，也不继承原模板清单。
因此下面的性能结论不能用于声明休眠后继续运行更快。
A 机 `/var/tmp/dbp29` 使用同一冻结运行时、内核和 `0.1.2-mmds.1` tools，
1 vCPU/1 GiB guest、512 MiB 低脏页工作集、单并发，OSS 路径接本地 S3 夹具。
发布节点统一开启记录，恢复节点使用独立 OverlayBD 缓存，仅切换
`consume_enabled`。两组均等待清单发布，完成源工作集校验并删除源 VM、确认
记录器退出后才测恢复；tools 文件预热并以 mincore 核验，宿主机和 S3 页缓存
未清空。两节点各限 4 CPU/32 GiB，绑定相同 CPU 0–7/NUMA 0。
这些数据不能与前述同节点恢复样本混算。

8 个独立 cell 按两个相反顺序运行，每 cell 含首次、后续各一次恢复；每个
backend/开关组合共 4 次，但仅有 2 个独立 cell。正式 80 条操作记录、16 次
恢复全部通过，另有 4 次预实验未并入统计。计时终点仍为 API 发起至第一条
返回准确 Sandbox ID 和 template ID 的 guest 命令。下表为**均值，毫秒**；
每格 n=2，保留全部首次与慢样本，不据此推断 p95 或生产 SLO。

| 后端及节点阶段 | 预取关闭 | 预取开启 |
| --- | ---: | ---: |
| DB 首次恢复 | 721.1 | 497.3 |
| DB 后续恢复 | 528.0 | 281.4 |
| FC 首次恢复 | 770.7 | 509.2 |
| FC 后续恢复 | 515.8 | 323.8 |

合并两阶段仅作等比例诊断汇总：DB 中位数 628.8→379.4 ms、均值
624.5→389.4 ms；FC 中位数 623.3→413.5 ms、均值 643.3→416.5 ms。
health 均值分别为 DB 287.1→72.5 ms、FC 290.2→86.5 ms，是主要改善项。
DB 外部命令等待均值 64.1→46.7 ms，FC 80.1→67.4 ms，因此不是把相同等待
移到 API 返回后。DB 8 次恢复外部身份首次正确；FC 关闭组 4/4、开启组 3/4
需要第二次探测。8 次开启组均核验实际预取数据读取，未仅凭任务完成日志判定。

收益有资源代价。DB 首次恢复窗口的 S3 下载 IP 字节均值约 168.4→221.9 MiB，
节点 `memory.current` 的 10 ms 采样峰值约 635.0→770.8 MiB；后续分别为
147.3→215.4 MiB、1311.3→1482.1 MiB。节点值包含保留缓存，采样峰值是实际
峰值的下界，并非 guest 或 VMM 专属内存。连续覆盖恢复与完整工作集校验的
下载总量基本不变：DB 约 597 MiB、FC 约 610–611 MiB。该流量包含协议头、
重传和后台读取，不是逻辑页大小，也不能外推所有业务都没有读放大。
工作集校验使用 1 秒轮询，约 5–6 秒的校验耗时不用于判断应用吞吐改善。

当前可用方案是对有预先生成清单、资源预算允许的快照启用消费；默认仍关闭。
立即恢复及没有清单的旧快照、低内存、高并发、真实远端 OSS 和 POSIX 的收益
不在本轮结论内。记录端会启动临时 VM，其成本没有通过“记录关闭”对照量化。
配置及异步发布边界见[配置参考](../configuration/reference.md#snapshotmemory_startup_pack)。
A 机预实验与正式成功现场归档后回收，配置/模块恢复，无自有运行资源残留，
剩余约 49.7 GiB。原始驱动、输入、逐样本结果及清理回执保存在
`/var/tmp/dbp29/bundle`，证据包 SHA-256 为
`aa1b14a2801ef34813a15ff5070f610b2e89b4b6e0a46d8663b169b8b13c28ef`。
Mac `/tmp/agentenv-p4-20261008/host-a/verified` 已完整下载并校验 513 个成员，
正式驱动、协议及配置恢复脚本与本地逐字一致。

tools 补验使用的完整 OCI archive SHA-256 为
`7e81acbe9f769e82b8eabccadc10ee2e91db91e1085f0144a08e42b852035560`，
FC 转换所得 OverlayBD 层为
`cdf92c857e0b8641c49c25900ebd17f8c91efffe7f271f6eca2b6cdd004d431b`，
DB 同源 ext4 为 `49ebed000c745938983f3e319591ceac4cd48525f673a950a459cc3ca38778c7`。
实际 Docker 构建的 envd 为 `9be13d070884160d4b13913acbb56eb29d76113ac79cc271de00c8fe9b2de756`，
与先前定向替换 envd 的性能镜像不是同一制品，不能互换其性能结论。
B 机证据 `/var/tmp/dbp28` 已归档并下载到 Mac
`/tmp/agentenv-p4-20261008/tools/evidence-verified`，135 个成员逐一校验通过，
证据包 SHA-256 为 `0930f782b987c1f4bd3522d1952d4d627287365cfe2136e504daa60391672bdc`。
功能现场与候选保留，临时主机配置和模块已恢复，无自有运行资源残留。

## 完整 tools 候选性能复测

后续复测固定使用上述 Docker/OCI 候选 `0.1.2-p4.1`。DB 使用同源 ext4，
FC 使用实际运行时转换得到的 OverlayBD tools 层；7 个传输文件逐一验证摘要，
guest 内另核验 envd、pivot-init、BusyBox 的摘要和 tools 版本。两种后端的
guest 内容相同，原生存储表示不同，因此结果包含各自 tools 路径的成本。
制品下载和转换在测量前完成，tools 文件页预热并通过 mincore 核验；这不表示
FC 的 ublk 设备页缓存全暖，也不计入首次部署下载/转换耗时。

运行时、内核和工作负载沿用冻结输入，源码相对 `33453da` 没有运行时或
tools 变更。A 机串行执行默认生命周期与独立缓存的预取对照，两组均反转顺序，
1 vCPU/1 GiB guest、512 MiB 低脏页工作集，节点 4 CPU/32 GiB，
CPU 0–7/NUMA 0、本地 S3 夹具、单并发。API 到准确 Sandbox ID/template ID
的首条 guest 命令仍是统一终点，制品摘要核验在就绪计时及该操作的资源采样
窗口外执行。
本轮和前述定向替换 envd 镜像的样本分别统计，不据跨轮差值认定优化收益。

默认生命周期（`/var/tmp/dbp31`）关闭记录和消费，使用同节点恢复、源 VM 保持
运行的既有协议。4 个 cell、72 条操作全部通过，24 次就绪后的实际 guest
制品核验通过；每后端每操作 n=4，来自两个 cell 的第 1/2 轮各一次。
表中单位为毫秒，保留全部样本，不能用于 p95/SLO 验收。

| 操作 | DB 均值 / 中位数 | FC 均值 / 中位数 |
| --- | ---: | ---: |
| 冷启动 | 828.6 / 830.3 | 871.9 / 874.9 |
| checkpoint 创建新实例 | 574.6 / 568.7 | 490.7 / 482.0 |
| 同 ID resume（已暂停） | 284.4 / 285.7 | 271.8 / 261.7 |

冷启动第 1/2 轮均值为 DB 969.3/687.8 ms、FC 1044.3/699.5 ms；
checkpoint 为 DB 637.8/511.4 ms、FC 499.3/482.0 ms；
resume 为 DB 319.1/249.8 ms、FC 302.4/241.3 ms。各子组 n=2，
新节点缓存与后续轮次的差异不能隐藏在单个中位数里。
DB checkpoint 范围 460.4–700.5 ms，FC 469.4–529.3 ms；该小样本下
DB 默认 checkpoint 仍较慢，health 均值为 332.2/245.4 ms，准备至释放阶段
为 174.7/179.8 ms。DB 外部身份首次均正确，FC 的 4 次 checkpoint 均需
第二次探测；已将这段等待计入表中。双方 resume 均保持 ID，DB 内部身份
检查阶段近零（0–1 µs）。resume 数值不含 pause API 耗时。
恢复后的完整工作集校验通过，未放宽正确性终点。

checkpoint 预取对照（`/var/tmp/dbp32`）统一开启发布端记录，等待清单发布后
删除源 VM 并确认记录器退出，使用独立恢复节点缓存，仅切换消费开关。
宿主机和 S3 页缓存未清空。8 个 cell、80 条操作、16 次恢复与 32 次 guest
制品核验全部通过；8 次开启组均确认实际数据预取。该组与默认生命周期的
源 VM/缓存条件不同，不能用两表差值计算预取收益。

| checkpoint 新实例 | 关闭预取均值 / 中位数 | 开启预取均值 / 中位数 |
| --- | ---: | ---: |
| DB | 684.3 / 708.8 ms | 395.2 / 374.2 ms |
| FC | 610.5 / 610.3 ms | 457.6 / 447.8 ms |

每个组合 n=4，仅有两个独立 cell。首次/后续子组各 n=2：DB 均值分别
825.6→489.2 ms、542.9→301.2 ms；FC 为 728.6→583.9 ms、492.3→331.3 ms。
DB health 均值 321.1→70.7 ms，
FC 242.9→115.8 ms，是主要改善阶段。DB 外部身份等待 70.4→50.4 ms，
FC 59.0→63.3 ms；上述端到端结果已包含外部等待，不能只按 API 返回计时。

预取将更多读取提前到恢复窗口。DB 恢复窗口 S3 下载 IP 字节均值
162.5→229.0 MiB，FC 181.0→208.2 MiB；节点 `memory.current` 的
10 ms 采样峰值均值分别为 1069.4→1251.4 MiB、1010.7→1074.9 MiB。
该内存指标包含节点保留缓存，是实际峰值的采样下界，不是 guest 专属内存。
连续覆盖恢复、制品摘要与完整 512 MiB 工作集校验的下载量，DB 为
597.2→597.7 MiB，FC 611.3→609.2 MiB；这是含协议头、重传和后台读取的
IP 流量，不是对象净字节，也不能证明其他负载没有读放大。工作集校验采用
1 秒轮询，其耗时不作为应用吞吐指标。

本轮确认完整候选在上述条件下仍有预取收益，默认消费仍关闭；同 ID
pause/resume 不使用该清单。立即恢复、低内存、高并发和真实远端 OSS 的
收益未覆盖，记录端额外 VM 的成本也未通过记录开关对照量化。

测试前完成磁盘及旧成功归档检查；本轮 12 个成功现场归档后回收 payload，
失败和功能证据保留。结束时 A 机剩余约 49.5 GiB，临时配置、模块、进程、
ublk 设备、命名空间和 cgroup 均恢复；B 机临时制品传输服务已停止。
一次准备失败来自生成的资产清单权限，修正为测试账号可读后通过预检，
发生在 VM 测量前；原始失败与修正回执保留，没有删除慢样本或重跑筛选。

原始协议、驱动、逐条结果、分析及恢复回执位于 A 机 `/var/tmp/dbp31`、
`/var/tmp/dbp32`；完整证据包 `/var/tmp/dbp32/bundle/evidence.tar.gz` 的
SHA-256 为 `28dce918f93452ca6ffe17fa5d2e881a183a5ec606176df69802d4aa7f2b6d25`。
Mac `/tmp/agentenv-p5-20261008/host-a/verified` 已下载并逐一核验 607 个成员；
47 个公共脚本、10 个实验驱动/协议文件及候选适配器与本地一致。
两组总计 152 条操作及 56 次实际 guest 制品核验通过；该结果完成候选性能
复测，不代表生产容量、发布或云端 CI 验收。

## 尚未关闭的限制

- 640 MiB 压力失败及 readiness 尾延迟保留为已知限制，快速失败/超时调整已搁置。
- 生产并发、容量与 SLO 应按实际部署目标验证；开发候选通过不等于所有负载通过。
- DB 仅声明已验证的 Linux x86_64/KVM 配置；跨 FC/DB 内存恢复不在支持范围内。
- 日常迭代保留兼容性拒绝契约；旧版本升级集成测试仅在格式/兼容规则变化或发布时专项运行。

## 追溯

一次性计划、逐轮日志、实验补丁和原始样本已从当前文档树移除，保留在 Git 历史。
需要审计上述数字时，可读取清理前提交中的相应路径，例如：

```bash
git show ab3db37:docs/src/internals/startup-alignment-2026-10-07/analysis.json
git show ab3db37:docs/src/internals/startup-alignment-2026-10-07/validation.json
git show ab3db37:docs/src/internals/test-cleanup-2026-10-07/validation.json
```

新实验的原始日志、输入回执与大体积制品保存在测试输出或 CI artifacts；更新本页
时只收敛结论、候选身份、证据位置和未覆盖范围，不新增逐轮过程文档。

本次证据位于测试主机 `iZk1afrni8l1k5rt5thkwdZ`：

- `/var/tmp/dbc7/{db-baseline,fc-baseline}/receipt.json`：运行时构建回执。
- `/var/tmp/dbc7/regression-final/results.json`、`logs/platform-final.log`：功能/平台结果。
- `/var/tmp/db-m4-performance.5_0v3ihi/result.json`：OSS30 最终重复批次，工具版本 `50a5e21`。
- `/var/tmp/dbp8/results/{result.json,paired.jsonl,samples.jsonl}`、`protocol.json`：`b813d45` 的 120 次创建诊断、原始计时与冻结协议。
- `/var/tmp/dbp8/{analysis.json,stages.json}`：输入/缓存/运行时逐样本复核、分批统计和 DB 分段分析；`evidence/host-restored.json` 为清理回执。
- `/var/tmp/dbp8/evidence.tar.gz`：118 个证据文件，SHA-256 `127ff1efdc27d66677826cf524cf3acfa32c3ccd8d81368698b94a6d3601433a`；Mac 下载后逐文件摘要核验通过。
- `/var/tmp/dbc7/readiness-aligned/result.json`、`benchmark-protocol.json`：统一口径矩阵及协议，工具版本 `1d23f66`。
- `/var/tmp/dbc7/evidence/analysis.json`：输入摘要复核及逐 cell 分析。
- `/var/tmp/dbc7/evidence/{cleanup,host-restored}.json`：测试前清理 84.25 GiB 的回执及结束后的权限/资源恢复核验；结束时无自有进程、ublk 设备、Lab cgroup 或挂载引用，失败现场文件保留。
- `/var/tmp/dbeg7/logs/{tools-final,recovery-final,lab-final}.log`：`5eb08f4` 的工具、真实 SIGKILL/引用审计及 Lab 回归结果；候选源码保留在 `/var/tmp/dbeg7/finalsrc`。
- `/var/tmp/dbp9/{results,pool-stages.json,campaign}`：网络复用对照、原始失败分组及执行回执；`evidence/preclean.json` 为前置清理回执。
- `/var/tmp/dbp10/evidence/heartbeat-failure.json`、`/var/tmp/dbp11/evidence/heartbeat-network.json`：心跳失败复现与实际出站规则；修正后的独立捕获检查位于 `dbp11/results`。
- `/var/tmp/dbp12/{protocol.json,cell.py,frozen.json,results,analysis.json,startup-followup.json}`：最终生命周期协议、外部驱动、输入绑定、逐样本数据及分析。协议 SHA-256 为 `77026a6e8cd66f5791985910256e61b60a4e9da4dd5a670ad0ffd638c062ae6e`；驱动为 `e0402277ea1fd16db3819d8bfd7ee29a783bc735441d45f0c9272a601d4bea3f`。
- `/var/tmp/dbp12/evidence/{campaign-executed,resume-verified,host-restored,final-host-check}.json`：原始执行、8 步复用、主机恢复及最终资源检查；`logs/tools-nonroot.log` 记录 UID 999 的最终工具测试。
- `/var/tmp/dbp12/evidence.tar.xz`：1265 个证据文件，SHA-256 `3ab1b6ba3e195b310f9526747aa871740a41ca025c158825d0a949695deb3a38`，包含失败诊断及修正后的原始样本；不包含已回收的成功 VM payload。由同内容的 gzip 包转换，转换回执为 `archive-verification-xz.json`。Mac 下载后逐文件摘要核验通过，36 个被测源码文件及协议、驱动与本地一致。

本轮新增证据：

- 第一台 `/var/tmp/dbp13/{db-before,fc-before,db-final}/receipt.json`、`protocol.json`、`results`、`analysis.json`：冻结构建、162 条样本及分段分析；`final-src` 固定为 `b9848bc`。
- 第一台 `/var/tmp/dbp13/evidence.tar.gz`：357 个文件，SHA-256 `21e3943baf10900425382f9b8175306955dabef81b7589ff88141ab2e1a530c7`。
- 第二台 `/var/tmp/dbp13/evidence.tar.gz`：292 个文件，SHA-256 `af9c2ce1bcdc71d8969120e70929feb7687b935dff18af8e836d5ace6d7d180b`，最终功能结果为 `evidence/functional-final.json` 与 `evidence/final-recorder.json`。
- Mac `/tmp/agentenv-ready-diagnosis-20261008/host-{a,b}-evidence.tar.gz`：下载副本；两包逐文件摘要及最终 9 个源码/测试文件与本地核验一致。两机的 `evidence/host-restored.json` 记录最终清理状态。

Mac 与 Linux 的被测脚本及协议摘要一致，运行时二进制重新核验匹配构建输入；
本页后续文字修订不改变被测工具及运行时。候选保留在本地开发分支，未推送、
发布或核验云端 CI。

状态 I/O 缓冲轮次（`dbp14`）：

- 第一台 `/var/tmp/dbp14/{candidate-src,db-candidate/receipt.json,protocol.json,frozen.json,results,analysis.json,io-analysis.json}`：`5b81aea` 冻结源码/构建、162 条样本、阶段及进程 I/O 分析；`evidence/final-source.json` 将 9 个 AgentENV 文件和 Kata capture 文件与本地逐一比对。
- 第一台 `/var/tmp/dbp14/evidence.tar.gz`：325 个文件，SHA-256 `1755b3887ee1787554c65de21515aa1164987e537e01228fc1233b782048be17`，含依赖 Git bundles、首次失败、最终构建/测试日志与原始性能证据。
- 第二台 `/var/tmp/dbp14/evidence.tar.gz`：181 个文件，SHA-256 `369a137795d92b17fe7f65526629263ac721f14bfee641ecea53f2cfad2a24b4`；`evidence/functional-qualified.json`、`evidence/qualified-recorder.json` 和 `logs/kata-final-capture.log` 为最终功能结果。
- Mac `/tmp/agentenv-restore-io-20261008/host-{a,b}-evidence.tar.gz` 为下载副本；`kata-buffered-state-io.bundle` 携带 `9b0d285` 之后的依赖提交，基础 commit 为 bundle 前置要求。两机 `evidence/host-restored.json` 记录最终资源和权限核验。

P1 收口轮次（`dbp15`）：

- 第一台 `/var/tmp/dbp15/{src,db/receipt.json,protocol.json,frozen.json,results,analysis.json,phases-analysis.json,ipc-analysis.json,network-analysis.json}`：冻结运行时、108 条常规诊断及分段/包头证据；`faults/` 为独立 36 条缺页诊断。`evidence/runtime-source-verified.json` 核验 14 个运行时/依赖文件。
- 第一台 `evidence.tar.gz`：515 个文件，SHA-256 `1ea92b5e58bea6e5d275320c50a7de7c52708695e0d4e900322c095c4693a8ec`。归档后的补充定向检查为 `logs/envd-unit.log`（15/15）。
- 第二台 `/var/tmp/dbp15/evidence.tar.gz`：184 个文件，SHA-256 `7379f49bbe3ecc8441be6ef7823345dad5ea23869c7f3b18a7748d0a52722f7e`，含功能、capture 和工程检查；`engineering/source-verification-final.json` 对应 `bbe7034` 的 16 个工程文件，规范化清单摘要为 `860d180a5c20410b0c234b35696e9cb05108ce3749854e8b5e748477b0b3d083`。
- Mac `/tmp/agentenv-p1-20261008/` 保存计划、分析、Git bundles 和 `host-{a,b}-evidence.tar.gz` 副本。两机 `evidence/host-restored.json` 记录运行资源恢复；运行时代码独立评审与工程清理边界复核无未处理阻断项。
