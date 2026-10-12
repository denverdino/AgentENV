# Harbor 集成测试使用阿里云 OSS

本入口只影响 Harbor 的隔离 Lab。AgentENV 默认配置和其他 M4 测试仍保持原行为。
`run_lab.py --oss-env PATH` 将快照仓库切换为指定 OSS bucket，跳过 MinIO 启动、
本地 bucket 创建与 MinIO 健康探针。未传此参数仍使用原来的本地夹具。

OSS 保存提交的快照元数据及内存、根盘等受管理层；原始任务镜像和现有依赖镜像
继续使用 ACR。镜像转换缓存、可写 upper、运行日志仍占用宿主磁盘。
仅传 `--oss-env` 仍使用批内模板。显式增加 `--shared-templates prepare/reuse`
才启用下文的跨批次共享模板；默认批次绑定继续检查 Lab run ID。

## 准备 `.env`

在远程测试机的代码目录执行。真实配置由运行账号持有，权限为 `0600`：

```bash
umask 077
cp scripts/harbor/.env.example scripts/harbor/.env
chmod 600 scripts/harbor/.env
${EDITOR:-vi} scripts/harbor/.env
```

如果由 root 创建，再将文件归属改为实际 Harbor 运行账号，例如：

```bash
chown aenv:$(id -gn aenv) scripts/harbor/.env
```

