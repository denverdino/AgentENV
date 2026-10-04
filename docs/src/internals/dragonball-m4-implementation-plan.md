# Dragonball M4 Single-host Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在授权测试机建立可重复、可清理的双节点模拟，验证跨节点恢复、回滚和同机测量。

**Architecture:** Python 标准库控制器建立私有网络和两个 namespace/cgroup 节点，复用 capability runner 执行非 root server。真实 API 驱动运行功能场景并输出原始 JSON 证据；不改变产品路由或默认 backend。

**Tech Stack:** Linux KVM/ublk、namespace、cgroup v2、Python 3 标准库、现有 Rust server 与 S3 fixture。

**Spec:** [单机演练设计](./dragonball-m4-single-host-design.md)

## Global Constraints

- 同机模拟不是跨物理宿主机资格；未冻结 SLO 不宣称通过。
- server/runner/构建使用 aenv；root 只管理测试基础设施。
- 默认仍 FC；不装 systemd、不改 Go/protobuf、不发布或推送。
- 仅新建 `/var/tmp/db-m4.*` 和对应独立 cgroup；保留所有旧状态。
- 每节点 12 GiB、4 CPU quota、2048 PID、swap=0；容量不足明确失败。

## Review Focus

- B 通过源绝对路径或宿主机 `/proc` 读到 A 状态，导致虚假的跨节点成功。
- 初始化失败/超时后遗留 runner、挂载或 cgroup。
- 断网错误被热缓存掩盖，或失败恢复删除持久快照。
- 回滚错误地把 DB same-ID resume 路由至 FC。
- 失败样本、kernel 差异、未测停顿被汇总成性能通过。

### Task 1: 隔离节点测试架

**Files:** Create `scripts/tests/dragonball_m4_lab.py`, `scripts/tests/test_dragonball_m4_lab.py`.

**Interfaces:** Produces `Lab(root: Path, run_user: str)` context manager; `Lab.start_node(name: str) -> Node`; `Node.run(argv: list[str], timeout: int = 30) -> CompletedProcess`; `Node.spawn(argv: list[str], log: Path) -> Popen`; `Node.disconnect()/reconnect()`; `Lab.close()`; `Node.address`, `Node.root`, `Node.cgroup`. CLI `--self-test` enters outer private namespaces and tests real isolation.

- [x] Write tests: missing controller support refuses before allocation; A/B distinct net/mount/PID IDs and UID != 0; same TCP port accessible independently; peer state and host proc inaccessible; disconnect fails connection then reconnect succeeds; success/exception cleanup leaves no cgroup processes. Assert behavior, not source strings.
- [x] Run `python3 scripts/tests/test_dragonball_m4_lab.py`; expected RED for missing implementation. Linux privileged tests run via explicit `--linux` and must not count skips as passes.
- [x] Implement resource-owned context manager, bounded readiness and teardown, private bridge and node namespace init, existing capability wrapper integration. Logs/state remain after teardown.
- [x] Run local tests and remote `python3 scripts/tests/test_dragonball_m4_lab.py --linux`; expected all assertions pass, no live owned processes/cgroups.
- [x] Commit `test: add isolated two-node Dragonball lab`.

### Task 2: 实际 server 跨节点恢复与故障

**Files:** Create `scripts/tests/dragonball_m4_scenarios.py`, `scripts/tests/test_dragonball_m4_scenarios.py`; modify Task 1 harness only for scenario integration.

**Interfaces:** Consumes Lab/Node. Produces `run_restore(lab: Lab, inputs: dict) -> dict`, JSON inputs with absolute DB server/config/assets, S3 binary and OCI image; result contains source/child IDs, backend identity, namespace evidence, fault outcomes and timings. HTTP requests use bounded timeouts and enforce expected status.

- [x] Write failing result-validation tests for missing RAM/volume oracle, same child ID and unexpected HTTP status; write actual remote scenario asserting committed snapshot restores on B after A stops and B cache starts empty.
- [x] Run unit tests and remote negative control; expected failures distinguish absent scenario from infrastructure readiness failures.
- [x] Implement real HTTP lifecycle and envd marker checks; start native S3 in outer test network as aenv, separate from A/B. Exercise disconnected B restore then reconnect/retry; verify durable catalog retained.
- [x] Run focused unit suite and actual Linux VM scenario; expected RAM/disk/volume/ID checks and fault assertions pass with cleanup receipt.
- [x] Commit `test: exercise Dragonball cross-node restore in single-host lab`.

### Task 3: 分池回滚与统一测量

**Files:** Extend `scripts/tests/dragonball_m4_scenarios.py` and its tests; create `scripts/tests/dragonball_m4_metrics.py`, `scripts/tests/test_dragonball_m4_metrics.py`.

