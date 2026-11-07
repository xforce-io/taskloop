---
name: verify-taskloop
description: 通过 TaskLoop 公开 Rust SDK 和受控进程验证业务执行、工具与停止契约。
---
# TaskLoop 验证手册

## Launch

先记录 origin、候选 SHA、git status，读 [功能地图](features/README.md)。使用锁定 Rust 工具链1.91.1及 Cargo.lock；Homebrew Cargo 1.91.1 无 rustup 时直接 cargo，CI 的 rustup 环境使用 cargo +1.91.1，准备本次临时测试目录；证据放 target/verify-taskloop 下，不使用用户业务数据库。

## Doctor

运行对应入口 `cargo test --locked --offline` 或 `cargo +1.91.1 test --locked --offline` 核验可用依赖；缺缓存可按锁文件联网获取，不升级。Node/Milkie/真实 Harness 仅在对应功能需要时检查；读取其版本与明确宿主路径，不复制登录材料或猜测地址。

## Drive

每条验收依功能文件从公开 SDK 触发，核对状态、产物、授权和停止；Unit/进程夹具不能替代设计要求的真实 Harness。严格区分业务执行成功、原生停止确认、观测完整性；未知不重放，晚到调用必须受 Bridge 拒绝。

## Evidence

保存候选/依赖 SHA、入口、命令、预期与实际、原始结果位置。每个 S 与细化验收分别写 pass/fail/blocked/not_run；缺环境不作 skip，不用CI替代真实路径。只记录固定错误与安全元数据，不输出连接或登录材料。

## Cleanup

只停止本次拥有的进程，核对停止后删除本次临时数据；未知执行保留关联与证据。禁止自动合入、发布或部署。
