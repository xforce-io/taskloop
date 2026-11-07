# 统一 Trace 查询与导出

主要覆盖 #7 S1/S2，详细验收为 `docs/design/7-unified-trace-query-export.md` v0.1 §11 S1.A1/S1.A2/S2.A1/S2.A2。

公开入口：Runtime::open_with_trace、Runtime::trace、Runtime::export_trace；不是读取 SQLite。

- S1.A1：启动有工具与 gate 的任务，运行中及结束后按实例/Run分页、增量续读和导出；核对阶段、Agent、实际工具及检查关系。实际 Milkie Sidecar 的确定性 Harness 证明跨语言协议；真实模型由 Epic 独立验证。
- S1.A2：双 principal/双实例查导出，越权 not_found；重开验证持久化；旧 open 返回 disabled。
- S2.A1：重复不加条；乱序/缺号/补齐/冲突分别核对；接收游标与来源序号分开，不用墙钟断言全局先后。
- S2.A2：退出断连/未结束/不可见能力与业务终态对照；采集故障及容量不能静默，且不能改写业务。

冻结候选后记录 SHA、版本、环境、命令、实际输出及所有子项结果。缺真实服务不算通过。
