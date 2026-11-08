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
| 可选产物依赖 | 显式声明且允许尚无产物、存在时仍按所选版本与权限提供的工作入口依赖。 | — |
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
| 交付确认序号 | 同一观测来源中连续成功交付至观察者的最大 SourceSequence，不表示已读取或已排队的最大序号。 | — |
| Span | 以 trace 和 span 标识关联开始、结束或活动的观测操作。 | — |
| TracePolicy | 宿主显式提供的 Trace 内容选择、容量、保留与降级契约。 | — |
| TraceContentRef | 受同一实例授权和观测容量期限约束的脱敏内容引用，不是业务产物或文件路径。 | — |
| SourceCleanup | 单个 Run 的源观测缓冲清理状态及其执行器绑定，不代表业务终态。 | — |
| RunBudgetPolicy | 宿主为 EntryContract 显式配置的执行期限、工具准入、模型迭代和候选容量策略。 | — |
| RunBudgetSnapshot | 某次 Run 生效限制、来源、消耗及触发原因的持久化公开视图，不是费用估算。 | — |
| Goal Spec | 用户提交的目标、输入、验收要求与执行约束组成的不可变目标规格。 | — |
| Planner | 在能力与约束内生成计划提案、不拥有执行派发或状态提交权的 Agent。 | — |
| DAG | 在锁定后不含回边、以明确依赖连接工作节点的有向无环计划。 | — |
| 节点 | DAG 内具有公共输入输出契约、由一个或多个 Worker 完成的工作单元。 | — |
| Capability | 宿主注册并锁定输入输出、执行方式、权限、预算和检查要求的可用节点能力。 | — |
| GoalSnapshot | 通过公开接口读取的目标、计划、节点、评价、预算及停止事实的版本化视图。 | — |
| PlanProposal | Planner 返回且尚未经 Runtime 接纳的结构化 DAG 候选。 | — |
| NodeContract | 规定节点公共端口、Worker成员、汇合和接纳要求的不可变能力契约。 | — |
| EvaluationReceipt | Runtime 将评价结果与检查身份、规则版本及确切对象摘要绑定后形成的接纳事实。 | — |
| GoalBudget | 目标下规划、检查和业务执行共享且以预留和结算约束的预算账目。 | — |
| GoalRequest | 携带幂等键、授权引用与Goal Spec的提交请求信封。 | — |
| GoalGrant | 宿主授予调用方使用指定Profile、能力与目标预算的授权契约。 | — |
| CheckRequirement | 冻结在Profile中的必需检查身份、阶段、规则及目标输出声明。 | — |
| CheckRequest | Runtime为一次检查生成、绑定确切对象与授权证据范围的不可复用请求。 | — |
| GoalClient | 不取得执行所有权、仅查询公开事实或向在线宿主提交命令的客户端。 | — |
| GoalSignalRequest | 绑定等待身份、版本和幂等键的目标补充输入请求。 | — |
| OrchestrationRegistry | 宿主显式注册并锁定Profile、能力、模板、检查和汇合实现的目录。 | — |
| OrchestrationOptions | 宿主显式启用编排并规定容量及协议限制的配置。 | — |
| 状态转移器 | 根据当前快照、已验证事件与转移规则计算状态变更和待执行动作，并由事务接纳后生效的Runtime组件。 | — |
| PlannerResult | Planner一次调用返回的计划提案或结构化补充输入请求，不授予状态提交权。 | — |
| terminal_intent | 首个决定性事件持久化的待终结业务结果与原因，在停止结算完成后提交为最终outcome。 | — |
| 节点执行成员 | 节点内部对某Worker的一次具名调用规格，具有member_id和EntryContract，最多产生一次Run，不等同于L4 Worker组件。 | — |
| 阻塞原因 | 目标不能继续业务执行的独立条件及对象定位，解除一项不自动解除其他条件。 | — |
| SharedDispatcher | 全宿主唯一的受控Run启动入口，通过共享账本完成物理准入并防止重复启动。 | — |
| Managed Run | 复用输入校验、预算、工具约束、Executor调用、候选及停止事实处理的受控执行管线。 | — |
| TransitionRule | Runtime内建的状态转移契约，固定事件、前态、条件、变更、动作及原子边界。 | — |
| TransitionDecision | 针对当前快照计算的待提交变更与动作，CAS失败后须丢弃并重新求值。 | — |
| RunRequest | 同Goal内持久保存的待执行调用请求，不占槽位且最多物化为一个Run。 | — |
| CapabilityPackage | 应用能力定义的离线清单表示及资源示例，首版不作为运行时文件加载协议。 | — |
| CheckInput | Runtime以冻结检查请求、规则和授权目标构造的只读checker执行输入。 | — |
| RuleSet | 应用声明的版本化领域检查项、通过条件及证据要求集合。 | — |
| RegistrationReceipt | 类型注册目录完成验证并封存后返回的锁定摘要和已注册引用回执。 | — |
| EvaluationSubject | 评价回执绑定的提案摘要或确切产物引用集合，不重复保存目标正文。 | — |
| GoalRunContext | Runtime指定的Goal执行归属，只标识Goal、角色、计划和检查身份，不授予访问权限。 | — |
| CliDriver | 持有前台宿主所有权、仅派发所选Goal并提供本地控制入口的应用驱动器。 | — |
| GoalCommandResult | 在线目标命令的固定回执与当前状态、待停止Run及阻塞原因摘要。 | — |
