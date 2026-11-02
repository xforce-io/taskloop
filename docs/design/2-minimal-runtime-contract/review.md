# TaskLoop 最小执行契约设计复核

日期：2026-11-02。目标：[Issue #2 L2 v0.1.0](../2-minimal-runtime-contract.md)。本记录为设计作者自查与消费者契约复核，不是独立实现审查或人工批准；总体状态 Draft。

| 关注/缺陷 | 处置与判断 |
|---|---|
| 总体 RFC 的 Python/CLI/租约/动态规划扩大 MVP | 一期唯一契约落在 Issue #2；历史 RFC 明示适用范围，不再驱动一期依赖 |
| 应用泄漏存储和执行尝试 | 私有 store_dir；公开快照补业务 inputs、state_id/visit/slot/摘要；应用不读表或反查 Run |
| 将研究 HTTP 字段塞进通用回执 | 通用工具信封仅关联/摘要/payload；HTTP schema 明确是历史研究 fixture，不属于协议 |
| 为可扩展性预建平台 | 一个 crate、一个真实 Harness、固定模型、静态绑定、平面状态，无独立 Planner/动态 Router/插件市场/服务队列 |
| blocked 一律释放 Worker 会让未知旧进程与新工作重叠 | 仅已确认执行结束的阻塞释放；未知中断/取消仍保留执行容量，不能用状态名推断已停止 |
| 单 Worker 中 gate 等另一个排队任务可能自等待 | 领域模型检查是本阶段受限回调，不同步等待同队列另一任务；预算/超时仍受控 |
| 把异常/空证据当通过 | pass 有限结果与真实证据；异常/不可用/协议错误保持原阶段阻塞；候选不合格与基础设施故障分开 |
| 用模型证据替代生产健壮性 | 85 项契约/离散模型/最小 SQLite CAS 见证通过；真实 Rust、完整事务、Harness/进程隔离和实际取消/重启未执行 |

复核结果：轻量范围、公共封装、领域边界、失败关闭与多实例不变量已自洽；Inquiry 的 D1–D5 均可映射到本稿 T1–T6。核心无 Inquiry 运行时依赖；两类业务仅用作通用设计 fixture。尚未选择的 Harness/模型/工具链必须经能力探针证明后固定，不能将设计提案当可用 crate SHA。

复现：运行 contract/verify_contract.py 得到 [verification-result.json](contract/verification-result.json)，并核对源码摘要和 trace。公开快照/工具事实的新案例只验证 Schema，不替代相应真实集成。跨仓 Schema/资源边界核对与 Inquiry 的[设计复核](https://github.com/xforce-io/inquiry/blob/feat/1-minimal-research-loop/docs/design/1-minimal-research-loop/review.md)配套。

可交人工 review，无已知未处理的设计阻塞项；未自批 Approved，未实现、合入或部署。

跨仓资源/接口/规格核对为 22 项，结果见 [consumer-check.json](consumer-check.json)，包含两份 L2 与检查器源码摘要。复现：在已安装 jsonschema 的设计验证环境运行 `python docs/design/2-minimal-runtime-contract/verify_consumer_contract.py /path/to/inquiry`。这只验证静态边界，不导入 Inquiry 产品代码，也不构成 TaskLoop 的运行时依赖。
