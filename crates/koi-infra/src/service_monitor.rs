//! Lightweight, configuration-driven service monitoring.
//!
//! This module deliberately performs only deterministic checks. It does not invoke the Agent's
//! tools or a model; a state transition is converted into an alert ingress by `MonitorAlertSink`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::alerts::{AlertInput, MONITOR_SOURCE_NAME};

/// Basic check types supported by the built-in monitor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceCheckKind {
    Http,
    Tcp,
    /// Checks the native service manager: `sc.exe` on Windows and `systemctl` on Unix.
    Service,
}

/// One configured service check.
#[derive(Clone, Debug, Deserialize)]
pub struct ServiceCheckConfig {
    /// Stable identifier used for state tracking and alert deduplication.
    pub id: String,
    pub kind: ServiceCheckKind,
    /// URL, `host:port`, or native service name depending on `kind`.
    pub target: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default = "default_check_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub expected_status: Option<u16>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub failure_threshold: Option<u32>,
    #[serde(default)]
    pub recovery_threshold: Option<u32>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// Runtime configuration for the built-in monitor.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ServiceMonitorConfig {
    pub enabled: bool,
    pub instance: String,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    pub failure_threshold: u32,
    pub recovery_threshold: u32,
    pub checks: Vec<ServiceCheckConfig>,
}

impl Default for ServiceMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            instance: "local".into(),
            interval_secs: 30,
            timeout_secs: 5,
            failure_threshold: 3,
            recovery_threshold: 2,
            checks: Vec::new(),
        }
    }
}

fn default_severity() -> String {
    "warning".into()
}

const fn default_check_enabled() -> bool {
    true
}

/// The monitor's only output capability. Keeping this boundary small makes the monitor usable
/// with the event store adapter without giving it access to Agent tools or control operations.
#[async_trait]
pub trait MonitorAlertSink: Send + Sync {
    async fn ingest_monitor_alert(&self, alert: AlertInput) -> Result<(), String>;
}

/// A long-running local monitor that emits only firing and resolved transitions.
pub struct ServiceMonitor {
    config: ServiceMonitorConfig,
    sink: Arc<dyn MonitorAlertSink>,
    client: Client,
    states: BTreeMap<String, CheckState>,
}

impl ServiceMonitor {
    /// Creates and validates a service monitor.
    ///
    /// # Errors
    ///
    /// Returns an error when the polling or threshold configuration is invalid, a check has a
    /// duplicate identifier, or an HTTP client cannot be built.
    pub fn new<S>(
        config: ServiceMonitorConfig,
        sink: Arc<S>,
    ) -> Result<Self, ServiceMonitorConfigError>
    where
        S: MonitorAlertSink + 'static,
    {
        validate_config(&config)?;
        let client = Client::builder()
            .user_agent("koi-service-monitor/0.1")
            .build()
            .map_err(|error| ServiceMonitorConfigError::HttpClient(error.to_string()))?;
        let states = config
            .checks
            .iter()
            .map(|check| (check.id.clone(), CheckState::default()))
            .collect();
        let sink: Arc<dyn MonitorAlertSink> = sink;
        Ok(Self {
            config,
            sink,
            client,
            states,
        })
    }

