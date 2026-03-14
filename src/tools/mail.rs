use super::traits::{Tool, ToolResult};
use crate::channels::email_channel::EmailConfig;
use anyhow::{anyhow, Result};
use async_imap::types::Fetch;
use async_imap::Session;
use async_trait::async_trait;
use futures_util::TryStreamExt;
use lettre::message::SinglePart;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use mail_parser::{MessageParser, MimeHeaders};
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::DnsName;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use uuid::Uuid;

type ImapSession = Session<TlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct MailTool {
    config: EmailConfig,
}

#[derive(Debug, Clone, Serialize)]
struct MailRecord {
    folder: String,
    message_id: String,
    from: String,
    to: Vec<String>,
    subject: String,
    timestamp: u64,
    body_preview: String,
}

impl MailTool {
    pub fn new(config: EmailConfig) -> Self {
        Self { config }
    }

    fn create_smtp_transport(&self) -> Result<SmtpTransport> {
        let creds = Credentials::new(self.config.username.clone(), self.config.password.clone());
        let transport = if self.config.smtp_tls {
            SmtpTransport::relay(&self.config.smtp_host)?
                .port(self.config.smtp_port)
                .credentials(creds)
                .build()
        } else {
            SmtpTransport::builder_dangerous(&self.config.smtp_host)
                .port(self.config.smtp_port)
                .credentials(creds)
                .build()
        };
        Ok(transport)
    }

    async fn connect_imap(&self) -> Result<ImapSession> {
        let addr = format!("{}:{}", self.config.imap_host, self.config.imap_port);
        let tcp = TcpStream::connect(&addr).await?;
        let certs = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let config = ClientConfig::builder()
            .with_root_certificates(certs)
            .with_no_client_auth();
        let tls_stream: TlsConnector = std::sync::Arc::new(config).into();
        let sni: DnsName = self.config.imap_host.clone().try_into()?;
        let stream = tls_stream.connect(sni.into(), tcp).await?;
        let client = async_imap::Client::new(stream);
        client
            .login(&self.config.username, &self.config.password)
            .await
            .map_err(|(error, _)| anyhow!("IMAP login failed: {error}"))
    }

    fn extract_text(parsed: &mail_parser::Message) -> String {
        if let Some(text) = parsed.body_text(0) {
            return text.to_string();
        }
        if let Some(html) = parsed.body_html(0) {
            return html
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(400)
                .collect();
        }
        for part in parsed.attachments() {
            let part: &mail_parser::MessagePart = part;
            if let Some(ct) = MimeHeaders::content_type(part) {
                if ct.ctype() == "text" {
                    if let Ok(text) = std::str::from_utf8(part.contents()) {
                        return text.chars().take(400).collect();
                    }
                }
            }
        }
        String::new()
    }

    fn parse_fetch(folder: &str, fetch: &Fetch) -> Option<MailRecord> {
        let body = fetch.body()?;
        let parsed = MessageParser::default().parse(body)?;
        let from = parsed
            .from()
            .and_then(|addresses| addresses.first())
            .and_then(|address| address.address())
            .unwrap_or("unknown")
            .to_string();
        let to = parsed
            .to()
            .map(|addresses| {
                addresses
                    .iter()
                    .filter_map(|address| address.address())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let subject = parsed.subject().unwrap_or("(no subject)").to_string();
        let message_id = parsed
            .message_id()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("generated-{}", Uuid::new_v4()));
        #[allow(clippy::cast_sign_loss)]
        let timestamp = parsed
            .date()
            .map(|date| {
                let naive = chrono::NaiveDate::from_ymd_opt(
                    i32::from(date.year),
                    u32::from(date.month),
                    u32::from(date.day),
                )
                .and_then(|day| {
                    day.and_hms_opt(
                        u32::from(date.hour),
                        u32::from(date.minute),
                        u32::from(date.second),
                    )
                });
                naive.map_or(0, |value| value.and_utc().timestamp() as u64)
            })
            .unwrap_or(0);

        Some(MailRecord {
            folder: folder.to_string(),
            message_id,
            from,
            to,
            subject,
            timestamp,
            body_preview: Self::extract_text(&parsed),
        })
    }

    async fn fetch_folder_recent(&self, folder: &str, limit: usize) -> Result<Vec<MailRecord>> {
        let mut session = self.connect_imap().await?;
        session.select(folder).await?;
        let mut uids: Vec<u32> = session.uid_search("ALL").await?.into_iter().collect();
        uids.sort_unstable();
        let selected: Vec<u32> = uids.into_iter().rev().take(limit.max(1)).collect();
        if selected.is_empty() {
            let _ = session.logout().await;
            return Ok(Vec::new());
        }
        let uid_set = selected
            .iter()
            .rev()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetch_stream = session.uid_fetch(&uid_set, "RFC822").await?;
        let fetches: Vec<Fetch> = fetch_stream.try_collect().await?;
        let _ = session.logout().await;
        Ok(fetches
            .iter()
            .filter_map(|fetch| Self::parse_fetch(folder, fetch))
            .collect())
    }

    fn candidate_folders(&self, explicit_folder: Option<&str>) -> Vec<String> {
        let mut folders = Vec::new();
        if let Some(folder) = explicit_folder
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            folders.push(folder.to_string());
        }
        folders.push("Sent".to_string());
        folders.push("Sent Messages".to_string());
        folders.push("INBOX".to_string());
        folders.push(self.config.imap_folder.clone());
        folders.sort();
        folders.dedup();
        folders
    }

