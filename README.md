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
