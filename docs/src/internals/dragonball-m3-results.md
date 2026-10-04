# Dragonball M3 平台集成验证

日期：2026-10-03。M3 Tasks 1–4 已完成：平台契约、编译时组件选择及独立
候选制品验证通过。最终生产代码为 `f784514`；不包含 M4 跨主机/性能资格。

## 范围与环境

保留编译时选择的 Firecracker/Dragonball，默认仍为 Firecracker。沿用 M2
生产 Kata pin `c77812d669737c11cb16321827acc1c79bf065f3`，不新增 Kata
runtime/shim，不切换生产流量，不删除历史状态。

测试机 `147.139.168.159`；Cargo/VM 为 `aenv` UID999。M3 源码、状态和日志
在 `/var/tmp/agentenv-dragonball-m3-20261003`，复用既有工具链与编译缓存。
真实 guest 串行执行，运行期间不替换其测试可执行文件。

## Task 1：OSS 与扩展

- 本地 S3-compatible 服务由非 root 测试子进程在独立网络命名空间启动，
  每次生成凭证和 bucket；不访问真实 OSS 账户。服务二进制来自缓存的
  `pgsty/silo:RELEASE.2026-09-16T00-00-00Z`，SHA256 为
  `a601d12c92f67e1f283e4b5da9a341c6dae965b6c013fb6abe2274f13950a113`。
- 新增真实测试 2/0（16.81 秒，`logs/task1-second.log`）：OSS commit 后停止
  并释放源 capture，从空 resolver cache 恢复两个 guest，检查磁盘/RAM marker、
  新身份、相同只读 ublk backing 上的私有 RAM 映射；错误 ABI 在分配前拒绝，
  独立 resolver 仍能读取原 durable catalog。
- 扩展通过真实 Orchestrator/HTTP 路径验证 opaque patch 原样转发、不同完整
  参数替换、patch 失败保留旧值，以及 snapshot/pause/resume/fork 参数保留。
  start 503 使创建失败，stop 503 不妨碍销毁，每个 runtime 的 start/stop 配对。
- 旧扩展失败与跨进程暂停恢复各 1/0；选定后端模板/recording 契约 DB 2/0、
  FC 2/0。DB library 946/0、FC library 975/0，各有普通 ignored 5 项，本步骤未
  把它们算作通过。证据见 `task1-db-baseline.log`、`task1-fc-template.log`、
  `task1-{db,fc}-lib.log`。

没有为这两项已工作的生产行为制造 RED。首次失败是测试夹具的 API/序列化
用法和 TOML 缺失键插入问题；修正仅限测试代码。

首次提交后复验曾有 FC `api::proxy::tests::proxy_forwards_large_request_bodies`
返回 504（100ms 定时器，日志约 216ms）；默认测试并发在本机 104 CPU 上触发。
同一测试独立重跑三次通过。保留原日志，没有修改产品超时或断言；最终全套
library 使用与 M2 资格脚本相同的 `--test-threads=2`，DB 946/0、FC 975/0。
这不是 Makefile 自带的并发默认值，也不声称已修复该偶发测试问题。

## Task 2：私有 registry 与 P2P

- 真实 TLS registry 2.8.3，二进制 SHA256
  `8be11cd37a7d43b87e2a2ad11eb5d14dc64d15a040a37485e7e1cbb4c6c9dd37`。
  使用临时自签证书、bcrypt 账户和独立 Docker/regctl 配置；不读取现有账户，
  不改系统 CA。测试上传已有 rootfs 的八个 native lowers，验证无凭证拒绝、
  有凭证解析、原生层保留 HTTPS 懒读取引用，并真实构建/启动 DB 模板读取 marker。
  首轮失败是夹具 CA 被错误用于 end-entity，修正证书用途后通过，未放宽 TLS。
- OSS capture 的实际 `P2pTransport::publish` 边界检查 durable commit 已存在；
  第二个真实 Iroh endpoint 校验 VM state、manifest 和 memory layer SHA256。
  关闭 provider，再使用全新第三个 endpoint 和空 resolver cache，从 OSS 恢复
  guest/RAM marker；注入 peer 发布失败仍保留可读的 durable commit 和运行源。
  这只是本地传输/仓库契约，不是 M4 跨主机验收。
- POSIX 测试删除自身临时仓库的 committed VM state：即使 peer 仍能提供同一
  内容，resolver 仍失败且不修复 durable 文件；没有触及历史仓库或安装备份。
- 全部 M3 用例 5/0（93.66 秒），P2P 单元 69/0；日志
  `task2-all-m3.log`、`task2-p2p-unit.log`。首轮夹具的字段名和 2 秒大文件传输
  超时已修正，改用正式 manifest 类型及默认 30 秒传输上限。
