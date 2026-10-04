# Dragonball M4 演练与资格结果

2026-10-04 验收口径调整：按用户要求优先完成功能测试，性能优化独立跟踪。
功能线单独跟踪并发EOF、发布/夹具初始化失败及适用候选复验；性能10%门槛与未冻结
SLO不再阻塞功能验收。以下历史证据和失败结论不改写，容量skip不算功能通过。

当前：已有单机、双向跨实例恢复及共同 guest kernel 功能证据；20轮统计对照共1,200个
样本无功能失败，但启动/恢复/fork 等 p95 超过相对10%门槛，性能未通过。后续10并发
发现的 DB 恢复 EOF 已修复并复验；发布超时在显式PL0配置下复验通过，原30秒条件
失败保留。2026-10-05最新候选双向跨实例复验和合并前回归通过，功能验收闭环；
容量受限组合、冻结SLO及生产门槛仍开放，不属于本次功能合并范围。
本页前半记录原始单机阶段；2026-10-03/04 双机、同内核及统计
工作见末尾补充，原始失败和不可比结果保留，不回填为成功。

## 2026-10-04 功能优先补测

最新用户指令排除容量测试：功能验证通过后合并全部修改到本地main，不推送。
剩余容量/性能组合不是本次合并门槛，也不被标为通过。

### 合并前跨实例功能复验

当前PL0候选（node `0f26837`、runner `2c094ff`制品、共同kernel）双向复验exit0，
原生功能断言全部通过：源节点进程停止、空目标缓存、断网500、重连恢复新ID、
RAM/rootfs/volume内容、独占卷新ID、catalog保留。两端制品SHA一致，实际UID999；
跨机数据和候选包传输均走172.21.70.157/158。此处是ECS跨实例，不证明物理宿主独立。

| 方向 | 源实验根 `/var/tmp/db-m4.` 后缀 | 目标实验根后缀 |
| --- | --- | --- |
| 旧→新 | `a4jwgxlr` | `hdt2bvkz` |
| 新→旧 | `7zlkcq72` | `nmlvvt7i` |

旧机根盘仅余2.6GiB，本次仅将新建Lab状态经私有mount namespace绑定到
`/tmp/aenv-pl0-peer.SRy4hX/retained-labs/<实验根名>`（现有tmpfs），未改产品或功能断言。
新机仍使用PL0磁盘；此混合存储配置只验证功能，不用于性能或主机断电耐久性结论。
旧机候选资产临时bind mount已卸载，tmpfs内状态保留；重启会丢失这些临时状态。
日志/结果/独立审计已另存新机
`/var/tmp/aenv-functional-candidate.r15iBk/merge-physical-evidence/` 和
`merge-physical-audit.log`，本地控制日志 `merge-physical.log` 也已归档。
审计确认四个自有cgroup消失、两端无aenv进程、新机ublk为空；旧机原有0–26号
设备基线不变，仅自动回收本轮27/28号。临时relay/传包服务均退出。

合并前全量回归首轮在setup-only依赖安装权限处中止；补齐临时apt-get权限后，
FC库测试980项通过、1项 `explicit_target_publishes_mixed_stack_end_to_end`
失败、5项忽略，失败为执行临时regctl脚本时 `ETXTBSY`。该用例单独1/1通过；
后续探针确认新机sudo-rs忽略`sudo -E`，首轮声明的8线程及第二轮声明的1线程均未
透传到capability runner，因此不能把它们描述成实际对应并发度。第二轮FC981项通过，
但21项integration因依赖/daemon路径丢失而在初始化阶段失败，并未进入VM验证。
通过测试期间独立sudoers的显式`env_keep`修复环境，实际探针确认state/deps/daemon
路径及`RUST_TEST_THREADS=1`均正确；临时配置在控制脚本退出时撤回。
随后完整套件用已验证的1线程调度重跑，不跳过用例、不改断言，原始日志均保留。
该轮FC库981项通过，但VM启动因本次编排选取的state路径过长失败；直接执行
Firecracker探针明确返回`path must be shorter than SUN_LEN`。随后改用隔离短路径
`/var/tmp/amg.96ZqXI`，未修改产品或用例；失败轮次遗留设备0经日志归属核验后回收，
回执 `merge-envkept-cleanup.log`。旧state与失败日志保留。
短路径下真实VM integration21/21、orchestrator3/3通过。随后显式ignored网络清理
测试4/5通过、1项失败：串行libtest将测试名前缀和`SLOT_IDX`放到同一行，父进程
未识别到已打印的标记。提交`c0ab867`仅在测试子进程首个标记前补换行，保留真实
网络分配、故意panic以及namespace/veth/route清理断言；同一原生测试先失败后
通过（1/1，0.23秒），`merge-network-green.log`。没有修改运行时代码。
最终全量结果另记，不能由这次单用例成功推断全套通过。
下一轮FC库981项、VM integration21项、orchestrator3项及显式网络清理5项均通过，
envd与常规storage套件通过；显式MinIO用例7项因测试账号不能连接Docker socket
全部在容器创建前失败。只读探针确认socket为`root:docker 0660`，aenv不在该组，
直接`docker info`返回permission denied。回归控制器仅在运行期间添加aenv的socket
ACL，退出时恢复原ACL并核对一致，不修改产品、永久组成员或测试断言。
失败记录保留在`merge-full-make-final.log`，补齐环境后的整套重跑另存
`merge-full-make-docker.log`；原有日志不覆盖。
该轮`make test`通过（包括显式MinIO 7/7），随后`test-unit`的runner父进程退出测试
在串行libtest下等待标记不返回。有界strace复现记录`test runner_helper ... `和
`guard-ready\n`拼接为同一行；终止本轮自有辅助进程后，保留原始失败和超时证据。
提交`6a57e3a`仅修正两个测试同步标记的行边界，并固定子测试串行以持续覆盖此问题；
真实父退出/子进程SIGKILL及回收断言不变。相同runner测试2/2通过、两个辅助用例
按设计ignored，耗时0.01秒；`merge-parent-marker-green.log`。无运行时代码变化。

### 2026-10-05 合并前完整回归

新机干净源码`6a57e3a`，UID999、隔离state `/var/tmp/amg.96ZqXI`、实际单线程测试，
完整`make test test-unit test-agent-integration fmt` **exit0**。日志
`/var/tmp/aenv-functional-candidate.r15iBk/merge-full-make-serial-fixed.log`，本地另有副本。
以下列单次套件结果，不重复累加Make依赖重跑或辅助子进程：

| 套件 | 通过 | 说明 |
| --- | ---: | --- |
| 默认FC库 | 981 | 另5项ignored由后续显式调用全部通过 |
| VM integration / orchestrator | 21 / 3 | integration另3项ignored不计通过 |
| envd filesystem / process | 1 / 3 | 真实debug容器 |
| OverlayBD库 / 注册表读取集成 | 383 / 4 | 库另3项ignored不计通过 |
| ublk库 / daemon库 / daemon集成 | 18 / 49 / 37 | capability runner |
| MinIO真实存储 | 7 | 显式`--ignored`执行 |
| Dragonball runner / 协议 | 47 / 25 | 不重复计辅助子测试；本次Make未执行硬件ignored项 |
| CLI / linux-cap | 110 / 3 | 非root测试 |
| snapshot OSS E2E | 6 | 显式`--ignored`执行 |

