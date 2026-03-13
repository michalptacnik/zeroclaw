use super::traits::{ProofArtifact, Tool, ToolResult, ToolResultMetadata};
use crate::channels::email_channel::{EmailChannel, EmailConfig};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct MailTool {
    security: Arc<SecurityPolicy>,
    config: Option<EmailConfig>,
}

impl MailTool {
    pub fn new(security: Arc<SecurityPolicy>, config: Option<EmailConfig>) -> Self {
        Self { security, config }
    }

    fn channel(&self) -> anyhow::Result<EmailChannel> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Email is not configured in channels_config.email"))?;
        Ok(EmailChannel::new(config))
    }

    fn require_mutation_budget(&self) -> anyhow::Result<()> {
        if !self.security.can_act() {
            anyhow::bail!("Action blocked: autonomy is read-only");
        }
        if !self.security.record_action() {
            anyhow::bail!("Action blocked: rate limit exceeded");
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for MailTool {
    fn name(&self) -> &str {
        "mail"
    }

    fn description(&self) -> &str {
        "Send and verify email over the configured IMAP/SMTP account. Prefer this over browser email when channels_config.email is configured."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send", "list_recent", "get", "verify_sent"]
                },
                "recipient": {
                    "type": "string",
                    "description": "Recipient email address for send"
                },
                "subject": {
                    "type": "string",
                    "description": "Optional subject for send"
                },
                "body": {
                    "type": "string",
                    "description": "Plain-text message body for send"
                },
                "message_id": {
                    "type": "string",
                    "description": "Message-ID to fetch or verify"
                },
                "folder": {
                    "type": "string",
                    "description": "Optional IMAP folder override"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of recent emails to list"
                },
                "attempt_id": {
                    "type": "string",
                    "description": "Run-scoped attempt ID for fresh-proof tagging"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;
        let channel = self.channel()?;
        let attempt_id = args
            .get("attempt_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        match action {
            "send" => {
                self.require_mutation_budget()?;

                let recipient = args
                    .get("recipient")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Missing 'recipient' parameter"))?;
                let body = args
                    .get("body")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Missing 'body' parameter"))?;
                let subject = args.get("subject").and_then(Value::as_str);

                let generated_message_id = format!(
                    "<zeroclaw-{}-{}@localhost>",
                    attempt_id.as_deref().unwrap_or("attempt"),
                    uuid::Uuid::new_v4()
                );
                let receipt =
                    channel.send_email(recipient, subject, body, Some(&generated_message_id))?;

                Ok(ToolResult::ok(
                    serde_json::to_string_pretty(&json!({
                        "status": "sent",
                        "message_id": receipt.message_id,
                        "from": channel.config.from_address,
                        "recipient": receipt.recipient,
                        "subject": receipt.subject,
                        "sent_at": receipt.sent_at,
                    }))
                    .unwrap_or_default(),
                )
                .with_metadata(ToolResultMetadata {
                    attempt_id,
                    turn_estimate: Some(2),
                    ..ToolResultMetadata::default()
                }))
            }
            "list_recent" => {
                let folder = args.get("folder").and_then(Value::as_str);
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(10)
                    .min(50) as usize;
                let messages = channel.list_recent_messages(folder, limit).await?;
                Ok(ToolResult::ok(
                    serde_json::to_string_pretty(&json!({ "messages": messages }))
                        .unwrap_or_default(),
                ))
            }
            "get" => {
                let message_id = args
                    .get("message_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Missing 'message_id' parameter"))?;
                let folder = args.get("folder").and_then(Value::as_str);
                let message = channel.get_message(message_id, folder).await?;
                Ok(ToolResult::ok(
                    serde_json::to_string_pretty(&json!({ "message": message }))
                        .unwrap_or_default(),
                ))
            }
            "verify_sent" => {
                let message_id = args
                    .get("message_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Missing 'message_id' parameter"))?;
                let verified = channel.verify_sent_message(message_id).await?;
                if let Some(message) = verified {
                    return Ok(ToolResult::ok(
                        serde_json::to_string_pretty(&json!({
                            "verified": true,
                            "message": message,
                        }))
                        .unwrap_or_default(),
                    )
                    .with_metadata(ToolResultMetadata {
                        attempt_id: attempt_id.clone(),
                        proofs: vec![ProofArtifact {
                            kind: "email_message".into(),
                            id: message_id.to_string(),
                            summary: format!(
                                "Fresh email proof from folder {} to {}",
                                message.folder,
                                message.recipients.join(", ")
                            ),
                            created_at: Some(chrono::Utc::now().to_rfc3339()),
                            attempt_id,
                            fresh: true,
                            fallback: false,
                            terminal: false,
                        }],
                        turn_estimate: Some(1),
                        ..ToolResultMetadata::default()
                    }));
                }

                Ok(ToolResult::failure(format!(
                    "Sent-message proof not found for message_id {message_id}"
                ))
                .with_metadata(ToolResultMetadata {
                    attempt_id,
                    turn_estimate: Some(1),
                    ..ToolResultMetadata::default()
                }))
            }
            _ => Ok(ToolResult::failure(format!("Unknown action: {action}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;

    #[test]
    fn mail_tool_schema_mentions_verify_sent() {
        let tool = MailTool::new(Arc::new(SecurityPolicy::default()), None);
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("verify_sent")));
    }

    #[tokio::test]
    async fn mail_tool_reports_missing_config() {
        let tool = MailTool::new(Arc::new(SecurityPolicy::default()), None);
        let err = tool
            .execute(json!({"action":"list_recent"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Email is not configured"));
    }
}
