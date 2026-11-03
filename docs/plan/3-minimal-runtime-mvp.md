# Issue #3 实现计划与当前事实

目标：端到端交付 TaskLoop Rust crate，再交付 Inquiry Web 与真实研究对照实验。用户 2026-11-03 已授权开发；设计基线 Issue #2 L2 v0.1.0。没有既有运行时代码，how: skip 无现成机制可讲。

顺序：真实 Harness 能力探针 → Rust 定义/私有存储/五操作/工具与 gate/适配器 → Rust 正反例与真实取消/隔离/重启 → 冻结候选和 verify → 独立 review → CI/发布门禁与合入。TaskLoop 稳定后 Inquiry 固定其已验证提交，完成 Web/CUA 与研究质量对照。

Harness 候选：本机 Codex CLI 0.166.0，gpt-6.1-sol 首个真实探针成功，期间有网络重连；Claude CLI 未登录，不依赖它。模型账号来自既有 Codex 登录，不复制或输出凭据。阶段客户端关闭继承的 MCP、shell、浏览器、技能发现等，只通过显式宿主工具桥访问输入与候选；实际约束须由探针验证，不能仅靠提示词。

Reviewer 按 ~/.config/keel/reviewer 绑定为 codex gpt-6.1-sol；用独立原生 review 上下文，记录实际模型/目标。涉及方法挂载、技能/权限合同等，按 keel-review 可能需要当前候选人工批准；必须先完成可审查交付，再处理最终门禁，不能自行降低要求。

现状：已有两个设计工作分支未合入 main，本实现分支从刚核对的 main 创建并引入现有设计资产；实现代码不在 main 上开发。真实能力/产品测试不能用原 Python 设计见证替代。生产部署目标尚未提供；先交付可在本地运行和验证的版本，不擅自部署公网。

## 已验证与实现约束

- 系统 HTTP(S)/SOCKS 代理为本机 127.0.0.1:9567（仅环境事实，不硬编码到产品）；命令行默认不自动继承系统代理。Cargo 通过任务进程环境 CARGO_HTTP_PROXY 使用它已完成依赖下载；Codex 也应在探针/本地启动时继承已有代理环境。不修改用户全局配置/凭据。
- Codex App Server 真实 dynamicTools 探针已通过：一个阶段、一个 probe_echo 调用、实际 model=gpt-6.1-sol。公开证据 initial-harness-probe.json；取消、越权/方法读取、A/B 隔离仍要做，不能提前宣称 D0 完成。
- 初步 Rust 定义加载器、重复 JSON 键拒绝、无网络 Schema 校验、Skill 字节锁与 Unix openat/O_NOFOLLOW 文件读取已实现；两业务解析的聚焦测试正在修正 serde 与原 Schema 的精确对应，不降低规范。
- 公开五操作采用 Rust 类型；业务信号/产物/门禁仍保持原协议。宿主能力公开扩展为领域 Tool/Gate 回调，实际执行控制、Run 标识和 CAS 私有。
- 私有存储拟采用 SQLite 行中的有界状态快照 + 原子 revision/CAS/待办顺序，产物字节先持久化；幂等和命令回执同事务，不增加队列服务。单宿主文件锁。
- Worker 调用工具桥，当前入口可读锁定方法、允许输入和信号；artifact.write 仅写声明候选槽位。领域工具返回 payload/附件，框架形成独立受信回执；门禁只读。
- 取消必须终止本次 Codex app-server 进程组并 wait 确认，不能把 future 取消当停止。每 Run 在创建进程前持久化 nonce，通过 app-server --startup-client-info 的 title 带 nonce 标识宿主所有权；恢复时可核对当前同用户的该 nonce 进程，不按 CLI 名称批量停止，不杀 PID 复用的无关进程。未知执行保持 blocked，确认停止后才释放执行容量，不自动重放。若该所有权方案探针不成立，先修正适配再交付。
- 方法包普通规范检查与机器集成检查分开；普通 Skill 不因缺 Playbook 失败。准备增加 validate_skill 与 Definition/Runtime 的定位明确检查，复用正式加载路径。


## 2026-11-03 开发进度（不是冻结验收）