    /// Runs until the supplied cancellation token is cancelled.
    pub async fn run(mut self, shutdown: CancellationToken) {
        if !self.config.enabled {
            info!("服务监测未启用");
            return;
        }
        if self.config.checks.iter().all(|check| !check.enabled) {
            info!("服务监测已启用，但没有启用的检查项");
            return;
        }

        let mut interval = time::interval(Duration::from_secs(self.config.interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        info!(
            instance = %self.config.instance,
            interval_secs = self.config.interval_secs,
            "服务监测已启动"
        );
        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    info!("服务监测已停止");
                    break;
                }
                _ = interval.tick() => self.run_cycle().await,
            }
        }
    }

    async fn run_cycle(&mut self) {
        for index in 0..self.config.checks.len() {
            if !self.config.checks[index].enabled {
                continue;
            }
            let check = self.config.checks[index].clone();
            let outcome = self.perform_check(&check).await;
            let previous = self.states.get(&check.id).cloned().unwrap_or_default();
            let alert = {
                let state = self.states.entry(check.id.clone()).or_default();
                transition_alert(
                    &self.config.instance,
                    self.config.failure_threshold,
                    self.config.recovery_threshold,
                    &check,
                    state,
                    &outcome,
                )
            };
            let Some(alert) = alert else {
                continue;
            };
            if let Err(error) = self.sink.ingest_monitor_alert(alert).await {
                error!(check_id = %check.id, %error, "写入服务监测告警失败，将在下次状态变化时重试");
                self.states.insert(check.id.clone(), previous);
            }
        }
    }

    async fn perform_check(&self, check: &ServiceCheckConfig) -> CheckOutcome {
        let timeout = Duration::from_secs(
            check
                .timeout_secs
                .unwrap_or(self.config.timeout_secs)
                .max(1),
        );
        match check.kind {
            ServiceCheckKind::Http => self.check_http(check, timeout).await,
            ServiceCheckKind::Tcp => self.check_tcp(check, timeout).await,
            ServiceCheckKind::Service => check_native_service(&check.target, timeout).await,
        }
    }

    async fn check_http(&self, check: &ServiceCheckConfig, timeout: Duration) -> CheckOutcome {
        let expected_status = check.expected_status.unwrap_or(200);
        let response = time::timeout(
            timeout,
            self.client.get(&check.target).timeout(timeout).send(),
        )
        .await;
        match response {
            Ok(Ok(response)) if response.status().as_u16() == expected_status => {
                CheckOutcome::healthy(format!("HTTP {}", response.status().as_u16()))
            }
            Ok(Ok(response)) => CheckOutcome::unhealthy(format!(
                "HTTP 状态码为 {}，期望 {expected_status}",
                response.status().as_u16()
            )),
            Ok(Err(_)) => CheckOutcome::unhealthy("HTTP 请求失败"),
            Err(_) => CheckOutcome::unhealthy("HTTP 请求超时"),
        }
    }

    async fn check_tcp(&self, check: &ServiceCheckConfig, timeout: Duration) -> CheckOutcome {
        match time::timeout(timeout, TcpStream::connect(&check.target)).await {
            Ok(Ok(_)) => CheckOutcome::healthy("TCP 连接成功"),
            Ok(Err(_)) => CheckOutcome::unhealthy("TCP 连接失败"),
            Err(_) => CheckOutcome::unhealthy("TCP 连接超时"),
        }
    }
}

fn validate_config(config: &ServiceMonitorConfig) -> Result<(), ServiceMonitorConfigError> {
    if config.instance.trim().is_empty() {
        return Err(ServiceMonitorConfigError::Invalid(
            "monitor.instance 不能为空".into(),
        ));
    }
    if config.interval_secs == 0 {
        return Err(ServiceMonitorConfigError::Invalid(
            "monitor.interval_secs 必须大于零".into(),
        ));
    }
    if config.timeout_secs == 0 {
        return Err(ServiceMonitorConfigError::Invalid(
            "monitor.timeout_secs 必须大于零".into(),
        ));
    }
    if config.failure_threshold == 0 || config.recovery_threshold == 0 {
        return Err(ServiceMonitorConfigError::Invalid(
            "monitor 的失败和恢复阈值必须大于零".into(),
        ));
    }

    let mut ids = BTreeSet::new();
    for check in &config.checks {
        if check.id.trim().is_empty() || check.id.chars().count() > 128 {
            return Err(ServiceMonitorConfigError::Invalid(format!(
                "检查项 id 无效：{}",
                check.id
            )));
        }
        if !ids.insert(check.id.clone()) {
            return Err(ServiceMonitorConfigError::DuplicateCheck(check.id.clone()));
        }
        if check.target.trim().is_empty() || check.target.chars().count() > 2_048 {
            return Err(ServiceMonitorConfigError::Invalid(format!(
                "检查项 {} 的 target 无效",
                check.id
            )));
        }
        if check.target.contains('\0') {
            return Err(ServiceMonitorConfigError::Invalid(format!(
                "检查项 {} 的 target 不能包含 NUL 字符",
                check.id
            )));
        }
        if check
            .timeout_secs
            .is_some_and(|timeout_secs| timeout_secs == 0)
        {
            return Err(ServiceMonitorConfigError::Invalid(format!(
                "检查项 {} 的 timeout_secs 必须大于零",
                check.id
            )));
        }
        if check
            .failure_threshold
            .is_some_and(|threshold| threshold == 0)
            || check
                .recovery_threshold
                .is_some_and(|threshold| threshold == 0)
        {
            return Err(ServiceMonitorConfigError::Invalid(format!(
                "检查项 {} 的阈值必须大于零",
                check.id
            )));
        }
        if matches!(check.kind, ServiceCheckKind::Http) {
            let url = reqwest::Url::parse(&check.target).map_err(|error| {
                ServiceMonitorConfigError::Invalid(format!(
                    "检查项 {} 的 HTTP target 无效：{error}",
                    check.id
                ))
            })?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(ServiceMonitorConfigError::Invalid(format!(
                    "检查项 {} 的 HTTP target 必须使用 http 或 https",
                    check.id
                )));
            }
        }
    }
    Ok(())
}

