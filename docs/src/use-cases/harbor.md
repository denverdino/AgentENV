# 使用 Harbor 在 Dragonball 上运行小型评测

本示例通过 AgentENV 的自定义 Harbor environment，在 Dragonball VM 中运行
终端和桌面任务。先用正负对照验证后端与评分器，再运行百炼
`qwen3.8-flash`。Oracle 或 GUI 对照的成功不计作模型成绩。

实现入口为 [scripts/harbor](../../../scripts/harbor/README.md)。本文的远程目录
对应 2026-10-09 已验证的串行示例环境；新机器的依赖准备见文末。
当前仓库入口已改为每个 trial 4c8g、每机最多 8 并发；原始规格与历史结果
保持独立。完整 Terminal-Bench 固定集合筛为 86 项，排除 3 个 Windows/QEMU
完整虚机任务；配置与筛选说明见 [集成经验](../../../HARBOR_INTEGRATION.md)。
完整模型评测保持停止，以下模型命令是小型诊断示例。

## 1. 选择任务

| 示例 | 任务 | 运行内容 | 适用目的 |
| --- | --- | --- | --- |
| 后端冒烟 | `hello-workdir`、`hello-user`、`hello-multi-step-simple` | Oracle 和 `nop`，共 6 个 trial | 工作目录、用户权限、跨步骤状态 |
| Terminal-Bench | `openssl-selfsigned-cert`、`log-summary-date-ranges` | Oracle / `nop` 对照，再运行 Terminus-2 + Qwen | 小型真实终端任务 |
| Computer Use | `computer-1-click-calibration` | GUI / `nop` 对照，再运行 Computer1 + Qwen | 点击、双击、右键、输入、拖拽、滚动、截图裁剪 |

Harbor 固定为 0.24.0，源码提交
`4b94505a91c5ddcb70b5740ac95718ddee13e5a0`；Terminal-Bench 任务提交固定为
`2fd12b88aafdd04a52c298e3940bcb189f9766d6`。任务、解答和 verifier 保留原样。
上述历史示例每轮单并发、每任务一次，无 trial 自动重试；单次模型 API 重试仍遵循 Harbor。

## 2. 连接远程环境并清理磁盘

在本地连接服务器，后续命令在远程 Bash 中运行：

```bash
ssh root@147.139.165.124
bash
cd /var/tmp/hb1008
```

root 只负责主机准备、namespace、资源回收；Harbor、AgentENV、VMM 和镜像
构建由非 root 账号 `aenv` 执行。该目录已有以下制品：

| 路径 | 内容 |
| --- | --- |
| `venv/` | 固定 Harbor 的 Python 环境 |
| `source/scripts/harbor/` | AgentENV 适配层、驱动、模型日志脱敏包装 |
| `inputs.json`、`assets.json` | 已验证的运行时及依赖路径 |
| `prepared-v1/` | 三个 hello 任务和冻结镜像 |
| `terminal-prepared-v2/` | 两个 Terminal-Bench 任务和冻结镜像 |
| `computer-prepared-v2/` | Computer Use 任务和冻结镜像 |
| `setup_host.py`、`restore_host.py` | 此环境的临时主机准备与恢复 |
| `rerun.sh`、`tb_rerun.sh`、`cu_rerun.sh` | 带互斥和退出清理的运行入口 |

先检查磁盘、inode 和残留资源，再清理超过一小时的未使用 Docker 构建缓存：

```bash
set -e
df -h /var/tmp
df -i /var/tmp
python3 - <<'PY'
import sys
sys.path.insert(0, '/var/tmp/hb1008/source/scripts/tests')
from dragonball_m4_host import require_clean_host
require_clean_host()
PY
docker builder prune -f --filter until=1h
python3 - <<'PY'
import shutil
free = shutil.disk_usage('/var/tmp').free
assert free >= 40 * 1024**3, '需要至少 40 GiB 可用空间'
print(f'可用空间：{free / 1024**3:.1f} GiB')
PY
```

预算包含 20 GiB 工作空间和 20 GiB 保留空间。保留镜像、快照、状态目录和
历史结果；残留资源检查失败时先查所属任务。运行入口还会检查主机互斥、
实际测试账号权限、源码、完整任务文件集合和镜像 hash；上述检查不替代这些准入。

## 3. 示例 A：不调用模型的后端冒烟

```bash
HB_RUN_OUTPUT="/var/tmp/hb1008/run-example-$(date -u +%Y%m%dT%H%M%SZ)"
bash /var/tmp/hb1008/rerun.sh "$HB_RUN_OUTPUT"
python3 -m json.tool "$HB_RUN_OUTPUT/receipt.json"
```

输出目录必须不存在；同机同时只能运行一个入口。三个任务的 Oracle 全部
步骤应得 1，`nop` 全部步骤应得 0。多步骤任务有两次评分，所以共 6 个 trial、
8 次步骤评分。无需百炼密钥。

