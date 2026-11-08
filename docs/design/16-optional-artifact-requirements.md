# Issue #16：可选产物依赖

版本：v0.1（Draft）。关联 [Issue #16](https://github.com/xforce-io/taskloop/issues/16)。L1 使用本票评论概要及本文件 §11 的验收映射；L2 为本文件。产品确认来源：2026-11-07 会话“端到端 fix”，修复已确认的补查上下文缺口，保持既有隔离边界。

## 1 背景

真实 Inquiry 补查 visit 2 的 `accepted_inputs` 只有 scope/plan。原始输入与源码调查见本机 `inquiry/work/verify/2-third-investigation/findings.json` F1。`Requirement` 当前把所有依赖视为必需，不能表达首次尚无、后续已有的历史产物。

## 2 名词解释

已有名词沿用 [名词表](../glossary.md)。可选产物依赖：显式声明、允许尚无产物但存在时仍按所选版本和权限提供的依赖。

## 3 目标与非目标

目标：首次入口不被不存在的可选历史阻塞；后续入口取得已声明的真实历史版本。非目标：自动读取全部历史、跨实例读取、放宽必需依赖或改变业务推进规则。

## 4 能力

业务定义可以在单个 Requirement 上声明 `optional: true`。省略或 false 保持原语义。存在产物时，latest/all 与 include_attachments 的既有规则不变。

### 4.1 UI/UX

N/A：纯 Rust SDK / JSON 业务定义契约，没有应用页面或公开 CLI。

## 5 思路与折衷

依赖解析仍由 TaskLoop 负责，允许可选依赖在不存在时贡献空集合。存在时不接受伪造值，不绕过版本、附件与读取授权。放弃应用执行器自行注入未授权内容和为每种历史组合复制流程状态。

## 6 架构

定义层声明依赖；运行时按当前实例历史解析；工具层仅授予解析结果。主路径为加载定义→可选依赖为空或选择真实产物→装配 Invocation→受控读取。失败路径为非法定义拒绝、必需依赖缺失执行前拒绝、未声明或跨实例读取拒绝。已结束流程要求保留必需终态产物，不允许通过可选终态要求伪造完成。

## 7 模块

定义 Schema / 序列化、历史解析与现有工具授权共同维持契约；不增加存储或第二份历史。

## 8 API/CLI

Requirement 新增布尔字段 `optional`，缺省 false，序列化 false 时省略以保留旧定义摘要。仅工作入口依赖允许 true；终态要求不接受 true。其他未识别字段、非布尔值继续拒绝。

## 9 边界

可选意味着允许没有对应历史产物，不意味着忽略非法 state/slot、已损坏产物、错误附件声明或读取失败。自状态依赖只读取既有已接受历史，不读取本次候选。latest/all 的选择发生在当前实例范围内。

## 10 迁移/兼容/回滚

旧 JSON 定义、摘要与运行行为不变。Rust 直接构造 Requirement 的调用者需要显式填写新字段；本 crate 尚为固定 SHA 的 0.1 开发接口，源码调用点同步更新。使用 true 的新定义必须固定支持该能力的 TaskLoop SHA；旧执行器会显式拒绝未知字段。回滚时同时恢复定义和依赖版本，不转换历史字节。

## 11 测试计划

| 验收 | 前置与入口 | 可判定结果 / 禁止结果 | 层级与证据 |
|---|---|---|---|
| S1.A1 | 公共 Runtime，首次工作入口，无可选历史 | 正常执行，可选集合为空，无虚构 ArtifactRef | Integration：真实 Runtime / Executor 输入记录；必需 |
| S1.A2 | 生产历史产物后再次进入入口，分别 latest/all | 接收所声明的版本和附件，不含其他历史 | Integration：两次入口记录、实际 artifact.read；必需 |
| S2.A1 | 旧定义、可选 true/false、非法类型和终态要求 | 旧摘要保持；非法定义拒绝；终态产物要求不可绕过 | Unit / Integration：固定摘要及加载结果；必需 |
| S2.A2 | 必需产物缺失、未声明及跨实例产物 | 缺失在执行前拒绝；读取越权拒绝 | Integration：拒绝码和无执行效果；必需 |

无用户页面，E2E 为独立 SDK 消费者的加载→运行→读取闭环；不启动真实模型证明依赖选择。

## 12 开放问题

N/A：本次不引入缺省的隐式依赖，也不改变终态要求。

## 13 关联

- Inquiry [Issue #5](https://github.com/xforce-io/inquiry/issues/5)，补查入口消费者。
- 既有 [EntryContract 与 Playbook](3-minimal-runtime-mvp.md)。
