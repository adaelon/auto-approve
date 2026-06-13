use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// LLM-returned approval decision.
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    /// Safe - auto-send the specified input.
    Approve { chunks: Vec<InputChunk> },
    /// Clearly unsafe.
    Deny { reason: String },
    /// Unsure - defer to human.
    Uncertain { reason: String },
}

/// Single input unit for auto-approval.
#[derive(Debug, Clone, serde::Serialize)]
pub enum InputChunk {
    /// Literal text to send to the PTY.
    Text(String),
    /// Named key press: "enter", "tab", "y", etc.
    Key(String),
}

/// Auto-approve configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AutoApproveConfig {
    pub enabled: bool,
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_context_lines: usize,
    pub request_timeout_secs: u64,
}

impl Default for AutoApproveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            max_context_lines: 80,
            request_timeout_secs: 10,
        }
    }
}

/// One line appended to `sessions/<id>/approval.log` per decision.
#[derive(serde::Serialize)]
pub struct ApprovalLogEntry {
    pub ts: String,
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<InputChunk>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Append one JSON line to `sessions/<session_id>/approval.log`.
/// Silently warns on any I/O or serialization failure.
pub fn append_approval_log(
    sessions_dir: &std::path::Path,
    session_id: &str,
    entry: &ApprovalLogEntry,
) {
    use std::io::Write as _;

    let line = match serde_json::to_string(entry) {
        Ok(l) => l,
        Err(err) => {
            warn!(%err, session_id, "approval log: serialize failed");
            return;
        }
    };
    let log_path = sessions_dir.join(session_id).join("approval.log");
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| writeln!(f, "{line}").map_err(Into::into));
    if let Err(err) = result {
        warn!(%err, session_id, "approval log: write failed");
    }
}

// ---------------------------------------------------------------------------
// OpenAI chat-completions response (minimal)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = "\
You are a safety gate for a NUMBERED confirmation prompt.
The screen shows a CLI agent (e.g. Claude Code, Codex) paused on a permission
menu like:
  Do you want to proceed?
  > 1. Yes
    2. Yes, and allow ... (always)
    3. No
Look at the command/action being confirmed and decide whether to auto-approve it.

Reply EXACTLY one line in one of these formats:
APPROVE: <number>
DENY: <short reason>
UNCERTAIN: <short reason>

Rules:
- The screen IS a numbered confirmation menu. Choose the plain \"Yes\" option \
  (the one that proceeds once, usually \"1\"), NOT a \"Yes, and always allow\" option.
- APPROVE safe actions: reading/listing files, checking versions, building, \
  running tests, git status/diff/log, installing known packages, routine inspection.
- DENY clearly destructive actions: rm -rf, mkfs, format disk, drop database, \
  overwriting critical configs, exposing/exfiltrating secrets, sudo rm.
- If the action's safety is genuinely unclear, reply UNCERTAIN.
- Reply with ONLY the single line, e.g. \"APPROVE: 1\".";

// ---------------------------------------------------------------------------
// AutoApprover
// ---------------------------------------------------------------------------

pub struct AutoApprover {
    config: AutoApproveConfig,
    client: reqwest::Client,
}

impl AutoApprover {
    pub fn new(config: AutoApproveConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .expect("failed to build reqwest client");
        Self { config, client }
    }

