pub mod channel;
pub mod dispatcher;
pub mod event;
pub mod prompt;

use std::{sync::Arc, time::Instant};
use tracing::{debug, info, trace, warn};

use crate::{
    config::AppConfig,
    db::Database,
    notification::{
        channel::{LocalOsNotificationChannel, NotificationChannel, WebPushChannel},
        dispatcher::Notifier,
        event::{NotificationEvent, NotificationTriggerRule},
        prompt::{compile_prompt_patterns, find_prompt_match, sanitize_body, strip_ansi_for_body},
    },
    session::{SessionEvent, SessionStore, SilentCandidate},
};

/// Periodically checks all running sessions for silence and emits local OS
/// notifications once per output epoch. Silence alone is sufficient to
/// trigger a notification. Prompt patterns are used to pick a better body
/// line but do **not** gate delivery.
pub(super) async fn run_notification_monitor(
    notifier: Arc<Notifier>,
    session_store: Arc<SessionStore>,
    config: Arc<AppConfig>,
    event_tx: tokio::sync::broadcast::Sender<SessionEvent>,
    notification_tx: tokio::sync::broadcast::Sender<NotificationEvent>,
    auto_approver: Option<std::sync::Arc<crate::approval::AutoApprover>>,
) {
    let silence = std::time::Duration::from_secs(config.silence_seconds);
    let suppression_window = std::time::Duration::from_secs(5);
    let min_notification_interval = std::time::Duration::from_secs(10);
    let patterns = compile_prompt_patterns(&config.prompt_patterns);

    // Auto-approve gate: only engage the LLM judge when the screen is an actual
    // numbered confirmation menu (e.g. claude/Codex permission prompt:
    // "Do you want to proceed? ❯ 1. Yes / 2. ... / 3. No"). This keeps human
    // notifications broad while preventing auto-approve from firing on idle
    // shell prompts, slash-command autocompletes, or other transient TUI state.
    let confirmation_re = regex::Regex::new(r"(?im)^\s*[❯>›]?\s*\d+\.\s+yes\b")
        .expect("valid confirmation regex");
    // Throttle/dedup window for the auto-approve judge, so a blinking
    // confirmation prompt is evaluated at most once per this interval.
    let auto_approve_cooldown = std::time::Duration::from_secs(6);

    info!(
        silence_seconds = config.silence_seconds,
        min_notification_interval_seconds = min_notification_interval.as_secs(),
        prompt_patterns = patterns.len(),
        "notification monitor started"
    );

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Auto-approve pass — runs every tick, independent of the output-silence
        // gate below. Coding-agent TUIs keep redrawing a blinking confirmation
        // prompt, so they never go "silent"; we must scan their live screen and
        // act on numbered confirmation menus directly.
        if let Some(ref approver) = auto_approver {
            for cand in session_store.auto_approve_candidates(auto_approve_cooldown) {
                let clean = strip_ansi_for_body(&cand.excerpt);
                if !confirmation_re.is_match(&clean) {
                    continue;
                }
                // Enter cooldown before judging so a persistent prompt is only
                // judged once per window even if input takes a moment to land.
                session_store
                    .mark_auto_approve_checked(&cand.session_id, std::time::Instant::now());
                run_auto_approve(
                    approver,
                    &session_store,
                    &config,
                    &cand.session_id,
                    &clean,
                    cand.output_epoch,
                )
                .await;
            }
        }

        let candidates: Vec<SilentCandidate> =
            session_store.silent_candidates(suppression_window, min_notification_interval);

        if !candidates.is_empty() {
            debug!(count = candidates.len(), "notification candidates detected");
        }

        for candidate in candidates {
            let session_id = candidate.session_id.clone();
            let excerpt = candidate.excerpt.clone();
            let output_epoch = candidate.output_epoch;

            debug!(session_id, "evaluating notification triggers for candidate");
            trace!(
                session_id,
                excerpt = excerpt.as_str(),
                output_epoch = ?output_epoch,
                "evaluating notification triggers for candidate in detail"
            );

            let (trigger_rule, trigger_detail) =
                if let Some(pattern) = find_prompt_match(&excerpt, &patterns) {
                    info!(
                        session_id,
                        trigger_rule = NotificationTriggerRule::RegexPattern.as_str(),
                        pattern = pattern.as_str(),
                        "notification triggered"
                    );
                    (NotificationTriggerRule::RegexPattern, Some(pattern.clone()))
                } else if let Some(llm_detail) = evaluate_llm_direct_trigger(&excerpt) {
                    info!(
                        session_id,
                        trigger_rule = NotificationTriggerRule::LlmCheck.as_str(),
                        "notification triggered"
                    );
                    (NotificationTriggerRule::LlmCheck, Some(llm_detail))
                } else if Instant::now().duration_since(output_epoch) >= silence {
                    info!(
                        session_id,
                        trigger_rule = NotificationTriggerRule::Silence.as_str(),
                        "notification triggered"
                    );
                    (NotificationTriggerRule::Silence, None)
                } else {
                    continue;
                };

            let body = sanitize_notification_excerpt(&excerpt);
            let event = NotificationEvent::input_needed_with_trigger(
                session_id.clone(),
                candidate.session_title.clone(),
                body,
                trigger_rule,
                trigger_detail,
            );

            if candidate.enabled_for_channels {
                let notifier = Arc::clone(&notifier);
                let event = event.clone();
                let session_id = session_id.clone();
                tokio::spawn(async move {
                    if !notifier.dispatch(&event).await.any_delivered() {
                        warn!(session_id, "notification delivery failed on all channels");
                    }
                });
            } else {
                debug!(
                    session_id,
                    "notifications disabled for session, skipping delivery to channels"
                );
            }

            // This is useful for supervisor agent to take over when to send notifications
            let _ = notification_tx.send(event.clone());

            let _ = event_tx.send(
                event
                    .into_session_event(candidate.last_total_bytes, candidate.enabled_for_channels),
            );

            session_store.mark_notified(&session_id, output_epoch, std::time::Instant::now());
        }
    }
}

