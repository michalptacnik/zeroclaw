pub mod pid;
pub mod preflight;

use crate::config::Config;
use anyhow::Result;
use chrono::Utc;
use std::future::Future;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

const STATUS_FLUSH_SECONDS: u64 = 5;

/// Wait for shutdown signal (SIGINT or SIGTERM)
async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;

        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, shutting down...");
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        tracing::info!("Received Ctrl+C, shutting down...");
    }

    Ok(())
}

pub async fn run(config: Config, host: String, port: u16) -> Result<()> {
    // Acquire PID file to prevent concurrent daemon instances.
    let pid_path = config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("zeroclaw.pid");
    let _pid_guard = pid::PidFileGuard::acquire(pid_path).await?;

    let initial_backoff = config.reliability.channel_initial_backoff_secs.max(1);
    let max_backoff = config
        .reliability
        .channel_max_backoff_secs
        .max(initial_backoff);

    crate::health::mark_component_ok("daemon");

    // Best-effort preflight checks — warnings only, never block startup.
    preflight::run_preflight_checks(&config);

    if config.heartbeat.enabled {
        let _ =
            crate::heartbeat::engine::HeartbeatEngine::ensure_heartbeat_file(&config.workspace_dir)
                .await;
    }

    let shutdown_token = CancellationToken::new();
    let drain_secs = config.daemon.shutdown_drain_secs.max(1);

    let mut handles: Vec<JoinHandle<()>> =
        vec![spawn_state_writer(config.clone(), shutdown_token.clone())];

    {
        let gateway_cfg = config.clone();
        let gateway_host = host.clone();
        let token = shutdown_token.clone();
        handles.push(spawn_component_supervisor_cancellable(
            "gateway",
            initial_backoff,
            max_backoff,
            token,
            move || {
                let cfg = gateway_cfg.clone();
                let host = gateway_host.clone();
                async move { crate::gateway::run_gateway(&host, port, cfg).await }
            },
        ));
    }

    {
        if has_supervised_channels(&config) {
            let channels_cfg = config.clone();
            let token = shutdown_token.clone();
            handles.push(spawn_component_supervisor_cancellable(
                "channels",
                initial_backoff,
                max_backoff,
                token,
                move || {
                    let cfg = channels_cfg.clone();
                    async move { crate::channels::start_channels(cfg).await }
                },
            ));
        } else {
            crate::health::mark_component_ok("channels");
            tracing::info!("No real-time channels configured; channel supervisor disabled");
        }
    }

    if config.heartbeat.enabled {
        let heartbeat_cfg = config.clone();
        let token = shutdown_token.clone();
        handles.push(spawn_component_supervisor_cancellable(
            "heartbeat",
            initial_backoff,
            max_backoff,
            token,
            move || {
                let cfg = heartbeat_cfg.clone();
                async move { Box::pin(run_heartbeat_worker(cfg)).await }
            },
        ));
    }

    if config.cron.enabled {
        let scheduler_cfg = config.clone();
        let token = shutdown_token.clone();
        handles.push(spawn_component_supervisor_cancellable(
            "scheduler",
            initial_backoff,
            max_backoff,
            token,
            move || {
                let cfg = scheduler_cfg.clone();
                async move { crate::cron::scheduler::run(cfg).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("scheduler");
        tracing::info!("Cron disabled; scheduler supervisor not started");
    }

    println!("🧠 ZeroClaw daemon started");
    println!("   Gateway:  http://{host}:{port}");
    println!("   Components: gateway, channels, heartbeat, scheduler");
    println!("   Ctrl+C or SIGTERM to stop");

    // Wait for shutdown signal (SIGINT or SIGTERM)
    wait_for_shutdown_signal().await?;
    crate::health::mark_component_error("daemon", "shutdown requested");

    // Signal all supervised components to stop and give them time to drain.
    tracing::info!(drain_secs, "Shutdown requested — draining components");
    shutdown_token.cancel();

    let drain = async {
        for handle in handles {
            let _ = handle.await;
        }
    };

    if tokio::time::timeout(Duration::from_secs(drain_secs), drain)
        .await
        .is_err()
    {
        tracing::warn!(
            drain_secs,
            "Drain timeout exceeded — some components may not have shut down cleanly"
        );
    }

    Ok(())
}

pub fn state_file_path(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("daemon_state.json")
}

fn spawn_state_writer(config: Config, shutdown_token: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = state_file_path(&config);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(STATUS_FLUSH_SECONDS));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let mut json = crate::health::snapshot_json();
                    if let Some(obj) = json.as_object_mut() {
                        obj.insert(
                            "written_at".into(),
                            serde_json::json!(Utc::now().to_rfc3339()),
                        );
                    }
                    let data = serde_json::to_vec_pretty(&json).unwrap_or_else(|_| b"{}".to_vec());
                    let _ = tokio::fs::write(&path, data).await;
                }
                () = shutdown_token.cancelled() => {
                    // Write a final snapshot before exiting.
                    let mut json = crate::health::snapshot_json();
                    if let Some(obj) = json.as_object_mut() {
                        obj.insert(
                            "written_at".into(),
                            serde_json::json!(Utc::now().to_rfc3339()),
                        );
                    }
                    let data = serde_json::to_vec_pretty(&json).unwrap_or_else(|_| b"{}".to_vec());
                    let _ = tokio::fs::write(&path, data).await;
                    break;
                }
            }
        }
    })
}

