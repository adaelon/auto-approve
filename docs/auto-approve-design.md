# Auto-Approve 技术方案

## 1. 概述

为 oly 增加自动审批能力：当 Claude Code 等交互式 Agent 在 PTY 会话中卡在确认提示时，
用廉价 LLM（OpenAI 兼容 API）判断 bash command 是否安全，安全则自动发送确认，
不安全或拿不准则通知人类。

## 2. 数据流

\\\
PTY 输出 → vt100 Parser → prompt 检测 (已有)
                                  ↓ input_needed = true
                        截取 vt100 screen 纯文本
                                  ↓
                          ┌─ AutoApprover ─┐
                          │  POST /v1/chat/  │
                          │  completions     │
                          └────┬────────────┘
                               │
                  ┌───approve──┼──deny/uncertain──┐
                  ↓            ↓                   ↓
            try_write_input  通知人类          通知人类
            (自动发送按键)  (已有通知路径)     (已有关通知路径)
                  ↓
            重置 input_needed
\\\

## 3. 新增类型 src/approval.rs

\\\ust
/// LLM 返回的审批决策
enum ApprovalDecision {
    /// 安全，自动发送指定输入
    Approve { chunks: Vec<InputChunk> },
    /// 明确不安全
    Deny { reason: String },
    /// 拿不准，交给人类
    Uncertain { reason: String },
}

/// 自动发送的输入单元
enum InputChunk {
    Text(String),
    Key(String),  // "enter", "tab", "y" 等
}

/// 自动审批配置
struct AutoApproveConfig {
    enabled: bool,
    api_url: String,            // 默认 "https://api.openai.com/v1"
    api_key: String,             // config.json 或 env OLY_AUTO_APPROVE_API_KEY
    model: String,               // 默认 "gpt-4o-mini"
    max_context_lines: usize,    // 发给 LLM 的屏幕最大行数，默认 80
    request_timeout_secs: u64,   // LLM 请求超时，默认 10
}
\\\

## 4. 配置

### 4.1 config.json

\\\json
{
  "auto_approve": {
    "enabled": false,
    "api_url": "https://api.openai.com/v1",
    "api_key": "",
    "model": "gpt-4o-mini",
    "max_context_lines": 80,
    "request_timeout_secs": 10
  }
}
\\\

### 4.2 CLI flags

\\\
oly daemon start \\
  --auto-approve \\
  --auto-approve-api-url https://api.openai.com/v1 \\
  --auto-approve-api-key sk-... \\
  --auto-approve-model gpt-4o-mini
\\\

合并优先级：CLI flags > 环境变量 OLY_AUTO_APPROVE_API_KEY > config.json

### 4.3 AppConfig 扩展

AppConfig 新增 \uto_approve: AutoApproveConfig\ 字段，
\with_runtime_overrides\\ 方法增加 auto-approve 相关参数。

## 5. LLM 系统提示

\\\
You are a safety gate for terminal commands.
A CLI agent is waiting for user confirmation to run a command.
Look at the bash command visible in the screen content and decide if it is safe to auto-approve.

Reply EXACTLY one line in one of these formats:
APPROVE: <text to send>
APPROVE: key:<key_name>
DENY: <short reason>
UNCERTAIN: <short reason>

Rules:
- Approve only clearly non-destructive commands: reading files, listing, git operations,
  installing packages from trusted sources, routine confirmations
- DENY commands involving: sudo, rm -rf, mkfs, format, drop database, overwrite configs,
  expose secrets, redirect critical system files
- When in doubt, reply UNCERTAIN
- For simple y/N confirmations of safe commands, prefer APPROVE: key:enter
- Never approve anything that could cause data loss or security exposure
\\\

## 6. 集成点

### 6.1 核心入口：input_needed 触发

当前流程（\src/notification/dispatcher.rs\）：
  input_needed 变 true → 发桌面通知 / 调 hook

新增流程：
  input_needed 变 true →
    if auto_approve.enabled:
      screen_text = session.screen_parser.screen().to_string()
      AutoApprover::judge(screen_text) →
        Approve → try_write_input(chunks) + 重置 input_needed + 记日志
        Deny/Uncertain → 走原有通知路径
    else:
      走原有通知路径

