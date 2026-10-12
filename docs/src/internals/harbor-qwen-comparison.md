# AgentENV + Dragonball：Qwen3.8 Flash / Max 对比报告

Terminal-Bench **2.0**，固定任务提交 `69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`；Harbor **0.24.0**。两轮各执行完全相同的 **71 项**。

**原生 verifier 得1分：Flash 29/71（40.8%），Max 39/71（54.9%），增加10项、14.1个百分点。** Max原生得分有15项提升、5项回退；AgentTimeoutError从33项降至14项。所有模型sandbox均删除，两机宿主恢复通过。

## 口径与配置

原资格集合74项，其中Mailman待新模板/运行时验证，视频提取和MTEB leaderboard缺必需素材绑定，三项仍未执行。另12项资格未合格继续排除。按原资格集合计算的原生得1比例为29/74=39.2%、39/74=52.7%；不能把71项成绩写成完整89项官方榜单成绩。

| 项目 | 两轮一致的配置 |
| --- | --- |
| 后端、任务分片 | AgentENV + Dragonball；A 41项、B 30项 |
| guest / 节点 | 每任务4 vCPU / 8192MiB；每机节点64c / 128GiB |
| 并发、调度 | 每机最多8槽，确认删除后持续补位；实际VMM峰值均8 |
| 模板与镜像 | 相同原始任务、镜像digest、已验证OSS模板、tools 0.1.2-p4.1 |
| Agent | Terminus-2；请求thinking=false、最多4096输出tokens；未设置25轮冒烟限制 |
| 预算、重试 | 原task.toml阶段预算；每项一次，零trial自动重试 |
| 凭据、记录 | 原生结果、消息/工具轨迹、token、阶段耗时、错误及删除前取证均保留 |

两轮实际配额、任务契约与server/runner/kernel/tools摘要逐项核对相同。驱动Python文件仅run_lab.py变化，用于增加Max模型CLI选项；Agent参数除模型名称外一致。没有重跑Oracle/nop或增加Docker对照。

## 得分与运行状态

原生reward与运行异常分别统计：Flash有两项Agent超时但verifier已得1（adaptive-rejection-sampler、llm-inference-batching-scheduler）。此前38.0%是“得1且无异常”口径；原生得分是40.8%，保持两条指标，不改写原始结果。

| 指标 | Flash | Max |
| --- | ---: | ---: |
| 原生 verifier 得1 | 29/71（40.8%） | 39/71（54.9%） |
| 得1且无运行异常 | 27/71（38.0%） | 39/71（54.9%） |
| 评测完成但未通过、无运行异常 | 11 | 18 |
| AgentTimeoutError | 33 | 14 |
| 其他原生运行异常 | 0 | 0 |
| 模型任务全部结束 / sandbox删除 / 宿主恢复 | 通过 | 通过 |

运行状态转换（不把无异常结束等同任务通过）：

| Flash状态 → Max状态 | 项数 |
| --- | ---: |
| 完成但未通过 → 完成但未通过 | 8 |
| 完成但未通过 → 通过 | 3 |
| 超时 → 完成但未通过 | 7 |
| 超时 → 超时 | 13 |
| 超时 → 通过 | 13 |
| 通过 → 完成但未通过 | 3 |
| 通过 → 超时 | 1 |
| 通过 → 通过 | 23 |

20项从超时转为无异常结束，另有compile-compcert新增超时。最终净减少19项超时；20项中也包含仍得0分的任务。

## 耗时与Token

| 指标 | Flash | Max |
| --- | ---: | ---: |
| 两机控制器总起止跨度 | 2小时17分5秒 | 1小时47分36秒 |
| A执行跨度 | 2小时17分2秒 | 1小时47分36秒 |
| B执行跨度 | 1小时8分14秒 | 0小时52分25秒 |
| 累计Agent执行时间（并行任务求和） | 20.31小时 | 13.20小时 |
| 已返回模型请求计数 | 2,627 | 1,661 |
| 模型请求往返中位数 / P95 | 12.15s / 35.62s | 8.00s / 35.07s |
| 环境setup中位数 | 2.60s | 2.74s |
| 输入tokens，包含缓存 | 73,583,119 | 50,666,015 |
| 其中缓存tokens | 70,052,864 | 47,757,056 |
| 未命中缓存输入tokens | 3,530,255 | 2,908,959 |
| 输出tokens | 913,515 | 945,842 |
| 已返回用量字段覆盖 | 71/71任务 | 71/71任务 |
| 账单费用 | 未提供 | 未提供 |

