# 固定 Inquiry 领域库的 Goal 集成消费者

本 crate 只用公开 TaskLoop API，固定 Inquiry `d4b05750a60b2158b325b468d51b53d0c2dca3ca`，通过 Cargo patch 将其 TaskLoop 依赖统一指向本仓库候选。它是 #10 S6 的可运行应用消费者，不是 Inquiry Web 迁移，也不代替 Inquiry #7 深度文章质量验收。

执行前显式提供：

```sh
export INQUIRY_SOURCE_ROOT=/absolute/path/to/clean/pinned/inquiry
export MILKIE_NODE=/absolute/path/to/node
export MILKIE_CLI=/absolute/path/to/pinned/milkie/dist/cli/index.js
export GROK_CONFIG_DIR=/absolute/path/to/logged-in/grok/config
export GROK_SESSION_DIR=/absolute/path/to/the/same/config/sessions
export INTEGRATION_MODEL=grok-4.6
```

不复制或输出凭据。Grok配置和会话目录由宿主指定，Milkie负责原生会话；只对本消费者创建的Run操作。按环境需要给子进程设置代理。不能在未登录或使用协议夹具时宣称真实模型通过。

在仓库根目录执行：

```sh
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- check /absolute/output/preflight
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- positive /absolute/output/positive
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- negative /absolute/output/negative
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- document /absolute/output/document
```

首次缺锁定依赖时可先 `cargo fetch --locked --manifest-path tests/inquiry_consumer/Cargo.toml`；不升级依赖来掩盖失败。`check`只核验两个注册目录、固定资源与依赖，不启动模型或抓网页。其余命令会调用真实模型；预算、规则和问题在调用前固定，不能失败后放宽。输出目录用于一个任务，失败证据保留，重新实验用新目录。

- positive：真实Planner生成受检DAG，应用使用用户明确指定的两份SQLite官方URL，经原Inquiry Fetch取得原文/快照，真实Agent分析、写作及独立语义评价。Rust checker调用原Inquiry分析/报告域校验，检查精确引用、分析血缘与引文上限。要求achieved。
- negative：相同资料无法支持特定Palantir业务数值。原规则不变，要求not_met；必须保留文章和缺口，不自动返工。
- document：替换为核心随包编译的document-review工厂，真实Planner、审阅Worker和质量检查Run；要求achieved，无Inquiry研究节点。

候选保存来源选择为`user-input`，不冒充搜索。应用适配器仅为已授权Artifact提供旧领域代码所需的state/slot视图，ID、SHA及内容不改；不访问私表、不补查隐藏证据。原始网页以base64 JSON包装通过公共端口传递，导出时校验原文SHA并还原HTML；规范化快照仍是原Fetch输出，引用只指向其确切公共产物版本。

证据目录保留目标、依赖/CLI摘要、固定Profile摘要、公开快照、产物、逐条引用回溯、评价、预算和Trace分页。最后一页为空表示分页结束；complete/issues按Trace原样记录，不能因为业务成功补造完整观测。程序退出0仅在本模式预期结论及回溯检查满足后发生，仍需验收者核对原文和领域结论。

开发检查：

```sh
cargo clippy --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml --all-targets -- -D warnings
```
