# TaskLoop 功能地图

| 功能文件 | 公开入口 | 范围 |
|---|---|---|
| [01-milkie-execution](01-milkie-execution.md) | MilkieExecutor、HostBindings、Runtime SDK、ToolBridge | #6 S1/S2 与原业务检查/授权回归 |
| [02-trace-query](02-trace-query.md) | Runtime Trace SDK | #7 S1/S2 四层关联、查询导出及完整性 |
| [03-trace-policy](03-trace-policy.md) | TracePolicy 与内容读取 SDK | #8 S1/S2 内容、容量、保留及降级 |
| [04-execution-budget](04-execution-budget.md) | RunBudgetPolicy、Runtime.budgets | #9 S1/S2 来源、强制与收尾 |
| [05-optional-requirements](05-optional-requirements.md) | Definition / Runtime / artifact.read SDK | #16 S1/S2 首次空历史、版本选择与授权 |
| [02-trace-query：尾部恢复](02-trace-query.md#issue-15尾部恢复与诊断) | MilkieExecutor、Trace查询/导出 | #15 S1/S2 有界追赶、交付确认序号与分项故障 |
