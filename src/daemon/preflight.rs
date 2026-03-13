use crate::config::Config;

/// A single preflight check result.
#[derive(Debug)]
struct CheckResult {
    name: &'static str,
    ok: bool,
    message: String,
}

impl CheckResult {
    fn ok(name: &'static str) -> Self {
        Self {
            name,
            ok: true,
            message: String::new(),
        }
    }

    fn warn(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            message: message.into(),
        }
    }
}

/// Run all startup preflight checks against `config`.
///
/// Each check is best-effort: failures are logged as warnings but do not
/// prevent the daemon from starting.  Returns `true` if every check passed,
/// `false` if any check produced a warning.
pub fn run_preflight_checks(config: &Config) -> bool {
    let checks = [
        check_workspace_dir(config),
        check_api_key_present(config),
        check_scheduler_poll_secs(config),
        check_shutdown_drain_secs(config),
        check_circuit_breaker_threshold(config),
    ];

    let mut all_ok = true;
    for result in &checks {
        if result.ok {
            tracing::debug!(check = result.name, "Preflight OK");
        } else {
            all_ok = false;
            tracing::warn!(check = result.name, warning = %result.message, "Preflight warning");
        }
    }

    if all_ok {
        tracing::info!("All preflight checks passed");
    } else {
        tracing::warn!("One or more preflight checks produced warnings — daemon will still start");
    }

    all_ok
}

// ── Individual checks ─────────────────────────────────────────────────────

fn check_workspace_dir(config: &Config) -> CheckResult {
    let path = &config.workspace_dir;
    if path.exists() {
        CheckResult::ok("workspace_dir_exists")
    } else {
        CheckResult::warn(
            "workspace_dir_exists",
            format!(
                "Workspace directory does not exist: {}. \
                 It will be created on first use.",
                path.display()
            ),
        )
    }
}

fn check_api_key_present(config: &Config) -> CheckResult {
    // Accept if env var is set, explicit key is set, or model_providers has entries.
    let env_key = std::env::var("ZEROCLAW_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| std::env::var("API_KEY").ok().filter(|k| !k.is_empty()));

    if env_key.is_some()
        || config
            .api_key
            .as_deref()
            .map(|k| !k.is_empty())
            .unwrap_or(false)
        || !config.model_providers.is_empty()
    {
        CheckResult::ok("api_key_present")
    } else {
        CheckResult::warn(
            "api_key_present",
            "No API key configured. Set api_key in config.toml or ZEROCLAW_API_KEY env var.",
        )
    }
}

fn check_scheduler_poll_secs(config: &Config) -> CheckResult {
    const MIN_SENSIBLE_POLL: u64 = 5;
    let poll = config.reliability.scheduler_poll_secs;
    if poll >= MIN_SENSIBLE_POLL {
        CheckResult::ok("scheduler_poll_secs")
    } else {
        CheckResult::warn(
            "scheduler_poll_secs",
            format!(
                "reliability.scheduler_poll_secs is {poll}s, which is below the minimum \
                 effective value of {MIN_SENSIBLE_POLL}s. The scheduler will clamp it."
            ),
        )
    }
}

fn check_shutdown_drain_secs(config: &Config) -> CheckResult {
    const MAX_SENSIBLE_DRAIN: u64 = 300;
    let drain = config.daemon.shutdown_drain_secs;
    if drain <= MAX_SENSIBLE_DRAIN {
        CheckResult::ok("shutdown_drain_secs")
    } else {
        CheckResult::warn(
            "shutdown_drain_secs",
            format!(
                "daemon.shutdown_drain_secs is {drain}s, which is unusually high. \
                 Consider a value ≤ {MAX_SENSIBLE_DRAIN}s."
            ),
        )
    }
}

fn check_circuit_breaker_threshold(config: &Config) -> CheckResult {
    let threshold = config.reliability.provider_circuit_breaker_threshold;
    if threshold == 0 || threshold >= 3 {
        CheckResult::ok("provider_circuit_breaker_threshold")
    } else {
        CheckResult::warn(
            "provider_circuit_breaker_threshold",
            format!(
                "reliability.provider_circuit_breaker_threshold is {threshold}, which may \
                 be too aggressive. Consider a value ≥ 3 or 0 to disable."
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_key() -> Config {
        let mut config = Config::default();
        config.api_key = Some("test-key".to_string());
        // Ensure workspace dir exists (default points to home).
        config.workspace_dir = std::env::temp_dir().join("zeroclaw-preflight-test");
        let _ = std::fs::create_dir_all(&config.workspace_dir);
        config
    }

    #[test]
    fn preflight_passes_with_valid_config() {
        let config = config_with_key();
        assert!(run_preflight_checks(&config));
    }

    #[test]
    fn check_api_key_warns_when_missing() {
        let mut config = Config::default();
        config.api_key = None;
        config.model_providers.clear();
        // Temporarily ensure env vars are absent for this test.
        // We can't easily unset env vars portably, so just check the result
        // based on current env — only assert if env is clean.
        if std::env::var("ZEROCLAW_API_KEY").is_err() && std::env::var("API_KEY").is_err() {
            let result = check_api_key_present(&config);
            assert!(!result.ok);
            assert!(result.message.contains("No API key"));
        }
    }

    #[test]
    fn check_workspace_dir_warns_when_missing() {
        let mut config = Config::default();
        config.workspace_dir = std::path::PathBuf::from("/tmp/definitely-does-not-exist-xyz123");
        let result = check_workspace_dir(&config);
        assert!(!result.ok);
        assert!(result.message.contains("does not exist"));
    }

    #[test]
    fn check_scheduler_poll_secs_warns_below_minimum() {
        let mut config = Config::default();
        config.reliability.scheduler_poll_secs = 2;
        let result = check_scheduler_poll_secs(&config);
        assert!(!result.ok);
    }

    #[test]
    fn check_scheduler_poll_secs_ok_at_minimum() {
        let mut config = Config::default();
        config.reliability.scheduler_poll_secs = 5;
        let result = check_scheduler_poll_secs(&config);
        assert!(result.ok);
    }

    #[test]
    fn check_drain_secs_warns_when_very_large() {
        let mut config = Config::default();
        config.daemon.shutdown_drain_secs = 9999;
        let result = check_shutdown_drain_secs(&config);
        assert!(!result.ok);
    }

    #[test]
    fn check_circuit_breaker_threshold_warns_when_too_low() {
        let mut config = Config::default();
        config.reliability.provider_circuit_breaker_threshold = 1;
        let result = check_circuit_breaker_threshold(&config);
        assert!(!result.ok);
    }

    #[test]
    fn check_circuit_breaker_threshold_ok_when_zero() {
        let mut config = Config::default();
        config.reliability.provider_circuit_breaker_threshold = 0;
        let result = check_circuit_breaker_threshold(&config);
        assert!(result.ok);
    }
}
