# 名词表

| 规范名 | 一句话定义 | 禁止别称 |
|---|---|---|
| TaskLoop | 协调任务闭环并复用第三方 Agent Harness 的轻量通用框架。 | — |
| Task | 具有目标、状态、执行过程和可判定结果的一等任务对象。 | — |
| Goal | 用于指导任务规划和结果评价的目标。 | — |
| Skill | 跨层使用、以独立 Git 资产进行版本管理的能力定义与配套资源。 | — |
| Schema | 定义领域数据和交换契约的结构规范。 | — |
| Application Runtime | 在 L5 管理任务生命周期和闭环推进的应用运行时。 | — |
| Decision | 在 L5 决定规划、评价、重规划及执行能力选择的职责。 | — |
| Routing | 在 L5 根据任务需求和约束选择能力、模型与 Agent Harness 的职责。 | — |
| Worker | 在 L4 适配并执行 L5 决策、向上返回执行结果的组件。 | — |
| Agent Harness | 在 L3 提供 Agent 执行会话及相关执行机制的第三方框架。 | — |
| Plan | 根据目标与当前状态形成任务计划的阶段。 | — |
| Route | 为待执行任务选择能力、模型与 Agent Harness 的阶段。 | — |
| Execute | 通过 Worker 和第三方 Agent Harness 执行任务的阶段。 | — |
| Evaluate | 根据目标和验收条件判断执行结果的阶段。 | — |
| Replan | 根据评价及新证据调整计划的阶段。 | — |
| Skill invocation | 在 L6 通过 Skill 方式调用 TaskLoop 的入口。 | — |