    fn send_metadata(
        &self,
        message_id: &str,
        to: &str,
        subject: &str,
        timestamp: u64,
    ) -> ToolResult {
        ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "message_id": message_id,
                "from": self.config.from_address,
                "to": [to],
                "subject": subject,
                "timestamp": timestamp,
            }))
            .unwrap_or_default(),
            error: None,
        }
    }
}

#[async_trait]
impl Tool for MailTool {
    fn name(&self) -> &str {
        "mail"
    }

    fn description(&self) -> &str {
        "Send and verify email over the configured IMAP/SMTP account. Prefer this over shell or browser email flows when available."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send", "list_recent", "get", "verify_sent"]
                },
                "to": { "type": "string" },
                "subject": { "type": "string" },
                "body": { "type": "string" },
                "message_id": { "type": "string" },
                "attempt_id": { "type": "string" },
                "folder": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'action'"))?;

        match action {
            "send" => {
                let to = args
                    .get("to")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Missing 'to'"))?;
                let subject = args
                    .get("subject")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Missing 'subject'"))?;
                let body = args
                    .get("body")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Missing 'body'"))?;
                let client_message_id = format!("<zeroclaw-{}@local>", Uuid::new_v4());
                let email = Message::builder()
                    .from(self.config.from_address.parse()?)
                    .to(to.parse()?)
                    .subject(subject)
                    .singlepart(SinglePart::plain(body.to_string()))?;
                let transport = self.create_smtp_transport()?;
                transport.send(&email)?;
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO)
                    .as_secs();
                Ok(self.send_metadata(&client_message_id, to, subject, timestamp))
            }
            "list_recent" => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                    .unwrap_or(10);
                let folder = args.get("folder").and_then(Value::as_str);
                let mut records = Vec::new();
                for candidate in self.candidate_folders(folder) {
                    if let Ok(mut batch) = self.fetch_folder_recent(&candidate, limit).await {
                        records.append(&mut batch);
                    }
                }
                records.sort_by_key(|record| std::cmp::Reverse(record.timestamp));
                records.truncate(limit);
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&records).unwrap_or_default(),
                    error: None,
                })
            }
            "get" => {
                let message_id = args
                    .get("message_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Missing 'message_id'"))?;
                for candidate in self.candidate_folders(args.get("folder").and_then(Value::as_str))
                {
                    let records = self
                        .fetch_folder_recent(&candidate, 25)
                        .await
                        .unwrap_or_default();
                    if let Some(record) = records
                        .into_iter()
                        .find(|record| record.message_id.eq_ignore_ascii_case(message_id))
                    {
                        return Ok(ToolResult {
                            success: true,
                            output: serde_json::to_string_pretty(&record).unwrap_or_default(),
                            error: None,
                        });
                    }
                }
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Message not found: {message_id}")),
                })
            }
            "verify_sent" => {
                let attempt_id = args
                    .get("attempt_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Missing 'attempt_id'"))?;
                let message_id = args.get("message_id").and_then(Value::as_str);
                let subject = args.get("subject").and_then(Value::as_str);
                let to = args.get("to").and_then(Value::as_str);
                let timestamp_cutoff = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO)
                    .as_secs()
                    .saturating_sub(900);

                for candidate in self.candidate_folders(args.get("folder").and_then(Value::as_str))
                {
                    let records = self
                        .fetch_folder_recent(&candidate, 25)
                        .await
                        .unwrap_or_default();
                    if let Some(record) = records.into_iter().find(|record| {
                        record.timestamp >= timestamp_cutoff
                            && message_id.is_none_or(|expected| {
                                record.message_id.eq_ignore_ascii_case(expected)
                            })
                            && subject.is_none_or(|expected| record.subject == expected)
                            && to.is_none_or(|expected| {
                                record
                                    .to
                                    .iter()
                                    .any(|recipient| recipient.eq_ignore_ascii_case(expected))
                            })
                    }) {
                        return Ok(ToolResult {
                            success: true,
                            output: serde_json::to_string_pretty(&json!({
                                "verified": true,
                                "message": record,
                                "proof": {
                                    "attempt_id": attempt_id,
                                    "summary": "Fresh email verification succeeded. Stop and report success.",
                                    "artifact_id": record.message_id,
                                    "fresh": true,
                                    "terminal": true,
                                }
                            }))
                            .unwrap_or_default(),
                            error: None,
                        });
                    }
                }
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Unable to verify a fresh sent message for this attempt".to_string(),
                    ),
                })
            }
            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unsupported mail action: {other}")),
            }),
        }
    }
}
