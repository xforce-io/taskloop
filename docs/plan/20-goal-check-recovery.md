# TaskLoop #20 实现与复现记录

## 依据与阶段

用户 2026-11-12 要求按现状评审建议端到端完成；[Issue #20](https://github.com/xforce-io/taskloop/issues/20) 承接 PR #19 两项保留 P2。基线 origin/main@8276788，工作分支 bugfix/20-goal-check-recovery。L1 复用 #10，L2 为 [v0.1](../design/20-goal-check-recovery.md)，S1/S2 映射功能地图 07-goal-acceptance-reliability.md。不改变 Inquiry #2 失败结论。

## 修改前证据（keel-how）

先只加入回归而未改生产逻辑，在本地临时数据库运行：

- `cargo test --locked --offline --lib checker_goal_inputs_must_be_declared_and_required -- --nocapture`：FAIL，optional 评价材料得到 Ok(RegistrationReceipt)，违反预期拒绝。初始试验复用原字段时先被 selector 拒绝，未复现目标；随后改为独立的额外评价字段，准确到达 checker 注册缺口。两次调查区分保留，不把前者当目标复现。
- `cargo test --locked --offline --lib acceptance_write_failure_is_settled_online_without_reexecution -- --nocapture`：实际触发 SQLite UPDATE 拒绝，consume_return 返回 injected acceptance failure；公开 Run 保持 Checking。恢复存储后维护仍为 Running，预期 Finished，FAIL。

## 实现

注册复用解析后的 Goal Schema，同时验证 properties 与 required。在线维护利用与结果接纳共享的协调锁，收束遗留 Checking；不重试规划、不重新接纳候选，公开 storage_error。已知活动时间按原账本结算，未知检查时间保持 incomplete 与 None，释放预留；首个已到期或取消意图仍由原规则保留。

## 开发检查

针对性注册回归通过；明确事务错误/幂等回归通过；公开 Runtime 自动调度测试在 Planner、Worker、最终检查三处分别通过。完整 `cargo test --locked --offline` 为 232 passed、0 failed、5 ignored；ignored 的真实模型/环境测试未执行，不宣称本修复验证了真实 Harness 故障。严格 Clippy 与 diff --check 通过。

冻结候选后的新跑、送审预检、独立审查和 CI 尚待执行；其原始证据保存于本工作树 `target/verify-taskloop/20-check-recovery/`，验收汇总必须绑定实际候选，不能直接用本节开发检查代替发布批准。仓库没有本次线上部署阶段。