- Rust 五操作、静态加载、普通 Skill 校验、SQLite/CAS/命令回执、产物字节锁定和宿主桥已有可运行实现。
- 发现 CLI dotted override 对带引号段按字面处理，旧方式实际上新建了名称含引号的 MCP 条目；已改为完整表覆盖，每次运行重新枚举并复核所有继承集成关闭，失败拒绝启动。不能使用修复前的方法探针证明隔离。
- 修复后真实 gpt-6.1-sol / Codex 0.166.0 两项探针通过：锁定方法随机值交接；A/B 同包隔离、两个实际越权拒绝、A 进程确认停止后 B 完成。仅证明这些明确场景，不代表恶意原生回调隔离或 Inquiry 研究质量。
- 真实 OS 子进程强制终止测试通过：候选保留、中断阻塞、不自动重放、未知执行占容量，确认停止后 B 执行；这项使用确定性执行器以隔离持久化语义，不伪称真实模型重启探针。
- 外部独立 Cargo package 通过五操作闭环，未引用私有模块或应用数据表；确定性执行器与真实模型探针分别记账。
- 普通检查目前通过，等待超时用例明确设置 1 秒，其余等待/重启测试使用 60 秒以避免测试自身跨过业务期限。
- 当前仍需核对完整验收覆盖、冻结候选复验、独立审查、平台 CI 和合入门禁。Inquiry 实现/真实研究对照/Web 驾驶未完成。


## 首轮独立审查与修复

候选 a9afbbf89b0226f0680a58ee5f4206bd99106fc0；Codex 原生独立只读 review，实际 gpt-6.1-sol，来源 bind-file；结果 CHANGES_REQUESTED / human: required，204.05 秒。完整原始记录位于被忽略的 work/verify/3-review-1/native-review.log；本轮累计审查次数 1，不清零。

问题依据：phase.rs 的 started 保存和句柄注册有空窗；shutdown 仅等待循环退出；超时取消 token 被当成用户取消；StopUnknown 覆盖 Cancelling；requirements 仅选同名输出；入口 Schema 未用于实际输入；write_blob 在完成前创建最终摘要路径。详细发现及原候选行号保留在原审查中。附件缺口另由公开 API 独立宿主复现；关闭/超时/取消意图/输入契约有实际失败回归；并发文件观察看到最终摘要路径的长度为 0（预期 33554432）。启动竞争的原始依据是可达多线程交错与独立审查，未冒称已稳定复现原窗口；修复后增加 64 次公开启动/取消竞争检查。

修复：协调 started 与取消句柄发布；核实未知停止后才能关闭，超时保留 JoinHandle 供重试；关闭后拒绝新工作；内部失败与取消意图区分；未知停止保留已受理的取消意图；显式 include_attachments 授权生产访问附件闭包；检查并传递实际阶段输入；临时文件同步后原子发布摘要路径。运行职责和领域边界不变。

新增回归：failure_boundaries.rs、attachments.rs 和 Store 原子发布观察。附件覆盖默认拒绝/显式允许、latest/all、无关生产阶段、A/B 实例。真实执行器和流程输入发生变化，旧真实 Agent 依据不能直接作为修复候选通过；确定性检查通过后必须重跑相关实际方法/取消/隔离场景，再做完整合并差异的第二轮独立审查。最终交付、Inquiry 与研究质量目标仍未完成。


## 第二轮独立审查与修复

候选 fb9b6e53fc49c5e3256e384e750bc80b5dc8f0b3；同一绑定 Codex 原生独立只读 review，CHANGES_REQUESTED / human: required，272.80 秒，累计第 2 轮。剩余 3 项 P2：最终提交 CAS 与取消竞争后缺少状态收敛；重启覆盖 Cancelling；已排队到期仍调用执行器。

问题依据保留在 work/verify/3-review-2/：queued-expiry-repro.log 实际观察到 B 已到期仍调用执行器 1 次；cancel-crash-repro.log 在实际取消回执后 kill 宿主，重启得到 Blocked 而非 Cancelling；orphaned-cancel-repro.log 为受控白盒注入审查指出的持久化孤立取消状态，观察到不收敛，并非伪称稳定重现全部线程时序。后续修复须对照这些原输入复验。

修复统一停止收尾：phase 保留本次执行已确认停止的事实，提交竞争后重读并收敛取消；后台复核所有 pending cancelling，包括无句柄和重启场景，未知停止仍占容量；恢复时保留取消意图；在派发前和调用执行器前拒绝零预算。新增实际取消后强制终止/重开且无需第二次取消的回归。普通 Skill 可选标准字段类型同时补齐，未扩展业务功能。

第三轮为当前默认允许的最后一轮独立审查；不能重置计数。当前仍未批准合入，Inquiry 产品实现和最终研究/UI 验证尚未完成。


## 第三轮审查、已备妥修复与人工门

候选 117b4fea9e15ff0b701658bf5b83eb65f68576d5；绑定 Codex 原生独立只读 review，212.61秒，CHANGES_REQUESTED / human: required。累计默认三轮已用完，不能自启第四轮或把新 SHA 当新任务。

剩余3项P2均已先复现后修复：gate-budget-repro.log 证明执行预算耗尽后仍调用领域检查；slot-name-repro.log 证明加载接受不可写输出槽；bare-path-repro.log 证明当前目录裸文件名失败。原始记录位于 work/verify/3-review-3/，不改写失败事实。

