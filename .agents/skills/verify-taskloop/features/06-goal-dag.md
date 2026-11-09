# Goal 提交、规划与 DAG 执行（Issue #10）

S1–S3的主要功能文件。事实源为 `docs/design/10-goal-dag-orchestration.md` §4、§6及§11的原18项验收；本文件定义验证路径，不记录候选已通过。

公开入口：`taskloop goal run/get/plan/signal/stop/resume/artifact`、`Runtime::open_orchestrated`、`Goals`、`GoalClient`。run/resume为前台宿主；无step操作。只读命令不得启动工作。

| 验收 | 驾驶路径与判定 |
|---|---|
| S1.A1 | 用可信HOST配置和GoalRequest启动前台CLI，分别取人工与NDJSON输出；核对目标身份、阶段、最终结论和产物语义一致。用户不写DAG、不查私表。受控协议对端只能证明CLI行为，真实Planner另按08执行。 |
| S1.A2 | 成功、未达标、检查错误、非法输入、等待及取消分别执行；核对退出码和公开status/outcome，错误保留原因，有产物不等于达标。 |
| S1.A3 | 前台等待时从另一客户端补充，暂停期间拒绝signal且保留nonce/期限；退出后只读查询、离线命令失败、显式resume。SIGINT/SIGTERM干净停止，SIGKILL后的已启动工作禁止重放。 |
| S2.A1 | autonomous与workflow分别由Planner提案，读取锁定Plan与执行记录；模板图骨架和权限不变。只测模板校验不算模板执行完成。 |
| S2.A2 | 对格式、环/自环、引用、输入连接、能力权限及预算/必需验收六类非法图逐项提交；核对位置/原因、业务Executor调用数0。 |
| S2.A3 | 受控Planner先非法后修正，核对新调用确实取得固定反馈；另耗尽次数、规划时间和调用额度，不能放宽原限制继续。 |
| S3.A1 | 依次执行顺序、独立分叉、全部汇合三种图；用独立Run开始结束及同时活动计数证明并发，不能仅凭图形或总耗时。 |
| S3.A2 | 单成员与多成员分别成功；输入不合法不得派发，成员失败/汇合不合法不得形成公共输出。多成员乱序完成仍按member_id确定性汇合。 |
| S3.A3 | 上游失败后后继dependency_failed；另一图让input节点等待、独立分支继续，核对等待无Worker Run且不占槽，补充或到期后可收束。 |

候选入口：

```sh
cargo test --locked --offline --test goal_cli
cargo test --locked --offline --test goal_sdk
cargo test --locked --offline --test goal_graphs
cargo test --locked --offline orchestration::tests
```

`goal_cli`运行真实CLI进程配合受控Milkie协议对端；`goal_sdk`为外部crate使用公开SDK及受控Executor。两者不启动真实模型，不能替代08的应用/Agent证据。上述命令没有覆盖到的表内路径须另取证，不能按整套命令成功自动汇总S行pass。

S2.A1真实模板入口：按`tests/inquiry_consumer/README.md`配置固定Milkie及真实Harness，执行消费者workflow模式；核对模板摘要由Runtime提供、提案严格匹配骨架及连接、独立质量检查通过。该路径与受控SDK模板测试分别记录，不以夹具替代真实Planner。

规划拒绝诊断须同时核对在线SDK与只读GoalClient的`runs[*].plan_validation_failure`，保留每次提案的分类及JSON Pointer；CLI人工输出应显示相同原因。定位：`an_invalid_plan_is_corrected_once_with_exact_feedback_before_business_dispatch`与`cli_preserves_reports_and_distinguishes_not_met_checker_error_and_planning_exhaustion`。

六类非法提案须经公开Goals提交后验证：`six_invalid_plan_categories_have_public_locations_and_zero_business_dispatch`逐类检查原始分类/位置、单Planner Run、业务节点与派发数0、槽位/预留释放。格式反例使用合法JSON但超过宿主节点容量的提案；字节格式/重复键等另由既有注册与验证器反例覆盖。

S3.A2的非法整体汇合另执行`individually_valid_member_outputs_cannot_accept_a_schema_invalid_join`：两成员各自Schema合法且均succeeded，联合对象违反整体Schema时节点failed，成员产物保留但无公共输出/最终检查。不能只用非法成员输出代替这一反例。

前台SDK的单目标选择另执行`review_regression_rejected_second_goal_resume_has_no_side_effects`：同一CliDriver已选择A后尝试恢复暂停的B，必须在提交B恢复之前拒绝；B的公开快照、revision、暂停状态和预算完全不变，驱动器仍选择A。
