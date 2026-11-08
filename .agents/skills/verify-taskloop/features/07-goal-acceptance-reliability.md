# Goal 评价、预算与可靠性（Issue #10）

S4–S5的主要功能文件。标准为 `docs/design/10-goal-dag-orchestration.md` §6.5–6.11及§11；不得把业务达标、原生停止和Trace完整性合成一个成功标志。

公开入口：Goals/GoalClient的状态、产物、评价、预算、Trace及控制命令；故障注入可使用受控Executor/独立CLI进程，结果从公开接口核对。

| 验收 | 驾驶路径与判定 |
|---|---|
| S4.A1 | plan、checkpoint、goal_final的pass/fail/error分别执行；data评价的fail为数据，不充当必需评价。checkpoint fail接纳评价与not_met意图须原子，error无公共成功输出；全部必需pass才achieved。 |
| S4.A2 | 过期、伪造身份/证据、检查不可用及工作成功但目标fail分别触发；不得成功，保留候选及已接纳文章/缺口。checker只能读取本请求target与允许字段。 |
| S4.A3 | 改动候选摘要、Spec/Profile/Plan/规则版本及依赖引用；拒绝失效事实，不因作者自报pass授予提交权。 |
| S5.A1 | submit/signal/stop/cancel同键重放及同键异参；迟到输出和重复结算；目标、Run、公共输出、回执及容量只生效一次。 |
| S5.A2 | 在Planner、业务及检查中取消/暂停/杀宿主；§6.9每个崩溃窗口分别注入。已启动未知不重跑且保留占槽，未启动挂起只在显式resume首次启动；pause中的既有检查fail/error不能被掩盖。保留已接纳产物。 |
| S5.A3 | 分别耗尽墙钟、规划、活动时间、Run、转移、工具及候选容量；同时争最后配额/共享Flow与Goal槽位。两个owner不能混读，未知停止占槽不阻挡另一Goal自己的到期事务。只用公开预算/progress/Trace诊断。 |

候选入口：

```sh
cargo test --locked --offline --test goal_cli
cargo test --locked --offline --test goal_sdk
cargo test --locked --offline orchestration::core::tests
cargo test --locked --offline store::goals::tests
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
```

重点核对 `evaluation_nodes_distinguish_checkpoint_fail_data_fail_and_check_errors`、`pause_accepts_started_checkpoint_results_without_hiding_fail_or_error`、`killed_hosts_reconcile_started_runs_without_replaying_any_business_stage`、`an_unknown_run_keeps_its_slot_without_blocking_another_queued_goals_deadline`。测试名只指向部分边界，不代替表中全部故障窗口与额度覆盖。

每项保存候选SHA、冻结配置、命令、公开快照、Run数、用量/停止事实及原始日志。独立CLI被SIGKILL只能证明真实进程恢复；协议夹具不能证明某个真实Harness已停止。

计划阶段另执行`plan_reviews_distinguish_pass_revisable_fail_and_unavailable_error_before_business`、`planning_attempts_share_goal_run_and_tool_limits_instead_of_resetting_them`与`expired_planning_deadline_stops_pending_retry_without_resetting_the_original_window`。共享宿主路径执行`legacy_flow_and_goal_share_one_physical_slot_until_the_flow_confirms_stop`，核对Flow占槽期间Goal的公开pending/占槽/Run数，确认停止后才准入，实测并发峰值为1。非法成员提交执行`schema_invalid_member_submission_is_rejected_before_public_join_or_final_check`，核对拒写仍计工具调用、候选字节为0且不产生公共汇合/最终检查。

时钟恢复执行`clock_blocking_is_idempotent_and_requires_explicit_resume_after_trust_returns`：注入未来高水位，拒绝业务准入和resume；被拒命令回执同键重放不增revision；重复recover无新事实不变；恢复可信后保持stopped、原期限和0Run，显式resume后才恢复运行资格。该用例注入Core时钟，不伪称真实系统时钟调整或宿主SIGKILL。