当前修复：检查剩余预算为零直接阻塞、不调用 gate；根 Schema 对输出槽名/附件前缀使用与 Bridge 写入一致的规则；空父路径按当前目录解释，安全文件读取保留。新增相同输入回归均通过，完整普通套件/Clippy/外部消费者通过。需要新候选上的受影响真实复验和额外完整独立审查，尚未执行；必须先取得用户对追加审查的明确授权。

共同根因：先前测试覆盖了主闭环，却遗漏了边界检查在不同入口/阶段的一致性。此次把可判定条件前置并保留实际失败回归，不降低质量或门禁；若获准仅追加1轮，退出条件是该冻结候选完整审查PASS与CI通过。仍非PASS则交用户决定，不继续默认循环。


## 实际 macOS CI 的取消提交竞争

旧准备候选 234ceaa4057ff591fb01b35c14931ab6decec7a9 的 CI run 41500689243 / job 125467070993 实际失败：confirmed_cancellation_never_precedes_a_new_executor_start 的 cancel 返回内部 revision_conflict。原始平台日志保存 work/verify/3-ci-cancel-race/original-ci.log；不把它视为测试通过或以重试掩盖。Ubuntu 测试/Clippy 当次通过，但消费者因默认 fail-fast 被取消。

机制：cancel 命令及幂等回执已受理；API 和后台/另一 API 同时确认停止，最终状态保存会竞争 CAS。主受理路径已有重试，后续停止状态更新却将冲突直接返回给无 expected_revision 参数的调用方。新增三个停止确认同时释放的确定性测试，原版实际失败，修复后通过。

修复仅在该已受理路径忽略最终提交的 revision_conflict，重新读并返回真实状态，其余存储错误仍返回；确认取消时清除旧停止错误及进度。CI matrix 关闭 fail-fast，以完整收集双平台结果。完整普通套件、Clippy、独立消费者已通过；新候选仍需必要真实复验及已用完三轮后的追加审查授权。

先前待确认问题针对 234ceaa；CI失败后该候选不能合入。新候选的人审和条件合入授权必须绑定新的完整SHA，不能把旧候选的批准移用。当前未收到追加审查授权，也未启动第四轮。


## 第四轮 Astra 审查与边界修复

用户实际授权对 28357be0b851ee2d5261a6f328c6473d9e5a2329 追加一轮 gpt-6-astra 独立审查，PASS 且 CI 通过后才可合入并继续 Inquiry。真实原生独立审查 350.80 秒，CHANGES_REQUESTED / human: required；累计第 4 轮，授权已使用，未满足合入条件。原始输出及授权原文位于 work/verify/3-review-4/，不覆盖旧记录。

剩余两项 P2：恢复时同逻辑 ID 的新执行器可以错误确认原运行停止；候选配置允许超过固定存储读取上限。共同根因仍是跨入口边界一致性不足。先补回归：实际 kill 宿主后换绑执行器，旧版确实调用了新实现的停止确认；128 MiB 配置旧版也实际被接受。原始失败日志保存在 work/verify/3-review-4-fixes/。大小问题的配置回归没有冒称执行过 65 MiB 模型附件。

修复将后台恢复、cancel、shutdown 统一到锁定绑定的停止确认；指纹不符保持未知，不释放容量、不伪称关闭成功。候选配置在打开存储前限于 1–67108864 字节，存储写读共用同一上限；加入 64 MiB 实际文件写入、读取、重复发布及超限拒绝回归。未扩展为多版本执行器注册或大文件平台。

新候选须保留四轮累计成本，按受影响路径复验并追加独立审查。未经新的明确追加授权，不运行第 5 轮或新增真实模型复验；先完成确定性检查和 CI，形成可审阅候选，再请求决定。旧精确候选的条件合入授权不转移到新 SHA。Inquiry、真实质量实验和 Web 驾驶仍未完成。

本地本轮普通检查结果：47 项通过、2 项真实模型测试按默认忽略；fmt、Clippy（-D warnings）和独立 Rust consumer 通过。最初新增大文件单测使用了已有临时根目录，因其非 0700 被正确拒绝；测试夹具改为由 Store 新建私有子目录，原失败日志保留。修复后真实模型复验、第五轮审查、合入和 Inquiry 未完成。


## 第五轮 Astra 审查及错误分类/证据范围修复

用户明确授权对候选 3975f3eab1d08fd661d4887c3ab01dc7c33d65ea 追加1轮累计第5轮 gpt-6-astra 审查，以及必要真实复验；仅同一候选通过全部门禁才可合入并继续 Inquiry。授权原文在 work/verify/3-review-5/user-authorization.md。三个真实 Run 本轮全部通过（A取消/B完成/锁定方法，2个测试），取消Run token未知，另外两次provider用量保留；双平台CI及Git SHA外部消费者通过。