fn spawn_component_supervisor<F, Fut>(
    name: &'static str,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    mut run_component: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        loop {
            crate::health::mark_component_ok(name);
            match run_component().await {
                Ok(()) => {
                    crate::health::mark_component_error(name, "component exited unexpectedly");
                    tracing::warn!("Daemon component '{name}' exited unexpectedly");
                    // Clean exit — reset backoff since the component ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    crate::health::mark_component_error(name, e.to_string());
                    tracing::error!("Daemon component '{name}' failed: {e}");
                }
            }

            crate::health::bump_component_restart(name);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

/// Like `spawn_component_supervisor` but exits the restart loop when
/// `shutdown_token` is cancelled.  The in-flight component future is
/// allowed to complete naturally; the cancellation only prevents the next
/// restart iteration.
fn spawn_component_supervisor_cancellable<F, Fut>(
    name: &'static str,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    shutdown_token: CancellationToken,
    mut run_component: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        loop {
            crate::health::mark_component_ok(name);

            // Run the component and race it against a shutdown signal.
            let result = tokio::select! {
                r = run_component() => r,
                () = shutdown_token.cancelled() => {
                    tracing::debug!("Daemon component '{name}' supervisor exiting due to shutdown");
                    return;
                }
            };

            // If shutdown was requested during the run we still exit cleanly.
            if shutdown_token.is_cancelled() {
                return;
            }

            match result {
                Ok(()) => {
                    crate::health::mark_component_error(name, "component exited unexpectedly");
                    tracing::warn!("Daemon component '{name}' exited unexpectedly");
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    crate::health::mark_component_error(name, e.to_string());
                    tracing::error!("Daemon component '{name}' failed: {e}");
                }
            }

            crate::health::bump_component_restart(name);

            // Wait for backoff or shutdown — whichever comes first.
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                () = shutdown_token.cancelled() => {
                    tracing::debug!(
                        "Daemon component '{name}' supervisor aborting backoff due to shutdown"
                    );
                    return;
                }
            }

            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

async fn run_heartbeat_worker(config: Config) -> Result<()> {
    let observer: std::sync::Arc<dyn crate::observability::Observer> =
        std::sync::Arc::from(crate::observability::create_observer(&config.observability));
    let engine = crate::heartbeat::engine::HeartbeatEngine::new(
        config.heartbeat.clone(),
        config.workspace_dir.clone(),
        observer,
    );
    let delivery = heartbeat_delivery_target(&config)?;

    let interval_mins = config.heartbeat.interval_minutes.max(5);
    let mut interval = tokio::time::interval(Duration::from_secs(u64::from(interval_mins) * 60));

    loop {
        interval.tick().await;

        let file_tasks = engine.collect_tasks().await?;
        let tasks = heartbeat_tasks_for_tick(file_tasks, config.heartbeat.message.as_deref());
        if tasks.is_empty() {
            continue;
        }

        for task in tasks {
            let prompt = format!("[Heartbeat Task] {task}");
            let temp = config.default_temperature;
            match crate::agent::run(
                config.clone(),
                Some(prompt),
                None,
                None,
                temp,
                vec![],
                false,
            )
            .await
            {
                Ok(output) => {
                    crate::health::mark_component_ok("heartbeat");
                    let announcement = if output.trim().is_empty() {
                        "heartbeat task executed".to_string()
                    } else {
                        output
                    };
                    if let Some((channel, target)) = &delivery {
                        if let Err(e) = crate::cron::scheduler::deliver_announcement(
                            &config,
                            channel,
                            target,
                            &announcement,
                        )
                        .await
                        {
                            crate::health::mark_component_error(
                                "heartbeat",
                                format!("delivery failed: {e}"),
                            );
                            tracing::warn!("Heartbeat delivery failed: {e}");
                        }
                    }
                }
                Err(e) => {
                    crate::health::mark_component_error("heartbeat", e.to_string());
                    tracing::warn!("Heartbeat task failed: {e}");
                }
            }
        }
    }
}

fn heartbeat_tasks_for_tick(
    file_tasks: Vec<String>,
    fallback_message: Option<&str>,
) -> Vec<String> {
    if !file_tasks.is_empty() {
        return file_tasks;
    }

    fallback_message
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(|message| vec![message.to_string()])
        .unwrap_or_default()
}

fn heartbeat_delivery_target(config: &Config) -> Result<Option<(String, String)>> {
    let channel = config
        .heartbeat
        .target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let target = config
        .heartbeat
        .to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match (channel, target) {
        (None, None) => Ok(None),
        (Some(_), None) => anyhow::bail!("heartbeat.to is required when heartbeat.target is set"),
        (None, Some(_)) => anyhow::bail!("heartbeat.target is required when heartbeat.to is set"),
        (Some(channel), Some(target)) => {
            validate_heartbeat_channel_config(config, channel)?;
            Ok(Some((channel.to_string(), target.to_string())))
        }
    }
}

fn validate_heartbeat_channel_config(config: &Config, channel: &str) -> Result<()> {
    match channel.to_ascii_lowercase().as_str() {
        "telegram" => {
            if config.channels_config.telegram.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to telegram but channels_config.telegram is not configured"
                );
            }
        }
        "discord" => {
            if config.channels_config.discord.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to discord but channels_config.discord is not configured"
                );
            }
        }
        "slack" => {
            if config.channels_config.slack.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to slack but channels_config.slack is not configured"
                );
            }
        }
        "mattermost" => {
            if config.channels_config.mattermost.is_none() {
                anyhow::bail!(
                    "heartbeat.target is set to mattermost but channels_config.mattermost is not configured"
                );
            }
        }
        other => anyhow::bail!("unsupported heartbeat.target channel: {other}"),
    }

    Ok(())
}

