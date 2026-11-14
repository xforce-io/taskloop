# #23 实施记录

来自既有Inquiry端到端目标和用户明确要求Milkie原生搜索。已核对Github origin，完整查重1页/12张Issue无同需求，新建#23，从origin/main@daf0fba分出feat/23-milkie-native-search-mode。

how：src/milkie.rs::MilkieConfig无搜索选项；execute在context.create前取得能力，随后仅发送tools/forwarding/timeoutMs/observation/maxModelIterations。identity摘要覆盖程序/环境/存储，应覆盖新授权。Milkie已在#283提供明确模式和原生记录，执行结果usage已有可信run/context标识，来源读取不必在TaskLoop另造状态。

L1 reuse显式绑定/执行/停止；L2 write v0.2。当前尚未验证，不宣称交付或开PR。


实现后补充：应用若自行从TaskLoop Run推导Milkie数据目录，会依赖适配器私有布局。因此复用既有owner归属记录及Sidecar公开execution.query/web_search.read，提供只读native_web_searches方法。真实Milkie #283记录使用顶层webSearch字段，已按真实公开类型核对；夹具同步该字段。没有额外模型调用。

开发门：3条native_search定向测试通过，Clippy零警告、真实Milkie不支持运行时前置拒绝通过、公开消费者通过。完整回归及冻结SHA验证结果另存target/verify-taskloop/23-native-search；这里不预写验收PASS。
