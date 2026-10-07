# Hypervisor 验证摘要与已知限制

最后核对：2026-10-07。运行时变更基线为 `a99fd09`，测试结构整理基线为 `ab3db37`。
以下是这些候选的已有证据，不代表后续修改自动通过，也不代表生产 SLO 验收。
日常执行方式见[功能回归](dragonball-regression.md)。

## 已验证范围

| 候选 | 验证结论 | 边界 |
| --- | --- | --- |
| `a99fd09` 启动/tools 策略 | runner/protocol 76 通过、11 ignored；envd 15、DB 单元 60 通过；11 项生命周期及 1 项真实 probe 保存/恢复通过 | 覆盖初始化上下文、鉴权/网络、held 身份、暂停恢复、发布/卷、回滚及重启；未重跑全量阶段/应用矩阵 |
| `ab3db37` 测试结构 | 编译后 78 个集成用例逐项保留；工具测试 79 通过、9 项需显式 namespace/controller/physical 输入而跳过；4 项真实冒烟通过 | 默认 60 个独立用例；原重复选择消除，旧 revision 身份专项保留为 opt-in |
| 两个候选 | workspace 全目标/全 feature 严格 Clippy、fmt、独立评审及测试资源清理通过 | GitHub 托管 CI 需在实际推送后确认 |

## 性能结论

`a99fd09` 的同机诊断共 64 个有效样本，固定 1 vCPU/1 GiB guest、并发 1、相同
内核/tools/rootfs 及 CPU/NUMA 策略。API / 从 API 发起至额外命令确认准确身份的
中位数如下，单位 ms；每组合常规/清缓存各 6 个样本，压力条件 4 个样本。

| 条件 | FC | DB 候选 |
| --- | ---: | ---: |
| tools 预热 | 672.5 / 721.0 | 709.1 / 714.1 |
| tools 文件缓存清空 | 680.3 / 726.8 | 732.6 / 736.4 |
| 768 MiB memory.high、512 MiB 辅助驻留负载 | 722.2 / 768.4 | 788.7 / 795.2 |

FC API 返回与 DB 身份屏障口径不同，不能仅按 API 数字比较 guest 身份就绪。
这些小样本不提供 p95/p99、完整存储冷缓存、应用吞吐或容量资格。更低的 640 MiB
回收阈值曾令旧 DB 严重停顿并 readiness 超时，失败未并入有效样本。

## 尚未关闭的限制

- 历史 OSS30 并发上传超时及 readiness/尾延迟问题仍需独立复现、修复和资格验证。
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