能力委派、service模板校验、CLI/BuildKit打包安装契约和格式检查也随Make通过；
没有安装systemd服务。硬件专项证据见先前M0–M3/组件资格及当前M4候选实测，
不能把本轮Make中的ignored项改记为通过。无Go/protobuf变更，本轮未重跑Go套件。
临时sudoers已撤回，Docker socket ACL恢复后与原记录逐字一致；所有失败轮次保留。
同一干净源码随后完成以下检查，控制器整体exit0：

```sh
cargo test --release --locked --offline -p agentenv --no-default-features --features hypervisor-dragonball --lib
cargo clippy --release --locked --offline --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
make test-hypervisor-packaging test-dragonball-m4-unit
```

DB库956通过/5 ignored；严格Clippy、fmt、双后端打包契约通过；M4驱动47通过、
3项需显式worker输入的用例skip。日志分别为同目录`merge-db-lib.log`、
`merge-clippy.log`、`merge-fmt.log`、`merge-packaging-driver.log`。
依赖`proc-macro-error2`的future-incompatibility提示仍保留，不是Clippy错误。

独立合并审查覆盖默认FC/编译选择、恢复身份与记录保留、内存backing生命周期、
capture/卷/fork回滚、runner等待兼容、OSS超时及夹具/清理；无Critical/Important/Minor。
其后两处测试标记修复另有真实RED→GREEN和上述完整回归，不涉及运行时代码。
功能验收至此闭环；容量测试按用户指令排除，性能/SLO、PVM/aarch64新资格、
物理宿主独立性、断电耐久性与生产切流不在本次结论内。默认Firecracker不变。
最终资源审计：aenv进程及测试容器为空；确认无持有者后，按本轮daemon日志归属
回收残留ublk0，回执`merge-final-device-cleanup.log`，随后新机ublk为空。
临时权限恢复记录已核对；历史实验文件、共享依赖、候选包及旧机设备基线未删除。

### 测试产物保留与磁盘占用

2026-10-04只读排查：新机约107个`db-m4.*`实验目录实际分配约398.93GiB，
`/var/tmp`总计约422GiB；旧机`/var/tmp`约144GiB，早期M4目录主要为大体积
`build-deps`编译产物。单个22GiB实验中约14GiB为临时S3仓库、7.3GiB为节点状态，
日志和JSON结果仅数MB。主要原因是多轮完整快照/缓存/测试仓库均保留，并非业务数据增长。
“运行时清理通过”仅指进程、设备、cgroup等清理，不表示历史测试文件已删除。
历史产物清理需要先保存结果/日志/配置/校验信息，再核对引用和活跃状态；当前回归
目录、最新候选包、共享依赖及归属不明文件不能直接删除。

### 前序功能补测记录

用户最新确认当前测试服务器为阿里云 ECS、PL0 云盘。环境按此记录；跨两个实例
的恢复证据不额外证明其底层物理机关系或裸金属平台资格。

使用下文 `2c094ff` 修复后的 runner 候选，保持原始 API/存储超时，不降低请求
并发。测试均在新机以 UID999 运行；没有开启性能优化或改变默认后端。

- 低脏页、1GiB、10并发恢复连续5批通过：50次恢复及50次完整512MiB工作集校验
  全部成功。根 `/var/tmp/db-m4.rhuj46cb`，逐批证据
  `functional-restore-batches.jsonl`；日志位于
  `/var/tmp/aenv-m4-runner-restart.gUJrxn/logs/functional-low-five-batches.log`，exit0。
  使用原始10并发capture准备快照，随后5批复用这些快照；不是5轮并发capture。
  每批之间删除子实例。原生 `samples.jsonl` 仅包含最后一批恢复，不能与逐批文件
  重复计数。fixture峰值8,319,684,608字节，无OOM，独立清理审计通过。
- 1GiB、100并发冷启动100/100成功，根 `/var/tmp/db-m4.26givyp5`，日志
  `logs/functional-cold-100.log`，exit0。节点预算160GiB/64CPU，fixture2GiB，
  容量检查先通过；冷启动检查包括 envd 命令就绪，不包含100并发快照/恢复或脏页
  工作集。fixture峰值891,781,120字节，无OOM，独立清理审计通过。
- 低脏页、1GiB、10并发完整生命周期组合141/141成功，根
  `/var/tmp/db-m4.ggkcxpx9`，日志 `logs/functional-low-allops-c10.log`，exit0。
  覆盖cold、工作集初始化、capture、restore、10个并发fork1请求、单请求fork10、
  16MiB写入与fsync、pause/same-ID resume、10个模板构建，以及各恢复路径完整
  工作集校验；不把单请求fork10误称为10个并发fork10请求。
- 高脏页、1GiB、10并发仍失败：`/var/tmp/db-m4.5a_x_bds` 创建/工作集初始化
  各10项成功，10个capture均在内存层发布阶段出现 S3 `write part timeout`，
  尚未进入恢复。fixture峰值9,914,793,984字节，无OOM，原始失败及日志保留。
  已核对 OpenDAL 0.55 的30秒分片超时与默认后台执行器；观察到共享磁盘等待，
  但尚未证明唯一根因，不能把失败归为性能问题后略过。

以上成功批次均另查确认自有 cgroup、UID999进程及 ublk 控制设备已回收。
这些结果增加功能覆盖，不代表全部功能资格已关闭，也不代表达到性能SLO。

### ECS/PL0 显式超时配置

用户允许针对慢存储增加等待预算。新增 `[backend.oss].timeout_secs`，省略仍使用
30秒；零值在解析配置时拒绝。该值透传快照发布/读取、镜像导出和 OverlayBD
远程层读取，不改变重试次数、分片大小、上传并发或默认虚拟化后端。
PL0功能复验使用存储120秒、测试驱动外层HTTP600秒，结果单独记录，不覆盖上述
30秒配置下的失败，也不代表存储吞吐改善。

新增配置透传/非法值测试先在原实现失败；修复后Linux下7项相关测试通过，包括
真实延迟HTTP服务验证1秒配置失败、4秒及未配置默认值成功。
证据位于 `/var/tmp/aenv-functional-candidate.r15iBk/timeout-{red,green}.log`。
本机M4驱动测试42项通过、3项Linux检查明确跳过，两种后端打包契约检查通过。
Linux全库测试使用8线程时Dragonball956项、Firecracker981项通过，各有5项忽略。
首次默认大并行测试保留1项失败：代理大请求上传的100毫秒测试限时返回504；该测试
单独复验通过，未修改其限时或断言。原日志 `db-lib.log` 与后续
`proxy-isolated.log`、`db-lib-threads8.log`、`fc-lib.log` 全部保留。
Linux全工作区 `cargo clippy --release --workspace --all-targets --all-features -- -D warnings`、
格式检查及M4驱动测试通过（42项通过，3项需显式worker输入的检查跳过）；
依赖 `proc-macro-error2` 的未来兼容性警告保留。

干净提交 `0f2683746dc2e44679a00615c392c64c04b1ad4e` 的 release Dragonball-only
节点程序构建通过，SHA256为
`18346b7f6eccb595a5e88c69acfadfa7e68fb7e3919e82c02b4352318f952dab`。
未覆盖旧候选程序、安装systemd服务或改变Firecracker默认值。