## 4. 示例 B：Terminal-Bench + 百炼 Qwen

在子 shell 中交互输入密钥，命令历史与配置只包含变量名：

```bash
(
  set -e
  trap 'unset DASHSCOPE_API_KEY' EXIT
  read -r -s -p 'DASHSCOPE_API_KEY: ' DASHSCOPE_API_KEY
  printf '\n'
  test -n "$DASHSCOPE_API_KEY"
  export DASHSCOPE_API_KEY
  HB_RUN_OUTPUT="/var/tmp/hb1008/run-tb-example-$(date -u +%Y%m%dT%H%M%SZ)"
  printf '输出目录：%s\n' "$HB_RUN_OUTPUT"
  bash /var/tmp/hb1008/tb_rerun.sh "$HB_RUN_OUTPUT"
)
```

先运行 4 个 Oracle / `nop` 对照 trial，全部符合预期后才启动 2 个模型 trial。
Terminus-2 最多 25 轮，关闭 thinking，每次最多输出 4096 tokens。
百炼接口为 `https://dashscope.aliyuncs.com/compatible-mode/v1`。
密钥只注入模型 CLI，不交给 guest，不写入 job JSON；模型日志落盘前脱敏。

官方终端工具和 verifier 需要下载依赖。实验室提供隔离的 HTTP/CONNECT
代理，并在评分前用实际 VM 验证 HTTP/HTTPS。这不代表任意 TCP/UDP 公网接入。

## 5. 示例 C：Computer Use + 百炼 Qwen

```bash
(
  set -e
  trap 'unset DASHSCOPE_API_KEY' EXIT
  read -r -s -p 'DASHSCOPE_API_KEY: ' DASHSCOPE_API_KEY
  printf '\n'
  test -n "$DASHSCOPE_API_KEY"
  export DASHSCOPE_API_KEY
  HB_RUN_OUTPUT="/var/tmp/hb1008/run-cu-example-$(date -u +%Y%m%dT%H%M%SZ)"
  printf '输出目录：%s\n' "$HB_RUN_OUTPUT"
  bash /var/tmp/hb1008/cu_rerun.sh "$HB_RUN_OUTPUT"
)
```

该任务原始声明为 2 vCPU、4 GiB、无 GPU；当前入口默认覆盖为 4c8g。guest 中运行 Xvfb、XFCE、Chromium，本地
页面为 `file:///app/click_calibration.html`，网络策略为 `no-network`。
模型调用发生在驱动侧；guest 不注入代理。Computer1 使用注册名 `litellm`
的通用视觉 provider，发送截图并执行桌面动作，最多 40 轮，禁用模型 shell 工具。

GUI 正向对照应得 1、`nop` 应得 0，然后才执行模型。GUI 对照通过截图颜色
定位控件，完成后从窗口标题读取随机 CODE；它用于验证环境和评分器。
模型必须自行操作并读图，成绩单独记录。官方 verifier 检查 PASS 状态和 CODE，
不强制校验 zoom，实际动作覆盖还需检查轨迹。

任务 agent 预算为 600 秒。固定 Harbor 的 Computer1 在取消后的 `finally`
中仍可能调用模型补写答案，因此这不是整个执行阶段的硬截止时间。
已有一次执行记录为 663.971 秒，含约 64 秒收尾；外层驱动另设总时限。

## 6. 检查成绩与清理

将输出目录替换为本轮打印的实际路径：

```bash
HB_RUN_OUTPUT=/var/tmp/hb1008/run-cu-example-替换为本轮时间戳
python3 - "$HB_RUN_OUTPUT" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
receipt = json.loads((root / 'receipt.json').read_text())
print('整体验收:', receipt.get('passed'))
print('清理:', receipt.get('cleanup'))
print('剩余 sandbox:', receipt.get('remaining_sandboxes', '未记录，查看失败日志'))
print('错误:', receipt.get('error'))
for path in sorted((root / 'jobs').glob('*/*/result.json')):
    row = json.loads(path.read_text())
    for step in row.get('step_results') or [row]:
        reward = ((step.get('verifier_result') or {}).get('rewards') or {}).get('reward')
        error = step.get('exception_info') or row.get('exception_info')
        print(row['task_name'], row['agent_info']['name'],
              step.get('step_name', '-'), reward,
              error.get('exception_type') if error else '-')
PY
```

模型普通 0 分是有效成绩；`AgentTimeoutError`、缺失结果、评分不完整和对照
失败属于评测异常，整体验收不通过。CLI 零退出码也不能替代这些检查。
正常结束应有 `cleanup="passed"`、所有 `agentenv.json` 状态为 `deleted`、
`remaining_sandboxes=[]`。主机恢复记录在
`evidence/host-restored-<输出目录名>.json`，主机级日志在
`evidence/<输出目录名>.log`。失败时保留原输出，下一轮使用新目录。

