# 经 Milkie 执行阶段

设计与细化验收：[6-milkie-executor-lifecycle v0.3](../../../../docs/design/6-milkie-executor-lifecycle.md) §11。S1/S2 均以本功能为主入口，无图形页面或新 CLI。

- S1.A1：固定 Milkie 与两种真实 Harness，宿主准备专用目录；HostBindings 注册 MilkieExecutor，Runtime.start 触发工具与 artifact.write，分别经通过/拒绝 gate，Runtime.get/read_artifact 核对结果。固定 Codex0.166.0/gpt-6-astra/high 与 Pi1.2.0/openai-codex/gpt-5.6-sol/high；以每轮真实证据判定结果。
- S1.A2：同一 SDK 配合进程协议夹具，核对版本能力、授权映射、Schema与输入/输出、重复和错误关联；不能把工具效果或聊天当接纳。
- S2.A1：两任务分别注入取消、超时、worker/Harness退出，核对 agent 停止事实、业务终态与另一任务不被误停；保留原产物。worker 崩溃的受信停止证明缺失时应 blocked/stop_unconfirmed，shutdown 同样拒绝；不可把这个预期错误误作假成功。按本次证据记录，不用整组探针进程退出码替代每项契约判断。
- S2.A2：丢弃 execute future 后用 stop_or_confirm 核对；未知不计确认、恢复不重新start；过期工具必须被原Bridge拒绝。
- S2.A3：运行 `cargo +1.91.1 test --locked`、`cargo +1.91.1 clippy --locked --all-targets -- -D warnings`、`cargo +1.91.1 run --locked --manifest-path tests/consumer/Cargo.toml`，保全旧执行器和公开消费者。

每次证据注明实现/设计/依赖SHA和命令；模拟路径只证明协议、所有权与业务边界，不替代真实 Harness。

确定性进程路径：`cargo test --locked --test milkie`。跨仓库路径：先构建固定 Milkie 版本，再显式提供 MILKIE_NODE、MILKIE_CLI，执行 `cargo test --locked --test milkie_actual_sidecar -- --ignored`。后者使用真实 Milkie worker 与工具分发器，但原生模型协议是本仓库夹具，不能替代 S1.A1/S2.A1 的真实模型验收。


## Issue #23：显式原生搜索与回执

[设计v0.2](../../../../docs/design/23-milkie-native-search-mode.md)；本功能为S1/S2主要入口，无新CLI或页面。

- S1：通过MilkieConfig::web_search选择Disabled/Cached/Live，经公开Executor执行。默认/Disabled不发送新约束且身份相同；Cached/Live约束及身份不同。缺少匹配能力时，在context.create前失败capability_denied，不能回退或启动模型。
- S2：通过MilkieExecutor::native_web_searches读取本次已结束运行，核对run/context/mode、complete及不透明结果；重开适配器仍能读取。禁用、未知运行、错模式、错context必须拒绝。原工具调用、停止与观测计量保持独立。
- 确定性协议：`cargo test --locked --test milkie native_search -- --nocapture`；完整回归与公开消费者按上文执行。
- 真实新版Milkie不支持的运行时：显式MILKIE_NODE/MILKIE_CLI，运行`cargo test --locked --test milkie_actual_sidecar real_milkie_refuses_native_search_before_context_on_unsupported_runtime -- --ignored --nocapture`，确认前置拒绝且无产物。该路径不消费模型，不能替代真实原生搜索。
- 真实原生搜索来源：Milkie #283固定Codex0.166.0/Sol/high的一次登记实跑，Succeeded/Stopped，23条结构化结果。Inquiry消费真实回执、独立抓取及研究质量由Inquiry #7验收；本协议夹具不证明文章质量。