- 既有 `snapshot_oss_e2e_test --ignored` 全部 6/0（5.67 秒）；
  `cargo clippy -p agentenv --all-targets --all-features -- -D warnings`、
  workspace fmt 和 diff 检查通过。保留既有 proc-macro-error2 future-compat 警告。

## Task 3：独立候选制品

真实 release 构建使用 Node `06f86328e0ae30f37215fde49127aa30b5bd6939`，
`make build-server-dragonball-release`，UID999、独立磁盘 target、4 个编译任务。
`server --component-info-json` 确认 DB-only、非 debug、Linux x86_64；默认 FC、
双 feature 和 DB-only CLI 测试分别 1/0。严格 CLI Clippy、fmt、离线打包回归通过。
双 feature 首次链接因固定 52 GiB 编译 tmpfs 耗尽失败；删除六个已记录 SHA 的
闲置、可重建测试二进制后复验通过，没有扩大 tmpfs 或改动历史状态。

候选 `/var/tmp/agentenv-dragonball-m3-20261003/candidates/agentenv-dragonball-m3-candidate.1.tar.gz`
的 SHA256 为 `eaf4d3f5bc9320682be7fda2823e4ee5072add233f3197fca23e704bfc58be57`。
实际二进制及资产 SHA256：

| 资产 | SHA256 |
| --- | --- |
| server | `8234fe986cc66c66cc9aa5455d3c8039dcddedff9b42ef14e032b36854449be3` |
| runner | `578c1fc1a32195cd58b9d45004b1746039a6c2ef36f49f3948f9a95ec7eb5e0b` |
| ublk-daemon | `79a1359cdd8e75584881c11fede043d6593713885f267e26a390a4ad9facc2c1` |
| kernel | `d3792b0442c673f774f6c31f58d41231478fe9b8ca5b56c2991f719181c1230b` |
| kernel config | `8744d945b242b2f35705afed72af2a2ba8570d673498af34cd5097e3a0424cac` |
| tools | `60ba806a44975c7b2ca382f680de9da87af22ac65e288a683a921d1829e1c03d` |

Release runner build ID 为
`sha256:0b54234bd0dd3a17f4f81e0800f518702d278c12e84adcfbdd23d0185a27124c`，
Kata pin、epoch 2、device profile 与 M2 相同，build ID 不冒充旧 debug runner。
包记录 kernel `vmlinux-6.1.175-dragonball-m2.1` 和 tools `0.1.0`；后者实测含
envd `0.5.15/9c3b7c5`、BusyBox `1.37.0`，保存四个组件的 LICENSE/NOTICE 和来源。
仅本地测试候选，不声明完整公开分发手续已完成。

异目录解包、独立空 deps/state 下实际 release server `--setup-only` 成功，未下载
Firecracker；tools 与包内摘要一致。随后使用原 DB-only integration harness、包内
release runner/kernel/tools/daemon 与公共依赖执行五项 M3 VM 契约，**5/0，166.46 秒**。
没有把 integration harness 本身描述成 release server 的 HTTP 全链路测试。

同一 tar 构建的 `agentenv-dragonball:m3-candidate` 镜像 ID 为
`sha256:f15792608a54ae5a1119ac076e6f8917b534ebcd32b7d7bedb464d245b3aab42`，
Linux/amd64。容器以 UID999、无网络执行身份和全部校验清单通过；真实 VM 在宿主
独立 namespace 下执行，不声称已验证 Docker 内 KVM 运行。原 FC Dockerfile 未改。
日志：`task3-release-build.log`、`task3-package-real.log`、
`task3-candidate-qualification.log`、`task3-docker-image.log`。

## Task 4：平台门槛

`make test-dragonball-m3` 是独立 DB-only 入口，原有默认测试/镜像含义不变。
真实入口首轮 5/0，但发现经典 sudo 即使带 `-E` 也会清除 `TMPDIR`。最小 namespace
探针先输出 `TMPDIR=unset` 并失败，改为在提权之后显式传入临时目录后通过；回归还
确认退出后目录不存在。修正后的完整入口 **5/0，165.65 秒**。缺 S3/registry、
配置、资产或 state，以及使用 FC/both server，均明确失败。

主机默认 sudo-rs `0.2.13` 不支持 `-E`，会丢失 namespace 验证变量；本阶段与 M2
一样通过测试专属 PATH 使用已安装的 `/usr/bin/sudo.ws`，不改全局 alternatives。
另一次 setup 失败来自临时 Cargo 脚本 PATH 漏掉 `/usr/sbin`，恢复路径后通过，
没有放宽产品检查或添加全局 sudo 权限。

