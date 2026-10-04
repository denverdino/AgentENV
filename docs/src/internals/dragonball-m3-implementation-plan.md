# Dragonball M3 平台契约与独立制品实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. 用户已要求自动完成后续工作；沿用 Native 执行，无常规确认等待。M2 的未关闭正确性问题阻止产品资格升级。

**Goal:** 为编译时可选 Dragonball 产品补齐 OSS、扩展、私有 registry/P2P 契约及可校验的独立制品，不删除 Firecracker。

**Architecture:** 复用已实现的 SelectedSandboxFactory、TemplateSandboxFactory、SnapshotManager、ImageResolver 和 P2pTransport。测试通过同一后端接口启动真实 VM；发布包包含独立 runner、明确 kernel/tools 及来源/校验信息。默认及双 feature 构建仍选择 Firecracker。

**Tech Stack:** 现有 Rust workspace、Linux x86_64/KVM、OverlayBD/ublk、S3-compatible 测试服务、OCI registry；不增加 Kata shim/runtime。

**Spec:** [总体设计](./dragonball-hypervisor-design.md)、[修订组件设计](./hypervisor-components-design.md)、[M1–M5 路线](./dragonball-m1-m5-roadmap.md)。组件装配的已完成范围见[验证记录](./hypervisor-components-verification.md)。

## Global Constraints

- 先关闭 M2 correctness/review gate；不以普通 oracle 或单机通过替代共享 COW、跨节点和性能资格。
- 默认和 both-feature 产品仍为 FC。DB-only 不依赖 FC client、不下载 FC binary；FC-only 不要求 DB runner。PVM/aarch64 保留 FC。
- historical `firecracker-manifest.json` 是兼容对象键，身份根据内容判断；未知 backend/ABI 必须拒绝且保留记录。
- 使用用户 fork 已验证的完整 Kata SHA；不修改 upstream/default branch，不把资格内核误称已发布的 release。
- 测试只用隔离 config/state 和运行期生成的测试服务凭证；不读取/发布真实 registry/OSS 凭证。runner 不接收仓库凭证。
- 构建/VM 测试为 UID999；root 仅准备测试主机。服务/容器/临时授权都必须归属该次运行，结束后撤回。guest suites 串行。
- 不改 scheduler protobuf，不迁移内存到不同 backend，不切换生产流量，不删除旧安装备份或历史 snapshot/paused。

## Review Focus

1. DB opaque state 或固定产物来自 OSS/P2P 时身份错配：在创建 runner/网络/卷前拒绝，原记录仍可读。
2. 扩展 patch 不是 merge-patch 或返回与请求不同完整参数：逐字转发、存储响应，snapshot/resume/fork 不丢失。
3. registry 只允许授权访问或 layer 延迟读取：使用运行用户凭证，恢复不能偷偷退回公共 tag/full pull。
4. P2P 发布失败、peer 丢失、POSIX 文件缺失：commit 后 best-effort；OSS 可回源，POSIX 不借 peer 修复 durable 文件。
5. 打包在非源码目录解压或混入另一构建 runner/kernel：相对配置可重定位，校验失败明确拒绝，不覆盖已有发布目录。

## 已完成接口与剩余文件归属

`src/sandbox/selected.rs`、`backend.rs`、`src/template/{builder,runner}.rs`、`src/setup/deps.rs` 的组件装配已实现并专项验证；本计划不再重写。新增平台用例放 `tests/integration/dragonball_m3.rs`；独立制品逻辑放 `scripts/release/package-dragonball.sh`，测试放 `scripts/tests/verify-package-dragonball.sh`。部署用 DB 配置和 Dockerfile 与既有 FC 制品分开，不改变默认镜像含义。

## Task 1: 真实 OSS 发布、恢复与扩展完整参数

**Files:** 新 `tests/integration/dragonball_m3.rs`，注册于 `tests/integration.rs`；必要修复限定 `dragonball/{factory,sandbox,paused}.rs` 和实际失败的仓库边界。

**Interfaces:** 使用 `OssBackend::new_with_publish_compression(...).into_parts()`、`SnapshotManager::from_parts(...)`、`CapturedSandboxSnapshot` 和现有 `SandboxBackend::update_custom_extension_params`。fixture 提供真实本地 S3-compatible 服务以及接收/记录 HTTP hook 的测试端点；不在生产代码增加测试开关。

- [x] 写 `dragonball_m3_oss_capture_restore_preserves_identity`：真实 fresh→写 marker→capture→OSS commit→停源→从独立空 resolver cache 恢复→验证 marker/身份/共享内存；删除源后仍可恢复。错误 ABI 拒绝并保留 catalog。
- [x] 写 `dragonball_m3_extension_patch_roundtrip`：发送包含数组/null/嵌套字段的 opaque patch；服务返回不同 full params；断言逐字转发、更新后 capture/pause/resume/fork 参数保留、各 runtime start/stop instance 配对；start/patch 错误传播，stop 错误 best-effort。
- [x] 运行实际用例，区分新覆盖已通过与真实失败；对失败写最小修复并保持 RED 日志。禁止故意撤销已工作实现制造 RED。
- [x] 重跑新增用例、M1 extension/restart、选定后端模板契约和两种后端单元测试；全部 exit0 后提交 `test: qualify Dragonball OSS and extension contracts`。

