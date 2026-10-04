# M5：双后端保留与恢复依赖交接

用户已批准自动执行 M2–M5；本阶段沿用 Native，不再次等待常规确认。
目标是可配置组件的交付闭环，不是 Firecracker 删除或生产切流。

## 范围

1. 审计测试机 `/var/lib/aenv` 和卸载备份
   `/var/tmp/agentenv-uninstall-20260930.6clQzq`。不启动旧 server、不运行 GC，
   不输出 credentials/env/access-token。暂停 RocksDB 用独立只读 reader，
   保留未知/损坏记录并标记人工复核；审计前后文件摘要一致。目录中无记录不代表
   外部 OSS/registry 或其他节点已无恢复依赖，不能据此授权删除。
2. 保留 FC backend/client/pool/schema/生成器及 PVM/aarch64 路径。
   补齐显式本地 FC 候选包，与已有 DB 包分别校验资产 SHA、release backend 身份、
   source revision、版本和 NOTICE。打包不下载、不覆盖、不发布、不切默认。
   新 FC 候选限定 linux/amd64 KVM，不取代现有其他平台发布路径。
3. 检查 FC-only / DB-only / both 的构建依赖隔离；单后端分别验证 persistence /
   restore identity。在独立空状态/缓存/deps 的 namespace 节点中使用异目录解包
   候选复验 cold/capture/restore/fork/pause/resume 和异类拒绝，复用 M4 测试架。

## 实施选择

使用独立 example 读取现有 RocksDB 默认 column family，避免给在线 server 添加
管理 API；只输出版本、backend、恢复身份和 guest 资产版本等白名单字段。
FC 打包器复用已有候选输入约定，不为两个后端再引入通用插件框架。
旧内存不能转换给另一后端；重建 OCI/recipe 必须用新 ID/alias。

## 验收与边界

只读/未知记录/缺目录、包身份错误/摘要错误/重名拒绝/异目录验证均有回归。
实际测试使用 aenv；root 仅编排隔离及读取 root-owned 卸载备份。
保留源代码、旧制品、历史状态；不自动清理 M4 证据。构建缓存复用不冒充全新 OS
安装，干净环境是明确的新 runtime state/deps/cache。
M4 真实双宿主机/SLO 门槛继续开放；PVM/aarch64 本机不能实测时只声明路径保留，
不以 x86_64 KVM 成功外推。没有旧暂停记录则不能声称实际历史 resume 已通过。
