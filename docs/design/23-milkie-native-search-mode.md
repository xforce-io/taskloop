# TaskLoop #23：显式透传Milkie原生搜索模式

版本v0.2；[Issue #23](https://github.com/xforce-io/taskloop/issues/23)。L1 reuse既有MilkieExecutor创建/执行/停止及调用方显式授权契约；用户在Inquiry端到端任务中明确要求调整Milkie原生搜索，透传是必要消费者适配。L2 write，本文件为唯一详细技术契约；不替代人审批准。

## 1 背景

Milkie #283的固定Codex0.166+Sol/high实跑返回23条结构化搜索结果。TaskLoop的MilkieConfig仅提供程序/连接/存储/环境配置，执行约束没有webSearch字段，阻碍调用方复用该原生能力。

## 2 名词解释

沿用[名词表](../glossary.md)。新增“原生搜索模式”以该表为准。

## 3 目标与非目标

目标：执行器绑定可显式选择Disabled/Cached/Live，只有协商支持后才透传。非目标：搜索服务、结果解释、来源保存、研究质量、原生CLI协议适配或修改通用编排状态机。

## 4 能力

### 4.1 UI/UX

N/A：没有新页面或命令，仅SDK绑定配置。调用方构造MilkieConfig→可选选择模式→绑定Executor→执行/查询/取消。默认未选择与Disabled保持原行为；Cached/Live缺少匹配能力则在context.create前拒绝，不能静默回退。

## 5 思路与折衷

采用绑定级强类型枚举，放弃任意JSON约束透传和读取全局Codex配置。调用方可将不同绑定用于研究与评价，框架不根据研究角色猜授权。原生搜索与宿主工具调用分别计量；本版本不声称可在请求前限制原生搜索次数。由适配器公开查询方法复用已有运行归属记录，经Milkie公开协议读取原生回执；应用不读取适配器的私有目录，不把来源正文塞进usage或Trace。

## 6 架构

MilkieConfig显式选择→执行器身份摘要→Sidecar能力握手→校验webSearch支持数组→context.create→execution.start约束→原有停止/效果核对及观测收尾。失败路径：模式不支持则capability_denied，尚未建立原生上下文；运行失败按既有规则保留，TaskLoop不判断搜索结果是否满足研究目标。

## 7 模块

仅milkie适配模块及公开重导出、原有协议/消费者测试。Milkie处理原生协议，Inquiry处理研究语义，核心DAG不变。

## 8 API/CLI

新增MilkieWebSearchMode枚举，默认Disabled，MilkieConfig::web_search(mode)显式选择。非Disabled模式必须在能力webSearch数组中找到对应cached/live；匹配后向execution.start.constraints.webSearch发送规范字符串。Disabled省略该字段，兼容没有新能力字段的旧Sidecar。非默认模式纳入Executor.identity的host_configuration_digest，不能与禁用/另一模式混用绑定身份。新增MilkieExecutor::native_web_searches(taskloop_run_id)，只读取本适配器已停止且Milkie确认成功的运行。先核对本地归属，再用execution.query核对原生run/context及显式模式，随后web_search.read返回回执。核对version/run/context/mode、complete布尔、最多64条和2MiB页；条目与结果保持opaque，完整性及可用来源由应用判断。不修改执行结果、不启动模型、不读原生私有文件。尚在运行返回source_busy；归属/模式/响应不符则拒绝；查询及进程关闭整体限20秒。无新CLI。

## 9 边界

不继承宿主环境中的原生搜索设置；不添加其它工具权限。时间、宿主工具预算、候选大小、原生停止和观测尾部规则不变。原生搜索不是宿主工具，不伪造工具回调或上游请求计量。

## 10 迁移/兼容/回滚

默认Disabled保持原有执行约束及身份摘要；旧消费者无需改动。启用新模式需要明确支持的Milkie构建；不支持即拒绝。回滚调用方需选择禁用并按正常停止保存记录，不在活动运行中变更绑定。

## 11 测试计划

S1：Integration覆盖默认/Disabled/Cached/Live实际约束、缺失能力前置拒绝、不同模式身份摘要不同。S2：Integration覆盖执行后及重开适配器读取、禁用/错模式/无归属/错context拒绝；通过真实新版Milkie Sidecar与确定性原生CLI夹具核对约束/宿主回调/停止；沿用现有Milkie适配与完整框架套件。真实原生联网由Milkie #283登记的独立结果证明，不再次消费模型。无图形用户入口，验证主要入口为tests/milkie.rs及tests/milkie_actual_sidecar.rs。

## 12 开放问题

调用方必须自行决定研究与评价的授权边界、通过适配器公开查询解释原生结果；这些不留给框架隐式处理。

## 13 关联

[Milkie #283](https://github.com/xforce-io/milkie/issues/283)、[Inquiry #7](https://github.com/xforce-io/inquiry/issues/7)、[TaskLoop交付整理 #20](https://github.com/xforce-io/taskloop/issues/20)。