async fn check_native_service(target: &str, timeout: Duration) -> CheckOutcome {
    #[cfg(windows)]
    {
        let mut command = Command::new("sc.exe");
        command.args(["query", target]).kill_on_drop(true);
        let output = time::timeout(timeout, command.output()).await;
        match output {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if output.status.success() && stdout.lines().any(|line| line.contains("RUNNING")) {
                    CheckOutcome::healthy("Windows 服务正在运行")
                } else {
                    CheckOutcome::unhealthy("Windows 服务未处于 RUNNING 状态")
                }
            }
            Ok(Err(_)) => CheckOutcome::unhealthy("无法查询 Windows 服务"),
            Err(_) => CheckOutcome::unhealthy("查询 Windows 服务超时"),
        }
    }

    #[cfg(unix)]
    {
        let mut command = Command::new("systemctl");
        command
            .args(["is-active", "--quiet", "--", target])
            .kill_on_drop(true);
        let output = time::timeout(timeout, command.status()).await;
        match output {
            Ok(Ok(status)) if status.success() => CheckOutcome::healthy("systemd 服务处于 active"),
            Ok(Ok(_)) => CheckOutcome::unhealthy("systemd 服务未处于 active 状态"),
            Ok(Err(_)) => CheckOutcome::unhealthy("无法查询 systemd 服务"),
            Err(_) => CheckOutcome::unhealthy("查询 systemd 服务超时"),
        }
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, timeout);
        CheckOutcome::unhealthy("当前平台不支持原生服务检查")
    }
}

#[derive(Clone, Debug, Default)]
struct CheckState {
    status: CheckStatus,
    consecutive_failures: u32,
    consecutive_recoveries: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CheckStatus {
    #[default]
    Healthy,
    Firing,
}

#[derive(Clone, Debug)]
struct CheckOutcome {
    healthy: bool,
    detail: String,
}

impl CheckOutcome {
    fn healthy(detail: impl Into<String>) -> Self {
        Self {
            healthy: true,
            detail: detail.into(),
        }
    }

