# 可选产物依赖（Issue #16）

事实源：docs/design/16-optional-artifact-requirements.md v0.1 §11。

入口为公开Definition加载、Runtime.start/signal及受控artifact.read，没有页面或公开CLI。

- S1.A1：空历史首次运行，声明optional=true不阻塞，不伪造ArtifactRef；观察实际Executor输入。
- S1.A2：同实例产物产生后再次运行，latest/all取得声明的已接受版本，include_attachments仍按生产者和版本授予；检查实际读取内容。产物列表顺序不是访问先后，不以随机Artifact ID排序冒充时间顺序。
- S2.A1：省略optional与false的旧定义摘要一致；非法类型、未知state/slot/selection及optional终态要求拒绝。
- S2.A2：必需缺失在Executor前拒绝；未声明和跨实例读取返回拒绝，产生零越权效果。

冻结候选后执行 `cargo test --locked --offline --test optional_requirements --test attachments`。两项均为公共SDK的确定性消费者，不启动真实模型，不称为Harness验收。普通套件覆盖既有定义加载、业务恢复和隔离。证据保存候选SHA、命令与原始输出，不能换SHA复用旧输出。
