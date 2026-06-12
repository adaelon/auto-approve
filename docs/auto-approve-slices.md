# Auto-Approve 实现切片

## Slice 1: 审批类型与 LLM 客户端
- **做什么**: 创建 src/approval.rs，定义 ApprovalDecision、InputChunk、AutoApproveConfig、AutoApprover 及 judge() 方法
- **不做什么**: 不动 config、cli、daemon — 只做纯逻辑模块
- **完成判据**: cargo check 编译通过，AutoApprover::judge() 能用 mock 响应返回正确决策

## Slice 2: 配置集成
- **做什么**: 在 src/config.rs 增加 AutoApproveConfig 字段，解析 config.json + CLI flags + 环境变量
- **不做什么**: 不改 daemon 启动流程，只让配置可加载
- **完成判据**: cargo check 编译通过，AppConfig::load() 能读取 auto_approve 段

## Slice 3: CLI flags
- **做什么**: 在 src/cli.rs 增加 --auto-approve、--auto-approve-api-url、--auto-approve-api-key、--auto-approve-model 参数
- **不做什么**: 不改 main.rs 分发逻辑
- **完成判据**: cargo check 编译通过，CLI help 输出包含新 flags

## Slice 4: Daemon 启动集成
- **做什么**: 在 src/daemon/lifecycle.rs 中将 AutoApprover 构建并传入 session store / notification dispatcher；在 src/main.rs 中传递 CLI flags 到 config
- **不做什么**: 不改 notification 分发逻辑
- **完成判据**: cargo check 通过，daemon 启动时能创建 AutoApprover 实例

## Slice 5: 通知路径集成
- **做什么**: 在 src/notification/dispatcher.rs（或 store.rs 的 prompt 检测路径），当 input_needed 变为 true 时先调 AutoApprover::judge()，Approve 则写入输入并重置状态，Deny/Uncertain 则走原通知路径
- **不做什么**: 不改 PTY 或 vt100 逻辑
- **完成判据**: 手动测试——启动 daemon with --auto-approve，观察 LLM 判断后的行为

## Slice 6: 审批日志
- **做什么**: 每次审批决策追加到 session 目录下 pproval.log，包含时间戳、决策、LLM 原始返回
- **不做什么**: 不做 Web UI 展示（后续）
- **完成判据**: 自动审批后 sessions/<id>/approval.log 有记录

## Slice 7: 端到端验证
- **做什么**: 完整测试——启动 daemon + auto-approve，跑一个需要确认的命令，验证自动审批和人类通知两条路径
- **不做什么**: 不加自动化集成测试（LLM 依赖不适合 CI）
- **完成判据**: demo 通过，方案文档和代码链路更新完成