新超时配置下高脏页、1GiB、10并发快照准备及连续5批恢复通过，exit0：
`/var/tmp/db-m4.m2915la5`，日志
`/var/tmp/aenv-functional-candidate.r15iBk/pl0-high-five-batches.log`。
50次恢复及50次完整512MiB工作集校验全部成功；5批复用并发准备的10份快照，
不能计为5轮并发capture。服务配置及四份OverlayBD生成配置均实查为120秒，
结果记录外层HTTP600秒。fixture峰值8,234,049,536字节，无OOM；独立审计确认
所有样本通过、自有cgroup消失、UID999无剩余进程且ublk控制设备为空。
原生60项样本含最后一批恢复，不能与逐批文件重复计数。
这关闭了新配置下该恢复场景的功能复验，不证明原30秒配置已稳定或性能达标；
其后同配置高脏页、1GiB、10并发完整生命周期组合141/141通过，exit0：
`/var/tmp/db-m4.bookxxqu`，日志 `pl0-high-allops-c10.log`。覆盖冷启动、capture、
restore、10个并发fork1请求、单请求fork10、写入/fsync、pause/same-ID resume、
10个模板构建和各恢复路径完整内存校验。fixture峰值8,962,646,016字节，无OOM，
独立资源清理审计通过。剩余矩阵覆盖仍单独跟进。

2/8GiB、低/高脏页的单并发恢复路径补测24/24通过：每个组合包含冷启动、工作集
初始化、capture、源实例完整内存校验、restore和子实例完整内存校验。2GiB工作集
为1GiB，8GiB工作集为4GiB，均不是抽样marker。四个实验均无OOM，独立检查确认
其cgroup消失，批次结束UID999进程及ublk控制设备为空。

| 内存 | 脏页 | 实验目录 `/var/tmp/db-m4.` 后缀 |
| --- | --- | --- |
| 8GiB | low | `6j3lc9qp` |
| 8GiB | high | `filr5xmh` |
| 2GiB | low | `i1fw0qr5` |
| 2GiB | high | `2a_drz03` |

首次8GiB/high尝试 `8vh4005x` 在任何VM启动前失败（0样本）：健康接口已就绪，
建桶仍返回HTTP503、XML错误码 `XMinioServerNotInitialized`。原响应和失败保留。
测试驱动提交 `00fd280645a31b20ce577796a41faae17abbf6dd` 仅在初始化阶段对这一
精确状态进行30秒总期限内的等待；权限错误、其他5xx、格式异常和传输失败不重试。
没有增加产品存储重试次数或VM操作重试。真实HTTP/curl回归先复现原503失败，修复后
恢复/错误分类检查通过；总期限用受控时钟验证以避免依赖250毫秒内的进程调度。
小范围审查无Critical/Important，已处理测试时限易抖动的Minor；本机和Linux驱动
测试均45项通过、3项需显式worker输入的检查跳过。

候选包 `/var/tmp/aenv-functional-candidate.r15iBk/pl0-candidate.tar.gz` 打包和重定位
校验通过，SHA256 `e8068c764cd508f1f93be5a68c8fb2ac3f182a4f4825d4301892227ef7cffc9e`。
节点源码仍为 `0f26837`，后三个大内存补测使用重定位后的程序与资产；共享daemon
复用此前已验证制品，NOTICE明确这一来源，不冒称与新节点同次构建。没有公开发布。

2GiB、10并发冷启动补测10/10通过：`/var/tmp/db-m4.36xua1i8`，日志
`pl0-ready-2048-cold-c10.log`。envd就绪检查全部成功，fixture无OOM，清理回执及
独立进程/设备检查通过。8GiB、10并发冷启动 `7ml1k3uj` 在任何VM操作前被运行时
磁盘保护拦截，0样本：该检查要求8GiB保留量加80GiB单批内存量，实际约51GiB空闲。
初始cold准入估算通过，但不代表运行时检查通过；保留两者结果，不绕过保护。
最新容量记录 `pl0-functional-capacity.json` 明确2GiB/10并发restore受磁盘容量限制，
8GiB/10并发及1GiB/100并发restore同时受内存、磁盘限制。容量跳过不计成功；
旧机空闲约2.6GiB，仅作只读检查，不在其上复跑最新候选或删除历史数据腾空间。

候选回滚 `frrwi9rn` 的20项功能检查及故障恢复 `o_jemydo` 的RAM/rootfs/volume
检查通过，但独立审计发现后者源节点强停后仍遗留两个ublk控制设备，原生
`cleanup=passed` 不能作为清理通过证据。两个设备均按该源节点daemon日志归属
手动回收，回执保留在该实验的 `ublk-cleanup.json`；没有删除历史文件或快照。
测试驱动修复 `31aea78` 在Lab停止后逐节点调用既有归属清理，分别保存回执，
任一失败使入口失败；操作失败及Lab关闭异常也尝试清理所有已建节点。
回归先失败再通过，小范围审查无Critical/Important，并补齐关闭异常测试。
本机与Linux驱动均47项通过、3项需显式worker输入的检查跳过；远程日志
`/var/tmp/aenv-functional-candidate.r15iBk/cleanup-ready-unit.log`。

修复后的原生入口复验exit0，日志 `pl0-cleanup-{rollback,restore}.log`：回滚根
`/var/tmp/db-m4.ltxddj2k`，20/20功能检查成功；恢复根 `/var/tmp/db-m4.7sh99lig`，
源节点强停、空缓存、断网500、重连恢复新ID、RAM/rootfs/volume内容、独占卷克隆
及catalog保留均通过。两次使用同一120/600秒配置、干净 `31aea78` 驱动、重定位
候选与共同kernel，实际节点均UID999、release、源码 `0f26837`。
独立审计 `pl0-cleanup-audit.log` 确认各自cgroup消失、逐节点清理回执存在且成功；
恢复源节点设备0/1由入口自动回收，批次结束UID999进程及ublk控制设备为空。
历史失败、手工清理回执及本次自动清理回执分别保留；没有安装服务或删除历史制品。

原始单机阶段（2026-10-03）：单宿主机双 namespace 节点的恢复、故障重试、分池回滚已实测
通过；**M4 跨物理主机与性能资格尚未完成**。默认仍为 Firecracker，未安装服务、
切换生产流量、推送或删除历史状态。设计见[单机演练](./dragonball-m4-single-host-design.md)。

## 环境与制品

测试机 `147.139.168.159`，Linux 7.0.0-28-generic、Xeon 8269CY、104 logical CPU、
187 GiB RAM。用户扩容后的系统盘 200 GiB，ext4 已占满扩容空间；本阶段未执行
分区或文件系统扩容。每节点独立 mount/net/PID namespace、home/run/deps、私有
`/tmp`，12 GiB memory.max、swap=0、4 CPU quota、2048 PID。root 仅编排，实际
server/runner 为 `aenv` UID999。单机仍共用 CPU、host kernel、页缓存和 ublk 控制器。

远程证据根目录：`/var/tmp/agentenv-dragonball-m4-20261003`。两个 release server
均为生产代码 `e6b5685`；DB-only/FC-only 身份由 `--component-info-json` 验证。
M3 候选资产保持不变，Kata pin 仍是 `c77812d669737c11cb16321827acc1c79bf065f3`。