fn evaluate_llm_direct_trigger(_excerpt: &str) -> Option<String> {
    None
}

/// Run the LLM judge on a confirmation screen and act on the decision: send the
/// approved input to the PTY (and suppress the human notification for this
/// epoch), or log the deferral. `clean_excerpt` must already be ANSI-stripped.
async fn run_auto_approve(
    approver: &crate::approval::AutoApprover,
    session_store: &SessionStore,
    config: &AppConfig,
    session_id: &str,
    clean_excerpt: &str,
    output_epoch: std::time::Instant,
) {
    use crate::approval::{ApprovalDecision, ApprovalLogEntry, append_approval_log};

    match approver.judge(clean_excerpt.to_string()).await {
        ApprovalDecision::Approve { chunks } => {
            info!(session_id, "auto-approver approved, attempting to send input");
            if session_store.write_session_input(session_id, &chunks).await {
                append_approval_log(
                    &config.sessions_dir,
                    session_id,
                    &ApprovalLogEntry {
                        ts: chrono::Utc::now().to_rfc3339(),
                        decision: "approve".to_string(),
                        chunks: Some(chunks),
                        reason: None,
                    },
                );
                info!(session_id, "auto-approved: skipping human notification");
                session_store.mark_notified(session_id, output_epoch, std::time::Instant::now());
            } else {
                warn!(session_id, "auto-approve write failed, deferring to human");
            }
        }
        ApprovalDecision::Deny { reason } => {
            append_approval_log(
                &config.sessions_dir,
                session_id,
                &ApprovalLogEntry {
                    ts: chrono::Utc::now().to_rfc3339(),
                    decision: "deny".to_string(),
                    chunks: None,
                    reason: Some(reason.clone()),
                },
            );
            warn!(session_id, %reason, "auto-approver denied, deferring to human");
        }
        ApprovalDecision::Uncertain { reason } => {
            append_approval_log(
                &config.sessions_dir,
                session_id,
                &ApprovalLogEntry {
                    ts: chrono::Utc::now().to_rfc3339(),
                    decision: "uncertain".to_string(),
                    chunks: None,
                    reason: Some(reason.clone()),
                },
            );
            warn!(session_id, %reason, "auto-approver uncertain, deferring to human");
        }
    }
}

fn sanitize_notification_excerpt(input: &str) -> String {
    strip_ansi_for_body(input)
        .lines()
        .map(str::trim)
        .map(sanitize_body)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn build_notifier(db: Arc<Database>, config: &AppConfig) -> Notifier {
    let mut channels: Vec<Box<dyn NotificationChannel + Send + Sync>> =
        vec![Box::new(LocalOsNotificationChannel {
            hook: config.notification_hook.clone(),
        })];

    if let (Some(vapid_public_key), Some(vapid_private_key), Some(vapid_subject)) = (
        config.web_push_vapid_public_key.clone(),
        config.web_push_vapid_private_key.clone(),
        config.web_push_subject.clone(),
    ) {
        match WebPushChannel::new(&vapid_private_key, &vapid_public_key, &vapid_subject, db) {
            Ok(channel) => {
                info!("web push channel enabled");
                channels.push(Box::new(channel));
            }
            Err(err) => {
                warn!(%err, "web push channel init failed, continuing without it");
            }
        }
    }

    Notifier::with_channels(channels)
}