`make test test-unit test-agent-integration clippy fmt` **整体 exit0**，日志
`task4-all-make.log`。Cargo/VM 仍为 UID999，只有 Make 的 host shell verifier
由 root 编排；测试并发限制 2、构建任务 4。主要结果：

- FC library 975/0，普通运行 ignored 5 项另行显式执行为 5/0。
- FC integration 21/0，普通 ignored 3 项不算通过；Orchestrator 3/0。
- overlaybd 383/0（3 个平台/压力 ignored）；其 daemon 集成 37/0，MinIO 7/0；ublk 18/0，
  daemon 49/0；CLI 110/0；既有 OSS E2E 6/0。
- workspace 全 targets/features 的严格 Clippy、fmt、capability/service/buildctl
  shell verifier 均通过。保留 `proc-macro-error2` future-compat 警告。

上述是修复前基线；修复后的重放结果另列于下文，不把普通 Make 跳过的项目
直接计作通过。

### 阶段审查与修复

独立平台审查覆盖 Node `c748eb4..620b199` 和 Kata `bfc470b..5b9bf96`，
发现两项 Important，无 Critical/Minor。单轮修复提交为
`f78451432ee6366b83658e044cb4ebad3178aff9`，未委派重复审查：

- API 在恢复/预约卷之前调用所选后端的纯快照预检。真实 API 回归逐次 poll 请求，
  观察卷目录与 reservation，先复现创建额外卷、增加 pending owner，再分别在
  FC-only 和 DB-only 下通过 2/0；最终返回 400，原 snapshot 仍可读取。
- M3 用例父进程持有独立进程组并采用 subreaper，超时后终止、等待并回收本地
  S3/registry。新增强制 10 秒超时测试先观察到两个存活服务，再验证回收与临时
  目录清理。修复后全部 M3 **6/0，181.89 秒**，无产品超时阈值变更。

日志 `review-preflight-{red,fc-green,db-green}.log`、
`review-timeout-{red,green}.log`、`review-fix-clippy.log`。
修复后 workspace 全 targets/features 严格 Clippy、fmt、离线打包回归通过；
完整平台重放另行记录，不由这些窄测试推断。

M2 首轮重放为 49/2：24 GiB 测试 tmpfs 耗尽，shutdown recording 明确报告
ENOSPC，8 GiB full oracle 在构造 guest memory 时失败。保留失败 runtime 日志
归档和文件清单，确认无存活 guest 后卸载并释放约 23 GiB 临时数据。改用独立
40 GiB 限额的测试 tmpfs 重跑，不改变产品阈值、40 GiB 可用内存前提、长期编译
tmpfs 或历史 M2 状态；首轮完整 runner 日志被重跑日志替换，不声称保留其全文。

修复后 M2 入口完整通过，真实 M1/M2 集成矩阵 **51/0，577.58 秒**，包括首轮
两个失败用例。随后双 feature 构建下 M0 **5/0，12.62 秒**，完整 M1
**32/0，444.62 秒**；选定后端模板 DB-only **2/0，57.67 秒**，双 feature
实际选择 FC **2/0，14.93 秒**。

双 feature 首次构建曾因另一个挂载中的增量编译缓存写满而失败：56 GiB tmpfs
同时保存 39 GiB 历史状态和 18 GiB build-incremental。确认没有编译器运行后，
仅清理可再生成的增量缓存，历史状态未动，构建复验通过。另将四个闲置测试
二进制移到磁盘并保留原路径符号链接，逐项 SHA 检查一致，约释放 3.5 GiB
tmpfs；磁盘相应增加占用，没有删除这些二进制。

FC 模板首轮 0/2 是测试路径问题：matrix-state 导致 API socket 达到 113 字节，
实际 FC 二进制探针明确报告 `path must be shorter than SUN_LEN`。保留失败
日志后使用原先较短的隔离 M3/state 路径，两个用例均通过，无生产代码变更。
M0/M1/M2 矩阵结束后已卸载其临时 tmpfs，释放约 19 GiB；这些临时数据由测试
重建，不直接恢复。日志 `task4-{make-m2,m0,m1,db-template,both-template}.log`、
`task4-both-integration-enospc.log`、`task4-fc-path-probe.log`、
`task4-incremental-cleanup.log`、`task4-cold-binaries.log`。

