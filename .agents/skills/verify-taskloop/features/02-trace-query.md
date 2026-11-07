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

## Issue #15：尾部恢复与诊断

事实源：docs/design/15-trace-tail-recovery-diagnostics.md v0.2 §11。

- S1.A1：公共SDK受控源126条事件，650ms回放延迟后仍收到126条、ID唯一，执行Span闭合，查询/导出一致；业务结果须在观测收尾前交回。
- S1.A2：首次回放失败、回调失败/超时/异常可在总窗口内恢复，含逐条10ms慢队列中的中间/末条一次失败，交付确认序号不跳过失败；成功事件不重复交付，宿主工具效果仅一次。
- S2.A1：回放超时、回调超时、队列溢出、源缺失/协议异常有分项原因且有界结束。关闭未确认的完整性判定用Unit验证，不能称为真实OS关闭失败复现；既有进程停止测试保留实际停止与未知的区别。
- S2.A2：执行实际Milkie Sidecar的受控原生CLI契约测试，核对公开Trace及取消/保留边界；另由Inquiry #5当前真实Harness研究证明新链路消费者关系。受控原生CLI不是真实模型成功证据。

入口：`cargo test --locked --offline --test milkie --test trace --test trace_policy`；Unit `cargo test --locked --offline --lib`；实际Sidecar按features/01已规定的MILKIE_NODE/MILKIE_CLI运行显式ignored目标。冻结候选另留SHA/依赖与完整原始输出。历史650ms失败、修复后对照与回调重试保持分开。
