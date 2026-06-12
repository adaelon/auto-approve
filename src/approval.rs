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
#[derive(Debug, Clone)]
pub enum InputChunk {
    /// Literal text to send to the PTY.
    Text(String),
    /// Named key press: "enter", "tab", "y", etc.
    Key(String),
}

/// Auto-approve configuration.
#[derive(Debug, Clone)]
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
    content: String,
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = "\
You are a safety gate for terminal commands.
A CLI agent is waiting for user confirmation to run a command.
Look at the bash command visible in the screen content and decide if it is safe to auto-approve.

Reply EXACTLY one line in one of these formats:
APPROVE: <text to send>
APPROVE: key:<key_name>
DENY: <short reason>
UNCERTAIN: <short reason>

Rules:
- Approve only clearly non-destructive commands: reading files, listing, git operations, \
  installing packages from trusted sources, routine confirmations
- DENY commands involving: sudo, rm -rf, mkfs, format, drop database, overwrite configs, \
  expose secrets, redirect critical system files
- When in doubt, reply UNCERTAIN
- For simple y/N confirmations of safe commands, prefer APPROVE: key:enter
- Never approve anything that could cause data loss or security exposure";

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

        let response = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(err) => {
                warn!(%err, "auto-approve LLM request failed");
                return ApprovalDecision::Uncertain {
                    reason: format!("request error: {err}"),
                };
            }
        };

        let chat: ChatResponse = match response.json().await {
            Ok(c) => c,
            Err(err) => {
                warn!(%err, "auto-approve LLM response parse failed");
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
            "max_tokens": 64,
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
                ApprovalDecision::Approve {
                    chunks: vec![InputChunk::Text(rest.to_string())],
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
                assert_eq!(chunks.len(), 1);
                assert!(matches!(&chunks[0], InputChunk::Text(t) if t == "yes"));
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
}