    fn unhealthy(detail: impl Into<String>) -> Self {
        Self {
            healthy: false,
            detail: detail.into(),
        }
    }
}

fn transition_alert(
    instance: &str,
    default_failure_threshold: u32,
    default_recovery_threshold: u32,
    check: &ServiceCheckConfig,
    state: &mut CheckState,
    outcome: &CheckOutcome,
) -> Option<AlertInput> {
    let failure_threshold = check.failure_threshold.unwrap_or(default_failure_threshold);
    let recovery_threshold = check
        .recovery_threshold
        .unwrap_or(default_recovery_threshold);

    if outcome.healthy {
        state.consecutive_failures = 0;
        if state.status != CheckStatus::Firing {
            state.consecutive_recoveries = 0;
            return None;
        }
        state.consecutive_recoveries = state.consecutive_recoveries.saturating_add(1);
        if state.consecutive_recoveries < recovery_threshold {
            return None;
        }
        state.status = CheckStatus::Healthy;
        state.consecutive_recoveries = 0;
        return Some(make_alert(instance, check, "resolved", &outcome.detail));
    }

    state.consecutive_recoveries = 0;
    if state.status == CheckStatus::Firing {
        return None;
    }
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    if state.consecutive_failures < failure_threshold {
        return None;
    }
    state.status = CheckStatus::Firing;
    state.consecutive_failures = 0;
    Some(make_alert(instance, check, "firing", &outcome.detail))
}

fn make_alert(
    instance: &str,
    check: &ServiceCheckConfig,
    status: &str,
    detail: &str,
) -> AlertInput {
    let display_name = check.name.as_deref().unwrap_or(check.id.as_str());
    let mut labels = check.labels.clone();
    labels.insert("check_id".into(), check.id.clone());
    labels.insert(
        "check_kind".into(),
        format!("{:?}", check.kind).to_ascii_lowercase(),
    );
    labels.insert("monitor_instance".into(), instance.into());
    labels.insert("state".into(), status.into());
    let summary = if status == "resolved" {
        format!("{display_name} 已恢复：{detail}")
    } else {
        format!("{display_name} 检查失败：{detail}")
    };
    AlertInput {
        source: MONITOR_SOURCE_NAME.into(),
        source_instance: instance.into(),
        native_event_id: format!("{}:{status}", check.id),
        name: "service_unhealthy".into(),
        severity: check.severity.clone(),
        summary,
        labels,
        scope: koi_core::domain::Scope::new("service", check.id.clone()),
        occurred_at: Utc::now(),
    }
}

#[derive(Debug, Error)]
pub enum ServiceMonitorConfigError {
    #[error("服务监测配置无效：{0}")]
    Invalid(String),
    #[error("服务监测检查项重复：{0}")]
    DuplicateCheck(String),
    #[error("服务监测 HTTP 客户端初始化失败：{0}")]
    HttpClient(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check() -> ServiceCheckConfig {
        ServiceCheckConfig {
            id: "api".into(),
            kind: ServiceCheckKind::Http,
            target: "http://127.0.0.1:8080/healthz".into(),
            name: Some("API".into()),
            severity: "critical".into(),
            enabled: true,
            expected_status: Some(200),
            timeout_secs: None,
            failure_threshold: None,
            recovery_threshold: None,
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn emits_only_after_failure_and_recovery_thresholds() {
        let check = check();
        let mut state = CheckState::default();
        let failed = CheckOutcome::unhealthy("HTTP 请求失败");
        assert!(transition_alert("local", 3, 2, &check, &mut state, &failed).is_none());
        assert!(transition_alert("local", 3, 2, &check, &mut state, &failed).is_none());
        let firing = transition_alert("local", 3, 2, &check, &mut state, &failed).unwrap();
        assert_eq!(firing.native_event_id, "api:firing");
        assert_eq!(firing.labels["state"], "firing");
        assert!(
            transition_alert(
                "local",
                3,
                2,
                &check,
                &mut state,
                &CheckOutcome::healthy("HTTP 200")
            )
            .is_none()
        );
        let resolved = transition_alert(
            "local",
            3,
            2,
            &check,
            &mut state,
            &CheckOutcome::healthy("HTTP 200"),
        )
        .unwrap();
        assert_eq!(resolved.native_event_id, "api:resolved");
        assert_eq!(resolved.labels["state"], "resolved");
    }

    #[test]
    fn rejects_invalid_http_targets() {
        let mut config = ServiceMonitorConfig::default();
        config.checks.push(ServiceCheckConfig {
            target: "not-a-url".into(),
            ..check()
        });
        assert!(ServiceMonitor::new(config, Arc::new(TestSink)).is_err());
    }

    struct TestSink;

    #[async_trait]
    impl MonitorAlertSink for TestSink {
        async fn ingest_monitor_alert(&self, _alert: AlertInput) -> Result<(), String> {
            Ok(())
        }
    }
}