| 资产 | SHA256 |
| --- | --- |
| DB server | `d46821e3ce3b7bc28c0e65ca5da1540dbbd0d4a498addcc32d761f267377266c` |
| FC server | `d09eb0f588521bdf52216ed3875d83c62734df6812ba221d051790ea631e7f70` |
| DB runner | `578c1fc1a32195cd58b9d45004b1746039a6c2ef36f49f3948f9a95ec7eb5e0b` |
| Firecracker 1.15.1-patch-v1 | `280fda256adc3093d9e14d8291b77eb3b8bab33f734a7fb96f0ab048a8aa6f51` |
| DB kernel 6.1.175-dragonball-m2.1 | `d3792b0442c673f774f6c31f58d41231478fe9b8ca5b56c2991f719181c1230b` |
| FC kernel 6.1.175 | `cd7c5a06a2026a2d8a0cddf60b41a5d76a6ac025e54439eb3ee5c9e57a60c67b` |
| tools 0.1.0 / envd 0.5.15 | `60ba806a44975c7b2ca382f680de9da87af22ac65e288a683a921d1829e1c03d` |

## 功能证据

- Linux 隔离/异常清理/网卡消失竞态 3/3，`logs/lab-race-green.log`。验证相同端口
  独立监听、节点 UID/namespace、peer state 与 `/tmp` 不可读、断网恢复、超时清理。
- `logs/restore-5.log` 和 `restore-6.log` 均 exit0。真实 release HTTP 创建 guest
  与独占卷，写入 RAM/rootfs/volume marker，提交 S3 snapshot，再停止源节点全部
  进程。B 空快照缓存下断网恢复失败（500），恢复网络后从同一 durable catalog
  成功启动新 ID，三种 marker 正确，独占卷克隆为新 ID，catalog 保留。
  最终重复证据 `/var/tmp/db-m4.jl68tjma/result.json`：snapshot
  `01a10088-681a-7251-94db-b2662451e0b6`，源
  `01a10088-459c-7243-812d-f73af5a47234`，子
  `01a10089-4e3e-7162-989e-8aac754f70dc`。
- `logs/rollback-4.log` exit0，`/var/tmp/db-m4._u112teq/result.json`：DB/FC 各自
  cold、capture、restore×3、fork1/3、guest 16 MiB fsync、pause/resume，20 个
  测量操作全部成功。测试选择器将新建入口改为 FC；两种 paused 实例仍在原后端
  same-ID resume，RAM/盘 marker 保留。异类 snapshot 双向 400，实例列表和
  durable catalog 不变。此选择器是测试驱动，不是生产 gateway 路由实现。

每次成功运行的 stdout 有 cleanup receipt；对应 namespace 进程和 cgroup 已回收。
`result.json`、`samples.jsonl`、server/serial/fixture 日志及测试状态保留供复查。
审查修复后的完整正常路径重放同样 exit0：`logs/review-final-restore.log`
（`/var/tmp/db-m4.kt8pua2x`）、`review-final-rollback.log`
（`/var/tmp/db-m4.5njz3lzs`，20/20 操作）与 `review-final-lab.log`（3/3）。

## 测量边界

这是 1 vCPU / 1 GiB、pool/compression/startup pack 关闭的串行 smoke。DB 先于 FC，
host page cache 未控制。不同 kernel，`conditions_comparable=false`，
`performance_qualified=false`。以下只是 `rollback-4` 的操作耗时原始摘要（秒），
不能用于宣称达到相对 FC 的 10% 门槛；每项一次，restore 为三次中位数。

| 操作 | DB | FC |
| --- | ---: | ---: |
| cold + guest command ready | 1.147 | 0.975 |
| capture API | 19.056 | 0.806 |
| warm-node restore | 1.198 | 0.171 |
| fork 1 / fork 3 | 8.474 / 8.206 | 0.334 / 0.403 |
| 16 MiB write+fsync RPC | 0.102 | 0.041 |
| pause API | 9.120 | 0.288 |
| same-ID resume API | 0.295 | 0.166 |

差距需要后续受控基准分析，不能因本轮功能成功而忽略。capture API 耗时不是
guest 停顿。保存了操作前后 CPU、RSS/PSS、cgroup memory.current/peak 和 io.stat；
peak 是节点累计高水位（包含缓存），不是每 VM 独立峰值。未测对象存储上传/读取字节、
独立 capture 停顿、template build timing、2/8 GiB 高脏页及 10/100 并发矩阵。
p95/p99 样本不足输出 null；先前失败运行也保留，不把最后一轮 0/20 失败率当成
整个开发过程或生产可靠性统计。

## 发现与修复

1. 原 OSS conditional create 使用 OpenDAL 0.55 S3 不支持的 `if_none_match("*")`，
   导致真实卷创建 HTTP500。`e6b5685` 改为 `if_not_exists(true)`，不改 ETag CAS。
   真实 OpenDAL adapter + HTTP 服务回归先复现 Unsupported，再验证首次创建、
   不覆盖既有值、拒绝旧 ETag、接受当前 ETag（3/3）。DB lib 949/0，另5 ignored。
2. 私有节点原先共享 `/tmp`；实际 peer marker 回归先失败，再通过独立磁盘目录
   bind 修复。节点退出后 veth 可异步消失，删除失败只在确认已不存在时容忍。
3. 断网后 link-UP 不等于 HTTP 可达；记录 ARP failed/incomplete，加入有界健康
   探测。网络恢复时间独立记录，不计入 restore 时间。
4. 同一 DB kernel 在 FC 下 virtio blk/net probe 返回 -22、无法挂载 vda 而 panic
   （`logs/fc-diagnostic-1.log`，`/var/tmp/db-m4.w5vdwlyg` 串口）。功能回滚改用
   此前验证的 FC kernel，保留不可比标志；未修改 kernel 以掩盖失败。
5. FC guest 命令的额外 `E2B_SANDBOX_ID` 断言失败，但 API201/命令通道成功。
   中立就绪检查改为显式传入 `envVars.M4_READY` 并核验精确值；该 smoke 不宣称
   FC/DB 自动身份环境变量路径完全相同，未修改产品行为。

## 重跑

Linux root 编排，要求非 root `aenv` 账户有 KVM/ublk 访问权限，Python >=3.11、
unshare/nsenter、iproute2、setpriv、curl（支持 aws-sigv4）、openssl，cgroup v2 已
在宿主根启用 cpu/memory/pids（io 可选）。不自动启用全局 controller 或安装依赖。
先运行无权限单元，再显式运行 Linux 测试：

```bash
make test-dragonball-m4-unit
python3 scripts/tests/test_dragonball_m4_lab.py --linux
python3 scripts/tests/test_dragonball_m4_termination.py --inputs /absolute/inputs.json
make test-dragonball-m4 AENV_M4_INPUTS=/absolute/inputs.json
make test-dragonball-m4 AENV_M4_INPUTS=/absolute/inputs.json AENV_M4_SCENARIO=rollback
```

`inputs.json` 的绝对路径字段：

| 字段 | 内容 |
| --- | --- |
| server / server_fc | DB-only / FC-only release server；相同 git revision |
| daemon | uvm-ublk-daemon |
| ublk_cli | uvm-ublk 控制程序；在启动任何实验前检查，用于节点停止后的精确自有设备清理 |
| assets | `dragonball_m0_assets` 生成的资产 JSON：runner_path/kernel_path/tools_path/image_path/image_configs |
| deps | 已准备 overlaybd/ 与 regctl/ 子目录 |
| s3 / registry | 原生 S3-compatible server / OCI registry 二进制 |
| firecracker / kernel_fc | rollback 专用 FC 二进制及已验证 guest kernel |