修复后的 `make test test-unit test-agent-integration clippy fmt` **整体 exit0**，
见 `task4-postfix-all-make.log` 与 `task4-postfix-regression.exit0`。FC library
977/0，ignored 五项显式 5/0；FC integration 21/0，Orchestrator 3/0，OSS E2E
6/0；envd 对真实容器的 filesystem 1/0、process 3/0；overlaybd 383/0、其
daemon 集成 37/0、MinIO 7/0，ublk 18/0、daemon library 49/0，CLI 110/0。
严格 workspace Clippy、fmt 及 shell verifiers 通过。普通 Make 中忽略的
KVM runtime CLI 三项已由 M2 入口显式执行，probe runner CLI 3/0、导出预算 1/0
和 FC startup-pack 1/0 也另行通过。`linux-cap --ignored` 选择 0 项，不能作
验证证据；最终实际执行该包三项普通测试，3/0。

完成前复验的实际 server 身份测试：默认 FC、双 feature、DB-only 分别 1/0；
DB library 948/0，五项 ignored 另行 5/0。修复后的完整
`make test-dragonball-m3` **6/0，169.73 秒**，包括强制超时回收用例。日志
`task4-final-{fc,both,db}-identity.log`、`task4-final-db-{lib,ignored}.log`、
`task4-final-linux-cap.log`、`task4-final-m3-make.log`。

未运行项：overlaybd 并发驱逐压力、io-uring discard/reopen 两项手动测试；
另一项 sparse-upper refusal 因 Linux 支持可靠 holes 而由代码标记不适用；
一项 overlaybd doc example 仍 ignored。runner 的 parent/filter helper 由父测试
调用，不把单独 ignored 的 helper 当成独立通过。M3 未重新执行 Kata APF trace、
完整 Kata suite 或 Go services 测试（未改这些模块）；M2 已记录的九项 Kata
全套基线失败没有在本阶段宣称修复，详见 [M2 记录](./dragonball-m2-results.md)。

### 修复后的独立候选 .2

新候选源提交 `f78451432ee6366b83658e044cb4ebad3178aff9`；远程 index/worktree
与该提交逐一核对后构建，第一版 tar 和镜像保持不变。
`candidates/agentenv-dragonball-m3-candidate.2.tar.gz` SHA256 为
`da9c7859e8930a45b2bf3c0cb0a95e4bd560a41b72ab8aa90664bd7fd6d46dd9`，
server SHA256 为 `c6f6d8f4f5d0b5d41f512c7f03952e6585f592746d79a887107a996175c7fb11`。
runner、daemon、kernel、kernel config、tools 的摘要和 runner build ID 与第一版
相同；生产 Kata pin 不变。`qualified-inputs.2.json` 保存本版完整输入。

`agentenv-dragonball:m3-candidate2` 镜像 ID 为
`sha256:29d7b3c2ca88b69e216d2b50659ef51687a8020610654380a24e41aedc454973`，
Linux/amd64，UID999、无网络的身份及完整校验清单通过。
本版在新的异目录、空依赖目录下实际 release server `--setup-only` 通过，
确认未下载 FC，tools 内容与包内相同；随后使用本版 release
runner/kernel/tools/daemon 完整执行 M3 用例，**6/0，169.06 秒**。
容器完整校验清单再次通过。日志 `task4-{release2-build,package2,image2}.log`、
`task4-candidate2-qualification.log`、`task4-candidate2-image-recheck.log`。
这是实际 release 组件与 VM harness 的验证，仍不声称 release server HTTP
全链路或 Docker 内 KVM 验证已完成，也未上传/tag/公开发布。

## 收尾与后续边界

最终检查整体 exit0，执行账本 Task 4 已关闭；修复后没有进一步生产代码变更。
本阶段 sudoers 已移出 `/etc/sudoers.d` 保存为 `revoked-m3-sudoers`，测试专属
sudo PATH 链接移除，aenv 的临时 Docker 组成员资格撤回，仅保留 aenv/kvm。
`visudo -c` 通过，AgentENV/aenv unit 列表为空，服务 `not-found/inactive/PID0`，
没有残留 aenv 进程、Docker 容器或 `db-m3-fixtures.*` 目录。未重新安装旧 server。
收尾时 187 GiB 内存中约 **70 GiB available**；历史 M2 状态、旧安装备份和
日志保留。未清理无关的长期 root Firecracker 进程。

保留 `feat/dragonball-m0` worktree 和本阶段 scratch 证据，不合并、不 push。
M4 仍需第二台兼容 KVM 主机及冻结的停顿/资源 SLO；公开分发、正式签名/许可
流程及生产默认切换没有在本阶段授权或完成。默认/双 feature 仍使用 FC，
DB-only 不依赖 FC client/binary，FC-only 不要求 DB runner。
