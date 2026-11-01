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
| Run | 叶子 Task 在固定输入与执行配置下的一次执行尝试。 | — |
| Router | 在 L5 实施 Routing 并返回可审计执行配置提案的组件。 | — |
| Agent Profile | 定义执行角色、能力及策略约束的可组合配置。 | — |
| PlanVersion | 某个 Goal 下不可变的任务成员、依赖及修订依据快照。 | — |
| SkillRef | 用于解析 Skill 来源、包路径与版本约束的未锁定引用。 | — |
| SkillLock | 固定 Skill 及依赖的 Git 提交、版本与内容摘要的不可变记录。 | — |
| ExecutionSpec | 绑定某次 Run 的输入、版本、权限、预算及执行器配置的不可变规格。 | — |
| RunEvent | 描述某次 Run 执行进度与结果并可去重的规范化事件。 | — |
| ExecutionResult | Worker 返回的执行结束状态、候选产物、用量及错误记录。 | — |
| DecisionProposal | Decision 基于指定快照提出且需 Runtime 校验的计划或状态推进建议。 | — |
| RouteProposal | Router 在硬约束内提出的执行配置选择及依据。 | — |
| RouteDecision | 已持久化的路由输入、候选、选择与拒绝依据记录。 | — |
| Evaluation | 针对确切对象版本和产物的质量评价结果及证据。 | — |
| Artifact | 具有不可变内容摘要、生产来源与访问边界的产物对象。 | — |
| Approval | 将批准或拒绝绑定到确切动作、对象版本、策略与有效期的授权记录。 | — |
| Event | 关联核心对象并按游标追加保存的状态或审计记录。 | — |
| ActionRecord | 记录外部动作幂等键、执行状态与回执的副作用账目。 | — |
| Tool Gate | 在外部工具动作执行前校验权限、审批与资源限制的网关。 | — |
| Rubric | 规定领域质量检查、阈值及所需证据的版本化评价规则。 | — |
| outbox | 与状态同事务写入并由运行时恢复投递的待派发记录。 | — |
| fencing token | 随租约接管递增并用于拒绝过期执行提交的隔离令牌。 | — |
| ADR | 记录架构决定、理由、代价与替代项的版本化记录。 | — |
| MVP | 用最小实现证明必需闭环及边界的可验证交付范围。 | — |