控制器读取 `assets.image_path` 的 native OverlayBD lowers（有 file/digest 字段），
上传到独立测试 registry；节点通过网络读取，不直接消费这些 prepared rootfs 路径。
DB runner/kernel/tools/daemon 与 deps 源须放在 `/tmp` 之外，节点私有 `/tmp` 会遮蔽
宿主同名路径；FC 二进制/kernel 在进入节点前复制。不要传入生产 state 或真实仓库。
Make 入口不构建、不安装、不自动选择下载版本。远程本轮的全部输入在证据根
`inputs.json`；在另一机器重跑需重新准备并验证这些资产，不能照抄本机绝对路径。

## 仓库回归与未完成门槛

M4 新 Python 单元 11/11，Linux 隔离/清理 3/3，真实 CLI 内外层 SIGTERM 清理
2/2（`logs/termination-green.log`）。最终独立审查无 Critical；控制器终止绕过清理
和夹具初始化残留 cgroup 两项均按 Important 处理（后者由 Minor 重评），单轮修复
均先复现失败再通过。外层等待内层清理，内层通过 finally 回收，初始化失败撤回自身
空 cgroup；不对 SIGKILL、宿主宕机或恶意 namespace 逃逸作保证。

`make test test-unit test-agent-integration clippy fmt` 整体 **exit0**，
`logs/full-make-3.log`，源码 `791e353`（与最终 M4 相同 Rust 代码）。Cargo/VM 为
UID999，测试并发2、构建任务4；Make 的 host shell verifier 由 root 编排。
主要结果：FC library 978/0，另5 ignored 显式运行5/0；integration21/0（3 ignored
未算通过）、Orchestrator3/0；envd filesystem1/process3；OverlayBD383/0（3 ignored）、
daemon integration37/0、MinIO7/0；ublk18/0、daemon49/0、CLI110/0、OSS E2E6/0。
workspace 全 targets/features 严格 Clippy、fmt 和 capability/service/buildctl verifier
通过。保留既有 proc-macro-error2 future-compat 警告。未将其它普通 ignored 算作通过。

前两轮分别因编译 deps tmpfs 接近满载时链接 SIGBUS、另一 incremental tmpfs
ENOSPC 中断，原日志 `full-make-{1,2}.log` 保留。空闲 deps 缓存迁到扩容磁盘
`/var/tmp/agentenv-dragonball-m4-20261003/build-deps`，旧访问路径保留 symlink；
incremental 迁到腾空的 `/var/tmp/agentenv-components-build-20261002/incremental`，
同样保留旧路径。没有扩大 tmpfs、丢弃缓存或改动历史 VM 状态，代价是约52 GiB
磁盘占用及较慢的编译读写。迁移后可用 RAM 约112 GiB。临时 Docker/sudo 授权已撤回。

仍开放：真实两台兼容宿主机、受控可比 kernel/缓存/资源条件、大工作集/并发矩阵、
预先冻结 capture 停顿与峰值资源 SLO、统计基准及灰度生产目标清单。
单机模拟不解除这些门槛，也不授权切换默认或退役 FC 历史 reader。

## 2026-10-03 双机与同内核补充

第二台 `147.139.165.124`（私网 `172.21.70.158`）已配置独立非 root 运行环境，
与旧机 `172.21.70.157` 双向通信。用户扩容后新机磁盘为 500 GiB，根 ext4 已同步
扩展，初次复查可用 461 GiB，保留共同内核及 smoke 后约 456 GiB；无需再次扩容
分区。此容量不是全矩阵保证，每个 cell 仍以当时剩余磁盘及 RAM 准入。

2026-10-04 用户明确要求所有跨机测试使用内网。已再次读取两端 `eth0` 核对
`172.21.70.157/20`、`172.21.70.158/20`；跨机测试数据、服务与传输均使用这些
内网地址。公网地址仅供本地控制端登录，不作为两台测试机之间的测试链路。

### 真实双向恢复与故障重试

修复审查问题及 TMPDIR 后的最终 `reviewed-tmpdir` 重放整体 exit0，控制端结果及审计保存在新机
`/var/tmp/aenv-m4-physical-20261003/evidence`；本地原始工作目录为
`/tmp/aenv-m4-physical-20261003.wLuKC1`。运行状态根：

| 方向 | 源节点状态 | 目标节点状态 |
| --- | --- | --- |
| 旧机→新机 | `/var/tmp/db-m4.89caot2r` | `/var/tmp/db-m4.strcgt8w` |
| 新机→旧机 | `/var/tmp/db-m4.y16qdpjn` | `/var/tmp/db-m4.szqb2s09` |

两端 boot ID 不同；相同优化版 DB server SHA256
`0af38b2e91be2dd15fe4e428cdbc05eac95afe31f3fdf53affa3cdca0b3fdd2b`，以及相同
runner/kernel/tools SHA，实际 UID999。此物理重放使用原已验证 DB kernel
`d3792b04…`，共同内核资格独立记录于下节；不要混为一次测试。
server 身份为 `d94710d` 构建树加已核验稀疏优化，并非宣称未修改的 release tag。

两个方向均在提交后停止源 workload cgroup，目标空缓存断网恢复 HTTP500，重连后
从同一 snapshot 成功生成新 sandbox ID；RAM/rootfs/volume 标记、独占卷克隆新 ID、
catalog 保留全部通过。durable layers 经两台机器的私网 TCP 传输，SSH 只编排和传递
小型配置/证书/证据，未预复制快照缓存。源宿主的 S3/OCI fixture 保持在线，故这不是
整机断电测试。私网 relay 仅监听指定地址的19000/15443，退出后撤销。

主代理独立重跑两个 JSON 的 `validate_physical`，均通过且 `cleanup_errors=[]`。
最终两端审计无本轮 cgroup、进程、监听器或新增 ublk 设备：新机为空，旧机历史
0–26号设备保留。仅精确删除本轮 daemon 日志记录的分配，不卸载全局模块。
capture/restore 单次观察值分别为旧→新2.483/2.039秒、新→旧2.630/2.164秒，
不是 p95，也不是 capture guest 停顿或性能 SLO 资格。

### 同一 guest kernel 的功能资格

原 FC 失败定位至 DB kernel 的 `CONFIG_ACPI=y` / `CONFIG_PCI=n` 组合：Linux
6.1.175 的 ACPICA 默认 region handler 包含 PCI_CONFIG，但实现受 PCI 配置约束，
初始化返回 `AE_BAD_PARAMETER`，后续 virtio 探测失败。独立候选启用 PCI 并改用
真实版本 `6.1.175-dragonball-m4-common.1`，未修改 Linux 源码、产品 boot args、
runner 或原有发布资产。`olddefconfig` 自动选中的依赖也保存在完整 config diff，
不能将变更简化为只有两个配置行。

