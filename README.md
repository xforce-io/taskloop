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

### 统一 Trace（显式启用）

`Runtime::open_with_trace(options, definitions, bindings, TraceOptions { max_events_per_run, max_bytes_per_run, source_max_events, source_max_bytes })` 启用独立观测投影；四项容量均由宿主提供，事件为 2..100000、字节为 4096..67108864。普通 `open` 保持不采集。

通过 `runtime.trace(&execution_id, TraceQuery { run_id: None, after: 0, limit: 100 }, &caller)` 读取；保存 `next` 续读，空页保持游标。`export_trace` 导出相同过滤和授权下的一页 JSON，逐页可导出全部已保留记录。查询和导出都按实例 owner 授权，不直接读存储。

`state.sqlite` 仍是业务事实；同目录 `trace.sqlite` 是版本化观测。每 Run 一个 trace，按实例可聚合多 Run。接收游标不表示跨进程发生顺序；`source_sequence`、来源单调时钟、原生标识和 Milkie 映射保留各自语义。`complete`、`issues`、`sources`、`open_spans` 明确描述缺号、冲突、未结束和能力不可见；最近活动来自已观察事件，不宣称模型仍在运行。观测异常不改变业务结果。默认记录的元数据不含提示或工具正文；累计 usage 保留原 scope，不当成本 Run 增量，也不将未知用量填零。

详见 [#7 契约](docs/design/7-unified-trace-query-export.md)。当前统一治理扩展由 #8 跟踪；这里没有自动导出到远端或自动删除记录。

### Trace 内容与保留治理

新应用使用 `Runtime::open_with_trace_policy(..., TraceOptions, TracePolicy)`。策略必须显式指定 `policy_id`、全局事件/逻辑字节/Run 限额、`retention_ms`、单内容引用限额；`content_tools` 为空即摘要模式。查询返回当前策略和每 Run 的固定策略，兼容入口显示 `legacy-metadata-manual`，不会伪称已配置全局保留。

显式内容模式须列出工具及输入/输出 JSON Pointer，并提供版本化 `TraceContentFilter`。只把选定字段传给过滤器；TaskLoop 随后再次屏蔽敏感键。过滤器负责业务特有秘密；默认不保存正文，摘要 hash 也不是匿名化保证。引用通过 `read_trace_content(execution_id, content_id, caller)` 读取，跨实例或 owner 返回 `not_found`，导出不内联正文。

容量拒收、过滤失败和超时可查，业务继续；过滤并行最多 8、单次等待 200ms。按接纳时间到期，重启不延长；维护、查询和写入都会检查，后台也按现有调度周期清理。清理只删除观测事件和内容；有界过期标记被淘汰后查询保留“未知缺口”，晚到事件不重新接纳。逻辑字节配额不等于 SQLite 文件大小，文件空闲页可复用；清理不承诺磁盘安全擦除。

[详细契约 #8](docs/design/8-trace-policy-data-governance.md)；观测库升级为 v2，业务库版本保持不变。

当前 #8 中央治理候选尚缺 Milkie 源端观测缓冲的公开清理链路，不能据中央库清理宣称全链路保留期限已完成；见设计 §12 和 S2.A3。