## Task 2: 私有 OCI 与 P2P 仓库所有权

**Files:** 继续 M3 integration；复用 `src/image/`、`src/p2p/`、`src/snapshot/repository/` 和 `crates/e2e-tests/tests/snapshot_oss_e2e_test.rs` 已有 helper/服务方式，按实际失败定位修改。

**Interfaces:** `ImageResolver` 使用配置的 regctl 和测试运行用户 Docker config；`P2pTransport` 提供两个独立本地 endpoint。它们只验证传输契约，不声称是 M4 两个 KVM 节点。

- [x] 写 `dragonball_m3_private_registry_template`：临时带认证 registry、唯一镜像名；无凭证拒绝，有 fixture 凭证 build/launch 成功，OverlayBD-native 层保留懒读取引用；manifest 不含凭证。
- [x] 写 `dragonball_m3_oss_p2p_commit_and_fallback`：真实 DB capture commit 后向 peer 发布；独立 consumer 验证实际固定产物与 layer digest；peer 失效恢复回源，发布失败不撤销 commit。
- [x] 写 `dragonball_m3_posix_missing_artifact_is_not_peer_repaired`：只删除 fixture 自有 committed 文件，peer 仍有该内容，resolve 必须失败且不得伪造本地修复。
- [x] 运行上述测试与既有 OSS/P2P 全套；只修实际失败，RED→GREEN 后提交 `test: qualify Dragonball registry and peer storage contracts`。

## Task 3: 独立、可重定位的候选发布包

**Files:** 新 `scripts/release/package-dragonball.sh`、`scripts/tests/verify-package-dragonball.sh`、`deploy/docker/Dockerfile.agentenv-dragonball`、`config/dragonball.toml`；Makefile 添加显式非默认构建/打包入口，文档解释候选与正式发布区别。

**Interfaces:** packaging 输入为 DB-only server、production runner、ublk-daemon、qualified kernel+config、版本化 tools、各来源版本/许可文件、目标 tar 路径；全部显式传入。输出包含 `manifest.json`、SHA256SUMS、LICENSE/NOTICE、相对配置和三个独立可执行文件；manifest 保存实际 `--runtime --version-json`、完整 Kata pin、kernel/tools digest 和版本。不得包含 `reply-fault`。

- [x] 写打包回归：缺资产/版本/许可拒绝，输出已存在拒绝，篡改资产 checksum 失败，从不同目录解包仍能解析 kernel/tools/runner，FC binary 和测试故障 example 不在 DB 包中。
- [x] 跑 RED：现有流程没有该制品；增加最小 staging/校验/原子成品创建，临时目录只清理本次所有内容。
- [x] 对真实远程 release 构建生成候选 tar 和校验清单，干净 deps/state 下 `server --setup-only`；断言没有下载 FC，实际运行 M3 契约。Docker 使用同一包，指定 linux/amd64；原 FC Dockerfile 不变。
- [x] 保存实际 SHA、license/source 清单和命令；提交 `feat: package an independent Dragonball server component`。仅产出测试候选，不自动打 tag、上传 release 或宣称公网 checksum 已发布。

## Task 4: 完整平台门槛与交接

**Files:** `Makefile`、新 `scripts/tests/run-dragonball-m3.sh`、manual CI opt-in、`docs/src/internals/dragonball-m3-results.md`。

**Interfaces:** `make test-dragonball-m3` 只使用 DB-only 产品，显式资产和隔离状态；Docker/service 前提缺失即失败。M2 核心、FC baseline、legacy unknown-reader 测试不删除。

- [x] 运行 DB-only/FC-only/both feature 构建与严格 Clippy、fmt、全部单元/选定模板/M1–M3 实测、默认 `make test test-unit test-agent-integration`；记录所有 ignored/未运行项。
- [x] 对 Node 和 Kata 全部平台相关差异做一次新的阶段审查；重要问题一个 RED→GREEN 修复流程，不重复委派审查。
- [x] 撤回本阶段授权，确认旧服务仍卸载；保存 M3 精确制品与测试证据，提交 `docs: record Dragonball platform qualification`。

## M4/M5 外部边界

M4 仍需第二台兼容 KVM 主机和冻结 capture 停顿/峰值资源 SLO，不能用本机目录或容器代替。M5 可先做 scoped roots 的只读依赖审计和双后端干净构建准备，但任何历史资产清理、生产切换或 release 上传必须有对应明确目标；本计划不提供隐含授权。