暂停中的Planner结果执行`paused_planner_results_preserve_valid_invalid_and_input_branches_until_explicit_resume`：在原生已返回、业务尚未接纳处暂停，分别保存合法计划、结构化非法原因或等待身份；重复消费不重算，stopped期间零后续请求/派发，resume后才锁定/补建修正请求/接受补充，Spec与已计Run不变。

进一步定位：

- `a_previous_goals_passing_evaluation_cannot_approve_new_public_artifacts`：复用原通过候选的相同字节，因目标Artifact不同而拒绝；新Goal工作仍成功、公共产物保留，但无通过回执。
- `changed_rules_cannot_resume_an_old_goal_or_reinterpret_its_locked_contract`：干净暂停并重开后改变规则版本，旧Spec/Profile/Plan/等待身份保持不变，拒绝resume且0新调用；同键同参回旧Goal、异参拒绝。
- `required_checks_keep_partial_pass_pause_and_late_fail_fast_results_distinct`：两必需检查先部分pass不成功；pause中第二pass可接纳但只stopped，resume才achieved；若先fail，迟到pass仅诊断、无第二Receipt，重复消费不增revision。
- `concurrent_run_reservations_cannot_spend_the_same_last_goal_budget`：同一Store的两份旧快照争最后工具/时间/候选预留，只有一个CAS提交；公开预算/占槽不重复，输家重读后pending。此例证明预留事务原子性，不冒称两个原生Worker执行。
- `recovery_before_first_reservation_requires_resume_and_does_not_invent_a_run`：预留前恢复保持0Run、0占槽及原期限，重复恢复幂等；显式resume后仅首次建立1个Run。
- `orphaned_blob_bytes_do_not_become_public_goal_artifacts_after_store_reopen`：只写blob未提交接纳，重开后仍无公共Artifact/成功结论，不能拿SHA经公开Artifact入口读取。

期限、停止及额度补充路径：

- `input_deadlines_expire_online_or_after_reopen_without_accepting_a_late_signal`：在线及离线跨等待期限分别失败为input_timeout，依赖明确失败、零公共产物且预留/占槽归零；旧nonce即使用当前revision补充也拒绝，无新Run。
- `a_wait_deadline_is_checked_at_admission_without_a_timer_transition`：在等待期限到达瞬间直接验证signal准入拒绝；不依赖定时事件先出队。
- `planner_review_workers_and_final_checks_obey_declared_tool_byte_and_time_budgets`：Planner、计划检查、业务成员、最终检查分别耗尽工具次数、候选字节、执行时间，共12组合；从公开RunBudget确认对应limits_hit，原Spec不变、预留/占槽清零。使用仅拥有Rust Future的受控Executor，不能当真实Harness停止证据。
- `cli_online_cancel_returns_an_intent_then_closes_without_erasing_the_accepted_report`：Planner、业务、最终检查三阶段分别在线取消；实际CLI退出7、Run数不再增加、槽位清零、Socket关闭，最终检查阶段保留已接纳文章；离线控制退出6。
- `failed_output_insertion_rolls_back_goal_budget_and_revision`：先写有效blob，测试库触发器在goal_artifacts的真实INSERT上注入故障；必须观察到注入错误，Goal状态/revision/账本/公共产物均回滚。保留的孤立blob不成为已接纳Artifact。
- `a_projection_panic_fails_the_node_without_poisoning_coordination_or_losing_members`：实际推进到节点checking后，以仅测试编译的线程局部钩子注入内建投影panic。必须失败为contract_error，保留成员产物、零公共输出/评价、不增加Run，预算/槽位归零，协调器仍可调用且重复检查不再推进；不把该注入描述为已发现自然可触发的业务输入。

版本重开路径使用明确的60秒未到期前置，并断言deadline_expired=false；等待过期由独立200毫秒用例验证。两条路径分别证明版本冻结与期限，不互相替代。