fn has_supervised_channels(config: &Config) -> bool {
    config
        .channels_config
        .channels_except_webhook()
        .iter()
        .any(|(_, ok)| *ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    #[test]
    fn state_file_path_uses_config_directory() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let path = state_file_path(&config);
        assert_eq!(path, tmp.path().join("daemon_state.json"));
    }

    #[tokio::test]
    async fn supervisor_marks_error_and_restart_on_failure() {
        let handle = spawn_component_supervisor("daemon-test-fail", 1, 1, || async {
            anyhow::bail!("boom")
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-fail"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("boom"));
    }

    #[tokio::test]
    async fn supervisor_marks_unexpected_exit_as_error() {
        let handle = spawn_component_supervisor("daemon-test-exit", 1, 1, || async { Ok(()) });

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-exit"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("component exited unexpectedly"));
    }

    #[test]
    fn detects_no_supervised_channels() {
        let config = Config::default();
        assert!(!has_supervised_channels(&config));
    }

    #[test]
    fn detects_supervised_channels_present() {
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "token".into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_dingtalk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.dingtalk = Some(crate::config::schema::DingTalkConfig {
            client_id: "client_id".into(),
            client_secret: "client_secret".into(),
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_mattermost_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.mattermost = Some(crate::config::schema::MattermostConfig {
            url: "https://mattermost.example.com".into(),
            bot_token: "token".into(),
            channel_id: Some("channel-id".into()),
            allowed_users: vec!["*".into()],
            thread_replies: Some(true),
            mention_only: Some(false),
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_qq_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.qq = Some(crate::config::schema::QQConfig {
            app_id: "app-id".into(),
            app_secret: "app-secret".into(),
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_nextcloud_talk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.nextcloud_talk = Some(crate::config::schema::NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: None,
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn heartbeat_tasks_use_file_tasks_when_available() {
        let tasks =
            heartbeat_tasks_for_tick(vec!["From file".to_string()], Some("Fallback from config"));
        assert_eq!(tasks, vec!["From file".to_string()]);
    }

    #[test]
    fn heartbeat_tasks_fall_back_to_config_message() {
        let tasks = heartbeat_tasks_for_tick(vec![], Some("  check london time  "));
        assert_eq!(tasks, vec!["check london time".to_string()]);
    }

    #[test]
    fn heartbeat_tasks_ignore_empty_fallback_message() {
        let tasks = heartbeat_tasks_for_tick(vec![], Some("   "));
        assert!(tasks.is_empty());
    }

    #[test]
    fn heartbeat_delivery_target_none_when_unset() {
        let config = Config::default();
        let target = heartbeat_delivery_target(&config).unwrap();
        assert!(target.is_none());
    }

    #[test]
    fn heartbeat_delivery_target_requires_to_field() {
        let mut config = Config::default();
        config.heartbeat.target = Some("telegram".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("heartbeat.to is required when heartbeat.target is set"));
    }

    #[test]
    fn heartbeat_delivery_target_requires_target_field() {
        let mut config = Config::default();
        config.heartbeat.to = Some("123456".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("heartbeat.target is required when heartbeat.to is set"));
    }

    #[test]
    fn heartbeat_delivery_target_rejects_unsupported_channel() {
        let mut config = Config::default();
        config.heartbeat.target = Some("email".into());
        config.heartbeat.to = Some("ops@example.com".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported heartbeat.target channel"));
    }

    #[test]
    fn heartbeat_delivery_target_requires_channel_configuration() {
        let mut config = Config::default();
        config.heartbeat.target = Some("telegram".into());
        config.heartbeat.to = Some("123456".into());
        let err = heartbeat_delivery_target(&config).unwrap_err();
        assert!(err
            .to_string()
            .contains("channels_config.telegram is not configured"));
    }

    #[test]
    fn heartbeat_delivery_target_accepts_telegram_configuration() {
        let mut config = Config::default();
        config.heartbeat.target = Some("telegram".into());
        config.heartbeat.to = Some("123456".into());
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "bot-token".into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
        });

        let target = heartbeat_delivery_target(&config).unwrap();
        assert_eq!(target, Some(("telegram".to_string(), "123456".to_string())));
    }
}