新机证据根 `K=/var/tmp/aenv-common-kernel.xGZSba`，非 root UID999、8 jobs 构建
exit0；`K/report.md`、`output/config.diff`、`compiler.txt`、`SHA256SUMS` 和构建日志
保留。共同 kernel SHA256 为
`5feae45cccfb354365e25be907e684533544c02e52df80c24032df2c727839cc`，配置 SHA256 为
`2e29a01ba73de7ae2dc2b96f0fd24726ffb95857ae67d08cb8ae2e546e4a3512`。

`K/smoke-2.log` 与 `/var/tmp/db-m4.ipc81td_/result.json`：DB/FC 两种 release
`e6b5685`、UID999、完全相同 kernel bytes/identity，cold/command、capture、
restore×3、fork1/3、fsync、pause/same-ID resume 及 RAM/rootfs oracle 共20/20。
cleanup passed，随后 cgroup 与 ublk 设备审计为空。首轮缺少复制 harness 相邻的
capability wrapper，在创建 VM 前失败；修正后使用上述原生 harness 重跑，原日志保留。

FC 的 ACPI 与 command-line MMIO 同时枚举产生8条重复资源 `-16` 警告；活动设备
正常且全部功能 oracle 通过。此发现路径差异仍需披露，尤其不能把 cold timing 当作
完全一致的启动过程。共同 kernel 只关闭内核可运行性前置项，不关闭性能资格。

### 显式性能与物理测试入口

```bash
make test-dragonball-m4-physical AENV_M4_HOSTS=/absolute/hosts.json AENV_M4_OUTPUT=/new/evidence
make test-dragonball-m4-performance AENV_M4_INPUTS=/absolute/performance-inputs.json
```

第二个命令默认仅生成容量计划；真正执行需显式 `AENV_M4_PERF_ARGS`，例如
`--memory-mib 1024 --dirty low --concurrency 1 --repeats 1 --node-memory-gib 32 --node-cpus 4`。
性能 inputs 在原 M4 字段上增加静态 Linux `workload` 绝对路径、用于精确回收本轮
设备的 `ublk_cli` 绝对路径及共同 `kernel_version`；物理 inputs 同样需要 `ublk_cli`。
DB/FC kernel 字节不一致会拒绝运行。并发、重复数或保留空间不足只记录 skip，
不自动降低请求规模或删除旧状态。每个样本保留 cell、失败、资源及缓存阶段；
p95/p99 分别至少需要20/100个成功样本，始终保留 `performance_qualified=false`，
直到未测项及 owner SLO 有独立证据闭环。

### 20轮统计对照：功能通过，性能门槛未通过

2026-10-03 14:06–15:34 UTC 的原生矩阵完整结束，2026-10-04 独立复核结果。
证据 `/var/tmp/db-m4-performance.zpp4ln5d/{conditions.json,cells.jsonl,samples.jsonl,result.json}`，
控制日志 `/var/tmp/aenv-m4-performance.FP7F2F/twenty-repeat-matrix.log`，报告
`failed_cells=0`。四组均20轮、各300个原始样本，合计1,200/1,200成功，恢复 RAM
校验、模板及生命周期均通过；四组 cleanup passed，独立复核 cgroup 全部不存在，
新机无 aenv 进程或 ublk 控制设备。该结果不是完整性能验收通过。

固定同一共同 kernel、tools/rootfs、release 身份，guest1GiB/1vCPU，持有512MiB
非零 RAM；低/高脏页每次分别改写约1%/50% guest RAM。节点限额32GiB/4CPU，
并发1、pool关闭、普通发布无压缩；每组新建节点缓存，宿主页缓存不刷新且未完全控制。
运行顺序 FC低→DB低→DB高→FC高，没有并行 VM 测试或构建。
`fork_n` 在此并发下 N=1，不作为大批量 fork 证据。

原生执行参数（以 root 控制器编排，实际 server/VM 为 aenv；须先核验 inputs 和容量）：

```bash
python3 scripts/tests/dragonball_m4_performance.py \
  --inputs /var/tmp/aenv-m4-performance.FP7F2F/inputs.json \
  --memory-mib 1024 --dirty low high --concurrency 1 --repeats 20 \
  --node-memory-gib 32 --node-cpus 4
```

各项 p95（秒；每格20个成功样本，p99全部未达到100样本要求）：

| 操作 | FC低脏页 | DB低脏页 | FC高脏页 | DB高脏页 |
| --- | ---: | ---: | ---: | ---: |
| cold API→envd ready | 0.733 | 1.151 | 0.738 | 0.877 |
| capture API | 8.635 | 9.588 | 8.625 | 9.663 |
| restore API→envd ready | 0.518 | 0.705 | 0.571 | 0.793 |
| fork-1 API→envd ready | 3.042 | 4.626 | 3.011 | 4.300 |
| fork-N（N=1）API→envd ready | 3.031 | 4.323 | 3.030 | 4.330 |
| pause API | 3.161 | 4.296 | 4.516 | 7.353 |
| resume API→envd ready | 0.241 | 0.381 | 0.997 | 1.016 |
| 空步骤模板 ready | 4.027 | 4.430 | 2.024 | 5.032 |
| guest16MiB随机写/fsync含RPC | 0.114 | 0.186 | 0.182 | 0.246 |

DB restore p95 比 FC 慢35.9%/38.8%，fork-1慢52.1%/42.8%，cold慢57.1%/19.0%；
均超过原设计10%相对工程门槛。capture API慢11.0%/12.0%，但它不等于 guest 停顿。
随机写测试含数据生成和 RPC，不等于独立块设备吞吐。不能依据功能0失败宣称性能通过，
也不能事后放宽门槛。`performance_qualified=false` 保持不变；具体停顿/峰值 SLO、
对象存储读写字节、严格缓存控制及更多工作集/并发覆盖仍未全部闭环。

状态根依次为 `/var/tmp/db-m4.6qwuxjjl`、`/var/tmp/db-m4.dybfir4s`、
`/var/tmp/db-m4.wdwahtrl`、`/var/tmp/db-m4.tfqs0def`，全部保留。复核时新机根盘
约358GiB可用，不清理历史证据以扩大矩阵。

只读分阶段诊断：两种 dirty 组各80次 payload>500MiB 的 DB 内存导出（排除空模板），
scan p50分别729/717ms，writer p50为2862.5/2885.5ms，总阶段p50为3600/3606.5ms。
高脏页 writer p95为5248ms，低脏页3007ms；来源为各自 `a.server.log` 的
`Dragonball memory capture completed` 事件。此分组包含 capture/fork/pause，
不是独立API分位数。现有稀疏方案对 standalone full capture 先扫描，再由 writer
读取非零页；保留 frozen lease 和取消后读任务持有规则。数据支持优先剖析写层及
二次读取路径，但未证明全部耗时由其中一个函数导致；不为消除差距绕过租约、
dirty/zero 或持久化同步，也不把尚未实现的优化计作通过。

### 2/8GiB 大工作集补测

2026-10-04，原生单轮矩阵 `/var/tmp/db-m4-performance.9c7wvi5p/result.json`：
2/8GiB、低/高脏页、并发1、双后端共8组，各15项，全120/120成功；全部 cleanup
passed，独立复核8个 cgroup 均不存在、无 aenv 进程及 ublk 控制设备。共同内核与
制品不变，节点32GiB/4CPU，guest持有配置内存的50%（分别1/4GiB）；所有恢复路径
执行完整工作集 checksum。单轮只作容量与功能覆盖，不能产生这些 cell 的 p95/p99。

