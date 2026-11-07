# 经 Milkie 执行阶段

设计与细化验收：[6-milkie-executor-lifecycle v0.1](../../../../docs/design/6-milkie-executor-lifecycle.md) §11。S1/S2 均以本功能为主入口，无图形页面或新 CLI。

- S1.A1：固定 Milkie 与两种真实 Harness，宿主准备专用目录；HostBindings 注册 MilkieExecutor，Runtime.start 触发工具与 artifact.write，分别经通过/拒绝 gate，Runtime.get/read_artifact 核对结果。真实服务尚待环境，保持 not_run。
- S1.A2：同一 SDK 配合进程协议夹具，核对版本能力、授权映射、Schema与输入/输出、重复和错误关联；不能把工具效果或聊天当接纳。
- S2.A1：两任务分别注入取消、超时、worker/Harness退出，核对 agent 停止事实、业务终态与另一任务不被误停；保留原产物。真实环境路径保持 not_run。
- S2.A2：丢弃 execute future 后用 stop_or_confirm 核对；未知不计确认、恢复不重新start；过期工具必须被原Bridge拒绝。
- S2.A3：运行 `cargo +1.91.1 test --locked`、`cargo +1.91.1 clippy --locked --all-targets -- -D warnings`、`cargo +1.91.1 run --locked --manifest-path tests/consumer/Cargo.toml`，保全旧执行器和公开消费者。

每次证据注明实现/设计/依赖SHA和命令；模拟路径只证明协议、所有权与业务边界，不替代真实 Harness。

确定性进程路径：`cargo test --locked --test milkie`。跨仓库路径：先构建固定 Milkie 版本，再显式提供 MILKIE_NODE、MILKIE_CLI，执行 `cargo test --locked --test milkie_actual_sidecar -- --ignored`。后者使用真实 Milkie worker 与工具分发器，但原生模型协议是本仓库夹具，不能替代 S1.A1/S2.A1 的真实模型验收。