本轮控制器跨度缩短21.5%，输入tokens减少31.1%，输出tokens增加3.5%。输入与缓存不能相加；累计阶段耗时含并行重叠，不能相加作为总墙钟。

控制器跨度从模型轮控制器启动到最终清理结束，不含准备、磁盘清理审计和静态检查。两轮分日执行，API/网络与运行时下载负载可能变化；模型请求往返包含客户端/网络/服务等待，不是纯服务端推理时间，最后取消的请求可能没有完整计时或用量。Max两个辅助预检各17tokens单列，未纳入71项用量。费用字段缺失，不能由token总量推定实际账单变化。

## 逐项变化与剩余问题

**原生得分提升15项：** `bn-fit-modify`, `break-filter-js-from-html`, `circuit-fibsqrt`, `cobol-modernization`, `count-dataset-tokens`, `custom-memory-heap-crash`, `feal-differential-cryptanalysis`, `fix-ocaml-gc`, `largest-eigenval`, `merge-diff-arc-agi-task`, `model-extraction-relu-logits`, `regex-log`, `sparql-university`, `tune-mjcf`, `winning-avg-corewars`.

**原生得分回退5项：** `adaptive-rejection-sampler`, `compile-compcert`, `configure-git-webserver`, `db-wal-recovery`, `sqlite-with-gcov`.

回退项优先复核。compile-compcert从无异常得1转为Agent超时；adaptive-rejection-sampler从“超时但得1”转为无异常得0，说明消除超时不必然提高产物质量。其他回退的具体原因需要查看Max轨迹/验收日志，不由分数直接判环境或模型责任。

**Max仍超时14项：** `compile-compcert`, `feal-linear-cryptanalysis`, `gcode-to-text`, `gpt2-codegolf`, `make-mips-interpreter`, `password-recovery`, `path-tracing`, `path-tracing-reverse`, `polyglot-rust-c`, `regex-chess`, `schemelike-metacircular-eval`, `torch-pipeline-parallelism`, `train-fasttext`, `write-compressor`.

Flash两项guest Python OOM已对应到模型生成的巨大前缀列表和不收敛生成循环；Max的14份删除前内核日志未命中OOM关键词。这是捕获日志的观察，不能替代连续内存压力测量。Flash两项响应协议循环和31项超时的具体证据见旧轮分析；未对Max每个失败项完成根因复现，不把它们统一判为Dragonball故障。

本次结果说明，同一AgentENV + Dragonball配置上，Max的得分和无异常结束比例更高。它不是VMM优化前后实验，环境setup中位数也没有显示同等规模的加速。后续优先分析5项回退、14项残留超时，以及性能断言、依赖准备、模型输出协议中未分离的贡献。

## 全部71项索引

