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

## 交付说明与测试时序收尾（2026-11-13）

README仍仅称P2由#20跟踪，并将已由PR#12合入且通过两真实Harness验收的Milkie接入标为候选/待验；#10设计首页仍写实现中、未完成验收。依据PR#12/#19/#21的合入记录更新交付状态，保留原设计版本和产品确认来源，不自批新设计、不重标历史验收。平台#6–#9残留的待人工批准/尚未合入说明亦已按对应已合入PR核对并收敛；#10的两项保留P2注明后来由PR#21修复。

文档初始候选cb39dac独立审查PASS/human optional；PR#22最初两平台CI通过，但同SHA分支首次Ubuntu失败于时钟恢复测试revision4 !=2。该测试把不可信高水位设为真实当前时间+400ms，却假定多次SQLite操作及recover仍在400ms内；临时增加450ms调度延迟在同断言复现，源码还原后原例通过。一次失败job重跑后来通过，原失败与所有尝试保留在target/verify-taskloop/20-readme-followup/；撤销过时运行请求时平台已明确completed/success，并未实际取消。

本次消除该测试的短实时时序假设：测试自己的不可信阶段使用远未来高水位，完成幂等断言后显式切换为可信时钟；另保留独立的原生时钟追上高水位正例。生产时钟、恢复、注册、预算和公共接口均未改变。相同450ms扰动在新测试夹具下通过且源码逐字还原；冻结后还须重跑套件。L1/L2对本次文案与测试-only变更不新增契约，原#20 v0.1及#10行为保持。

因源文件变化，cb39dac的审查及CI仍归原候选，不能作为最终候选通过。PR#22已改为草稿，新候选在最终独立审查与当前CI通过前不合入；Inquiry的真实文章验收继续独立进行，不由这些测试代替。
