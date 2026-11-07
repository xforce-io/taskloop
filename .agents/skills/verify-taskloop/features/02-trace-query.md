# 统一 Trace 查询与导出

主要覆盖 #7 S1/S2，详细验收为 `docs/design/7-unified-trace-query-export.md` v0.3 §11 S1.A1/S1.A2/S2.A1/S2.A2。

公开入口：Runtime::open_with_trace、Runtime::trace、Runtime::export_trace；不是读取 SQLite。

- S1.A1：启动有工具与 gate 的任务，运行中及结束后按实例/Run分页、增量续读和导出；核对阶段、Agent、实际工具及检查关系。实际 Milkie Sidecar 的确定性 Harness 证明跨语言协议；真实模型由 Epic 独立验证。
- S1.A2：双 principal/双实例查导出，越权 not_found；重开验证持久化；旧 open 返回 disabled。
- S2.A1：重复不加条；乱序/缺号/补齐/冲突分别核对；接收游标与来源序号分开，不用墙钟断言全局先后。
- S2.A2：退出断连/未结束/不可见能力与业务终态对照；采集故障及容量不能静默，且不能改写业务。

冻结候选后记录 SHA、版本、环境、命令、实际输出及所有子项结果。缺真实服务不算通过。

## 业务交付与观测尾部

阶段结束不是采集结束。慢observation.read/终态回调不得使已完成阶段变成limit_reached；TracePage按真实缺号/开放操作/最终状态变化，usage中的pending为交付快照。Runtime.shutdown等待finish_observations再完成观测存储关闭；直接适配器调用需要最终状态时显式调用该方法。固定截止线仍强制原生执行，不因后台采集延长。

新增回归：tests/milkie.rs的observation_replay_cannot_hold_an_already_stopped_execution_result及observation_only_wait_cannot_turn_a_completed_runtime_phase_into_timeout，前者已在原e7ae5a1复现后修复；后者补充公共Runtime阶段期限验证。src/trace.rs的固定SQLite指令预算测试验证一万条历史后的单条尾页不全量读正文，另覆盖旧事件摘要回填/整Run退役隔离/原始时间范围。边界不能仅靠计时快慢判定。

同类失败隔离另覆盖畸形观测尾页只报告不完整、不改业务接纳，以及原生停止证明后迟到宿主请求不得再产生效果。

采集结束屏障：business_and_source_terminal_do_not_complete_an_unfinalized_collector核对业务/来源已终止仍observation_pending，adapter.summary成功后可完整、失败仍不完整；派生摘要v1迁移v2不补造结束证据。