**Interfaces:** Consumes Task 2 lifecycle; produces `summarize(samples: list[dict], conditions: dict) -> dict`, raw samples retaining operation/backend/status/duration/resource fields; report explicitly `performance_qualified: false` until every frozen gate exists and passes. FC and DB entrypoints remain explicit.

- [x] Write RED tests: new-create selection changes to FC but DB resume stays DB; mismatched backend restore rejected; errors count in failure rate; insufficient sample count cannot qualify p99; missing SLO/kernel mismatch prevents performance qualification.
- [x] Implement test-only entrypoint selection and serial backend-neutral HTTP measurements, excluding cold build/download time where explicitly labeled. Record all skipped capacity cases and unmeasured capture pause/IO bytes.
- [x] Run unit suite and Linux FC/DB rollback + serial samples; expected functional assertions pass, no invented performance gate, raw samples retained.
- [x] Commit `test: add split-pool rollback and backend-neutral measurements`.

### Task 4: 可重复执行与证据闭环

**Files:** Modify `Makefile`, `docs/src/internals/dragonball-m1-m5-roadmap.md`; create `docs/src/internals/dragonball-m4-results.md`.

**Interfaces:** Consumes Tasks 1–3 CLI and JSON evidence; produces opt-in `test-dragonball-m4` target and exact rerun instructions, host/resource/artifact identities, simulation results and open physical/SLO gates.

- [x] Add explicit-input Make entrypoint, reject missing paths/root misuse before VM allocation; exercise failure and successful invocation.
- [x] Run all new Python tests, remote lab/scenarios, formatting and relevant repository regression (`make test test-unit test-agent-integration clippy fmt`); preserve logs and actual exclusions.
- [x] Perform one fresh-context final review; address Important/Critical findings RED→GREEN and rerun affected/full checks.
- [x] Record measured results and scoped cleanup, mark simulated deliverables separately from remaining physical/performance qualification. Commit `docs: record Dragonball M4 single-host simulation`.

## 2026-10-03 补充执行：已完成 Tasks 1–4 不重做

依据同一 spec 的“物理双机与容量补充验收”补充章节，自动执行 Tasks 5–8。
保留本轮开始前 `memory.rs` 和 M5 结果中的已验证稀疏优化；该改动在最终回归一并复验。

### Task 5: 真实双宿主机恢复及故障

**Files:** Create `scripts/tests/dragonball_m4_physical.py`, `scripts/tests/test_dragonball_m4_physical.py`；共享 helper 如必须调整，先协调调用契约。

**Interfaces:** Consumes Lab/Node、Services/fixtures/start_server 和现有 API/guest oracle；produces explicit two-host CLI、每端 owned root、host boot ID、artifact hashes、source-stopped、empty-cache、fault/retry、markers、volume clone、cleanup receipt JSON。

- [x] RED：重复 host identity 拒绝、缺失 source-stop/cache/marker/volume 证据不得通过；传输截断/EOF/初始化失败清理属于本轮资源。Run `python3 scripts/tests/test_dragonball_m4_physical.py`，Expected FAIL（缺少 physical implementation）。
- [x] GREEN：SSH 控制端与私网 relay 只负责编排；复用非 root VM/fixture，不复制 durable layers 给 B。Run 同上，Expected PASS。
- [x] Run 两方向真实场景，Expected distinct host IDs、same binary identity、source node stopped、cold cache、disconnected failure、reconnected markers/clone/catalog 全通过，cleanup exit0；禁止把 fixture 在线说成整机宕机。
- [x] 提交本任务代码/测试：与共享 cleanup/矩阵一起提交为 `36b99aa`，避免 Make/import 的中间版本缺失依赖；最终证据保留于 results 和两端状态根。

### Task 6: 可比 guest kernel 资格

**Files:** 本轮独立远程测试资产及配置/命令/SHA 记录；未经核验不覆盖产品 deps manifest。

**Interfaces:** Consumes 已验证 FC/DB release 候选；produces 同 SHA kernel 的双后端 smoke 证据或具体不可比阻塞。

- [x] 复核原 FC virtio probe -22 日志及两种内核配置，定位实际差异；旧失败保留。
- [x] 必要时非 root 在新机构建独立候选，记录源版本/config/boot args；不修改 runner ABI 或身份校验绕过。
- [x] 两种后端验证 cold、guest command、capture/restore，Expected 同一 kernel SHA 且 oracle 通过。失败则保持比较门槛开放，不换不同 kernel 冒充可比。

### Task 7: 容量受限的可重复性能矩阵

2026-10-04 调整：用户要求优先完成功能测试，性能优化独立排期。复用矩阵驱动的
功能断言，但不以10%门槛或统计样本数作为功能验收前置；原性能运行记录保持不变。

#### 功能收尾（先执行，未完成不得由性能延期掩盖）

