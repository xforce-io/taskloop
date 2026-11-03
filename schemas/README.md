# 当前机器格式

本目录是 Rust 定义加载和检查使用的 v1 JSON Schema。`bundle.schema.json` 描述 Playbook 与 EntryContract；`task-input.schema.json` 是保留内建输入，业务引用名称而不复制。

其余信封描述通用交换字段；首版 crate 提供 Rust 类型接口，没有 HTTP/JSON 服务。运行时行为与边界以 [Issue #3 L2](../docs/design/3-minimal-runtime-mvp.md) 为准。

`docs/design/2-minimal-runtime-contract/contract/` 的同名 Schema 是历史设计见证的冻结资源，不再作为当前格式的第二事实源。其 Python 检查只复现旧设计案例，不能替代当前 Rust 或真实 Agent 验收。
