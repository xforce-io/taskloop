# TaskLoop #10 实现计划

基线：[Issue #10](https://github.com/xforce-io/taskloop/issues/10)，L2 v0.10.3，设计提交 cbed510cd539f719d3302e1695eb69b2d15a904c；本机Grok审查PASS。L1复用原概要提案及v0.2的18项细验收，用户已明确授权“调整完文档，keel端到端完成一个版本开发测试和review”。不另加包加载器或step入口。

## 当前机制与复用依据

- `src/runtime.rs::Runtime::open_impl/worker_loop/enter`：v1固定Flow，串行等待phase；enter在排队时已建立未started的active并计Run次数。
- `src/store.rs::Store::open/commit`：所有权文件锁、v1 SQLite、IMMEDIATE revision CAS与内容寻址blob。
- `src/phase.rs::Bridge/run_checked`：工具、候选、计量与停止；目前直接耦合单active Record，需抽出共享执行责任与owner适配。
- `src/execution.rs::Executor/ToolBridge`、`src/milkie.rs`：沿用受控执行、原生停止/未知、观测尾部及显式artifact.write提交。
- 既有功能地图01–05和原套件为保全门；真实Sidecar/Harness默认ignored不算已验证。

## 工作顺序

1. 类型契约、冻结注册、受限Schema/selector、计划/模板校验及共享推进/检查判定；对应S2/S4。
2. Store v2原子迁移、Goal及共享run_leases、命令幂等与原子预算；旧Flow queued不占槽，保留原ID/epoch/已计次数；对应S5。
3. 提取共享Managed Run/Bridge，Flow与Goal经唯一Dispatcher；Goal协调、并行成员、汇合、输入等待、检查、停止及恢复；对应S2–S5。
4. Rust Goals/GoalClient、前台CLI及本地控制Socket，内置文档审阅Profile；无需step；对应S1/S6。
5. 独立消费者及真实Inquiry领域能力集成，固定应用/执行依赖SHA并分别记录确定性、真实Sidecar与真实Agent证据；不声称Inquiry #7文章质量完成；对应S6。
6. 冻结候选，更新功能地图06/07/08及S1–S6/18项证据表；keel-verify→独立code-review→CI→keel-release。若Reviewer判human required，只在全部结果可评审后请求当前候选的人工门。

## 验证与交付边界

- 每层先聚焦Unit/Integration，再用真实CLI/SDK走用户路径；使用Rust 1.91.1和锁文件。
- 新增功能文件：06-goal-dag.md（S1–S3）、07-goal-acceptance-reliability.md（S4–S5）、08-application-integration.md（S6）。
- 原18项验收不删、不降门槛；缺环境或未运行记blocked/not_run，不当skip/pass。
- 不改用户Inquiry当前工作树；应用集成优先复用固定版本领域库和公开API。若需应用改动，单独隔离并保留跨仓库引用。
- 代码完成、文档PASS或PR创建均不等于端到端完成；合入及适用发布要求以keel-release为准。

## 实现检查点（2026-11-08）

- 已接入类型构造注册、资源冻结、受限Schema及字段选择、计划/模板/检查点校验，共享许可与检查结果内容接纳；相关Goal入口尚未接入运行时。
- 已接入Store v2原子迁移与旧Flow物理账本；queued保留原RunId/epoch/次数，started未确认完整保留占槽；启动事务要求先预留。
- 新增21项Unit，覆盖注册、计划、检查、许可、迁移及占槽；本轮 `cargo test --locked --offline` 155通过、5项原有真实环境用例ignored。仅常规套件证据，不计真实Agent/E2E达标。
- L2 v0.10.4具体化原有产物字段selector的登记位置（可选artifact_selectors），节点成员形状上限对齐既有32安全边界；原18项验收不变。
- 尚待：Goal事务/预算与协调器、共享Managed Run/Dispatcher完整接线、CLI/客户端、两个业务/真实Agent、功能地图、独立review及发布。当前dead_code警告来自尚未接线部分，不以此候选进入最终Clippy/review门。

## 实现检查点二（2026-11-08）

- Goal专用状态/CAS/回执/预算表已落地，工具和字节准入独立事务同时更新Run剩余预留与Goal总额；取消后仍按原Run身份接受停止/消耗事实。新增12项Unit（预算4、时钟2、Goal事务6）。
- `ManagedBridge/ManagedRunOwner/execute_managed`已从旧Flow抽出并由Flow实际复用；Schema、工具、候选、附件、Trace及原生超时/停止不复制。停止查询有15秒超时及panic边界。
- 常规完整套件167通过、5项原有环境用例ignored；原始日志：`target/verify-taskloop/10-dev/foundation-tests.log`。此处仍不是Goal执行E2E或独立review结论。
- L2 v0.10.5补全Planner固定planning_feedback输入；资源示例及字节摘要同步，既有离线语义例8项反例仍通过。EvaluationReceipt仅复制目标身份摘要，目标正文保留在有界CheckInput/内容寻址blob中。
- Goal记录/检查输入/预算类型已定义，但Goal协调器、共享Dispatcher完整接线和外部候选解码前限额尚未完成；CLI/两个真实业务/真实Agent/最终review与release继续保持待办。
