//! Normalized alert inputs shared by local monitors and external webhook adapters.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use koi_core::domain::{Scope, SourceName};
use serde_json::{Map, Value};
use thiserror::Error;

/// The source used by the built-in service monitor.
pub const MONITOR_SOURCE_NAME: &str = "monitor";
/// The default source used by the external alert webhook.
pub const ALERTMANAGER_SOURCE_NAME: &str = "alertmanager";
/// An alternative generic source name for deployments that do not use Alertmanager.
pub const WEBHOOK_SOURCE_NAME: &str = "webhook";

const MAX_ALERTS_PER_REQUEST: usize = 100;
const MAX_LABELS_PER_ALERT: usize = 64;
const MAX_LABEL_CHARS: usize = 128;
const MAX_LABEL_VALUE_CHARS: usize = 1_024;
const MAX_TEXT_CHARS: usize = 4_000;

/// A source-independent alert after transport-specific parsing.
#[derive(Clone, Debug)]
pub struct AlertInput {
    pub source: String,
    pub source_instance: String,
    pub native_event_id: String,
    pub name: String,
    pub severity: String,
    pub summary: String,
    /// 来自可信告警规则配置的简短说明；外部 Webhook 不会凭请求正文设置该字段。
    pub description: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub scope: Scope,
    pub occurred_at: DateTime<Utc>,
}

/// Parses either an Alertmanager-compatible payload or one normalized alert object.
///
/// The `source` argument is supplied by server configuration rather than the request body. This
/// prevents an untrusted webhook payload from choosing a different authorization source.
///
/// # Errors
///
/// Returns an error when the source name or JSON payload is invalid, or when the payload exceeds
/// the bounded alert and label limits.
pub fn parse_webhook_alerts(
    payload: &Value,
    source: &str,
) -> Result<Vec<AlertInput>, AlertPayloadError> {
    SourceName::new(source)
        .map_err(|error| AlertPayloadError::Invalid(format!("Webhook 来源无效：{error}")))?;
    let object = payload.as_object().ok_or(AlertPayloadError::NotObject)?;
    if let Some(alerts) = object.get("alerts") {
        let alerts = alerts
            .as_array()
            .ok_or_else(|| AlertPayloadError::Invalid("alerts 必须是数组".into()))?;
        if alerts.is_empty() {
            return Err(AlertPayloadError::Empty);
        }
        if alerts.len() > MAX_ALERTS_PER_REQUEST {
            return Err(AlertPayloadError::TooMany(MAX_ALERTS_PER_REQUEST));
        }
        alerts
            .iter()
            .enumerate()
            .map(|(index, alert)| parse_grouped_alert(object, alert, source, index))
            .collect()
    } else {
        Ok(vec![parse_single_alert(object, source)?])
    }
}

fn parse_grouped_alert(
    root: &Map<String, Value>,
    raw_alert: &Value,
    source: &str,
    index: usize,
) -> Result<AlertInput, AlertPayloadError> {
    let alert = raw_alert
        .as_object()
        .ok_or_else(|| AlertPayloadError::Invalid(format!("alerts[{index}] 必须是对象")))?;
    let mut labels = collect_labels(root.get("groupLabels"), "groupLabels")?;
    merge_labels(
        &mut labels,
        collect_labels(root.get("commonLabels"), "commonLabels")?,
    )?;
    merge_labels(&mut labels, collect_labels(alert.get("labels"), "labels")?)?;

    let name = clean_text(
        string_from(alert, &["name", "alertname"])
            .or_else(|| label_value(&labels, "alertname"))
            .or_else(|| string_from(root, &["name", "alertname"]))
            .or_else(|| Some("external_alert".into())),
        "告警名称",
        MAX_TEXT_CHARS,
    )?;
    let state = normalize_state(
        string_from(alert, &["status", "state"])
            .or_else(|| string_from(root, &["status", "state"]))
            .as_deref(),
    );
    let severity = clean_text(
        label_value(&labels, "severity")
            .or_else(|| string_from(alert, &["severity", "priority"]))
            .or_else(|| Some("warning".into())),
        "严重级别",
        64,
    )?;
    let summary = clean_text(
        nested_string(alert, "annotations", &["summary", "description"])
            .or_else(|| nested_string(root, "commonAnnotations", &["summary", "description"]))
            .or_else(|| string_from(alert, &["summary", "message", "description"]))
            .or_else(|| Some(name.clone())),
        "告警摘要",
        MAX_TEXT_CHARS,
    )?;
    let source_instance = clean_text(
        string_from(alert, &["sourceInstance", "source_instance"])
            .or_else(|| string_from(root, &["sourceInstance", "source_instance", "receiver"]))
            .or_else(|| Some("alertmanager".into())),
        "来源实例",
        MAX_TEXT_CHARS,
    )?;
    let fingerprint = clean_text(
        string_from(alert, &["fingerprint", "id"]).or_else(|| {
            Some(fingerprint(&format!(
                "{name}|{}|{source_instance}|{index}",
                stable_labels(&labels)
            )))
        }),
        "告警指纹",
        MAX_TEXT_CHARS,
    )?;
    let occurred_at = timestamp_from(alert, &["startsAt", "occurredAt", "occurred_at"])
        .or_else(|| timestamp_from(root, &["startsAt", "occurredAt", "occurred_at"]))
        .unwrap_or_else(Utc::now);

    labels.insert("state".into(), state.clone());
    let scope = infer_scope(&labels, &name);
    Ok(AlertInput {
        source: source.into(),
        source_instance,
        native_event_id: format!("{fingerprint}:{state}"),
        name,
        severity,
        summary,
        description: None,
        labels,
        scope,
        occurred_at,
    })
}

