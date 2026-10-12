# Qwen 任务超时归因（2026-10-10）

本轮已完成记录中的 29 项 `AgentTimeoutError`，确认有一个 AgentENV 启动兼容性贡献项
（Mailman 的运行目录被遮蔽）和一个外部输入阻断项（YouTube）。其余存在依赖准备、
外部仓库选路、解题和等待的混合影响，不能统一归为 Dragonball 性能或模型能力问题。
目前没有宿主 OOM、磁盘耗尽或 VMM 崩溃导致这些超时的证据。

## 范围与口径

- 只读调查，不重跑任务、不新增模型请求、不改变运行候选、超时预算或成绩。
- 远程批次：两机 `/var/tmp/hb1008/qwen-continuous-20261010`；A 为
  `147.139.168.159`，B 为 `147.139.165.124`。
- 归因证据采集于 2026-10-10 21:09 中国时间：A 完成 38 项、超时 14 项；
  B 完成 32 项、超时 15 项。21:24 进度仍为完成 70/74，严格成功 32、正常
  0 分 9、异常 29，A 还有 4 项运行中。本报告不覆盖之后产生的结果。
- 原始阶段预算、倍率 1、每项一次、零自动重试；每个 guest 4c8g，两机节点
  64c/128GiB，最多八槽。没有 Docker 对照，不据此估算 AgentENV 的性能损失。
- Harbor 0.24.0，提交 `4b94505a91c5ddcb70b5740ac95718ddee13e5a0`；
  Terminal-Bench 提交 `69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`。

本地完整证据和逐任务索引：
`/tmp/agentenv-harbor-qwen-continuous-20261010/timeout-investigation/REPORT.md`。
包括脱敏后的原生 result/config/trajectory、异常栈、guest 删除回执、宿主日志、
归因清单和 SHA-256 清单。首次 SSH 传输超时的截断文件单独保留，未参与分析；
那是取证传输失败，不是 benchmark 任务失败。

## 需要优先处理的任务

| 任务 | 原始 Agent 预算 | 证据与归因 | 确定性 |
| --- | ---: | --- | --- |
| `mailman` | 30 分钟 | OCI 层有 `/run/mailman3`，guest 首次启动报该目录下 `master.pid` 不存在；补建目录后继续发生 LMTP 就绪和配置问题 | 确认环境贡献项，整项超时为混合原因 |
| `extract-moves-from-video` | 30 分钟 | yt-dlp 多次收到 YouTube 的登录/机器人校验，任务要求的输入无法正常下载 | 确认外部素材阻断，未证明 Dragonball 故障 |
| `mteb-leaderboard` | 60 分钟 | 多个猜测的 HF 仓库返回不存在/需认证，最后才转向结果 Git 仓库；两宿主直连同一 HF URL 也返回 401 | 外部仓库可访问性与 Agent 选路混合；不支持“AgentENV 代理屏蔽 MTEB” |
| `compile-compcert` | 40 分钟 | 先选不兼容 Coq 8.18，后通过 opam 安装 8.16.1；空轮询累计指定等待约 30 分钟，内部安装日志未导出 | 错误版本选择已证实；下载、编译、资源问题尚无法区分 |
| `torch-pipeline-parallelism` | 15 分钟 | apt 首次被 Agent 自设的 120 秒命令超时终止，重试及 CPU torch 安装成功；随后继续调试实现 | 依赖准备消耗预算，但不能归为持续安装失败 |
| `financial-document-processor` | 20 分钟 | 前段 OCR/系统工具安装已成功，后续主要探索手写 OCR；末段再安装 pip | 依赖开销与解题混合，末次安装不是整项超时的解释 |

### Mailman：镜像目录与 guest 启动语义不一致

对已有 `mailman.tar` 做只读 OCI 层检查：层
`sha256:184fb57f3d7e712a9102af39e7fc060c97c5007ccf24b194262aa8ffecb63e1b`
包含 `run/mailman3`，后续层没有对应 whiteout；基础层的 `/var/run` 指向 `/run`。
轨迹第 9 步报 `/run/mailman3/master.pid` 的 `FileNotFoundError`，第 10 步创建目录。
[pivot-init](../../../tools-image/pivot-init) 第 65–69 行在 `/run` 挂载新的 tmpfs，
只补建 `/run/lock`，能解释镜像目录不可见。实际安装的 tools 镜像内该脚本尚未
独立提取核对；已确认的是镜像到 guest 的目录差异及其启动失败。

这不是镜像下载丢失，也不意味着应保留旧 PID/socket。后续修复应恢复所需运行
目录及 owner/mode，优先复用包的初始化/tmpfiles 规则。还要独立检查 LMTP 就绪
问题；轨迹后来已出现 8024 监听，剩余时间仍用于 Postfix、权限和邮件流程调试。
单凭服务库的“system too busy”提示不能证明 Dragonball 调度造成超时。

### 外部资源：Oracle 合格没有覆盖全部实际输入路径

视频任务的原始 Oracle 直接写出结果，不下载 YouTube；其通过只能验证该解法及
评分器执行，不能证明 Agent 所需视频可以下载。应在环境资格阶段增加实际素材
可访问性检查，按源地址及 digest 缓存输入，避免用 Oracle 答案替代输入。