| guest配置 | dirty | FC状态根 | DB状态根 | FC/DB节点累计峰值GiB |
| --- | --- | --- | --- | ---: |
| 2GiB | low | `db-m4.d78p21a_` | `db-m4.h8ysqn0y` | 7.35 / 7.23 |
| 2GiB | high | `db-m4.iwj57f5w` | `db-m4.0g0lp3un` | 8.27 / 8.03 |
| 8GiB | low | `db-m4.3welsueu` | `db-m4.bi5ms0cv` | 26.99 / 26.64 |
| 8GiB | high | `db-m4.bh2s12br` | `db-m4.6ac_4nno` | 30.52 / 27.44 |

状态根均在新机 `/var/tmp/`，含缓存的整节点累计峰值不是每 VM 内存用量。
结束后约306GiB可用。预检证据 `db-m4-performance.s9twheor`，控制日志
`/var/tmp/aenv-m4-performance.FP7F2F/capacity-2g-8g-20261004.log`；执行参数为
`--memory-mib 2048 8192 --dirty low high --concurrency 1 --repeats 1 --node-memory-gib 32 --node-cpus 4`。
原 SSH 传输在长时间静默后未返回退出回执：远程最终 JSON/日志已完整写入，且无测试
控制器、VM 或 cgroup，但 SSH session 仍在、发送队列有未确认数据。精确终止本轮
本地传输及远程空会话后，工具返回255；不把这个退出码写成0，也不将其混为120项
oracle失败。后续长任务启用 `ServerAliveInterval=30` / `ServerAliveCountMax=3`；
已有结果不重跑，原日志保留。

### 本轮回归与审查明细

10并发首轮在2026-10-04失败，证据
`/var/tmp/db-m4-performance.1g1_lb7z/result.json`：四组共120项，81成功、39失败，
不是完整并发资格通过。内核 OOM 记录、S3 的 `Killed` 日志及上传连接拒绝一致指向
测试夹具固定2GiB cgroup；实测 DB-low fixture `oom=4/oom_kill=1`，VM节点
`oom=0/oom_kill=0`。文件页缓存也计入夹具限额，不能只看 S3 进程 RSS。
四组状态 `db-m4.cl6lv6a7`、`db-m4.midotjfv`、`db-m4.q1kbcnnz`、
`db-m4.ocx74k7t` 全部保留；结束后四个 cgroup、不属于历史基线的 ublk 设备和
aenv 进程均不存在。

对此作测试架最小修复：显式 `--fixture-memory-gib`（2–128，默认仍2），容量
预检为节点限额 + 夹具限额 + 10GiB宿主余量；清理前写入
`fixture-resources.json` 的 max/peak/events，任何 OOM 都使 cell 失败，记录或
清理一项失败仍尝试其余自有资源。新增回归先复现预算遗漏、记录缺失及 OOM 漏判，
修复后本地39项通过、3项 Linux 显式跳过；远程同套件及实际 Linux C2/Lab4 exit0。
独立修复暂存根 `/var/tmp/aenv-m4-fixture-budget.mkLFN4` 保留原始测试架不变。
1GiB、low/high、10并发、单轮、节点32GiB/16CPU、夹具16GiB 的新预检
`/var/tmp/db-m4-performance.ihufy42v` 两组准入；真实重放结果另行记录，不覆盖
上述39个失败样本。未修改产品 VM 内存或放开无限制资源。

修复提交 `cd100f1` 的原生10并发重放整体 exit1，结果
`/var/tmp/db-m4-performance.79qwin_w/result.json`，控制日志为修复暂存根的
`c10-replay.log`。FC低/高两组各141/141通过，包含 fork-10 与完整 RAM 校验；
DB低组48成功、2个restore失败，DB高组20成功、10个capture发布失败。全362项中
350成功、12失败，不能算并发资格通过。四组夹具峰值7.35–9.35GiB，OOM计数均0，
说明夹具预算修复有效，但没有解决下列另行发现的问题：

- DB低：Start/WaitReady 分别在0.633/0.787秒返回 `early eof`；节点累计峰值
  18.3GiB，低于32GiB上限。保留的 runner stderr 为空，内核日志未找到 OOM/core
  事件；退出原因待诊断，不能臆断为超时或自动重试后抹去失败。
- DB高：10个并行capture在90–96秒返回上传失败，完整错误链为 S3 multipart
  `write part timeout`。S3未被OOM杀死；其原因与低组EOF分别排查，不放宽门槛。

状态根 FC低`db-m4.d7_htbtu`、DB低`db-m4.v5xa2zi8`、DB高`db-m4.pzhuey37`、
FC高`db-m4.0gmkver6` 全部保留。FC cleanup passed；DB因操作失败保留保守
cleanup状态，但主代理运行后独立检查四个cgroup不存在、aenv进程及ublk设备为空，
根盘约251GiB可用。后续广泛矩阵暂缓，只做显式标为诊断的缩小重放。

