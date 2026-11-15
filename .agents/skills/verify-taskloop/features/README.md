# TaskLoop 功能地图

| 功能文件 | 公开入口 | 范围 |
|---|---|---|
| [01-milkie-execution](01-milkie-execution.md) | MilkieExecutor、HostBindings、Runtime SDK、ToolBridge | #6 S1/S2 与原业务检查/授权回归；#23 S1/S2 显式搜索及归属回执 |
| [02-trace-query](02-trace-query.md) | Runtime Trace SDK | #7 S1/S2 四层关联、查询导出及完整性 |
| [03-trace-policy](03-trace-policy.md) | TracePolicy 与内容读取 SDK | #8 S1/S2 内容、容量、保留及降级 |
| [04-execution-budget](04-execution-budget.md) | RunBudgetPolicy、Runtime.budgets | #9 S1/S2 来源、强制与收尾 |
| [05-optional-requirements](05-optional-requirements.md) | Definition / Runtime / artifact.read SDK | #16 S1/S2 首次空历史、版本选择与授权 |
| [02-trace-query：尾部恢复](02-trace-query.md#issue-15尾部恢复与诊断) | MilkieExecutor、Trace查询/导出 | #15 S1/S2 有界追赶、交付确认序号与分项故障 |
| [06-goal-dag](06-goal-dag.md) | Goal CLI、Goals/GoalClient SDK | #10 S1–S3 提交、规划、等待与DAG执行 |
| [07-goal-acceptance-reliability](07-goal-acceptance-reliability.md) | Goal评价、预算、控制及公开progress | #10 S4–S5 评价、幂等、隔离、停止与崩溃恢复；#20 S1–S2 评价输入与在线接纳恢复 |
| [08-application-integration](08-application-integration.md) | Inquiry公开Goal消费者、真实Milkie/Harness | #10 S6 真实研究、未达标及第二业务 |