    pub async fn judge(&self, screen_text: String) -> ApprovalDecision {
        let truncated = self.truncate_screen(&screen_text);

        let body = self.build_request_body(&truncated);
        let url = format!("{}/chat/completions", self.config.api_url.trim_end_matches('/'));

        debug!(model = %self.config.model, "sending auto-approve request");

        let response = match self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                warn!(%err, "auto-approve LLM request failed");
                return ApprovalDecision::Uncertain {
                    reason: format!("request error: {err}"),
                };
            }
        };

        let status = response.status();
        let raw_body = match response.text().await {
            Ok(b) => b,
            Err(err) => {
                warn!(%err, "auto-approve LLM response read failed");
                return ApprovalDecision::Uncertain {
                    reason: format!("response read error: {err}"),
                };
            }
        };
        debug!(status = %status, body = %raw_body, "auto-approve LLM raw response");

        let chat: ChatResponse = match serde_json::from_str(&raw_body) {
            Ok(c) => c,
            Err(err) => {
                warn!(%err, status = %status, body = %raw_body, "auto-approve LLM response parse failed");
                return ApprovalDecision::Uncertain {
                    reason: format!("response parse error: {err}"),
                };
            }
        };

        let content = match chat.choices.first() {
            Some(c) => c.message.content.trim().to_string(),
            None => {
                warn!("auto-approve LLM returned empty choices");
                return ApprovalDecision::Uncertain {
                    reason: "empty LLM response".to_string(),
                };
            }
        };

        self.parse_decision(&content)
    }

    fn truncate_screen(&self, text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() <= self.config.max_context_lines {
            return text.to_string();
        }
        let start = lines.len() - self.config.max_context_lines;
        lines[start..].join("\n")
    }

    fn build_request_body(&self, screen_text: &str) -> serde_json::Value {
        serde_json::json!({
            "model": self.config.model,
            "messages": [
                { "role": "system", "content": SYSTEM_PROMPT },
                { "role": "user", "content": screen_text }
            ],
            "temperature": 0.0,
            "max_tokens": 1024,
        })
    }

    fn parse_decision(&self, content: &str) -> ApprovalDecision {
        let line = content.lines().next().unwrap_or("").trim();

        if let Some(rest) = line.strip_prefix("APPROVE:") {
            let rest = rest.trim();
            if let Some(key_name) = rest.strip_prefix("key:") {
                ApprovalDecision::Approve {
                    chunks: vec![InputChunk::Key(key_name.trim().to_string())],
                }
            } else if rest.is_empty() {
                ApprovalDecision::Approve {
                    chunks: vec![InputChunk::Key("enter".to_string())],
                }
            } else {
                // Always append Enter so the PTY submits the typed text
                ApprovalDecision::Approve {
                    chunks: vec![
                        InputChunk::Text(rest.to_string()),
                        InputChunk::Key("enter".to_string()),
                    ],
                }
            }
        } else if let Some(reason) = line.strip_prefix("DENY:") {
            ApprovalDecision::Deny {
                reason: reason.trim().to_string(),
            }
        } else if let Some(reason) = line.strip_prefix("UNCERTAIN:") {
            ApprovalDecision::Uncertain {
                reason: reason.trim().to_string(),
            }
        } else {
            warn!(content = %line, "auto-approve LLM returned unparseable response");
            ApprovalDecision::Uncertain {
                reason: format!("unparseable LLM response: {line}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_decision_approve_with_text() {
        let approver = test_approver();
        let dec = approver.parse_decision("APPROVE: yes");
        match dec {
            ApprovalDecision::Approve { chunks } => {
                assert_eq!(chunks.len(), 2);
                assert!(matches!(&chunks[0], InputChunk::Text(t) if t == "yes"));
                assert!(matches!(&chunks[1], InputChunk::Key(k) if k == "enter"));
            }
            _ => panic!("expected Approve, got {dec:?}"),
        }
    }

    #[test]
    fn parse_decision_approve_key() {
        let approver = test_approver();
        let dec = approver.parse_decision("APPROVE: key:enter");
        match dec {
            ApprovalDecision::Approve { chunks } => {
                assert_eq!(chunks.len(), 1);
                assert!(matches!(&chunks[0], InputChunk::Key(k) if k == "enter"));
            }
            _ => panic!("expected Approve with key, got {dec:?}"),
        }
    }

    #[test]
    fn parse_decision_approve_bare_defaults_to_enter() {
        let approver = test_approver();
        let dec = approver.parse_decision("APPROVE:");
        match dec {
            ApprovalDecision::Approve { chunks } => {
                assert!(matches!(&chunks[0], InputChunk::Key(k) if k == "enter"));
            }
            _ => panic!("expected Approve, got {dec:?}"),
        }
    }

    #[test]
    fn parse_decision_deny() {
        let approver = test_approver();
        let dec = approver.parse_decision("DENY: destructive command");
        match dec {
            ApprovalDecision::Deny { reason } => assert_eq!(reason, "destructive command"),
            _ => panic!("expected Deny, got {dec:?}"),
        }
    }

    #[test]
    fn parse_decision_uncertain() {
        let approver = test_approver();
        let dec = approver.parse_decision("UNCERTAIN: ambiguous context");
        match dec {
            ApprovalDecision::Uncertain { reason } => assert_eq!(reason, "ambiguous context"),
            _ => panic!("expected Uncertain, got {dec:?}"),
        }
    }

    #[test]
    fn parse_decision_unparseable_falls_back_to_uncertain() {
        let approver = test_approver();
        let dec = approver.parse_decision("I think this is safe");
        match dec {
            ApprovalDecision::Uncertain { reason } => {
                assert!(reason.contains("unparseable"));
            }
            _ => panic!("expected Uncertain fallback, got {dec:?}"),
        }
    }

    #[test]
    fn truncate_screen_keeps_last_n_lines() {
        let approver = test_approver();
        let input = (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let truncated = approver.truncate_screen(&input);
        assert!(truncated.lines().count() <= 80);
        assert!(truncated.starts_with("line 20"));
    }

    fn test_approver() -> AutoApprover {
        AutoApprover::new(AutoApproveConfig::default())
    }

    // append_approval_log tests
    #[test]
    fn append_approval_log_writes_json_line() {
        let base = std::env::temp_dir().join(format!("oly-test-{}", std::process::id()));
        let session_id = "alog-write";
        let sessions_dir = base.join("sessions");
        std::fs::create_dir_all(sessions_dir.join(session_id)).unwrap();

        let entry = ApprovalLogEntry {
            ts: "2026-01-01T00:00:00Z".to_string(),
            decision: "approve".to_string(),
            chunks: Some(vec![InputChunk::Key("enter".to_string())]),
            reason: None,
        };
        append_approval_log(&sessions_dir, session_id, &entry);

        let content = std::fs::read_to_string(sessions_dir.join(session_id).join("approval.log")).unwrap();
        assert!(content.contains("\"decision\":\"approve\""), "missing decision");
        assert!(content.contains("\"ts\":\"2026-01-01T00:00:00Z\""), "missing ts");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn append_approval_log_appends_multiple_lines() {
        let base = std::env::temp_dir().join(format!("oly-test-multi-{}", std::process::id()));
        let session_id = "alog-multi";
        let sessions_dir = base.join("sessions");
        std::fs::create_dir_all(sessions_dir.join(session_id)).unwrap();

        for decision in ["approve", "deny", "uncertain"] {
            append_approval_log(
                &sessions_dir,
                session_id,
                &ApprovalLogEntry {
                    ts: "2026-01-01T00:00:00Z".to_string(),
                    decision: decision.to_string(),
                    chunks: None,
                    reason: None,
                },
            );
        }

        let content = std::fs::read_to_string(sessions_dir.join(session_id).join("approval.log")).unwrap();
        let lines: Vec<_> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3);
        let _ = std::fs::remove_dir_all(&base);
    }
}