后续诊断将快照准备串行化以隔离上传问题，恢复仍为每批10并发；这些结果不加入
正式性能统计。`db-m4.iaofyblv` 中10个快照准备成功、恢复9成功/1失败，进程追踪
记录 runner 因 `SIGSYS` 退出。全调用追踪 `db-m4.jeyixd0f` 和低输出量追踪
`db-m4.tidx1mls` 均60项通过，说明追踪可能影响时序，不能据此抹去失败。
复用同批快照的有界重复诊断 `db-m4.a3e2jnqx`：第一批10个恢复成功，第二批
8成功/2失败后停止；失败线程172426/172428执行 `restart_syscall`（219）后被
SIGSYS终止，父进程 wait4 记录一致。原允许列表确实缺少该调用。
[Linux 调用说明](https://man7.org/linux/man-pages/man2/restart_syscall.2.html)说明
它用于恢复部分被停止/继续打断的等待调用。追踪本身也可能触发这种路径，因此
此处只确认具体兼容缺陷；修复后的无追踪恢复仍须复验原始 EOF，不能推定已消失。
完整诊断脚本、日志和状态保留在修复暂存根及上述各根目录，结束后自有进程、
cgroup和ublk均已独立复核为空。

兼容修复仅增加 `SYS_restart_syscall`，其余规则不变。新增真实过滤器子进程测试：
观察阻塞中的 `poll`，执行 STOP/CONT，要求正常超时退出；原代码在新机 UID999
运行时明确因 SIGSYS 失败，补丁后通过，原有禁止调用及线程继承检查仍通过。
独立只读审查无 Critical/Important；两项测试健壮性意见（停止等待的 deadline、
原始 waitpid 已回收子进程时避免二次回收掩盖错误）已处理并复验。
最终 runner/协议套件72项通过，显式 KVM CLI6项及 KVM adapter 单测1项通过，
严格 clippy、workspace fmt、release 构建 exit0；4个仅供子进程使用的 helper
未另算独立通过项。依赖 `proc-macro-error2` 的 future-incompatibility 警告仍保留。
证据根 `/var/tmp/aenv-m4-runner-restart.gUJrxn`，`logs/restart-red.log` 保留原始失败；
`runner-suite.log`、`runner-kvm-cli.log`、`runner-clippy.log`、`fmt.log` 为最终复验。
候选 runner SHA256 `07825077060208728dcdf680705d37a0219d54c5a5823f105aa4dcbe4c2caa2f`，
build ID `sha256:fb11bfdfa1e78864d791de39ad086ae691b644c70104d6aef316da55c3262bac`。
独立 `candidate-inputs.json`/`candidate-assets.json` 仅替换 runner 路径，旧资产未覆盖；
原生并发重放使用新 runner 创建的新快照。对照运行 `db-m4-performance.x5cz91ja`
中 FC 的60项全部通过；DB 在 S3 bucket 初始化阶段 curl22，0个VM样本，不能计作
候选恢复失败或通过。独立空载 S3 就绪诊断10次均HTTP200，未复现该初始化竞态；
具体失败响应尚未捕获，不通过增加重试宣称修复。

随后独立运行未修改的 `run_cell`，同一低脏页/1GiB/10并发/32GiB节点/16CPU/
16GiB fixture 条件，候选 `db-m4.wbvz4q1i` exit0，60项全部通过：cold、工作集
初始化、并发capture、源RAM校验、并发restore、子RAM校验各10项。没有 strace、
没有串行化capture或降低并发；仅额外观察 cgroup I/O 并保留 curl 错误响应体。
日志 `logs/candidate-db-c10-low.log`，I/O观察根 `db-m4-diagnostic.lqn0npyb`。
这是一次候选功能成功，不足以关闭原始间歇性 EOF，也不关闭高脏页发布超时或
相对10%性能门槛。运行时观察到 fixture I/O pressure full avg10约75.7%，无OOM；
提示共享盘等待显著，但尚不足以证明此前上传超时的唯一根因。

提交 `36b99aa` 的本地测试架回归 `make test-dragonball-m4-unit` exit0：37项通过，
3项 Linux worker 检查在 macOS 显式跳过；这些特权路径已分别在两台 Linux 上完成
physical16/16及 cleanup1/1，最终规范 TMPDIR 的 Linux Lab4/4、C 损坏注入2/2也通过。
`make test-hypervisor-packaging fmt` exit0，两种独立打包契约包含身份、禁止覆盖、
重定位和篡改拒绝；预期负向校验的错误输出不计作测试失败。

旧机重跑 `make test test-unit clippy fmt` 整体 exit0，日志
`/var/tmp/agentenv-dragonball-perf-20261003/logs/m4-continuation-full-make.log`。
测试仍以非 root UID999 运行。主要结果为 FC lib978/0（5个 ignored 另行显式运行
5/0）、integration21/0（3 ignored）、OverlayBD383/0（3 ignored）、ublk18/0、
daemon49/0、daemon integration37/0、MinIO7/0、CLI110/0；workspace 严格 Clippy
和 fmt 通过。其它 ignored 不算通过，保留 proc-macro-error2 future-compat 警告。
测试后临时 sudoers 已撤回，aenv 不在 docker 组，无遗留 aenv 进程。

远程源码是 `d94710d` 加 M5 inventory 修复和本轮稀疏 RAM 优化；主代理核对
`d94710d..006ed4a` 的 Rust 差异仅 inventory 文件，并验证远程 inventory 与
memory.rs SHA 分别为 `85b4b197…` / `539c6b0d…`，与本地一致。这是实际测试代码
来源，不把二进制内的旧 git_commit 字段当成干净提交证明。

独立审查无 Critical，四项 Important 均先验证问题再修复：脏页 workload 改写前
检查旧值；清理某项失败仍继续其它自有资源；SSH 超时后有界终止并回收本地传输；
性能 cell 成功/失败都回收 daemon 日志记录的精确 ublk ID。Linux 实际损坏注入
覆盖低脏页前缀与高脏页区域，旧 helper 会漏检，修复后退出5；主代理独立重跑
两个 Linux C 测试通过。真实两请求 cold batch 人为使一个失败，另一个 VM 已启动，
保留2条原始样本和1次失败，随后设备/cgroup 清理通过，证据
`/var/tmp/db-m4-performance-audit.7o0xg96x/audit.json`。

首次全操作性能 smoke 的模板构建在两种后端均因隔离 `/tmp` bind mount 的跨
挂载重命名返回 EXDEV；之前每端14项操作已成功，失败样本及状态保留于
`/var/tmp/db-m4-performance.e_4jj4h9`，不能记为完整 smoke 通过。共享 Lab 的
TMPDIR 改为 node.root/tmp 的规范路径，私有 `/tmp` bind mount 和 peer 路径遮蔽
保留。真实 Linux rename 先复现 errno18，再由修复后4个 Linux Lab 测试通过，
包含 peer 的 `/tmp` 和规范临时目录不可访问检查；随后上述双向物理恢复重跑通过。
产品存储代码未作绕过。

最终修复后全操作 smoke 整体 exit0，
`/var/tmp/db-m4-performance.h_ltxstu/result.json`：FC/DB 各15/15，包含真实512MiB
非零 RAM、各恢复路径完整 checksum、模板、块 I/O 及生命周期。两种后端同 kernel
且条件身份可比；每项只有一次，仍是 `performance_qualified=false`。主代理独立
校验两个 cell 的所有样本、cleanup 和审计文件；状态根分别
`/var/tmp/db-m4.mkvkz8m9` / `/var/tmp/db-m4.2rfma2v2`。审计记录为
`/var/tmp/aenv-m4-performance.FP7F2F/canonical-smoke-cleanup-audit.json`，无遗留
cgroup/ublk；DB 的一个停止后控制设备已按本轮日志精确回收。

旧机额外 DB lib 重跑953/0（另5 ignored），随后 M2 真实1/2GiB场景继续执行。
进入8GiB场景时旧盘可用空间由约5GiB降至1.6GiB，主代理主动向本轮 integration
进程发送 SIGTERM，避免填满根盘。因此
`logs/m4-continuation-db-m2.log` 的整体退出码非零，不计作五项 M2 通过，也不是
ENOSPC 产品失败。保留状态和日志；退出后可用2.6GiB，无 aenv 进程，临时授权已
撤回，旧 ublk 0–26号基线不变。大场景转移到扩容后的新机复验，不删除旧证据腾空间。

扩容后的新机复验整体 exit0，证据根 `/var/tmp/aenv-m2-regression.WNrdwY`。
同一 integration 二进制 SHA256
`61e59100d31679f8f585402364ad7bd221503280047fdecbecc2fc666d12ebae`，原 M2
kernel `d3792b04…`（不是共同性能内核），5项真实 VM 测试全部通过，0失败、0忽略，
耗时237.57秒：1/2/8GiB held full dump 逐字节 oracle、失败发布不丢 dirty 状态、
清零后重写及 fork。8GiB 跨 guest 地址空洞场景 balloon queue 进度0→14。
测试/runner/daemon 均 UID999，独立 cgroup 上限96GiB、8 CPU、4096 PID，swap0；
磁盘保留8GiB、截止1800秒，均未触发。累计 cgroup 内存峰值23,027,474,432字节
含缓存，不是单 VM 性能峰值；没有 OOM。最小可用磁盘459,026,026,496字节。
主代理另读原始结果确认5/5及无 guard stop；最终独立审计 `audit.json` 通过，
无自有进程/cgroup/ublk。仅回收本轮孤立设备9/10，日志及状态全部保留。
没有新增常驻权限、宿主服务或模块变更。此前旧机被主动中止的日志仍保留为中止，
不由本次成功覆盖。
