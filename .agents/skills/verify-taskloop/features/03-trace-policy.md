# Trace 治理

主要覆盖 #8 S1/S2；验收为 `docs/design/8-trace-policy-data-governance.md` v0.5 §11。

公开路径：Runtime::open_with_trace_policy、trace/export_trace、read_trace_content 和维护入口。

- S1.A1：默认摘要模式含合成凭据/环境/提示，查/导出及存储核对无正文，实际策略可查。
- S1.A2：显式工具与字段 + 脱敏器；仅选择内容可读；两个实例和 owner 引用无越权；配置非法明确拒绝。
- S2.A1：全局/Run/内容容量、写入、退出/队列、脱敏超时不静默；业务结果与未启用采集对照，保留 #6/#7。
- S2.A2：到期清理、晚到、重开、旧库和未来版本分别判定；不能到期后伪造完整。

冻结 SHA 后逐项保存实际命令和输出。确定性治理验收不代表真实模型验收。

- S2.A3：中央期限触发 Milkie 源端观测缓冲的公开清理；区分运行中/停止未知及清理未确认，不能凭中央记录删除就宣称源数据已清理。在冻结候选上执行真实 Milkie Sidecar + 确定性 Harness，冷读 Milkie 公开 SDK 确认 expired，并核对配置/认证和业务产物保留；另验证重启、绑定变化、清理阻塞和接纳满时不新增源 journal。未完成这些证据不得写 pass。

审查回归：S2.A3 包含启动握手/调用尚可能启动时期限到达，不得提前 not_applicable；S2.A1 包含重开后降低 max_runs，须腾出足够空间，未确认来源阻止接纳。

观测尾部补充：业务完成后源清理仍需等采集资源结束（retention_waits_for_observation_tail_after_business_completion）；关闭先完成采集再清未刷盘标记。#7摘要查询沿用本功能的过期可见性，旧Run迁移、容量和删除隔离均须重验。

内容脱敏独立收尾：slow_content_redaction_cannot_consume_the_business_execution_deadline从100ms公开业务路径对照开/关采集；content_pending_is_visible_and_shutdown_drains_before_clean_reopen核对业务已成时仍content_pending、关闭等待、同Span追加tool.content引用及重开不造未刷盘。队列上限8的回归走真实queue_content入口，超载可查且不无限持有字段。旧事件内嵌引用保留，新内容从tool.content追加事件读取。