主要产出：

| 文件 | 用途 |
| --- | --- |
| `receipt.json` | 整体验收、运行时 hash、VMM UID、清理和错误 |
| `jobs/<阶段>/<trial>/result.json` | 单 trial 或多步骤评分与异常 |
| `jobs/<阶段>/<trial>/agentenv.json` | sandbox 与镜像标识、删除状态 |
| `jobs/<阶段>/<trial>/agent/` | 模型轨迹、截图、请求响应或 GUI 对照记录 |
| `jobs/<阶段>/<trial>/verifier/` | 原始评分器日志与 Computer Use 校验说明 |

运行入口生成 job 配置并调用 `harbor run --config`。历史 job JSON 中的 API
地址属于临时 Lab，Lab 退出后不再可用，应通过入口重跑。

已有验证结果：Terminal-Bench `run-tb-002` 的 Qwen 两项均得 1；Computer Use
`run-cu-003` 的 GUI 对照得 1、`nop` 得 0，Qwen 得 0 并记录超时，卡在输入框
坐标定位。这是小型接入评测，不代表完整 benchmark 得分或生产容量。

## 7. 在新环境准备任务制品

需要 Linux、KVM、ublk、Docker、非 root 运行账号、Python 3.12+ 和固定 Harbor
环境；Computer Use 对照还需 `Pillow==12.3.0`。按
[Dragonball 回归指南](../internals/dragonball-regression.md)准备运行时、内核、
tools、OverlayBD、registry / S3 夹具以及 capability 委派。

以具有 Docker socket 权限的非 root 账号，在 AgentENV 仓库根目录运行以下
准备命令。路径须替换为实际位置；每个输出目录必须全新，其父目录须已存在。

```bash
HB_HARBOR_SOURCE=/path/to/pinned-harbor
HB_HARBOR_MANIFEST=/path/to/harbor-manifest.json
HB_TB_SOURCE=/path/to/pinned-terminal-bench-2
HB_TB_MANIFEST=/path/to/terminal-bench-manifest.json
HB_PREPARE_ROOT=/path/to/prepared-parent

python scripts/harbor/prepare_tasks.py \
  --harbor-source "$HB_HARBOR_SOURCE" --source-manifest "$HB_HARBOR_MANIFEST" \
  --output "$HB_PREPARE_ROOT/hello"

python scripts/harbor/prepare_tasks.py \
  --harbor-source "$HB_HARBOR_SOURCE" --source-manifest "$HB_HARBOR_MANIFEST" \
  --terminal-bench-source "$HB_TB_SOURCE" --terminal-bench-manifest "$HB_TB_MANIFEST" \
  --output "$HB_PREPARE_ROOT/terminal-bench"

python scripts/harbor/prepare_tasks.py \
  --harbor-source "$HB_HARBOR_SOURCE" --source-manifest "$HB_HARBOR_MANIFEST" \
  --computer-use --output "$HB_PREPARE_ROOT/computer-use"

python -B -m unittest discover -s scripts/harbor -p 'test_*.py'
```

manifest 格式与生成要求见 [适配层说明](../../../scripts/harbor/README.md#从源码准备)：
从干净、固定提交的 checkout 记录源码与完整任务文件的 SHA256；Harbor manifest
包含 `src/`、`pyproject.toml`、三个 hello 目录，Computer Use 还需包含完整
`examples/tasks/computer-1-click-calibration/`。Terminal-Bench manifest 包含两个
完整任务目录。准备命令冻结镜像 archive 与 hash；运行时使用 OCI digest。

准备制品后，`run_lab.py` 接受 `--inputs`、`--prepared`、`--output`、`--harbor`、
`--run-user`，增加 `--model qwen3.8-flash` 才运行模型；省略则只跑评分对照。
它仍需主机准备、恢复和互斥 wrapper。远程 `setup_host.py`、`restore_host.py`
及资产路径是该实验室的环境配置，不是通用一键安装工具；不能直接把这些
绝对路径复制到新机器。CPU / 内存受 VM 配置限制，`storage_mb` 尚未作为硬配额执行。

配置字段与接口以固定版本的
[JobConfig](https://github.com/harbor-framework/harbor/blob/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/src/harbor/models/job/config.py) 和
[BaseEnvironment](https://github.com/harbor-framework/harbor/blob/4b94505a91c5ddcb70b5740ac95718ddee13e5a0/src/harbor/environments/base.py) 为准。
[官方最新配置说明](https://github.com/harbor-framework/harbor/blob/main/docs-mintlify/core-concepts/jobs/configs.mdx)
可能包含较新版本的字段；升级时需重新复核 agent 工厂构造、provider 注册名、
日志脱敏与超时收尾。
