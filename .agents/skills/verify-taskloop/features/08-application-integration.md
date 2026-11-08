# 真实应用与第二业务集成（Issue #10）

S6的主要功能文件。标准为 `docs/design/10-goal-dag-orchestration.md` §11；Inquiry文章质量仍归Inquiry #7，不能用TaskLoop协议测试代替研究能力验收。

| 验收 | 真实入口、结果及证据 |
|---|---|
| S6.A1 | 固定Inquiry、TaskLoop、Milkie及Harness版本，应用从公开Goal SDK注册研究能力/Schema/规则并提交目标。真实Agent规划、工具获取、分析、写作及领域评价形成闭环；从最终文章引用的opaque产物ID+SHA回溯保存快照段落和精确引文。记录真实调用、原始产物、领域检查结果及Runtime接纳事实。 |
| S6.A2 | 在预先固定的领域规则和预算下运行材料不支持目标的任务；真实评价返回fail，Runtime为not_met，保留文章与缺口，不能自动返工、新建Goal或失败后降低规则。 |
| S6.A3 | 用同一TaskLoop候选，替换能力、Schema、Skill和规则执行第二类最小业务（可使用内置document-review）；公开产物与评价达成闭环，核心不得包含研究专用分支。 |

进入前确认模型已登录、Milkie固定路径/构建可用及实际执行预算；只使用宿主配置的凭据，不复制或输出登录材料。先跑确定性契约与受影响消费者检查，再运行真实Agent，不以反复研究调用排查基础协议错误。

Inquiry消费者须固定源码SHA，通过公开API集成并复用真实领域工具/引用检查；可使用隔离集成crate或应用分支，不改动用户当前工作树。若使用用户指定URL，发现依据明确记user-input，不冒充真实搜索。

原始证据至少包含：候选与依赖SHA、配置/规则摘要、目标和输入出处、Goal/Plan/Run公开视图、工具事实、原文与文章Artifact引用、逐条引用回溯、Evaluation与Receipt、预算和Trace完整性。Trace降级如实记录，不能补造agent/tool事件。

真实服务未运行、只跑受控协议夹具或只验证离线Schema时，本节验收保持not_run/blocked；不得记skip或pass。真实集成命令在应用消费者可运行后登记，不提供当前不存在的命令。


## 隔离消费者入口

固定Inquiry领域库的消费者见 `tests/inquiry_consumer/README.md`，依赖SHA及Cargo.lock随源码冻结。按该文件显式准备真实Milkie/Grok环境，在仓库根执行：

```sh
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- check /absolute/output/preflight
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- positive /absolute/output/positive
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- negative /absolute/output/negative
cargo run --locked --offline --manifest-path tests/inquiry_consumer/Cargo.toml -- document /absolute/output/document
```

check只验证注册，不计S6通过。positive需核对article.md、citation-backtrace.json、原文/快照及公开Evaluation；negative需核对not_met与保留的文章/缺口；document需证明替换Profile后真实执行同一核心。保留失败目录和真实Trace降级，不在失败后修改规则或配额来改判原任务。
