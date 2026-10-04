# Dragonball 候选包

该入口生成本地 Linux x86_64/KVM 候选制品，不改变默认 Firecracker 产品，
不上传 release。资格内核仍是候选资产。源码目录中的 `config/dragonball.toml`
是打包模板，必须由打包过程填入已验证的 kernel/tools 版本。

```bash
make build-server-dragonball-release
make test-package-dragonball
make package-dragonball DRAGONBALL_PACKAGE_INPUTS=/absolute/qualified.json \
  DRAGONBALL_PACKAGE_OUTPUT=/existing/directory/candidate.tar.gz
```

输入 JSON 必须明确提供以下字段。所有输入文件路径均为绝对路径，不能为空；
`assets` 的每个成员都包含 `path` 和事先测量的 `sha256`，不是打包时猜测来源。

| 字段 | 内容 |
| --- | --- |
| `version` | 本地候选版本 |
| `source_revision`, `kata_revision` | 两个源码仓库的完整 40 位 SHA |
| `kernel_version`, `tools_version` | 本次资格使用的版本 |
| `sources` | `node`, `kata`, `kernel`, `tools` 的来源说明/地址 |
| `assets` | `server`, `runner`, `daemon`, `kernel`, `kernel_config`, `tools` |
| `licenses` | `node`, `kata`, `kernel`, `tools`，各自包含 `license` 和 `notice` 文件路径 |

`server` 必须是 DB-only release 产品；其 `--component-info-json` 输出在不读取
config、不初始化 host 的情况下报告已编译组件。默认和双 feature 产品仍报告
Firecracker，并被该打包入口拒绝。runner 使用生产 `--runtime --version-json`
核对完整 Kata pin、device profile 和 snapshot epoch，不接纳 M0 probe 身份。

解包后包含三个二进制、kernel/config/tools、来源版本清单、组件 LICENSE/NOTICE
以及 `SHA256SUMS`。从任意 cwd 执行 `bash /path/agentenv-dragonball/verify.sh`
检查文件摘要和实际二进制身份。校验清单不是签名或公开发布的来源证明。
资产路径相对于包内 config 文件解析；使用独立可写的 `AENV_HOME_PATH`、
`AENV_RUNTIME_PATH` 和 `AENV_DEPS_PATH`，不要复用生产 state。

```bash
AENV_HOME_PATH=/isolated/state AENV_RUNTIME_PATH=/isolated/run \
  AENV_DEPS_PATH=/isolated/deps \
  /path/agentenv-dragonball/bin/server \
  --config /path/agentenv-dragonball/config/dragonball.toml --setup-only
```

`--setup-only` 仍会准备非 hypervisor 公共依赖；不是离线完整 OS 镜像。包不含
Firecracker、故障注入 example 或 Kata shim。内核模块、KVM/ublk、运行时权限由
已有 host setup 流程管理，不能以打包通过替代 VM 资格测试。

Docker 入口仅消费同一已校验 tar，不重新构建另一份 runner。将候选 tar 和该
Dockerfile 放入独立 context，传入外部测量的 tar SHA，避免发送整个源码/缓存：

```bash
docker build --platform linux/amd64 \
  --build-arg BUNDLE=candidate.tar.gz --build-arg BUNDLE_SHA256=<actual-sha256> \
  -f Dockerfile.agentenv-dragonball -t agentenv-dragonball:candidate .
```

默认 Ubuntu 26.04 对齐当前候选构建主机的 libc；换基础镜像需重新验证动态库兼容。
镜像运行仍需要 KVM/ublk 和正常的 runtime capability 配置，不自动启动 systemd
或操作既有服务。原 Firecracker Dockerfile 不变。正式公开分发前仍需相应 release
审批、完整依赖许可材料与源码提供流程；本入口不宣称完成正式发布合规审查。

## M3 平台资格入口

在已配置 KVM/ublk 的 Linux x86_64 主机，以非 root 用户运行：

```bash
export AENV_CONFIG_PATH=/absolute/qualified-test.toml
export AENV_TEST_STATE_DIR=/isolated/m3-state
export AENV_TEST_DEPS_PATH=/isolated/deps
export AENV_M3_S3_SERVER=/absolute/s3-compatible-server
export AENV_M3_REGISTRY_SERVER=/absolute/registry
mkdir -p "$AENV_TEST_STATE_DIR"
make test-dragonball-m3
```

测试配置应沿用已验证的 DB kernel/tools、关闭 warm pools，显式包含
`[snapshot.memory_startup_pack]`。入口编译 DB-only server、生产 runner 和
ublk-daemon，准备资产并串行执行真实 OSS、扩展、私有 registry 与 P2P 用例。
原生 S3/registry fixture 在独立 namespace 内以测试用户运行，不需要生产账户；
主机须提供 `openssl`、`htpasswd`、`debugfs`、网络工具和现有 capability runner
所需授权。缺少前提时失败，不以 skipped 作为资格通过。临时大文件使用独占的
`/var/tmp/db-m3-fixtures.*`，namespace 退出后清理。

Integration Tests 工作流的 `dragonball_m3` 是默认关闭的手动开关，要求专用
`dragonball-m3` runner 和同名受保护 environment；需由仓库管理员配置审批策略，
YAML 本身不会创建审批规则。PR 事件不会触发该特权 job。