填写 `HARBOR_OSS_ENDPOINT`、`HARBOR_OSS_REGION`、`HARBOR_OSS_BUCKET`、
`HARBOR_OSS_PREFIX` 和 AK/SK；STS 同时填写 token 和 expiration。
示例 endpoint 为雅加达内网地址，必须按 bucket 的实际地域调整；同地域 ECS
可以选择内网 endpoint，具体见[阿里云 OSS endpoint 配置](https://help.aliyun.com/zh/oss/developer-reference/2-0-manual-preview-version)。

只接受 `KEY=VALUE`、整行注释和单/双引号包裹的字面值；不执行 `source .env`，
不展开 `$VAR`、命令替换或反引号。重复键、未知键、缺少必填项、公开权限或
symlink 均在创建 Lab 前拒绝。`.env` 只被 Harbor 入口及它的凭据子进程读取，
不会注入 guest、模型进程或项目全局环境。

仓库忽略 `.env` 和 `.env.*`，只允许 `.env.example` 提交。提交前运行：

```bash
git check-ignore scripts/harbor/.env
python3 scripts/harbor/check_env_files.py
```

`make test-harbor-tools` 和 CI 也检查 Git index 中的真实 dotenv 文件，强制添加
同样会被检查拒绝。不得强制添加或推送 `.env`，不得把 AK/SK 写入命令行、
示例、日志、回执或候选包。复制候选时只包含显式选中的代码和示例文件。

## Bucket 与连接要求

事先创建专用测试 bucket；本入口不创建或删除云 bucket。运行账号需要测试
prefix 内的对象读取、写入、列举、删除及分片上传相关权限。默认每轮将独立的
`db-m4.*` run ID 追加到配置 prefix；共享模板使用固定的 `shared-templates-v1`
后缀。回执记录完整 prefix。
普通对象读写、分片上传、发布及读取必须在真实验收中验证，单个 HTTP 健康检查
不能替代这些操作。S3 兼容路径不能把 `If-None-Match` 条件 PUT 当作配置准入；
当前快照层上传和 alias 绑定已有对应处理，必须按项目实际路径验收。

配置使用 HTTPS、bucket 地域和 virtual addressing。AK/SK 通过
`credential_process` 返回给 AgentENV/OverlayBD；生成的服务配置不保存 AK/SK。
更新私有 `.env` 可以供后续凭据读取使用；endpoint、bucket、prefix 等配置变更
应在下一轮生效，不能在活动评测中切换仓库。STS 到期前更新整组凭据。

Lab 内服务的外部请求使用已有测试代理；代理出站位于宿主原网络 namespace。
内网 OSS endpoint 的 DNS/TLS 与可达性须在该宿主网络中验证。原始任务的网络
策略仍独立生效；桌面任务声明无网络时不会因此开放 guest 网络。

## 执行

先完成原有的镜像准备、运行资产准备和非 root 账号设置，见
[README](README.md#交付缓存准备与阶段检查)。该入口包含完整传输发布、各机缓存清单、
冻结控制脚本、阶段执行及共享模板资格检查。运行前检查磁盘、inode、内存和残留进程/设备；OSS 不替代
宿主空间准入。保持 4 vCPU / 8192 MiB、原始任务预算、原始评分器，先单任务
Oracle/nop，再扩大并发。root 仅编排，实际服务、Harbor 和 VMM 使用 `aenv`。

在现有受 host lease 保护的控制器内，将原 `run_lab.py` 命令增加：

```bash
--oss-env /absolute/path/to/scripts/harbor/.env
```

完整命令形状如下；路径必须替换为当前已冻结的输入和全新的输出目录：

```bash
python3 scripts/harbor/run_lab.py \
  --inputs /var/tmp/hb1008/inputs.json \
  --prepared /absolute/path/to/prepared \
  --output /absolute/path/to/new-oss-run \
  --harbor /var/tmp/hb1008/venv/bin/harbor \
  --run-user aenv \
  --oss-env /absolute/path/to/scripts/harbor/.env \
  --dependencies /absolute/path/to/dependencies.json \
  --template-start --n-concurrent-trials 1
```

此命令要求宿主已由现有控制器准备，root Python 能导入固定 Harbor，且控制器
在成功、失败和中断后均恢复临时主机设置。不要绕过现有 `setup_host.py` /
`restore_host.py` 和 host lease，直接在共享测试机启动并行 root 控制器。

无需提供 `s3` 可执行文件；当前共享 inputs.json 保留它也不会启动 MinIO。
本地隔离 OCI registry 仍用于固定 digest 导入，不能把“无需 MinIO”解释为
“无需 registry、KVM、ublk 或本地磁盘”。

## 两台物理服务器共享任务模板

两台机器使用相同 endpoint、region、bucket 和基础 prefix，即可共享同一任务的
干净模板。共享仓库位于 `<HARBOR_OSS_PREFIX>/shared-templates-v1/`，缓存键包含
任务全文件 SHA、原镜像 digest、CPU/内存与 server/runner/kernel/tools SHA，
不包含服务器或 Lab ID。生产端 run ID 留在记录中用于追溯，消费端不要求一致。

在上面的完整命令中增加一个模式，输出目录仍须每轮独立：

```bash
# 未命中时 capture 干净模板并发布；命中时复用。
--shared-templates prepare

# 只消费已提交模板；未命中、配置不一致或已删除时失败。
--shared-templates reuse
```

该参数要求同时指定 `--oss-env`、`--template-start` 和 `--dependencies`，
并使用固定全套件交付格式的 prepared 输入。模板未包含解答、测试、verifier
临时数据或模型凭据。每个 trial 从快照创建新 Sandbox ID、独立可写 upper，
核对首条身份与初始工作区；日志和结果保存在本轮输出目录。

索引位于 `harbor-template-index/<cache-key>/<snapshot-id>.json`。capture API
完成提交后才发布索引，读取时核对绑定及 durable catalog record。不同生产者
写入不同 snapshot ID，读取端按字典序选取同一个有效记录；可能重复构建，
没有分布式锁，也不依赖 OSS 不支持的条件 PUT 或同名 alias。损坏或失效索引
会直接失败，不静默重建。共享仓库不由批次清理删除，持久模板 GC 尚未实现。

`reuse` 的传输预检直接从模板启动，记录新 Lab 的
`cache_before_first_restore` 和 `transport_probe_start_mode`，避免先冷启动镜像
预热缓存。后续 Oracle/nop 已可能命中本地缓存，不能全部称为冷恢复。
CPU 型号、KVM 和快照格式仍须兼容；运行资产 SHA 一致不能替代 VMM 的兼容检查。
共享快照不是同 ID pause/resume 的跨机迁移，也不提供 alias/volume 等可变记录的
分布式事务。不要同时删除仍被消费的快照。

## 验收与回收

1. `receipt.json.snapshot_store.backend` 为 `aliyun-oss`，endpoint/bucket/region/
   实际仓库 prefix 正确；进程清单中没有 MinIO。
2. 真实模板 capture 完成，相关元数据和层写入该 OSS prefix；新 sandbox 从
   同一提交的 snapshot 恢复，首条命令身份正确，工作区摘要一致。
3. Oracle=1/nop=0，结果没有框架异常；未改变任务、解答、verifier 或超时预算。
4. sandbox 全部 deleted，Lab 清理和宿主恢复成功，无测试 cgroup/ublk 残留。
   `.env`、AK/SK、token 未进入回执、归档或 Git index。
5. 云端对象不会随本地 Lab payload GC 删除。保留日志、评分、摘要和失败现场；
   只有确认无引用的成功测试 prefix 才能另行回收，不删除整个 bucket。

列举权限需覆盖 bucket 资源；仅授权 prefix 下的对象读写不等于允许
`ListObjectsV2`。2026-10-10 两机实际配置探针的 DNS/TLS 通过，带条件头的
PUT 返回 `400 NotImplemented`，对象列举返回 `403 AccessDenied`。前者为
探针与 OSS 兼容路径不一致，后者在更新授权后复测通过。

同日修订探针在两机以 UID999 验证普通 PUT、HEAD/GET、Range GET、列举、
5 MiB + 1 KiB 两段上传/完成/读取、中止上传与测试对象删除，全部通过；
上传与读取内容逐字节一致，独立随机测试 prefix 清理通过。条件 PUT 仍为
已知不支持项。证据在 `/tmp/agentenv-harbor-oss-validation-20261010/REPORT.md`。
这是对象仓库配置验证；应用层最小验证另见下文。

2026-10-10 最小真实 Harbor 验证在第二台服务器完成：原始
`openssl-selfsigned-cert`、4 vCPU / 8192 MiB、单并发，Oracle=1/nop=0，
两个 trial 无框架异常。干净模板捕获到 OSS，两个恢复实例均生成新 Sandbox ID，
首条身份命令及初始工作区摘要检查通过；VMM 实际 UID999。运行期间无 MinIO
进程、无 `s3.log`，本地 registry 仍使用。云端只读审计确认本轮 prefix 下
11 个对象、773,147,926 bytes、无未完成分片；保留云端快照作为证据。
构建 sandbox、trial、Lab 清理与宿主恢复均通过，无进程/cgroup/ublk/命名空间引用残留。
本地成功 Lab 数据和失败现场保留；这不是 payload GC 或跨批持久模板资格。

第二轮暴露服务代理绕过名单缺少默认 guest 控制网段 `10.11.0.0/16`，
envd 就绪 HTTP 请求误走外部代理。先用代理哨兵回归测试复现失败，再补充绕过
规则；冻结 candidate-v8 后，两机非 root 各 99 项驱动测试通过，真实用例通过。
第一轮为临时外层控制器提前复制环境导致 host lease FD 名称未传递，已修正；
失败记录均保留。任务、Oracle、verifier、阶段预算和 Rust 运行二进制没有修改。
报告在 `/tmp/agentenv-harbor-oss-smoke-20261010/REPORT.md`。
该结果仅覆盖单机最小 capture/restore 与评分链路，未验证跨机冷缓存恢复、
两机同 prefix 并发写入、容量或完整任务集。


2026-10-10 共享模板验收：冻结 candidate-v10，两机非 root 各 108 项驱动测试
通过，无 skip。B 发布一份 openssl 干净模板，B 新 Lab 与 A 新 Lab 都用
`reuse` 命中同一 snapshot ID，无 builder。两消费端首次传输预检从空本地
快照缓存恢复；随后共三组 Oracle/nop 均为 1/0，Oracle 各 6 项断言通过，
nop 各 6 项断言按预期失败，无框架异常。所有恢复实例 ID 不同、初始工作区
摘要一致、最终 deleted；实际 VMM UID999，未启动 MinIO，宿主恢复成功。
共享 prefix 只有 12 个对象、773,517,666 bytes，catalog 与索引各一个，
消费后未增加模板，无未完成分片。对象、本地 payload 与失败证据保留。
两机均为 x86_64、Xeon Platinum 8269CY；本次没有验收异构 CPU、并发生产者、
共享模板 GC、容量或完整任务集。A 首次准备因临时脚本引用 B 的旧路径在 VM
前失败，改为交付同一 SHA/OCI blobs 归档后通过，未改变任务或评分标准。
报告：`/tmp/agentenv-harbor-shared-20261010/REPORT.md`。