2026-10-04 最新用户指令：不再要求容量测试；完成功能验证后将全部修改合并至
本地main。下面容量组合不作为本次合并门槛，未覆盖状态保留，不能改记为通过。
合并前仍核验当前候选的功能、兼容/回滚、故障恢复、资源清理及全量回归；
不推送远端，不切换默认后端或生产流量。

- [x] 关闭 S3 初始化失败和高脏页10并发发布失败的PL0配置复验：保留具体响应/错误链，修改先回归 RED→GREEN。默认仍30秒，功能测试显式设置存储120秒、外层请求600秒，保持并发不变；新配置5批恢复及完整生命周期组合通过，S3精确初始化状态等待已验证。原30秒条件的失败保留，不据此宣称性能达标。
- [x] 新 runner 低/高脏页各5批、每批10并发恢复，逐批校验每个子实例完整工作集并清理；低脏页使用原30/180秒配置，高脏页按用户授权使用120/600秒配置，证据见结果页。快照复用不计作5轮并发capture资格，原30秒高脏页失败保留。
- [ ] 容量矩阵（用户明确移出本次合并范围）：1/2/8GiB、低/高脏页、1/10并发生命周期及100并发cold的剩余组合保持未覆盖；已有结果保留，不继续容量补测。
- [x] 最新候选重新打包、重定位验证及单机隔离恢复/回滚复验；相关仓库回归、逐节点清理修复与独立审计完成。2026-10-05干净源码`6a57e3a`完整`make test test-unit test-agent-integration fmt` exit0，DB-only release库956项通过；本次与历史结果分别记录，证据见结果页合并前回归章节。
- [x] 最新候选双向跨实例复验通过，只走172.21.70.157/158；旧机新Lab使用独立tmpfs-backed状态避开满盘，历史状态不删除。只验证功能，不证明落盘耐久性、性能或底层物理宿主关系；逐节点清理及独立审计通过，证据见结果页合并前复验章节。

#### 性能线（功能优先，不阻塞上述收尾）

相对10%门槛优化、充分重复的p95/p99、精确capture停顿/单VM峰值/对象存储字节及
冻结SLO继续保持未完成。下方原统计矩阵checkbox仅表示性能线，不代表功能结论。

**Files:** Create `scripts/tests/dragonball_m4_performance.py`, `scripts/tests/test_dragonball_m4_performance.py` 和必要 guest RAM workload helper；Modify `Makefile` 添加显式入口。

**Interfaces:** Consumes Tasks 5/6 的身份与候选输入以及原 Lab/API helpers；produces 按 memory/dirty/concurrency 分组的 raw JSONL、资源估算/实际值、p50/p95/p99、失败率、capacity-skip 和未观测项。

- [x] RED：低内存/磁盘组合拒绝或明确跳过；禁止将 serial 样本当并发；不同 cell 不混算；失败保留；少于20/100成功样本无p95/p99。Run `python3 scripts/tests/test_dragonball_m4_performance.py`，Expected FAIL（缺少矩阵实现）。
- [x] GREEN：实现最小 cell driver，真实非零 guest RAM 改写、恢复 checksum、并发屏障、API lifecycle 计时及 guest block I/O；始终明确是否缺 SLO/停顿/读取字节/模板等条件。Run 同上，Expected PASS。
- [ ] 使用预先记录的 cell/config/order/cache 策略串行跑两种后端，排除本任务并行构建；先小组合再容量允许的大组合。Expected 每项有原始成功/失败/容量拒绝证据，不虚构覆盖或降阈值。
- [x] 提交矩阵与测试：`36b99aa`（与物理 harness 共用 cleanup 和 Make 入口）；运行矩阵证据另行记录。

### Task 8: 回归、审查和开放门槛交接

**Files:** Modify `Makefile`、M4/M5 results 和 roadmap，保留已完成阶段历史记录。

**Interfaces:** Consumes Tasks 5–7 所有证据；produces 最终 scoped qualification 状态及可重复命令。

- [x] Run 新/旧 Python 单元、双后端打包契约、相关真实 VM 场景，以及 `make test test-unit clippy fmt` 和 DB 专项；全部最终回归通过，旧机 M2 容量保护中止与新机5/5复验分别保留，ignored 不算通过。统计矩阵独立按 Task 7 记录。
- [x] 独立 fresh-context 审查本轮完整变更及继承约束；Important/Critical 单轮 RED→GREEN 修复并重跑适用套件。
- [x] 分别交接功能结果与性能待办，更新跨ECS实例资格、容量未覆盖项和平台/生产边界。2026-10-05最新候选跨实例/回滚、完整Make、DB-only库、严格Clippy/fmt、打包与驱动回归通过；见结果页。容量按用户指令排除，性能与生产门槛不冒充通过；仅合并本地main，不推送/发布/切流。
