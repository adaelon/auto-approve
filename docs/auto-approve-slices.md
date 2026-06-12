# Auto-Approve 实现切片

## Slice 1: 审批类型与 LLM 客户端 ✅
- **做什么**: 创建 src/approval.rs，定义 ApprovalDecision、InputChunk、AutoApproveConfig、AutoApprover 及 judge() 方法
- **不做什么**: 不动 config、cli、daemon — 只做纯逻辑模块
- **完成判据**: cargo check 编译通过，AutoApprover::judge() 能用 mock 响应返回正确决策

## Slice 2: 配置集成 ✅
- **做什么**: 在 src/config.rs 增加 AutoApproveConfig 字段，解析 config.json + env OLY_AUTO_APPROVE_API_KEY
- **不做什么**: 不改 daemon 启动流程，只让配置可加载
- **完成判据**: cargo check 编译通过，AppConfig::load() 能读取 auto_approve 段

## Slice 3: CLI flags + with_runtime_overrides 扩展
- **做什么**:
  1. `src/cli.rs` `DaemonStartArgs` 新增四个 flag：`--auto-approve`（bool）、`--auto-approve-api-url <URL>`、`--auto-approve-api-key <KEY>`、`--auto-approve-model <MODEL>`
  2. `src/config.rs` `AppConfig::with_runtime_overrides()` 签名扩展，接收四个新参数并覆写 `self.auto_approve` 对应字段
- **不做什么**: 不改 main.rs 调用点（S4 做）；不改 daemon 启动逻辑
- **完成判据**: cargo check 通过；`oly daemon start --help` 列出四个新 flag；with_runtime_overrides 现有测试仍绿

## Slice 4: API key 校验 + AutoApprover 构建 + detached 转发 ✅
- **做什么**:
  1. `src/main.rs` — daemon start 分支调用 with_runtime_overrides 时传入 S3 的四个新 CLI 字段
  2. `src/daemon/lifecycle.rs` `run_foreground`：
     - 校验：enabled=true 且 api_key 为空 → 返回 AppError::Config 并打印明确错误，daemon 退出
     - 构建 `Option<Arc<AutoApprover>>`，传给 run_notification_monitor（签名预留，S6 消费）
  3. `src/daemon/lifecycle.rs` `spawn_detached`：传递 `--auto-approve`、`--auto-approve-api-url`、`--auto-approve-model`；**API key 走 env var**（`.env("OLY_AUTO_APPROVE_API_KEY", key)`），不加 CLI arg 避免 ps 泄露
- **不做什么**: 不改 notification monitor 内部逻辑（S6 做）
- **完成判据**: cargo check 通过；enabled=true + key 为空时 daemon start 立即报错退出

## Slice 5: SessionStore::write_session_input
- **做什么**: 在 SessionStore 新增异步方法：
  ```
  pub async fn write_session_input(session_id, chunks: &[InputChunk]) -> bool
  ```
  内部找到 running session 的 PtyHandle，翻译 chunk（Text→raw bytes，Key→按 oly send 现有解析逻辑：enter→\r，tab→\t，y→y 等）并调 try_write_input；任一失败返回 false
- **不做什么**: 不改 notification monitor；不新增非测试的 public API
- **完成判据**: cargo check 通过；单元测试覆盖 Text 和 Key chunk 写入

## Slice 6: Notification monitor 集成
- **做什么**: `run_notification_monitor` 签名增加 `auto_approver: Option<Arc<AutoApprover>>`；在 candidate 循环 trigger 确定后、dispatch 人类通知前插入：
  - judge() 拿到 Approve → write_session_input → 成功则 mark_notified + continue（跳过人类通知），失败则 fall through
  - Deny / Uncertain → fall through 走原通知路径
  - 占位 warn! 记录决策（S7 替换为正式日志）
- **不做什么**: 不改 PTY、vt100 逻辑
- **完成判据**: cargo check 通过；lifecycle.rs 传 auto_approver 的调用点编译通过

## Slice 7: Approval log
- **做什么**: 在 src/approval.rs 加 `append_approval_log(sessions_dir, session_id, entry)`，追加写 JSON 行到 `sessions/<id>/approval.log`；字段：ts、decision、chunks、llm_raw；写失败只 warn!；将 S6 的占位 warn! 替换为真实调用
- **不做什么**: 不做 Web UI 展示；不做 log rotation
- **完成判据**: 触发一次自动审批后 sessions/<id>/approval.log 有 JSON 记录

## Slice 8: E2E 验证 + 收口
- **做什么**: 启动 daemon with --auto-approve，跑需要确认的命令，验证 Approve 路径（自动发送，不弹通知）和 Deny/Uncertain 路径（弹人类通知）；更新 SESSION_CHECKPOINT.md 和本文件完成状态
- **不做什么**: 不加自动化集成测试（LLM 依赖不适合 CI）
- **完成判据**: 两条路径 demo 均通过；文档与代码状态一致
