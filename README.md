# TaskLoop

以 Skill 方法和业务流程驱动、封装任务执行与可靠性的轻量 Rust crate，复用第三方 Agent Harness。

一期应用只通过 start/get/signal/cancel/read_artifact 使用实例和产物；流程、研究等领域语义属于应用。内部状态、Worker、存储和恢复不成为应用依赖。

- [最小执行契约 Issue #2](https://github.com/xforce-io/taskloop/issues/2)
- [Issue #2 L2 v0.1.0](docs/design/2-minimal-runtime-contract.md)：前置设计基线，Draft。
- [设计见证与机器契约](docs/design/2-minimal-runtime-contract/contract/README.md)
- [历史总体 RFC](docs/RFC-0001-architecture.md)：更宽的架构方向，Python/CLI/动态规划等未作为一期依赖。
- [名词表](docs/glossary.md)

当前有可构建的 Rust 0.1.0 开发版本，尚未冻结验收、独立审查或正式发布。

- [实现 Issue #3](https://github.com/xforce-io/taskloop/issues/3)
- [实际集成契约与验收](docs/design/3-minimal-runtime-mvp.md)
- [当前机器格式](schemas/README.md)

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

真实 Agent 测试需先安装并登录 Codex CLI 0.166.0；固定实际模型 gpt-6.1-sol，使用真实账户额度，不在普通测试或 CI 中静默调用：

```sh
cargo test --locked --test codex_live -- --ignored --nocapture --test-threads=1
```

首版运行在 Unix。宿主使用自己的私有存储目录；TaskLoop 创建 0700 新目录，拒绝打开权限过宽的已有目录。业务输入不承载身份，Caller 由可信宿主构造。


### Milkie 执行接入（#6 候选）

`MilkieExecutor` 经 Milkie Sidecar 接入原生 Harness，TaskLoop 继续拥有工具授权、候选接纳与业务状态。宿主提供固定的 Milkie CLI、连接文件、专用原生存储和执行数据根；不复制登录材料、不自动回退 CodexExecutor。

```rust,ignore
let config = taskloop::MilkieConfig::new(
    "/absolute/node", "/absolute/connection.json", "/absolute/milkie-runs",
    taskloop::MilkieStorage {
        config_dir: "/absolute/dedicated-config".into(),
        session_dir: "/absolute/dedicated-sessions".into(),
    },
)?.args(["/absolute/installed-milkie/dist/cli/index.js"]);
let executor = std::sync::Arc::new(taskloop::MilkieExecutor::new(config)?);
let bindings = taskloop::HostBindings::default().executor("research.agent", executor);
```

Milkie 依赖固定于上游 #277/#278 的契约与提交，当前兼容补充候选为60b08f1；从该提交构建/打包并记录宿主配置版本，不能把旧 registry 0.1.1 当作已包含 Sidecar。公开协议与能力在执行前核对。详情见 [设计](docs/design/6-milkie-executor-lifecycle.md) 和 [验收路径](.agents/skills/verify-taskloop/features/01-milkie-execution.md)。原生模型成功验收仍待补齐。