fn parse_single_alert(
    object: &Map<String, Value>,
    source: &str,
) -> Result<AlertInput, AlertPayloadError> {
    let mut labels = collect_labels(object.get("labels"), "labels")?;
    let name = clean_text(
        string_from(object, &["name", "alertname", "alert"])
            .or_else(|| label_value(&labels, "alertname"))
            .or_else(|| Some("external_alert".into())),
        "告警名称",
        MAX_TEXT_CHARS,
    )?;
    let state = normalize_state(string_from(object, &["status", "state"]).as_deref());
    let severity = clean_text(
        label_value(&labels, "severity")
            .or_else(|| string_from(object, &["severity", "priority"]))
            .or_else(|| Some("warning".into())),
        "严重级别",
        64,
    )?;
    let summary = clean_text(
        string_from(object, &["summary", "message", "description"])
            .or_else(|| nested_string(object, "annotations", &["summary", "description"]))
            .or_else(|| Some(name.clone())),
        "告警摘要",
        MAX_TEXT_CHARS,
    )?;
    let source_instance = clean_text(
        string_from(object, &["sourceInstance", "source_instance", "receiver"])
            .or_else(|| Some("webhook".into())),
        "来源实例",
        MAX_TEXT_CHARS,
    )?;
    let fingerprint = clean_text(
        string_from(
            object,
            &["fingerprint", "nativeEventId", "native_event_id", "id"],
        )
        .or_else(|| Some(fingerprint(&format!("{name}|{}", stable_labels(&labels))))),
        "告警指纹",
        MAX_TEXT_CHARS,
    )?;
    let occurred_at = timestamp_from(
        object,
        &["occurredAt", "occurred_at", "startsAt", "starts_at"],
    )
    .unwrap_or_else(Utc::now);

    labels.insert("state".into(), state.clone());
    let scope = infer_scope(&labels, &name);
    Ok(AlertInput {
        source: source.into(),
        source_instance,
        native_event_id: format!("{fingerprint}:{state}"),
        name,
        severity,
        summary,
        description: None,
        labels,
        scope,
        occurred_at,
    })
}

fn collect_labels(
    value: Option<&Value>,
    field: &str,
) -> Result<BTreeMap<String, String>, AlertPayloadError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| AlertPayloadError::Invalid(format!("{field} 必须是对象")))?;
    if object.len() > MAX_LABELS_PER_ALERT {
        return Err(AlertPayloadError::TooMany(MAX_LABELS_PER_ALERT));
    }
    let mut labels = BTreeMap::new();
    for (key, value) in object {
        if key.chars().count() > MAX_LABEL_CHARS {
            return Err(AlertPayloadError::Invalid("告警标签名过长".into()));
        }
        let Some(value) = scalar_string(value) else {
            continue;
        };
        if value.chars().count() > MAX_LABEL_VALUE_CHARS {
            return Err(AlertPayloadError::Invalid("告警标签值过长".into()));
        }
        labels.insert(key.clone(), value);
    }
    Ok(labels)
}