### 6.2 AutoApprover 实现

\\\ust
impl AutoApprover {
    async fn judge(&self, screen_text: String) -> ApprovalDecision {
        // 1. 截断到 max_context_lines
        // 2. POST /v1/chat/completions，带 system prompt + screen_text
        // 3. 解析 LLM 返回的 APPROVE/DENY/UNCERTAIN 行
        // 4. 超时 / HTTP 错误 / 解析失败 → Uncertain
    }
}
\\\

### 6.3 写入输入后重置通知状态

自动发送后需重置 \input_needed\ 并跳过本轮人类通知，
避免"刚自动批准又弹通知"。

## 7. 错误处理

| 场景 | 策略 |
|------|------|
| LLM 请求超时 | 视为 Uncertain，走通知人类 |
| LLM 返回无法解析 | 视为 Uncertain，走通知人类 |
| API key 缺失/无效 | daemon 启动时校验：enabled=true 但 key 为空则报错退出 |
| 网络不可达 | 视为 Uncertain，走通知人类 |

核心原则：**宁可多提醒，不可误审批**。

## 8. 依赖

- \eqwest\（已有依赖）— 调 LLM API
- \serde_json\（已有依赖）— 解析响应
- 无新增 crate

## 9. 安全考虑

- API key 仅存于 config.json 和内存，不落日志
- LLM 审批不缓存：同一命令在不同上下文下安全性可能不同
- 自动审批日志记录：每次审批决策写 \pproval.log\（追记到 session 目录）
- 审批结果通过 SSE 推送到 Web UI，人类可事后审计
## 10. 决策记录

### §10.1 LLM 审批方式：内建 vs 外部 hook

**决策**: 内建到 daemon，直接用 reqwest 调 OpenAI 兼容 API。

**否决**:
- A: 外部 hook 服务——多一个进程要管理，部署复杂度上升，延迟也更高

**命门**: 新增依赖只有逻辑（reqwest 已在），但 API key 管理需要用心。

**何时回头**: 如果出现需要支持非 HTTP 协议的模型后端，考虑抽成 trait。

### §10.2 LLM 接口协议：OpenAI 兼容 API

**决策**: 只支持 OpenAI 兼容 \/v1/chat/completions\。

**否决**:
- A: 同时支持 Anthropic Claude 原生 API——增加实现复杂度，兼容 API 已有 vLLM/Ollama/LiteLLM 代理

**何时回头**: 用户明确提出需要直连 Anthropic API 且无法用代理时。

### §10.3 上下文来源：当前 vt100 屏幕文本

**决策**: 只发送 \screen_parser\ 当前屏幕文本给 LLM，不附历史输出。

**否决**:
- A: 拼接历史 output.log——token 成本高，安全判断只需看当前屏幕上的 command
- B: 手动提取 bash command 再发给 LLM——多一层解析，不如让 LLM 直接从屏幕文本读 command

**命门**: screen_parser 必须在 input_needed 触发时已包含完整提示内容。

**何时回头**: 如果用户反馈 LLM 经常误判因为缺少上下文，可添加可选的最近 N 行历史。

### §10.4 失败策略：LLM 出错一律通知人类

**决策**: 超时、网络错误、返回解析失败均视为 Uncertain，走已有通知路径。

**否决**:
- A: 出错时重试——增加延迟，且连续失败时不应阻塞通知
- B: 出错时默认 Approve——违反"宁可多提醒，不可误审批"原则

**何时回头**: 如果特定错误模式（如临时网络抖动）频繁触发误通知，可加有限重试。

### §10.5 自动审批后重置 input_needed

**决策**: Approve 发送输入后立即重置 input_needed，跳过本轮人类通知。

**否决**:
- A: 发送后仍通知人类"已自动审批"——噪音太大，审批日志已记录

**命门**: 必须确保 try_write_input 成功后才重置，写入失败时仍通知人类。

**何时回头**: 用户要求"每次自动审批都通知我"时，可加 config 开关。
