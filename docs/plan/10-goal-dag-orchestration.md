# TaskLoop #10 实现计划

基线：[Issue #10](https://github.com/xforce-io/taskloop/issues/10)，L2 v0.10.3，设计提交 cbed510cd539f719d3302e1695eb69b2d15a904c；本机Grok审查PASS。L1复用原概要提案及v0.2的18项细验收，用户已明确授权“调整完文档，keel端到端完成一个版本开发测试和review”。不另加包加载器或step入口。

## 当前机制与复用依据

- `src/runtime.rs::Runtime::open_impl/worker_loop/enter`：v1固定Flow，串行等待phase；enter在排队时已建立未started的active并计Run次数。
- `src/store.rs::Store::open/commit`：所有权文件锁、v1 SQLite、IMMEDIATE revision CAS与内容寻址blob。
- `src/phase.rs::Bridge/run_checked`：工具、候选、计量与停止；目前直接耦合单active Record，需抽出共享执行责任与owner适配。
- `src/execution.rs::Executor/ToolBridge`、`src/milkie.rs`：沿用受控执行、原生停止/未知、观测尾部及显式artifact.write提交。
- 既有功能地图01–05和原套件为保全门；真实Sidecar/Harness默认ignored不算已验证。

## 工作顺序

1. 类型契约、冻结注册、受限Schema/selector、计划/模板校验及共享推进/检查判定；对应S2/S4。
2. Store v2原子迁移、Goal及共享run_leases、命令幂等与原子预算；旧Flow queued不占槽，保留原ID/epoch/已计次数；对应S5。
3. 提取共享Managed Run/Bridge，Flow与Goal经唯一Dispatcher；Goal协调、并行成员、汇合、输入等待、检查、停止及恢复；对应S2–S5。
4. Rust Goals/GoalClient、前台CLI及本地控制Socket，内置文档审阅Profile；无需step；对应S1/S6。
5. 独立消费者及真实Inquiry领域能力集成，固定应用/执行依赖SHA并分别记录确定性、真实Sidecar与真实Agent证据；不声称Inquiry #7文章质量完成；对应S6。
6. 冻结候选，更新功能地图06/07/08及S1–S6/18项证据表；keel-verify→独立code-review→CI→keel-release。若Reviewer判human required，只在全部结果可评审后请求当前候选的人工门。

## 验证与交付边界

- 每层先聚焦Unit/Integration，再用真实CLI/SDK走用户路径；使用Rust 1.91.1和锁文件。
- 新增功能文件：06-goal-dag.md（S1–S3）、07-goal-acceptance-reliability.md（S4–S5）、08-application-integration.md（S6）。
- 原18项验收不删、不降门槛；缺环境或未运行记blocked/not_run，不当skip/pass。
- 不改用户Inquiry当前工作树；应用集成优先复用固定版本领域库和公开API。若需应用改动，单独隔离并保留跨仓库引用。
- 代码完成、文档PASS或PR创建均不等于端到端完成；合入及适用发布要求以keel-release为准。

## 实现检查点（2026-11-08）

- 已接入类型构造注册、资源冻结、受限Schema及字段选择、计划/模板/检查点校验，共享许可与检查结果内容接纳；相关Goal入口尚未接入运行时。
- 已接入Store v2原子迁移与旧Flow物理账本；queued保留原RunId/epoch/次数，started未确认完整保留占槽；启动事务要求先预留。
- 新增21项Unit，覆盖注册、计划、检查、许可、迁移及占槽；本轮 `cargo test --locked --offline` 155通过、5项原有真实环境用例ignored。仅常规套件证据，不计真实Agent/E2E达标。
- L2 v0.10.4具体化原有产物字段selector的登记位置（可选artifact_selectors），节点成员形状上限对齐既有32安全边界；原18项验收不变。
- 尚待：Goal事务/预算与协调器、共享Managed Run/Dispatcher完整接线、CLI/客户端、两个业务/真实Agent、功能地图、独立review及发布。当前dead_code警告来自尚未接线部分，不以此候选进入最终Clippy/review门。

## 实现检查点二（2026-11-08）

- Goal专用状态/CAS/回执/预算表已落地，工具和字节准入独立事务同时更新Run剩余预留与Goal总额；取消后仍按原Run身份接受停止/消耗事实。新增12项Unit（预算4、时钟2、Goal事务6）。
- `ManagedBridge/ManagedRunOwner/execute_managed`已从旧Flow抽出并由Flow实际复用；Schema、工具、候选、附件、Trace及原生超时/停止不复制。停止查询有15秒超时及panic边界。
- 常规完整套件167通过、5项原有环境用例ignored；原始日志：`target/verify-taskloop/10-dev/foundation-tests.log`。此处仍不是Goal执行E2E或独立review结论。
- L2 v0.10.5补全Planner固定planning_feedback输入；资源示例及字节摘要同步，既有离线语义例8项反例仍通过。EvaluationReceipt仅复制目标身份摘要，目标正文保留在有界CheckInput/内容寻址blob中。
- Goal记录/检查输入/预算类型已定义，但Goal协调器、共享Dispatcher完整接线和外部候选解码前限额尚未完成；CLI/两个真实业务/真实Agent/最终review与release继续保持待办。


## 实现检查点三（2026-11-08）

- 已接公开 `Runtime::open_orchestrated`/`Runtime::goals`，共享Dispatcher同时管理旧Flow与Goal，原生执行复用Managed Run；Goal启动前原子登记身份、槽位和停止Token。
- 已接规划有界修正/锁定、节点输入/成员请求/固定投影、最终检查、规划补充输入，以及Goal侧期限/恢复/停止对账的初版。诊断候选与公共接纳分开；热计量不递增业务revision，状态CAS合并并发实际消费。
- 进程内受控Executor已跑规划→业务节点→最终检查，区分achieved/not_met/error；公开Runtime自动推进正例及规划等待→pause拒绝signal→resume→signal幂等正例通过。此处不是实际Agent或CLI验收证据。
- 替换Dispatcher后的完整常规回归176通过、5项既有环境用例ignored（`shared-dispatcher-regressions.log`）；之后补充输入等最新改动仍在聚焦验证，不把旧完整回归移挂为最新候选证据。
- L2 v0.10.6补全业务revision与计量/诊断分离及附件Schema元数据；原18项验收保持不变。恢复/停止、并行/检查点/两业务等负路径仍需补全和取证；外部候选解码前限额、Goal Trace归属、CLI/客户端、真实业务与Agent、功能地图、独立review和release尚未完成。


## 实现检查点四（2026-11-08）

- `a115a10`是Goal协调/共享Dispatcher的本地检查点；尚未推送或作为最终验收候选。随后新增的解码限额与Trace接线独立验证。
- Milkie对Goal JSON候选采用RawValue借用片段预检；5MiB反例经原Run关联核验后仅拒绝，无候选Value或正常工具调用。注册要求Executor明确支持该边界，旧Flow默认不变。
- Goal协调事件与原生Run共用Goal trace_id及既有Trace政策；协调观测流无业务Run/槽位/次数。受控SDK测试验证完整事件归属、节点/检查事件和关闭Span；模型观察不可用时complete=false，同时业务仍可achieved。Goal源清理先查持久Run状态，不能删除仍可启动/未停止的源。
- L2 v0.10.7明确上述契约；原18项验收不变。普通回归、真实Agent、应用集成、CLI/只读客户端、并发与恢复负例、项目verify及独立review/release仍需按最终候选完成。


## 实现检查点五（2026-11-08）

- `bd4b872`保存候选解码限额/统一Trace归属检查点；完整常规套件181通过、5项环境用例ignored，日志`goal-observation-full-regression.log`。未推送，未作为最终候选送独立review。
- GoalClient只读状态/计划/预算/产物/评价和Trace投影已接入：READ_ONLY、当前OS用户/0700检查、无owner.lock/迁移/协调。公开SDK正例同时验证宿主持锁时及退出后的查询、跨principal拒绝、预算/版本不变。
- 补全未知停止返回后的独立探测，确认后仅结算/释放物理占用，保留interrupted且禁止resume重播；受控反例通过。恢复无事实变化不升revision，未提交checking消耗显式不完整；失败准备成本仍记实际时间。
- 消耗业务转移额度时通过cleanup原子失败，不能在调度循环永久重试；未启动工作不调用Executor。该反例暴露过启动锁重入，已修复并重跑通过。
- L2 v0.10.8补充只读客户端边界，原18项验收未变。当前客户端/停止等增量仍在开发验证；在线Socket/CLI、真实业务/Agent、并发/崩溃/检查点完整取证、功能地图、独立review与release尚未完成。


## 实现检查点六（2026-11-08）

- 本地Unix Socket已实现大端长度前缀/4MiB限制、peer UID核验、可信宿主Caller映射、32连接与读写期限；请求不能提供principal，也不开放resume/任意方法。在线结果含固定回执及当前status/pending，不把受理意图当停止成功。
- 客户端暂停/补充输入/同键回执已通过真实本机Socket走通，端点关闭后host_not_running且版本不变，无离线队列。超限长度先拒绝、额外身份字段拒绝，37项编排聚焦测试通过。
- CliDriver及只派发所选Goal的启动模式已接入，SDK默认仍可服务多个Goal/Flow；尚未完成最终CLI二进制与配置工厂用户路径验证。Tokio增加net/signal，仅新增锁定依赖socket2 0.6.5，无旧包升级。
- 编译内置文档审阅工厂按Rust定义注册入口/节点/Profile，读取明确命名Schema/Skill/规则；不加载package.json。通用CLI与SDK消费者将复用这一工厂。
- 后续仍需：完整CLI/信号/恢复与锁释放验证，实际并行/评价节点/崩溃窗口、两个真实业务/Agent、功能地图、最终候选独立review及release。所有18项验收未降低、未提前勾选。


## 实现检查点七（2026-11-08）

- CLI二进制、可信HOST配置及编译内置document-review工厂已接通；真实CLI进程配合受控Milkie协议对端，覆盖成功/查询/导出/同键重放、not_met/检查错误/规划耗尽、在线取消，以及SIGINT/SIGTERM暂停后恢复。测试从公开NDJSON读取Goal身份，不查私有业务表。
- 外部SDK测试验证同节点成员真实并发且按成员身份汇合、两个Goal共享容量及owner隔离、未知Run保留容量且另一排队Goal按自己的墙钟到期。上述对端与Executor均受控，不是真实Agent或Inquiry集成证据。
- CLI首次测试暴露Planner在已有请求物化并运行后被重复创建的问题：准入现在覆盖所有尚未完成业务接纳的Planner调用，合法修正仍能在结果事务内创建下一请求；SDK延迟Planner回归覆盖这一窗口。
- Planner执行视图包含生效限制和显式imports摘要；干净暂停关闭该宿主协调观测源，保留Goal根Span供resume，Trace可重建摘要升级v3，历史事件字节不改。
- 最新完整常规套件192通过、5项环境用例ignored，日志`target/verify-taskloop/10-dev/latest-full-regression.log`；严格Clippy通过，日志`clippy-strict-retest.log`。离线契约校验通过资源摘要、引用、投影及8项语义反例，不能据此声明E2E完成。
- L2 v0.10.12集中重复协议、明确状态提交生效点及无step路径，规则/Decision记法不要求新增规则引擎或持久化层；原18项验收逐字保持。待完成：评价节点/崩溃窗口完整验证、真实Inquiry与第二业务/Agent、功能地图、最终候选独立review及release。


## 实现检查点八（2026-11-08）

- 公开SDK新增实执行评价节点覆盖：checkpoint的pass/fail/error、data的fail/error分别断言公共产物、Receipt、Goal结论及后继是否启动；同一受控checker在pause期间返回fail/error仍终结，pass暂停后显式resume不重跑已接纳检查。
- input节点等待不创建Run，pause保留nonce和原期限且拒绝signal；resume后同键signal仅消费一次，输出value供后继执行。workflow模式通过冻结模板完成同一Runtime执行链，不只测试图结构校验。
- CLI进程在Planner/Worker/final三个原生已启动窗口被SIGKILL；同键run先恢复为blocked/interrupted、拒绝resume、原生调用数不增，final阶段已接纳报告保留。仅证明这三个已启动窗口，不替代§6.9所有崩溃窗口。
- GoalSnapshot新增GoalProgress只读投影：pending请求、通用准入排队原因、目标/宿主占槽及宿主未知停止槽数；与业务记录/产物在同一只读事务读取。Goals与GoalClient共用投影，跨owner只给计数，不泄露其他目标身份。SDK用未知占槽正例比对两入口一致。
- 新建功能地图06/07/08，逐项映射S1–S6和原18项细验收，保留真实Agent/应用、故障窗口及尚未覆盖路径的取证要求，不将测试命令成功等同于整条S通过。
- 完整常规回归197通过、5项既有环境用例ignored（`goal-nodes-recovery-full-regression.log`），随后完善CLI人工进度/产物引用及NDJSON节点/progress投影，按受影响CLI套件复验（`goal-human-progress-cli-retest.log`）；严格Clippy见`goal-human-progress-clippy.log`。这些均为开发证据，最终干净候选仍按verify重新取证。
- L2 v0.10.13补全已承诺的progress字段，原18项验收逐字不变。下一阶段继续补齐其余可靠性边界并构建固定Inquiry领域库的隔离消费者；真实服务、最终verify、独立review、CI及release尚未完成。


## 实现检查点九（2026-11-08）

- 新建隔离的`tests/inquiry_consumer` crate，Cargo固定Inquiry d4b05750并以公开TaskLoop路径依赖统一其核心；资源目录在运行前核对同SHA且工作树干净。未改用户Inquiry源码/HEAD。
- 应用复用Inquiry Fetch、ReadSource、check_domain、verify_citation。旧state/slot仅在授权材料视图适配，Artifact ID/SHA/内容不变；原始HTML无损base64包装经固定公共JSON端口传递，导出还原并核对原文SHA。Planner、分析、写作、语义检查走真实Milkie，领域引用检查走显式受控Executor，无私表访问或程序生成假Plan。
- 两类注册（Inquiry与内置document-review）已通过无模型预检，消费者严格Clippy通过。功能地图08登记positive/negative/document公开命令和逐条引用回溯证据要求；尚不能据注册/编译记S6通过。
- 第一次真实正例在原生启动前失败：默认Milkie工作树缺execution-sidecar。改用干净98b5ec1c重新构建，现有真实Sidecar+受控Pi协议用例通过（`milkie-278-protocol-preflight.log`），此用例仍不是真实模型证据。
- 第二次真实正例进入固定Sidecar，但专用Grok环境auth_failed，未取得有效模型输出；第三次显式选机器已有Codex环境，因默认0.167.0不符合Milkie锁定0.166.0而拒绝。三个失败目录positive-1/positive-2/positive-3-codex原样保留，未提高配额/放宽领域规则，也未修改默认Harness配置或复制登录材料。
- 消费者增加显式Harness/模型选择、Milkie源提交及CLI摘要、原生版本记录与创建Goal前的Codex版本核对。正在从官方release下载0.166.0至本任务隔离目录，完成后校验官方SHA并显式加入仅本进程PATH；不替换系统Codex。真实S6仍待完成，Goal/独立review/release不标完成。


## 实现检查点十（2026-11-08）

- 三个持久化恢复窗口新增聚焦用例：reserved且未started保持RunId/已计次数、释放槽位后只在resume首次启动；原生已结束但未业务接纳保持诊断且禁止重放；节点checking中断保留成员结果但不重算公共成功。它们验证恢复逻辑，不冒称三种实际SIGKILL进程用例。
- 原反例`recovery-window-core-tests.log`复现两处重复recover无新事实仍升revision：首轮已将Run/节点归类interrupted，次轮走无在途Run分支时错误补pause时间。修复仅在无blocker且安全stopped时记录恢复暂停；blocked保持原事实。三个用例重跑通过（`recovery-window-core-retest.log`），严格Clippy通过（`recovery-window-clippy.log`）。
- 原固定Codex下载进程确认后，因实测代理线路更快而停止旧进程并切换路线；新下载完成，107610108字节、官方asset SHA256 c48824462fc57681eafb0925d97b11b589010475613904f570b42fea53e240d5匹配，隔离二进制自报codex-cli 0.166.0。`verified-download.json`保存官方来源及二进制摘要；未替换系统默认Codex。
- 准备在该固定环境继续真实S6。此前失败不改判；完整常规回归、真实正/负例/第二业务、最终verify与独立review/release继续待完成。


## 实现检查点十一（2026-11-08）

- 恢复修复后完整常规回归200通过、5项环境用例ignored（`recovery-fix-full-regression.log`）。随后补随包document-review Profile的text规划补充Schema及CLI真实进程等待→stop退出→resume→signal同键重放用例，nonce/期限保留且最终原生调用数为4；原Schema未注册时通用CLI无法进入合法等待路径，此次落实原S1路径，无新增状态机或包加载器。
- 该Profile与离线示例同步，L2 v0.10.14只具体化已有等待能力。更新后完整常规回归201通过、5项环境用例ignored（`cli-wait-profile-full-regression.log`），严格Clippy通过（`cli-wait-profile-clippy.log`），离线契约及原18项验收一致性通过。
- 正例positive-4-codex166已真实执行模型，但Codex发行包缺同版本code-mode-host，模型明确报告无法调用工具；Runtime未将最终聊天当产物。官方辅助包SHA256 7abd50fe5f033495a7b693c45fb82dc57093a18f1c4ba3e8043577de46c5c7c5已核对，加入隔离PATH；消费者增加创建Goal前的组件存在检查与摘要记录。
- positive-5-codex166真正完成Planner/来源选择/两份官方网页获取；分析Run真实读取24次来源后在240秒期限内未提交，Runtime failed/limit_reached且report dependency_failed。原始捕获、候选、预算和Trace保留，未自动返工、未改判成功。此例证明执行/期限边界，不算S6.A1正例通过。
- 下一次显式验证改进应用入口的方法：先60秒内聚焦阅读、挑选8–12条关键证据后提交，避免把时间耗在穷举引文；问题、RuleSet及全部预算不变。可显式选当前机器默认的gpt-6.1-sol模型，Harness/模型仍由可信配置冻结，不自动回退。真实正/负例/第二业务与最终verify/review/release仍待完成。


## 实现检查点十二（2026-11-08）

- document-1-sol在006f664候选真实运行Planner/审阅/质量检查，结果achieved，原始产物、Evaluation、预算与Trace保留；这是S6第二业务的开发证据，不移挂为后续候选的最终验收。
- positive-6-sol与negative-1-sol均真实完成规划、获取、分析和文章生成，但消费者Rust checker混用了Inquiry read_source的artifact_id参数，核心artifact.read实际要求id，导致检查执行失败。已修复参数；新增受限Bridge边界用例，调用原Inquiry域检查分别验证合法引用pass、材料不足fail及伪造引用fail。未修改原失败Goal。
- 新增留存公开产物的离线复验入口：通过GoalClient读取确切Plan/Artifact/SHA，校验修正后的领域适配器，且断言原Goal revision/outcome不变。它进一步发现positive-6分析把“无阻断缺口”的限制仍放入gaps并标ready，原Inquiry域契约正确返回fail。入口指令现明确ready要求gaps为空，非阻断限制进入judgments.limitations；原RuleSet和配额不变，不重写旧文章或改判旧任务。正/负留存复验均按实际fail通过，严格Clippy通过。
- 消费者export模式通过只读GoalClient导出公开快照、锁定Plan与Trace，无Runtime/模型/恢复；保留原执行证据，新增只读导出文件单列。
- 源码版本准备为0.2.0（尚非发布）：核心完整回归201通过、5项环境用例ignored，日志v020-candidate-full-regression.log；核心与Inquiry消费者严格Clippy通过，消费者域契约1通过/1需显式留存数据的工具用例ignored（该用例已另在正/负留存目录执行）。旧外部Flow消费者锁文件补入新增的5项依赖、未升级原依赖，--locked --offline实际运行通过。
- 下一步在冻结0.2.0候选上重新执行真实正例、未达标例及第二业务；仍需其余图形/可靠性验收核对、最终keel-verify、独立review及release。S6整体未标pass，端到端未完成。


## 实现检查点十三（2026-11-09）

- 公开SDK新增节点级顺序、分叉和全部汇合的实际调度测试，核对公共输入、上游结束/下游启动顺序与并发峰值；等待节点不占槽且独立分支继续，上游失败阻断汇合并保留已接纳兄弟产物。非法自环计划先返回精确错误与候选摘要、业务执行数为0，随后在同一Goal内有限修正成功。四项受控Executor测试通过，日志`dag-repair-branches-retest.log`，不冒称真实Agent。
- Workflow生产者/消费者回归复现：Runtime原先只传模板，而协议要求Planner回显模板摘要；旧fixture自己计算摘要掩盖了缺口。保留失败日志`workflow-digest-producer-regression.log`；Runtime现同步提供冻结模板的`template_digest`，fixture只回显并校验其来源，复验通过。L2 v0.10.15细化已有字段，原18项验收不变。
- 隔离消费者新增固定Workflow模式，真实Planner必须按Runtime模板及摘要提交提案，复用原document-review Worker与质量检查。三注册目录预检、消费者与核心严格Clippy通过；真实Workflow尚待执行。
- 上一冻结候选110b35c的S6开发结果：positive-7-v020 achieved（12条引用全部回溯）；negative-2-v020 not_met（保留文章、3项缺口，无自动返工）；document-2-v020 achieved。研究Trace对Rust内置执行保留complete=false，第二业务Trace完整。原证据位于`target/verify-taskloop/10-real-inquiry/`且不换绑新SHA。
- 最终候选验收表、keel-verify、独立代码review与release仍待完成；开发测试通过不代表端到端完成。


## 实现检查点十四（2026-11-09）

- 真实Workflow在d332075完成Planner与审阅Worker，二者原生停止均确认，固定模板摘要和锁定计划有效；最终检查已写入评价候选，但Milkie返回process_failed/stopped=false，Runtime正确blocked、保留1槽且无EvaluationReceipt。原Goal和预算不改判。`workflow-1-d332075/failure-audit.json`绑定原始证据摘要；只读导出未启动工作。
- 诊断区分事实与推断：最终原生活动距unknown记录382ms，未耗尽8250ms停止等待；结合固定Milkie ProcessTracker代码，快路径false指向此前库存读取异常被锁存。具体异常未记录，不能断言是主机负载或750ms超时。后续10秒空闲库存探针stopped=true，但不能替代原Run停止确认。
- 新增计划检查pass/fail/error三路径的公开SDK测试，复现要求举证时计划检查收到获准输入却无对应evidence_refs。现由共用issue_check入口按实际授权投影生成goal-input引用，移除节点检查中的重复生成，未新增状态或规则。三路径分别成功、有限修正后planning_exhausted、check_unavailable，业务启动边界与原Spec/配额不变。
- 补齐规划时限、Run与工具预算耗尽测试；修正不能重置原期限或上限。非法成员值在artifact.write被Schema拒绝，调用计费但候选字节0，节点失败且不产生公共汇合或最终检查。Flow和Goal在同一Runtime争唯一物理槽位，Flow在执行时Goal零Run/公开排队，确认停止后Goal才执行，实测峰值1。
- 公开查询原先只返回最终planning_exhausted，单次结构化失败仅在内部候选记录。GoalRunSnapshot现以plan_validation_failure只读投影对应提案的原始错误，无新增存储表或状态；在线/只读SDK一致，CLI人工及JSON均展示原错误位置。L2 v0.10.16细化此既有诊断承诺，原18项验收逐字保留。
- 聚焦计划检查/预算/非法成员/共享槽位/CLI诊断测试均通过；旧d332075完整套件205通过、5项环境用例ignored。新候选完整回归及严格Clippy继续核对，真实Workflow整条验收仍为blocked，最终keel-verify、独立review、CI与release尚未完成。


## 实现检查点十五（2026-11-09）

- 六类非法图由公开Goals实际提交：格式/容量、环、引用、连接、能力授权和预算/验收身份均有对应PlanValidationFailure及JSON Pointer，单Planner Run且零业务派发/零公共输出，预留和槽位释放；5项graph测试全部通过（`graph-all-rejection-categories-retest.log`）。不只调用纯验证器，不把Schema合法等同计划合法。
- 未来高水位注入证明clock_untrusted阻断准入/resume，重复recover无新事实幂等；恢复可信后仍stopped并保留原期限，只有显式resume才恢复运行资格，未产生Run。最初测试把被拒命令的持久化回执revision误算成recover变更，已按实际契约分开核对拒绝回执重放和恢复幂等，未改生产逻辑；复验通过（`clock-trust-recovery-retest.log`），严格Clippy通过。
- 复验准备工具首次接入探针如实保留29次原生调用尝试（含启动前失败），可确认原生耗时累计1528.395秒；不是所有开发命令耗时或模型上游请求数。尚无独立代码review，不能导入旧设计review冒充。`keel reverify --adopt-history`实测BLOCKED：requires existing review history；`--initial`仅接受空历史。探针不是完整复验计划或产品验收。已向用户询问本次是否允许留存原始记录、手工核算后继续，或先修工具；回答前不新增依赖该选择的模型调用。
- 本次仅补验收测试与功能地图，L2 v0.10.16和原18项验收不变。正式verify/独立review/CI与合入仍待完成，真实Workflow旧失败不改判。


## 实现检查点十六（2026-11-09）

- 补齐§11.1明确要求的pause中Planner合法/非法/needs_input三分支：在原生已返回但未业务接纳窗口发出pause，三类结果各自持久化，重复consume不重算，暂停期间不锁定/补建/派发后续工作；resume后分别锁定计划、补建有界修正请求或保留nonce/期限并接受补充，始终只计原1个Run且冻结Spec不变。用例通过（`paused-planner-three-branches.log`），未改生产逻辑。
- 98b6323开发套件运行期间补入该测试，旧日志不移挂最终候选；提交后重新冻结并执行必要套件。真实模型复验仍等待已发出的流程选择，未清空历史或把旧设计审查冒充独立代码review。


## 实现检查点十七（2026-11-09）

- d84f8dc已推送，macOS/Ubuntu CI均success（run 42500766167）；Issue #10设计摘要更新到L2 v0.10.16、链接该候选并明确未完成正式验证/独立代码review/合入，其余正文与未勾验收未变。旧Flow外部公开消费者与3项真实Milkie+受控Pi协议用例均通过，不能当真实模型成功。
- 真CLI非法Goal输入首次在宿主Unix socket绑定前因macOS SUN_LEN失败，未进入输入校验；原记录保留。同一非法输入改用较短私有Store后，人工/JSON均以退出码2保留input_invalid及/document_text位置，未创建原生调用；仅证准入反例，不记整条真实Agent CLI验收通过。
- 本轮补7个测试：合法成员的非法整体汇合；跨Goal重放相同通过评价字节；规则升级后拒绝恢复旧冻结Goal；三个资源维度的旧快照预留CAS竞争；双必需检查的部分pass/pause接纳和fail-fast迟到pass；预留前恢复；孤立blob重开后不可当公共Artifact。生产逻辑不变，原18项验收及L2版本不变。
- 迟到检查用例首次错误假定手工Core夹具拥有2物理槽，实际旧Runtime构造器只有1槽；修正为原生先确认停止并释放物理槽、结果仍待业务接纳，再执行第二检查，随后按指定顺序消费两结果。不扩大宿主容量或改变生产行为。
- 聚焦库测试93通过、SDK测试14通过、严格Clippy通过；最新冻结候选完整套件继续单列。d84f8dc的CI不移挂本轮SHA；正式验收与独立review未宣称完成。新增真实模型调用仍等待用户已收到的历史接入方式选择，不将未答复当批准。

## 实现检查点十八（2026-11-09）

- 67a8e04的CI run 42503986788：macOS通过、Ubuntu的规则升级重开测试失败，原日志与失败结论保留。源码及CI时序表明原5秒等待/10秒墙钟前置可能在慢宿主重开前到期；CI未打印完整快照，具体到期原因仍属推断。该测试两次宿主与请求统一显式60秒有效窗口，仍要求未到期、旧冻结版本不变且拒绝恢复；独立200毫秒在线/离线到期反例保持期限验证，生产逻辑与真实任务预算未改。
- 补Planner/计划检查/业务成员/最终检查各工具、候选字节、执行时间的12组合，实际触发拒绝或超时并核对公开limits_hit、Spec不变及预留/槽位释放。受控Executor仅拥有Rust Future，按原Run确认其退出，不代表真实Harness停止。SDK共16项通过（role-budget-expiry-version-sdk-retest.log）。
- CLI在线取消扩展到Planner、业务、最终检查三阶段，退出/保留产物/无追加Run/占槽/Socket与离线拒绝均验证（three-stage-cli-cancellation.log）。另以纯判定测试证明等待期限不依赖timer先出队（wait-admission-deadline-unit.log）。
- 数据库写失败测试原本因缺blob在SQL写入前拒绝，不足以证明写失败回滚。现写有效blob并在真实Artifact INSERT上注入触发器故障，断言命中注入错误及Goal/revision/预算/公共产物全部回滚；孤立blob仍私有保留。用例通过（actual-sql-write-failure-rollback.log），严格Clippy通过（final-boundary-fixtures-clippy.log）。
- 按用户refine要求精简L2为v0.10.17，§6.1删重复图并改为成员执行→公共接纳→后继就绪三步，明确Runtime内部职责与Harness内部执行循环。§4、§6.2–6.11、§11与前版逐字一致，十三节、本地链接及离线契约检查通过；不改变原18项验收或新增执行组件。
- 上述为聚焦开发证据；冻结候选完整套件与对应CI另行保存，不能把旧SHA证据改挂。正式verify、真实模型复验、独立代码review及release仍未完成；历史接入选择尚待用户答复。
