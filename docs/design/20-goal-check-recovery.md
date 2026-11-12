# TaskLoop #20：评价材料与接纳失败恢复

版本 v0.1。关联 [Issue #20](https://github.com/xforce-io/taskloop/issues/20)、[PR #19](https://github.com/xforce-io/taskloop/pull/19)。

L1: reuse：沿用 #10 v0.10.17 的必要评价输入、存储错误不能变成功、未知工作不重跑与预算收束契约；用户 2026-11-12 授权端到端处理评审建议。没有新增界面或改变已确认业务路径。L2: write：本文件明确注册和在线恢复契约。Issue S1/S2 分别映射原 S4.A2/S5.A2，功能地图为 07-goal-acceptance-reliability.md。

## 1 背景

在 8276788 基线上增加的两条回归分别观察到：可选评价字段成功注册；接纳事务写失败后恢复存储，在线 maintain 仍保持 Running/Checking。两项均是 PR #19 保留 P2 的实际复现，不是新能力。

## 2 名词解释

沿用 [名词表](../glossary.md)，无新增术语。

## 3 目标与非目标

目标为必要评价材料不能静默遗漏、接纳失败在原宿主明确终结且不重跑。非目标为重做评价、自动重试模型、扩展规划或修改 Inquiry 质量标准。

## 4 能力

修复开发者注册与公开状态查询路径。4.1 UI/UX：N/A，本次无新页面或 CLI。

## 5 思路与折衷

注册时要求 checker 的每个 goal_input_fields 同时属于 Goal Schema 的 properties 与 required，复用现有 Schema 引用解析。选择提前拒绝错误配置，放弃靠运行时缺字段再失败。

接纳失败选择失败关闭，不重新验证或接纳已失去完整提交事实的候选。候选仍可诊断，代价是该次研究须明确失败；不拿存储恢复当作业务成功，也不隐式再跑 Planner。

## 6 架构

注册层只处理声明边界；Runtime 协调器管理持久接纳和收束；Store 保持事务与状态不变量。

consume_return 在同一 coordinator 锁内完成 Checking→检查→接纳。maintain 也持同一锁，因此它能观察到的 Checking Run 已无正在进行的 consume_return；该状态表示未提交接纳（事务失败或检查中断）。在存储恢复后，维护将其标记 storage_error，结束业务权限、结算已知活动时间、释放预留并保留候选；未知检查时间继续标记 incomplete，check_elapsed_ms 保持 None。其他仍在运行的原生工作按既有终结清理确认停止后才完成 Goal。已有到期/取消终结意图保持不变。

重开恢复继续沿用原 interrupted 路径，不修改已结束 Goal。维护重复执行不增加 revision、Run 或账单。

## 7 模块

registry 负责字段完整性；lifecycle 负责在线检查中断收束；coordinator 保持原检查与事务边界。无新通用抽象。

## 8 API/CLI

没有新接口。无效字段仍返回 input_invalid，路径 /checkers/goal_input_fields。在线接纳中断从公开 Goal/Run error 可见 storage_error，候选不成为公共成功产物。

## 9 边界

不接纳孤立 blob、候选或旧评价；不吞掉实际存储错误。维护自身写失败时保留原记录，后续维护可重试收束，业务不能重跑。存储正常时正在进行的检查受同一协调锁保护，不能被误判。

## 10 迁移/兼容/回滚

无存储格式变更。已有合法注册与旧 Flow 不变；声明可选评价材料的无效注册需改为必填。回滚代码不回写失败结果；历史候选及状态仍保持原事实。

## 11 测试计划

| 验收 | 入口与可判定结果 | 层级 |
|---|---|---|
| S1 | 注册可选、未声明、引用到可选对象及合法必填 Schema；前三者拒绝，后者成功 | Integration |
| S2 | SQLite 触发器阻止 Checking 后接纳事务，确认真实错误；删除触发器后同宿主维护，公开状态 failed、Run settled、无额外执行或接纳、预留释放、未知检查时间不补零；重复维护幂等 | Integration |
| 保全 | 原崩溃恢复、暂停、期限、检查、Goal SDK、旧 Flow、完整 cargo test 与严格 Clippy | Integration/Unit |

S2 还须通过在线 Runtime 调度入口证明维护会自动发生。受控 Executor 证明状态与事务，不冒充真实 Harness 故障；本修复无需模型生成质量验收。冻结候选结果另存验证表。

## 12 开放问题

N/A：本次为既有契约修复，不引入新产品选择。

## 13 关联

- [#10 设计](10-goal-dag-orchestration.md)
- [Inquiry #7](https://github.com/xforce-io/inquiry/issues/7)
