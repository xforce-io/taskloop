# 名词表

| 规范名 | 一句话定义 | 禁止别称 |
|---|---|---|
| TaskLoop | 协调任务闭环并复用第三方 Agent Harness 的轻量通用框架。 | — |
| Task | 具有目标、状态、执行过程和可判定结果的一等任务对象。 | — |
| Goal | 用于指导任务规划和结果评价的目标。 | — |
| Skill | 以 SKILL.md 向 Agent 提供方法与资源的版本化包，可附带机器契约，但本身不等于 CLI 或状态机。 | — |
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
| EntryContract | 定义一次受控工作的 Skill、执行器、输入输出、工具、检查器和允许结果的契约。 | — |
| Playbook | 业务提供的机器可读平面流程定义，声明状态、允许转移、信号和限制。 | — |
| FlowInstance | 按锁定定义创建并保存当前状态、访问记录和已接受事实的流程实例。 | — |
| ExecutionId | 供应用查询、信号、取消及授权关联的不透明实例标识。 | — |
| ExecutionRequest | 应用启动实例所需的定义摘要、业务输入、授权、限制及幂等信息。 | — |
| ExecutionSnapshot | 公开实例视图，包含业务输入、状态、版本、等待要求与阶段产物，隐藏内部存储和调度。 | — |
| StateVisit | 一个实例每次进入状态时形成且不可复用的访问记录。 | — |
| RunSubmission | 受信适配器交回的候选清单及执行关联，不是原生 CLI 的强制 stdout 格式。 | — |
| CheckResult | 检查管线产生的 pass/fail/error、有限业务结果和可解析证据。 | — |
| HostBindings | 宿主在初始化时注入受控执行器、工具、检查器和只读方法包的公开配置面。 | — |
| BindingRegistry | TaskLoop 内部解析和锁定 HostBindings 逻辑引用的映射，不向应用暴露存储实现。 | — |
| ArtifactReceipt | 受信工具桥记录的执行关联、工具身份、摘要、时间及领域 payload 事实。 | — |
| run_epoch | 实例内部递增且用于拒绝旧 Run 提交的有效代数，不表示分布式租约。 | — |
| DefinitionSource | 宿主明确配置的业务定义文件与资源根，不来自 Agent 任意路径。 | — |
| DefinitionRef | TaskLoop 加载并锁定定义后提供给宿主引用的逻辑标识与摘要。 | — |
| ArtifactRef | 公开的产物标识、摘要、槽位与状态访问定位，不要求反查内部执行表。 | — |
| ToolContext | 供领域工具使用当前授权、期限、预算和受限产物能力的调用上下文。 | — |
| ToolOutput | 领域工具返回的业务 payload 与实际内容，由框架桥接层形成受信回执。 | — |
| GateContext | 供领域 gate 只读取得当前输入、候选、授权事实与检查证据句柄的上下文。 | — |
| TaskLoopError | 向宿主返回的稳定错误码、摘要及重试信息，不等于业务结果或生命周期。 | — |
| MilkieExecutor | 通过 Milkie 本地 Sidecar 实现 Executor 契约的 TaskLoop 适配器。 | — |
| Sidecar | 宿主启动并通过本地 stdio 管理的 Milkie 执行协议进程。 | — |
| ToolCallMetadata | 关联一次宿主工具调用的 Milkie 调用标识、原生标识及可选 traceparent，不赋予额外权限。 | — |
| Milkie | 用于统一执行与适配 Agent Harness 的 TypeScript 框架。 | — |
| Trace | 关联业务阶段、Agent 和工具等运行时观测事实的时间线，不作为业务状态权威。 | — |
| TraceCursor | 统一 Trace 按接收持久化顺序续读的位置，不表示全局发生顺序。 | — |
| SourceSequence | 同一观测来源提供的原始序号，不能跨来源比较。 | — |
| Span | 以 trace 和 span 标识关联开始、结束或活动的观测操作。 | — |
| TracePolicy | 宿主显式提供的 Trace 内容选择、容量、保留与降级契约。 | — |
| TraceContentRef | 受同一实例授权和观测容量期限约束的脱敏内容引用，不是业务产物或文件路径。 | — |