| 任务 | Flash原生reward / 状态 | Max原生reward / 状态 |
| --- | --- | --- |
| `adaptive-rejection-sampler` | 1 / 超时 | 0 / 完成但未通过 |
| `bn-fit-modify` | 0 / 完成但未通过 | 1 / 通过 |
| `break-filter-js-from-html` | 0 / 超时 | 1 / 通过 |
| `build-pov-ray` | 1 / 通过 | 1 / 通过 |
| `cancel-async-tasks` | 0 / 超时 | 0 / 完成但未通过 |
| `chess-best-move` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `circuit-fibsqrt` | 0 / 超时 | 1 / 通过 |
| `cobol-modernization` | 0 / 超时 | 1 / 通过 |
| `compile-compcert` | 1 / 通过 | 0 / 超时 |
| `configure-git-webserver` | 1 / 通过 | 0 / 完成但未通过 |
| `constraints-scheduling` | 1 / 通过 | 1 / 通过 |
| `count-dataset-tokens` | 0 / 完成但未通过 | 1 / 通过 |
| `crack-7z-hash` | 1 / 通过 | 1 / 通过 |
| `custom-memory-heap-crash` | 0 / 超时 | 1 / 通过 |
| `db-wal-recovery` | 1 / 通过 | 0 / 完成但未通过 |
| `distribution-search` | 1 / 通过 | 1 / 通过 |
| `dna-assembly` | 0 / 超时 | 0 / 完成但未通过 |
| `dna-insert` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `extract-elf` | 0 / 超时 | 0 / 完成但未通过 |
| `feal-differential-cryptanalysis` | 0 / 超时 | 1 / 通过 |
| `feal-linear-cryptanalysis` | 0 / 超时 | 0 / 超时 |
| `filter-js-from-html` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `financial-document-processor` | 1 / 通过 | 1 / 通过 |
| `fix-code-vulnerability` | 1 / 通过 | 1 / 通过 |
| `fix-git` | 1 / 通过 | 1 / 通过 |
| `fix-ocaml-gc` | 0 / 超时 | 1 / 通过 |
| `gcode-to-text` | 0 / 超时 | 0 / 超时 |
| `git-leak-recovery` | 1 / 通过 | 1 / 通过 |
| `git-multibranch` | 1 / 通过 | 1 / 通过 |
| `gpt2-codegolf` | 0 / 超时 | 0 / 超时 |
| `hf-model-inference` | 1 / 通过 | 1 / 通过 |
| `kv-store-grpc` | 1 / 通过 | 1 / 通过 |
| `large-scale-text-editing` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `largest-eigenval` | 0 / 超时 | 1 / 通过 |
| `llm-inference-batching-scheduler` | 1 / 超时 | 1 / 通过 |
| `log-summary-date-ranges` | 1 / 通过 | 1 / 通过 |
| `make-mips-interpreter` | 0 / 超时 | 0 / 超时 |
| `merge-diff-arc-agi-task` | 0 / 超时 | 1 / 通过 |
| `model-extraction-relu-logits` | 0 / 超时 | 1 / 通过 |
| `modernize-scientific-stack` | 1 / 通过 | 1 / 通过 |
| `mteb-retrieve` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `multi-source-data-merger` | 1 / 通过 | 1 / 通过 |
| `nginx-request-logging` | 1 / 通过 | 1 / 通过 |
| `openssl-selfsigned-cert` | 1 / 通过 | 1 / 通过 |
| `password-recovery` | 0 / 超时 | 0 / 超时 |
| `path-tracing` | 0 / 超时 | 0 / 超时 |
| `path-tracing-reverse` | 0 / 超时 | 0 / 超时 |
| `polyglot-c-py` | 0 / 超时 | 0 / 完成但未通过 |
| `polyglot-rust-c` | 0 / 超时 | 0 / 超时 |
| `portfolio-optimization` | 1 / 通过 | 1 / 通过 |
| `prove-plus-comm` | 1 / 通过 | 1 / 通过 |
| `pypi-server` | 1 / 通过 | 1 / 通过 |
| `pytorch-model-cli` | 1 / 通过 | 1 / 通过 |
| `query-optimize` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `raman-fitting` | 0 / 超时 | 0 / 完成但未通过 |
| `regex-chess` | 0 / 超时 | 0 / 超时 |
| `regex-log` | 0 / 超时 | 1 / 通过 |
| `reshard-c4-data` | 1 / 通过 | 1 / 通过 |
| `sam-cell-seg` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `sanitize-git-repo` | 0 / 完成但未通过 | 0 / 完成但未通过 |
| `schemelike-metacircular-eval` | 0 / 超时 | 0 / 超时 |
| `sparql-university` | 0 / 完成但未通过 | 1 / 通过 |
| `sqlite-db-truncate` | 1 / 通过 | 1 / 通过 |
| `sqlite-with-gcov` | 1 / 通过 | 0 / 完成但未通过 |
| `torch-pipeline-parallelism` | 0 / 超时 | 0 / 超时 |
| `train-fasttext` | 0 / 超时 | 0 / 超时 |
| `tune-mjcf` | 0 / 超时 | 1 / 通过 |
| `video-processing` | 0 / 超时 | 0 / 完成但未通过 |
| `vulnerable-secret` | 1 / 通过 | 1 / 通过 |
| `winning-avg-corewars` | 0 / 超时 | 1 / 通过 |
| `write-compressor` | 0 / 超时 | 0 / 超时 |

## 原生证据

Flash执行：2026-10-11 19:53:14 → 2026-10-11 22:10:19；Max执行：2026-10-12 09:56:20 → 2026-10-12 11:43:57，均为UTC+8。

两机原生记录分别保留在 `/var/tmp/hb1008/terminal-benchmark-20261011/shard-{0,1}-r1/run` 与 `/var/tmp/hb1008/qwen-max-20261012/shard-{0,1}/run`；sandbox只在删除回执核对后记完成。所有14项Max异常取证成功，清理无错误，冻结候选摘要核对通过。systemd非零退出来自任务异常汇总，队列reason=completed且pending=0，不是仍在运行或清理失败。

结构化对比、逐项 CSV、来源摘要和最终清理证明保存在测试制品目录
`/tmp/agentenv-harbor-qwen-max-20261012/`，Flash 来源位于
`/tmp/agentenv-harbor-benchmark-20261011/`。原生结果路径如上，
[超时分析](harbor-qwen-timeout-analysis.md)和
[集成记录](../../../HARBOR_INTEGRATION.md)提供仓库内追溯入口。