第5轮原生独立只读 Astra 审查 305.68 秒，CHANGES_REQUESTED / human required：2项P2，当前未合入，追加授权已使用，不自动第6轮。全部五轮及每次真实Run的计量保留在 work/verify/3-review-5-fixes/cumulative-history.json；早期探索总体计量仍未知。

依据：phase.rs 将 GateContext.accepted 设为 requirements 选中的产物，但证据校验却遍历 current.history 全量；Bridge 持久化失败只返回执行器，phase 随后根据执行器的 Failed/Completed 选择业务失败/拒绝，丢失框架错误分类。原始公开入口反例证明 latest 不授权的首次产物仍被接纳，以及写候选时损坏全新测试存储的 blobs 子目录、保留SQLite可写后，两类执行器返回都导致 Failed 而非 Blocked。原始 evidence-before.log/storage-before.log 保留，非假称真实模型磁盘故障；另保留最初测试夹具误设可选输出被静态规范正确拒绝的日志。

修复：证据检查只认本次授权产物集合、当前检查句柄和当前Run受信回执；桥记录首次 storage_error/revision_conflict，实际执行停止且未取消时，在业务转移前维持当前阶段 blocked。无新公共接口、调度器或故障平台。补充6种证据正反例覆盖旧visit、未授权槽、未授权附件、其他生产阶段、all与显式附件授权；补充执行器失败/忽略错误/成功重试、领域产物持久化和SQLite回执故障反例。SQLite故障采用测试库内受控触发器白盒注入，产品不暴露私有表接口。

共同根因：同一可信边界在读取、证据接纳和执行器结果消费处未统一。策略改为让当前授权集合及桥接层基础设施失败直接约束最终转移，而不依赖领域gate或执行器自行解释。先完成原失败对照和完整确定性检查/双平台CI，形成新精确候选，再请求必要真实复验及仅1轮完整独立审查的新授权。未获新授权前不启动第6轮，也不将旧候选条件合入授权移用；Inquiry与真实质量/Web目标仍未完成。

本地完整普通检查：53项通过，2项真实模型测试默认忽略；fmt、Clippy（-D warnings）和独立 Rust 消费者通过。新候选的真实复验/第6轮审查未运行；仍需新精确SHA的追加与条件合入授权。


## 第六轮 Astra 审查与停止状态落库失败

用户实际授权对 0cf3a3deeaa06116f70b78ae5e1ce455f1f0a457 追加1轮累计第6轮 Astra 完整独立审查及必要真实复验。三个实际Run本轮通过（A取消、B隔离完成、锁定方法交接，2个测试）；当前候选双平台CI已通过。授权原文及真实输出位于 work/verify/3-review-6/。

原生独立只读 gpt-6-astra 审查 362.69秒，CHANGES_REQUESTED / human required，剩1项P2：StopUnknown更新及phase补救保存均失败时，持久记录仍为Running、active.started=true、stop_unknown=false；worker只核查unknown/cancelling，漏掉残留已启动Run而派发B。尚未合入，授权已使用，不自动第7轮。

问题先复现：在新测试实例A进入执行后，用测试SQLite触发器拒绝A行更新、保留B可写；真实公开入口返回StopUnknown，原记录确实残留三个字段，旧实现实际启动B一次。原始 stop-capacity-before.log 保留在 work/verify/3-review-6-fixes/。这是受控确定性执行器与白盒存储故障，不冒称真实模型停止状态故障。

修复按已有单Worker串行事实处理：阶段返回后扫描到的任何active.started Run都必须停止核查；旧Run缺最终提交时保持当前阶段blocked、撤销旧代数且不重放；未知或停止结果写入失败均保留容量。两条回归分别证明存储恢复不代替停止确认、停止确认不代替结果提交。没有增加数据库、恢复平台或新公开操作。

共同根因是调度把停止状态成功落库当成未知执行的唯一事实源，遗漏保存本身失败时既有started证据。策略改为让残留started本身触发保守核查，不能以新增重试或模型指令替代容量约束。先完成原失败对照、全部确定性检查及CI，再请求新精确候选上的必要真实复验和仅1轮完整独立审查；非PASS不默认追加，历史轮数/成本保留。Inquiry与整体质量/Web目标仍未完成。

本轮本地完整普通检查55项通过，2项真实模型测试默认忽略；fmt、Clippy（-D warnings）与独立Rust消费者通过。新候选真实复验和第7轮审查尚未运行，需新的明确追加授权及同一候选条件合入授权；六轮原始记录不变。