fn merge_labels(
    target: &mut BTreeMap<String, String>,
    incoming: BTreeMap<String, String>,
) -> Result<(), AlertPayloadError> {
    if target.len().saturating_add(incoming.len()) > MAX_LABELS_PER_ALERT {
        return Err(AlertPayloadError::TooMany(MAX_LABELS_PER_ALERT));
    }
    target.extend(incoming);
    Ok(())
}

fn string_from(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(scalar_string))
}

fn nested_string(object: &Map<String, Value>, object_key: &str, keys: &[&str]) -> Option<String> {
    object
        .get(object_key)
        .and_then(Value::as_object)
        .and_then(|nested| string_from(nested, keys))
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn label_value(labels: &BTreeMap<String, String>, key: &str) -> Option<String> {
    labels.get(key).cloned()
}

fn clean_text(
    value: Option<String>,
    field: &str,
    max_chars: usize,
) -> Result<String, AlertPayloadError> {
    let value = value.unwrap_or_default().trim().to_owned();
    if value.is_empty() {
        return Err(AlertPayloadError::Invalid(format!("{field}不能为空")));
    }
    if value.chars().count() > max_chars {
        return Err(AlertPayloadError::Invalid(format!("{field}过长")));
    }
    Ok(value)
}

fn normalize_state(value: Option<&str>) -> String {
    match value
        .unwrap_or("firing")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "resolved" | "resolve" | "ok" | "healthy" | "up" => "resolved".into(),
        _ => "firing".into(),
    }
}

fn timestamp_from(object: &Map<String, Value>, keys: &[&str]) -> Option<DateTime<Utc>> {
    let raw = string_from(object, keys)?;
    DateTime::parse_from_rfc3339(&raw)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn infer_scope(labels: &BTreeMap<String, String>, name: &str) -> Scope {
    if let Some(service) = labels
        .get("service")
        .or_else(|| labels.get("service_name"))
        .or_else(|| labels.get("job"))
    {
        return Scope::new("service", service.clone());
    }
    if let Some(instance) = labels.get("instance") {
        return Scope::new("instance", instance.clone());
    }
    Scope::new("alert", name.to_owned())
}

fn stable_labels(labels: &BTreeMap<String, String>) -> String {
    labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(";")
}

fn fingerprint(input: &str) -> String {
    let mut value = 0xcbf2_9ce4_8422_2325_u64;
    for byte in input.as_bytes() {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{value:016x}")
}

#[derive(Debug, Error)]
pub enum AlertPayloadError {
    #[error("Webhook 告警负载必须是 JSON 对象")]
    NotObject,
    #[error("Webhook 告警负载不能为空")]
    Empty,
    #[error("Webhook 告警数量不能超过 {0}")]
    TooMany(usize),
    #[error("Webhook 告警负载无效：{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alertmanager_payload_and_keeps_state_in_the_alert_identity() {
        let payload = serde_json::json!({
            "status": "firing",
            "receiver": "koi",
            "alerts": [{
                "status": "firing",
                "labels": {
                    "alertname": "ApiDown",
                    "severity": "critical",
                    "service": "api"
                },
                "annotations": {"summary": "API 不可用"},
                "fingerprint": "abc123"
            }]
        });
        let alerts = parse_webhook_alerts(&payload, ALERTMANAGER_SOURCE_NAME).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source, ALERTMANAGER_SOURCE_NAME);
        assert_eq!(alerts[0].native_event_id, "abc123:firing");
        assert_eq!(alerts[0].scope, Scope::new("service", "api"));
        assert_eq!(alerts[0].labels["state"], "firing");
    }

    #[test]
    fn parses_a_normalized_single_alert() {
        let payload = serde_json::json!({
            "name": "disk_full",
            "severity": "warning",
            "summary": "磁盘空间不足",
            "status": "resolved",
            "sourceInstance": "server-1",
            "nativeEventId": "disk-1",
            "labels": {"service": "storage"}
        });
        let alerts = parse_webhook_alerts(&payload, WEBHOOK_SOURCE_NAME).unwrap();
        assert_eq!(alerts[0].native_event_id, "disk-1:resolved");
        assert_eq!(alerts[0].scope, Scope::new("service", "storage"));
    }
}
