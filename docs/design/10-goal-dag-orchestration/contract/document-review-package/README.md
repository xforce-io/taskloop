# 文档审阅能力包示例

这是可校验的能力定义离线清单示例，不是首版运行时加载协议，也非已实现CLI或真实模型测试。应用代码应构造并注册同组Rust类型，复用已有Schema/Skill资源读取，不实现package.json加载器。宿主仍须提供可信的`milkie` Executor、Core内建Schema和授权；文件不会安装Skill、执行脚本或加载任意Rust代码。

| 文件 | 含义 |
|---|---|
| package.json | 应用类型定义的离线清单：公共端口、成员、入口、预算、领域规则、checker及Profile |
| schemas/*.json | 应用输入输出Schema；摘要绑定实际文件字节 |
| skills/document-review/SKILL.md | Worker方法；目录摘要沿用SkillPackage字节映射算法 |
| rules/document-quality.json | 应用提供的领域判断标准，不是Runtime Gate代码 |
| plan.example.json | 使用该目录的合法计划形状；零摘要为占位，运行时必须换成冻结事实 |
| check-input.example.json | Runtime构造的只读检查输入：报告目标和明确授权的原文输入 |
| evaluation.example.json | 示例评价内容，不含Runtime生成的Receipt身份 |

数据路径：Goal.inputs.document_text → review节点document端口 → reviewer成员stage_input.document → report候选 → 内建Schema检查 → 固定成员输出投影 → 公共report产物 → 独立quality checker → Runtime核验规则/证据/身份并接纳评价。

`package.json`内的Profile、NodeContract和EntryContract是不同对象；同名`review@1`分别位于独立目录，解析必须携带目录类型。Planner只能选择capabilities目录，不能把entries当作业务节点。

quality checker能看到的`goal_inputs.document_text`由注册的goal_input_fields显式允许；它不是从report推测原文。例子的产物/目标摘要及nonce不代表真实接纳事实，仅证明结构；注册校验使用真实资源摘要，运行时输入仍需按冻结事实重建和授权。

checkpoint-node.example.json和evaluation-node.example.json分别演示阻断与非阻断评价能力：只有目标产物来自公共端口，整个CheckInput身份由Runtime建立，不能通过普通input_bindings注入。它们是独立NodeContract示意，未声称包含在本例单节点计划中。