MTEB 轨迹中的响应明确提示仓库不存在或需要认证；401 本身不能区分不存在、
私有或 gated。两宿主禁用代理、无认证的 HTTPS 探针均复现
`mteb/leaderboard` API 401，控制仓库 `allenai/qasper` API 均为 200。
冻结的 egress 代理只透传 HTTPS CONNECT，不检查 HTTPS 路径、不生成 401。
因此轨迹中模型关于代理拦截的解释没有证据支持。

原始 MTEB Oracle 使用 `embeddings-benchmark/results` 的历史提交；Agent 长时间
寻找其他仓库，最后才启动该结果仓库的 clone。既存在所选外部资源不可获取，
也存在路径选择和等待消耗，不能全部归为网络环境问题。

### CompCert：证据不足，不把无输出轮询当成死锁

原始镜像是最小 Ubuntu，安装依赖属于任务步骤。Oracle 固定 OCaml 4.14.2、
Coq 8.16.1、Menhir 20230608，使用 `OPAMJOBS=1` 和 `make -j1`；Agent 的
版本与并行选择不同。前面的源代码下载、apt 和 opam 初始化有成功记录。

后段安装重定向到 `/tmp/opam_coq.log`，轨迹只有轮询，没有导出内部日志；
sandbox 删除后已无法从当前证据区分下载、编译、依赖求解或 guest 内资源故障。
因此保持待定，不将其归为已证实的 VMM 卡死。

## 超时预算与等待机制

29 项的 Agent 执行时长均到达原始预算，与异常消息中的期限相差不超过
0.05 秒，没有提前触发超时的证据。截止时异常栈 15 项在模型请求，14 项在
Terminus-2 的等待；取消所在位置只表示截止时正在做什么，不能直接当作根因。

按这 29 项任务相加（并行任务累计时长，不是整轮墙钟时间）：

| 记录项 | 累计秒数 | 占 Agent 累计时长 |
| --- | ---: | ---: |
| Agent 执行 | 45,600.45 | 100% |
| 已完成模型 API 请求 | 21,939.28 | 48.1% |
| 已记录命令指定的最短等待 | 22,930.40 | 50.3% |

已核对安装版本的 Terminus-2：命令使用 `block=False`，把模型指定的 `duration`
传给 `min_timeout_sec`，上限 60 秒；即使命令提前完成，仍会等待到该时长。
这是标准 Harbor 的行为，不是 AgentENV 独有修改。参见固定提交的
[TmuxSession](https://github.com/harbor-framework/harbor/blob/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/src/harbor/agents/terminus_2/tmux_session.py)
与 [Terminus-2](https://github.com/harbor-framework/harbor/blob/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/src/harbor/agents/terminus_2/terminus_2.py)。

模型时间来自原生 `api_request_times_msec`，只包含已经返回的请求；最后被取消的
请求不计入。命令执行、下载和编译可以发生在指定等待窗口内，不能重复加算。
剩余约 1.6% 包含未完成操作及其他开销，不是纯 VMM 成本。没有独立命令完成
时间戳，不能从 50.3% 推算可优化的空等比例，也不能据此扩大所有任务期限。

## 其他 23 项与宿主证据

其他 23 项目前未见明确平台故障，轨迹主要为算法探索、代码错误、自测失败或
缺少完成确认。例如 `polyglot-c-py` 反复收到编译错误，`path-tracing-reverse`
反复读解码结果，`custom-memory-heap-crash` 的目标本身含程序崩溃调试。
这些应用错误不能计为 VMM 崩溃；同时，未见平台故障不等于已证明平台没有
性能影响或所有失败都来自模型能力。

`cobol-modernization` verifier 为 1，但 Agent 未完成确认且原生异常为超时；
严格成功数不因本次调查改变。

- A 21:17 的节点/Lab `memory.events`、B 收尾的节点 `memory.events` 均无
  `oom`/`oom_kill`，A 节点内存约 37.7 GiB，上限 128 GiB。
- 所调查超时任务删除前 guest 可用空间最低约 47.64 GiB；宿主也有充足空闲。
  没有磁盘耗尽证据。
- 已采集的 server 日志未见 ERROR/panic，内核日志未见宿主 OOM 或 I/O 错误。
  启动容量建议、ublk 设备就绪过渡警告不能直接解释 Agent 阶段超时。
- B 全部清理通过、无剩余 sandbox；服务最终 exit 1 来源于 trial 异常，
  不能误读为宿主清理失败或队列中途停止。
- 缺少每个 guest 的超时前进程、内存、dmesg 和重定向日志。宿主 OOM 为零
  不排除 guest 内 OOM；空 egress 日志也不证明没有网络错误。

## 后续工作顺序（尚未实施）

1. 修复运行目录初始化兼容性，复验 Mailman 首次启动路径；保留新的 `/run`
   生命周期，恢复目录而非旧 PID/socket。
2. 在 `AgentTimeoutError` 后、删除 sandbox 前增加有界只读取证：当前进程、
   CPU/内存、dmesg 和关键任务日志。与 verifier 异常取证区分，避免结果为
   普通 0 分时跳过超时现场。
3. 补实际外部输入的资格检查；缓存视频素材和正确版本的结果仓库。
   资格应验证 Agent 需要的输入，不仅是 Oracle 的执行路径。
4. 根据已记录的安装需求复用 deb/wheel/opam 下载缓存，保持原始镜像、任务
   安装步骤及版本语义；安装成功与安装阻塞分别统计。
5. 若优化 Terminus-2 的完成检测，使用独立实验配置并保留标准基线，本轮不
   修改等待行为、扩大预算、重跑或重写成绩。
