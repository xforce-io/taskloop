# 执行预算与受控收尾

主要覆盖 #9 S1/S2；标准为 `docs/design/9-explicit-execution-budget-finalization.md` v0.1 §11。

公开入口：RuntimeOptions.entry_budgets、Runtime::budgets、Invocation、ToolBridge、MilkieExecutor。

- S1.A1：请求/Grant/阶段/适配器来源交集；兼容值透明，显式预算超过240秒的可控时钟执行路径通过；运行中、结束后、重开与越权。
- S1.A2：工具、时间、容量、模型迭代分别触发，不支持迭代在 native start 前拒绝；真实 Sidecar+确定性Harness 验证跨语言，不能当真实模型成功。
- S2.A1：探索耗尽、并发和失败均计量；保留2次提交，总量/Schema/权限/gate不突破。
- S2.A2：时间余量到界拒绝探索；硬截止、取消、晚到、提交失败真实保留，回归现有执行和观测路径。

冻结候选逐项写证据；缺必需环境不记 skip，不重跑未经授权的研究质量实验。

## 驾驶与判定

- `cargo test --locked --offline --test budget`：公开 SDK 验证来源交集、运行中、越权、重开、兼容值、并发/失败/保留、候选字节、时间余量、硬截止和同步 gate 超时。可控时钟的250秒执行/130秒检查仅证明 Runtime 期限机制，不代表真实模型耗时。
- `MILKIE_NODE=<固定绝对路径> MILKIE_CLI=<Milkie 固定候选 dist/cli/index.js> cargo test --locked --offline --test milkie_actual_sidecar -- --ignored`：实际 Sidecar 完成合法提交/取消；迭代用确定性 Pi API 执行实际生成的钩子，核对2次请求、0次宿主工具、model_iteration_limit。原生 Pi、模型服务和真实账号在 Epic 另验，不能替代。
- `cargo run --locked --offline --manifest-path tests/consumer/Cargo.toml`：独立 crate 仅通过公共 API 配置并读取预算、产物和 trace。
- 回归执行、存储错误、停止未知和 trace：`cargo test --locked --offline`。不运行 ignored 真实服务测试也不能将其记为已通过。
