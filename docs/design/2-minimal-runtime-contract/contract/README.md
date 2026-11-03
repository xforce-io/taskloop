# TaskLoop 最小执行契约：机器格式与设计见证

唯一 L2：[最小执行契约](../../2-minimal-runtime-contract.md)。本目录不是运行时，也不是应用依赖包；应用固定未来经验证的 Rust crate，不 import 本目录 Python。

本目录保留 Issue #2 的冻结设计见证。当前运行时的机器格式位于根目录 [schemas](../../../../schemas/README.md)，实际接入见 [Issue #3 L2](../../3-minimal-runtime-mvp.md)。以下为旧见证使用的格式：

| 文件 | 责任 |
|---|---|
| bundle.schema.json | Playbook/EntryContract 的机器结构 |
| start-request.schema.json、snapshot.schema.json | 实例请求/公开视图；视图包含业务输入和阶段定位，应用不查内部 Run |
| signal-request.schema.json、cancel-request.schema.json | 业务信号/取消 |
| task-input.schema.json | 内建任务输入格式；业务包引用保留名称，不复制文件 |
| check-result.schema.json | 检查返回 |
| submission.schema.json | 内部适配器候选关联，应用不使用 |
| tool-receipt.schema.json | 框架工具事实信封，领域数据在 payload |

其它 JSON、skills/、registry.example.json 是研究和审阅 fixture，冻结来源为 Inquiry 提交 584eaa80dd899f3b181b66d4f68ace3309de290d。receipt.schema.json 是 fixture 的 HTTP 获取事实 Schema，不是框架协议。生产研究资源以 Inquiry 为准；TaskLoop 的公开契约不引用 Inquiry 仓库，不内置研究状态名或 HTTP 字段。fixture gate/工具回执只提供固定字节的关系检查，不能证明实际研究质量。

verify_contract.py、requirements.txt、verification-result.json、trace.examples.json 是设计见证。模型支持 work/wait/terminal、幂等/迟到、取消/信号、多个独立实例及最小 SQLite CAS；新快照/通用回执案例只验证 Schema。recheck 为后续扩展约束见证，不是一期 API。真实 Rust 构建、Harness 方法挂载、输入可见性、进程/网络隔离、完整事务与实际取消/恢复均 not_run。

复现（独立临时环境，Python 仅用于验证设计）：

```sh
python3 -m venv work/contract-check-env
work/contract-check-env/bin/python -m pip install -r docs/design/2-minimal-runtime-contract/contract/requirements.txt
work/contract-check-env/bin/python docs/design/2-minimal-runtime-contract/contract/verify_contract.py
```

测试不安装本目录 Skill，也不启动 Agent/网络。产物记录源码/依赖摘要，不能据案例数量宣称产品已实现或架构获批。
